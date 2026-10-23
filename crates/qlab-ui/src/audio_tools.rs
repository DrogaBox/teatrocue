//! Convertidor de audio (con `ffmpeg`) y copia de pistas de un CD de audio a archivos.
//!
//! Todo corre en un hilo aparte ([`Job`]): la interfaz sigue respondiendo y se puede cancelar. El resultado de cada archivo
//! llega por un canal como [`Event`].

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;

// ---------------------------------------------------------------------------------------------
// Formatos y opciones
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Format {
    /// WAV sin pérdida, 16 bits (el que usan los CD).
    Wav16,
    /// WAV sin pérdida, 24 bits.
    Wav24,
    /// FLAC sin pérdida (la mitad de tamaño que WAV).
    Flac,
    /// MP3 con pérdida.
    Mp3,
    /// OGG Vorbis con pérdida.
    Ogg,
}

impl Format {
    pub const ALL: [Format; 5] = [Format::Wav16, Format::Wav24, Format::Flac, Format::Mp3, Format::Ogg];

    pub fn label(self) -> &'static str {
        match self {
            Format::Wav16 => "WAV 16 bits (calidad CD)",
            Format::Wav24 => "WAV 24 bits",
            Format::Flac => "FLAC (sin pérdida, ocupa la mitad)",
            Format::Mp3 => "MP3",
            Format::Ogg => "OGG Vorbis",
        }
    }

    pub fn ext(self) -> &'static str {
        match self {
            Format::Wav16 | Format::Wav24 => "wav",
            Format::Flac => "flac",
            Format::Mp3 => "mp3",
            Format::Ogg => "ogg",
        }
    }

    pub fn lossy(self) -> bool {
        matches!(self, Format::Mp3 | Format::Ogg)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Channels {
    Keep,
    Mono,
    Stereo,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ConvertOptions {
    pub format: Format,
    /// Calidad de los formatos con pérdida: kbps de MP3 (128, 192, 256, 320). OGG usa el equivalente.
    pub kbps: u32,
    /// `None`: la misma frecuencia que el original.
    pub sample_rate: Option<u32>,
    pub channels: Channels,
    /// Igualar el volumen percibido (EBU R128, -16 LUFS): varias pistas de distinto origen suenan parejas.
    pub normalize: bool,
}

impl Default for ConvertOptions {
    fn default() -> Self {
        Self { format: Format::Wav16, kbps: 320, sample_rate: None, channels: Channels::Keep, normalize: false }
    }
}

fn ffmpeg() -> Command {
    let mut c = Command::new("ffmpeg");
    c.stdin(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    c
}

/// Los argumentos de `ffmpeg` entre la entrada y la salida para estas opciones.
pub fn codec_args(o: &ConvertOptions) -> Vec<String> {
    let mut a: Vec<String> = ["-vn", "-map", "0:a:0", "-map_metadata", "0"].map(String::from).to_vec();
    if o.normalize {
        a.extend(["-af", "loudnorm=I=-16:TP=-1.5:LRA=11"].map(String::from));
    }
    match o.format {
        Format::Wav16 => a.extend(["-c:a", "pcm_s16le"].map(String::from)),
        Format::Wav24 => a.extend(["-c:a", "pcm_s24le"].map(String::from)),
        Format::Flac => a.extend(["-c:a", "flac", "-compression_level", "8"].map(String::from)),
        Format::Mp3 => a.extend(["-c:a", "libmp3lame", "-b:a"].map(String::from).into_iter().chain([format!("{}k", o.kbps)])),
        Format::Ogg => a.extend(["-c:a", "libvorbis", "-q:a"].map(String::from).into_iter().chain([format!("{}", ogg_quality(o.kbps))])),
    }
    // loudnorm sube la frecuencia a 192 kHz por dentro: sin pedirla, se vuelve a la del original (o a 48 kHz).
    match (o.sample_rate, o.normalize) {
        (Some(r), _) => a.extend(["-ar".to_string(), r.to_string()]),
        (None, true) => a.extend(["-ar".to_string(), "48000".to_string()]),
        (None, false) => {}
    }
    match o.channels {
        Channels::Keep => {}
        Channels::Mono => a.extend(["-ac", "1"].map(String::from)),
        Channels::Stereo => a.extend(["-ac", "2"].map(String::from)),
    }
    a
}

fn ogg_quality(kbps: u32) -> u32 {
    match kbps {
        0..=128 => 4,
        129..=192 => 6,
        193..=256 => 8,
        _ => 10,
    }
}

/// `dir/stem.ext`, o `dir/stem (2).ext` si ya existe (nunca se pisa un archivo).
pub fn unique_path(dir: &Path, stem: &str, ext: &str) -> PathBuf {
    let stem = sanitize(stem);
    let mut p = dir.join(format!("{stem}.{ext}"));
    let mut n = 2;
    while p.exists() {
        p = dir.join(format!("{stem} ({n}).{ext}"));
        n += 1;
    }
    p
}

/// Un nombre de archivo válido en Windows y Linux.
pub fn sanitize(name: &str) -> String {
    let s: String = name.chars().map(|c| if matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') || c.is_control() { '_' } else { c }).collect();
    let s = s.trim().trim_end_matches('.').trim().to_string();
    if s.is_empty() {
        "audio".into()
    } else {
        s
    }
}

// ---------------------------------------------------------------------------------------------
// Trabajos en segundo plano
// ---------------------------------------------------------------------------------------------

/// Lo que cada hilo informa sobre cada elemento (un archivo o una pista).
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    Started(usize),
    /// Avance del elemento, de 0 a 1.
    Progress(usize, f32),
    Done(usize, PathBuf),
    /// El elemento no se pudo hacer; los demás siguen.
    Failed(usize, String),
    /// Terminó todo (o se canceló).
    Finished { cancelled: bool },
}

/// Un trabajo en marcha: se lo consulta con [`Job::poll`] y se lo cancela con [`Job::cancel`].
pub struct Job {
    rx: Receiver<Event>,
    cancel: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Job {
    fn spawn(name: &str, wake: impl Fn() + Send + 'static, run: impl FnOnce(&Sender<Event>, &AtomicBool) + Send + 'static) -> Job {
        let (tx, rx) = channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let c = cancel.clone();
        let handle = std::thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                run(&tx, &c);
                let _ = tx.send(Event::Finished { cancelled: c.load(Ordering::Relaxed) });
                wake();
            })
            .ok();
        Job { rx, cancel, handle }
    }

    pub fn poll(&self) -> Vec<Event> {
        self.rx.try_iter().collect()
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        self.cancel();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// ¿Hay `ffmpeg`?
pub fn ffmpeg_available() -> bool {
    qlab_engine::ffmpeg_pcm::available()
}

/// Convierte `inputs` a `dest` con las opciones dadas. `wake` despierta la interfaz cuando hay novedades.
pub fn convert(inputs: Vec<PathBuf>, dest: PathBuf, opts: ConvertOptions, wake: impl Fn() + Send + Sync + 'static) -> Job {
    let wake = Arc::new(wake);
    let w = wake.clone();
    Job::spawn("conversion-audio", move || w(), move |tx, cancel| {
        let _ = std::fs::create_dir_all(&dest);
        for (i, input) in inputs.iter().enumerate() {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            let _ = tx.send(Event::Started(i));
            wake();
            let stem = input.file_stem().map_or_else(|| "audio".to_string(), |s| s.to_string_lossy().into_owned());
            let out = unique_path(&dest, &stem, opts.format.ext());
            match convert_one(input, &out, &opts, cancel, &mut |f| {
                let _ = tx.send(Event::Progress(i, f));
                wake();
            }) {
                Ok(()) => {
                    let _ = tx.send(Event::Done(i, out));
                }
                Err(e) => {
                    let _ = tx.send(Event::Failed(i, e));
                }
            }
            wake();
        }
    })
}

/// Un archivo. Se escribe con nombre provisional y se renombra al terminar: nunca queda un archivo a medias con el nombre bueno.
pub fn convert_one(input: &Path, out: &Path, opts: &ConvertOptions, cancel: &AtomicBool, progress: &mut dyn FnMut(f32)) -> Result<(), String> {
    if !ffmpeg_available() {
        return Err("no se encontró ffmpeg (viene en el paquete de instalación)".into());
    }
    let tmp = out.with_file_name(format!(".{}.parcial", out.file_name().map_or_else(|| "audio".into(), |n| n.to_string_lossy().into_owned())));
    let mut cmd = ffmpeg();
    cmd.args(["-hide_banner", "-nostdin", "-y", "-progress", "pipe:1", "-nostats", "-i"]).arg(input).args(codec_args(opts)).args(["-f", muxer(opts.format)]).arg(&tmp);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("no se pudo ejecutar ffmpeg: {e}"))?;
    let stdout = child.stdout.take().ok_or("ffmpeg no abrió su salida")?;
    let mut stderr = child.stderr.take().ok_or("ffmpeg no abrió su salida de errores")?;
    // La duración la dice ffmpeg al abrir el archivo («Duration: 00:03:21.45»), en su salida de texto.
    let errors = Arc::new(std::sync::Mutex::new(String::new()));
    let e2 = errors.clone();
    let err_thread = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s);
        if let Ok(mut g) = e2.lock() {
            *g = s;
        }
    });
    let mut duration: Option<f64> = None;
    let mut lines = BufReader::new(stdout).lines();
    // Mientras ffmpeg trabaja, el texto de stderr se consulta de a ratos para conocer la duración.
    loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = err_thread.join();
            let _ = std::fs::remove_file(&tmp);
            return Err("cancelado".into());
        }
        let Some(Ok(line)) = lines.next() else { break };
        if duration.is_none() {
            duration = errors.lock().ok().and_then(|g| parse_duration(&g));
        }
        if let (Some(us), Some(d)) = (line.strip_prefix("out_time_us=").and_then(|v| v.trim().parse::<f64>().ok()), duration) {
            if d > 0.0 {
                progress(((us / 1e6) / d).clamp(0.0, 1.0) as f32);
            }
        }
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    let _ = err_thread.join();
    if !status.success() {
        let _ = std::fs::remove_file(&tmp);
        let text = errors.lock().map(|g| g.clone()).unwrap_or_default();
        let last = text.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("ffmpeg falló").trim().to_string();
        return Err(last);
    }
    std::fs::rename(&tmp, out).map_err(|e| format!("no se pudo guardar {}: {e}", out.display()))?;
    progress(1.0);
    Ok(())
}

fn muxer(f: Format) -> &'static str {
    match f {
        Format::Wav16 | Format::Wav24 => "wav",
        Format::Flac => "flac",
        Format::Mp3 => "mp3",
        Format::Ogg => "ogg",
    }
}

/// «Duration: 01:02:03.45» de la salida de ffmpeg, en segundos.
pub fn parse_duration(text: &str) -> Option<f64> {
    let rest = text.split("Duration:").nth(1)?.trim_start();
    let t = rest.split(',').next()?.trim();
    let mut parts = t.split(':');
    let (h, m, s): (f64, f64, f64) = (parts.next()?.parse().ok()?, parts.next()?.parse().ok()?, parts.next()?.parse().ok()?);
    Some(h * 3600.0 + m * 60.0 + s)
}

// ---------------------------------------------------------------------------------------------
// Copia de CD de audio
// ---------------------------------------------------------------------------------------------

/// Bytes de un sector de audio de un CD: 588 muestras estéreo de 16 bits.
pub const SECTOR_BYTES: usize = 2352;
/// Sectores por segundo de un CD.
pub const SECTORS_PER_SEC: u32 = 75;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Track {
    pub number: u8,
    /// Primer sector (LBA, el 0 es el inicio del área de datos del disco).
    pub start: u32,
    pub sectors: u32,
    /// `false` en una pista de datos (CD mixto): no se puede copiar como audio.
    pub audio: bool,
}

impl Track {
    pub fn secs(&self) -> f64 {
        f64::from(self.sectors) / f64::from(SECTORS_PER_SEC)
    }
}

/// Lectora de CD: lee el índice (TOC) y sectores de audio crudos.
pub trait Drive: Send {
    fn name(&self) -> String;
    fn tracks(&mut self) -> Result<Vec<Track>, String>;
    /// Lee `count` sectores desde `lba` (2352 bytes cada uno).
    fn read(&mut self, lba: u32, count: u32) -> Result<Vec<u8>, String>;
}

/// Qué pasó al copiar una pista.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RipReport {
    pub sectors: u32,
    /// Sectores que la lectora no pudo leer (quedan en silencio).
    pub unreadable: u32,
}

/// Cabecera de un WAV PCM 16 bits estéreo 44,1 kHz con `data_len` bytes de audio.
pub fn wav_header(data_len: u32) -> [u8; 44] {
    let mut h = [0u8; 44];
    h[0..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&(36 + data_len).to_le_bytes());
    h[8..16].copy_from_slice(b"WAVEfmt ");
    h[16..20].copy_from_slice(&16u32.to_le_bytes());
    h[20..22].copy_from_slice(&1u16.to_le_bytes()); // PCM
    h[22..24].copy_from_slice(&2u16.to_le_bytes());
    h[24..28].copy_from_slice(&44_100u32.to_le_bytes());
    h[28..32].copy_from_slice(&(44_100u32 * 4).to_le_bytes());
    h[32..34].copy_from_slice(&4u16.to_le_bytes());
    h[34..36].copy_from_slice(&16u16.to_le_bytes());
    h[36..40].copy_from_slice(b"data");
    h[40..44].copy_from_slice(&data_len.to_le_bytes());
    h
}

/// Sectores por lectura: 16 es lo que aguantan sin quejarse todas las lectoras.
const CHUNK: u32 = 16;

/// Copia una pista a un WAV. Lo que la lectora no logra leer (tras reintentar sector por sector) queda en silencio y se cuenta.
pub fn rip_track(drive: &mut dyn Drive, track: &Track, out: &Path, cancel: &AtomicBool, progress: &mut dyn FnMut(f32)) -> Result<RipReport, String> {
    if !track.audio {
        return Err("es una pista de datos, no de audio".into());
    }
    let tmp = out.with_file_name(format!(".{}.parcial", out.file_name().map_or_else(|| "pista".into(), |n| n.to_string_lossy().into_owned())));
    let mut file = std::fs::File::create(&tmp).map_err(|e| format!("no se pudo crear {}: {e}", tmp.display()))?;
    let data_len = track.sectors.checked_mul(SECTOR_BYTES as u32).ok_or("pista demasiado larga")?;
    let result = (|| -> Result<RipReport, String> {
        file.write_all(&wav_header(data_len)).map_err(|e| e.to_string())?;
        let mut unreadable = 0;
        let mut done = 0;
        while done < track.sectors {
            if cancel.load(Ordering::Relaxed) {
                return Err("cancelado".into());
            }
            let n = CHUNK.min(track.sectors - done);
            let lba = track.start + done;
            match drive.read(lba, n) {
                Ok(b) if b.len() == n as usize * SECTOR_BYTES => file.write_all(&b).map_err(|e| e.to_string())?,
                _ => {
                    // Falló el bloque: sector por sector, con reintentos; el que no se pueda leer queda en silencio.
                    for s in 0..n {
                        let mut got = None;
                        for _ in 0..3 {
                            if let Ok(b) = drive.read(lba + s, 1) {
                                if b.len() == SECTOR_BYTES {
                                    got = Some(b);
                                    break;
                                }
                            }
                        }
                        match got {
                            Some(b) => file.write_all(&b).map_err(|e| e.to_string())?,
                            None => {
                                unreadable += 1;
                                file.write_all(&[0u8; SECTOR_BYTES]).map_err(|e| e.to_string())?;
                            }
                        }
                    }
                }
            }
            done += n;
            progress(done as f32 / track.sectors.max(1) as f32);
        }
        file.flush().map_err(|e| e.to_string())?;
        Ok(RipReport { sectors: track.sectors, unreadable })
    })();
    drop(file);
    match result {
        Ok(r) => {
            std::fs::rename(&tmp, out).map_err(|e| format!("no se pudo guardar {}: {e}", out.display()))?;
            Ok(r)
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Un pedido de copia: la pista y el nombre (sin extensión) con que se guarda.
#[derive(Clone, Debug)]
pub struct RipItem {
    pub track: Track,
    pub name: String,
}

/// Copia las pistas de `drive` a `dest` y, si `format` no es WAV 16 bits, las convierte (el WAV intermedio se borra).
/// Cada `Done` trae el archivo final; las pistas con sectores ilegibles avisan en un `Failed` aparte solo si no se pudo copiar.
pub fn rip(mut drive: Box<dyn Drive>, items: Vec<RipItem>, dest: PathBuf, opts: ConvertOptions, wake: impl Fn() + Send + Sync + 'static) -> (Job, Arc<std::sync::Mutex<Vec<(usize, u32)>>>) {
    let damaged = Arc::new(std::sync::Mutex::new(Vec::new()));
    let d2 = damaged.clone();
    let wake = Arc::new(wake);
    let w = wake.clone();
    let job = Job::spawn("copia-cd", move || w(), move |tx, cancel| {
        let _ = std::fs::create_dir_all(&dest);
        for (i, it) in items.iter().enumerate() {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            let _ = tx.send(Event::Started(i));
            wake();
            let wav = unique_path(&dest, &it.name, "wav");
            // Con otro formato, la copia directa es la primera mitad del avance y la conversión la segunda.
            let convert_after = opts.format != Format::Wav16 || opts.normalize || opts.sample_rate.is_some() || opts.channels != Channels::Keep;
            let share = if convert_after { 0.5 } else { 1.0 };
            let report = rip_track(drive.as_mut(), &it.track, &wav, cancel, &mut |f| {
                let _ = tx.send(Event::Progress(i, f * share));
                wake();
            });
            match report {
                Err(e) => {
                    let _ = tx.send(Event::Failed(i, e));
                }
                Ok(r) => {
                    if r.unreadable > 0 {
                        if let Ok(mut g) = d2.lock() {
                            g.push((i, r.unreadable));
                        }
                    }
                    if !convert_after {
                        let _ = tx.send(Event::Done(i, wav));
                    } else {
                        let out = unique_path(&dest, &it.name, opts.format.ext());
                        match convert_one(&wav, &out, &opts, cancel, &mut |f| {
                            let _ = tx.send(Event::Progress(i, 0.5 + f * 0.5));
                            wake();
                        }) {
                            Ok(()) => {
                                let _ = std::fs::remove_file(&wav);
                                let _ = tx.send(Event::Done(i, out));
                            }
                            Err(e) => {
                                let _ = tx.send(Event::Failed(i, format!("copiada a {} pero no se pudo convertir: {e}", wav.display())));
                            }
                        }
                    }
                }
            }
            wake();
        }
    });
    (job, damaged)
}

// ---------------------------------------------------------------------------------------------
// Lectoras reales
// ---------------------------------------------------------------------------------------------

/// Lectoras de CD con un disco de audio puesto (o con cualquier disco: el TOC dice después si hay audio).
pub fn list_drives() -> Vec<Box<dyn Drive>> {
    #[cfg(windows)]
    {
        win_drive::list()
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}

/// ¿Este sistema sabe leer CDs de audio?
pub const CD_SUPPORTED: bool = cfg!(windows);

/// Pistas a partir de las posiciones (en sectores) del TOC: `starts[i]` es el inicio de la pista i y `leadout` el final del disco.
pub fn tracks_from_toc(entries: &[(u8, bool, u32)], leadout: u32) -> Vec<Track> {
    entries
        .iter()
        .enumerate()
        .map(|(i, &(number, audio, start))| {
            let end = entries.get(i + 1).map_or(leadout, |n| n.2);
            Track { number, start, sectors: end.saturating_sub(start), audio }
        })
        .collect()
}

/// Minutos:segundos:cuadros del TOC a número de sector (el disco empieza en 00:02:00).
pub fn msf_to_lba(m: u8, s: u8, f: u8) -> u32 {
    ((u32::from(m) * 60 + u32::from(s)) * SECTORS_PER_SEC + u32::from(f)).saturating_sub(150)
}

#[cfg(windows)]
mod win_drive {
    use super::{msf_to_lba, tracks_from_toc, Drive, Track, SECTOR_BYTES};
    use std::ffi::c_void;

    type Handle = *mut c_void;
    const INVALID_HANDLE: Handle = -1isize as Handle;
    const GENERIC_READ: u32 = 0x8000_0000;
    const FILE_SHARE_READ: u32 = 1;
    const FILE_SHARE_WRITE: u32 = 2;
    const OPEN_EXISTING: u32 = 3;
    const IOCTL_CDROM_READ_TOC: u32 = 0x0002_4000;
    const IOCTL_CDROM_RAW_READ: u32 = 0x0002_403E;
    const DRIVE_CDROM: u32 = 5;
    /// `CDDA` de `TRACK_MODE_TYPE`.
    const TRACK_MODE_CDDA: u32 = 2;

    extern "system" {
        fn CreateFileW(name: *const u16, access: u32, share: u32, security: *mut c_void, disposition: u32, flags: u32, template: Handle) -> Handle;
        fn DeviceIoControl(h: Handle, code: u32, input: *const c_void, in_len: u32, output: *mut c_void, out_len: u32, returned: *mut u32, overlapped: *mut c_void) -> i32;
        fn CloseHandle(h: Handle) -> i32;
        fn GetLogicalDrives() -> u32;
        fn GetDriveTypeW(root: *const u16) -> u32;
    }

    pub struct Cd {
        letter: char,
        handle: Handle,
    }

    // SAFETY: el handle se usa desde un solo hilo a la vez (el hilo de la copia).
    unsafe impl Send for Cd {}

    impl Drop for Cd {
        fn drop(&mut self) {
            // SAFETY: handle abierto por CreateFileW.
            unsafe {
                CloseHandle(self.handle);
            }
        }
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub fn list() -> Vec<Box<dyn Drive>> {
        let mut out: Vec<Box<dyn Drive>> = Vec::new();
        // SAFETY: llamadas simples de kernel32.
        let mask = unsafe { GetLogicalDrives() };
        for i in 0..26u32 {
            if mask & (1 << i) == 0 {
                continue;
            }
            let letter = (b'A' + i as u8) as char;
            let root = wide(&format!("{letter}:\\"));
            if unsafe { GetDriveTypeW(root.as_ptr()) } != DRIVE_CDROM {
                continue;
            }
            let path = wide(&format!("\\\\.\\{letter}:"));
            let h = unsafe { CreateFileW(path.as_ptr(), GENERIC_READ, FILE_SHARE_READ | FILE_SHARE_WRITE, std::ptr::null_mut(), OPEN_EXISTING, 0, std::ptr::null_mut()) };
            if h != INVALID_HANDLE {
                out.push(Box::new(Cd { letter, handle: h }));
            }
        }
        out
    }

    impl Drive for Cd {
        fn name(&self) -> String {
            format!("Lectora {}:", self.letter)
        }

        fn tracks(&mut self) -> Result<Vec<Track>, String> {
            // CDROM_TOC: Length (2, big endian), FirstTrack, LastTrack y 100 entradas de 8 bytes.
            let mut buf = [0u8; 4 + 100 * 8];
            let mut got = 0u32;
            // SAFETY: buffer propio de tamaño conocido.
            let ok = unsafe { DeviceIoControl(self.handle, IOCTL_CDROM_READ_TOC, std::ptr::null(), 0, buf.as_mut_ptr().cast(), buf.len() as u32, &mut got, std::ptr::null_mut()) };
            if ok == 0 {
                return Err("no hay un disco en la lectora, o no se pudo leer su índice".into());
            }
            let (first, last) = (buf[2], buf[3]);
            if first == 0 || last < first {
                return Err("el disco no tiene pistas".into());
            }
            let entry = |k: usize| &buf[4 + k * 8..4 + k * 8 + 8];
            let mut entries = Vec::new();
            for k in 0..usize::from(last - first + 1) {
                let e = entry(k);
                let control = e[1] & 0x0F;
                entries.push((e[2], control & 0x04 == 0, msf_to_lba(e[5], e[6], e[7])));
            }
            let lo = entry(usize::from(last - first + 1));
            let leadout = msf_to_lba(lo[5], lo[6], lo[7]);
            Ok(tracks_from_toc(&entries, leadout))
        }

        fn read(&mut self, lba: u32, count: u32) -> Result<Vec<u8>, String> {
            // RAW_READ_INFO: DiskOffset (i64, en bytes de 2048 por sector), SectorCount y TrackMode.
            let mut info = [0u8; 16];
            info[0..8].copy_from_slice(&(i64::from(lba) * 2048).to_le_bytes());
            info[8..12].copy_from_slice(&count.to_le_bytes());
            info[12..16].copy_from_slice(&TRACK_MODE_CDDA.to_le_bytes());
            let mut out = vec![0u8; count as usize * SECTOR_BYTES];
            let mut got = 0u32;
            // SAFETY: entradas y salida propias de tamaño conocido.
            let ok = unsafe { DeviceIoControl(self.handle, IOCTL_CDROM_RAW_READ, info.as_ptr().cast(), info.len() as u32, out.as_mut_ptr().cast(), out.len() as u32, &mut got, std::ptr::null_mut()) };
            if ok == 0 || got as usize != out.len() {
                return Err("la lectora no pudo leer el sector".into());
            }
            Ok(out)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    /// CD simulado: cada sector tiene un patrón que depende de su número (para comprobar el orden y el contenido).
    struct FakeCd {
        tracks: Vec<Track>,
        bad: Vec<u32>,
        reads: u32,
    }

    fn pattern(lba: u32) -> Vec<u8> {
        (0..SECTOR_BYTES).map(|i| ((lba as usize * 7 + i) % 251) as u8).collect()
    }

    impl Drive for FakeCd {
        fn name(&self) -> String {
            "CD de prueba".into()
        }
        fn tracks(&mut self) -> Result<Vec<Track>, String> {
            Ok(self.tracks.clone())
        }
        fn read(&mut self, lba: u32, count: u32) -> Result<Vec<u8>, String> {
            self.reads += 1;
            if (lba..lba + count).any(|s| self.bad.contains(&s)) {
                return Err("sector ilegible".into());
            }
            Ok((lba..lba + count).flat_map(pattern).collect())
        }
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("qlab-audiotools-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn wait(job: &Job) -> Vec<Event> {
        let mut all = Vec::new();
        let t0 = std::time::Instant::now();
        loop {
            all.extend(job.poll());
            if all.iter().any(|e| matches!(e, Event::Finished { .. })) {
                return all;
            }
            assert!(t0.elapsed() < std::time::Duration::from_secs(60), "tiempo agotado: {all:?}");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[test]
    fn the_cd_index_becomes_tracks_with_their_lengths_and_the_data_track_is_marked() {
        // Tres pistas de audio y una de datos al final (CD mixto); el lead-out cierra el disco.
        let t = tracks_from_toc(&[(1, true, 0), (2, true, 15_000), (3, true, 30_000), (4, false, 40_000)], 50_000);
        assert_eq!(t.iter().map(|t| (t.number, t.start, t.sectors, t.audio)).collect::<Vec<_>>(), vec![(1, 0, 15_000, true), (2, 15_000, 15_000, true), (3, 30_000, 10_000, true), (4, 40_000, 10_000, false)]);
        assert!((t[0].secs() - 200.0).abs() < 1e-9, "15 000 sectores a 75 por segundo son 200 s");
        // 00:02:00 es el sector 0; la posición del TOC cuenta los 2 s de pausa inicial.
        assert_eq!(msf_to_lba(0, 2, 0), 0);
        assert_eq!(msf_to_lba(3, 20, 15), (3 * 60 + 20) * 75 + 15 - 150);
    }

    #[test]
    fn a_ripped_track_is_a_valid_wav_with_exactly_the_sectors_in_order() {
        let dir = tmpdir("rip");
        let mut cd = FakeCd { tracks: vec![], bad: vec![], reads: 0 };
        let track = Track { number: 2, start: 100, sectors: 40, audio: true };
        let out = dir.join("Pista 02.wav");
        let mut last = 0.0;
        let r = rip_track(&mut cd, &track, &out, &AtomicBool::new(false), &mut |f| last = f).unwrap();
        assert_eq!(r, RipReport { sectors: 40, unreadable: 0 });
        assert!((last - 1.0).abs() < 1e-6);
        let bytes = std::fs::read(&out).unwrap();
        assert_eq!(bytes.len(), 44 + 40 * SECTOR_BYTES);
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(bytes[40..44].try_into().unwrap()) as usize, 40 * SECTOR_BYTES);
        assert_eq!(&bytes[44..44 + SECTOR_BYTES], pattern(100).as_slice());
        assert_eq!(&bytes[44 + 39 * SECTOR_BYTES..], pattern(139).as_slice());
        assert!(std::fs::read_dir(&dir).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().ends_with(".parcial")), "no queda el archivo provisional");
        // Un WAV de verdad: ffmpeg lo lee con la duración esperada (40 sectores = 0,533 s).
        if ffmpeg_available() {
            let o = ffmpeg().args(["-hide_banner", "-i"]).arg(&out).output().unwrap();
            let d = parse_duration(&String::from_utf8_lossy(&o.stderr)).expect("duración");
            assert!((d - 40.0 / 75.0).abs() < 0.02, "{d}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unreadable_sectors_are_retried_then_left_silent_and_counted_without_shifting_the_rest() {
        let dir = tmpdir("bad");
        let mut cd = FakeCd { tracks: vec![], bad: vec![105], reads: 0 };
        let track = Track { number: 1, start: 100, sectors: 20, audio: true };
        let out = dir.join("p.wav");
        let r = rip_track(&mut cd, &track, &out, &AtomicBool::new(false), &mut |_| {}).unwrap();
        assert_eq!(r.unreadable, 1);
        let b = std::fs::read(&out).unwrap();
        assert_eq!(b.len(), 44 + 20 * SECTOR_BYTES, "el largo no cambia: el tiempo no se corre");
        assert!(b[44 + 5 * SECTOR_BYTES..44 + 6 * SECTOR_BYTES].iter().all(|&x| x == 0), "el sector malo queda en silencio");
        assert_eq!(&b[44 + 6 * SECTOR_BYTES..44 + 7 * SECTOR_BYTES], pattern(106).as_slice(), "el siguiente está en su lugar");
        // Una pista de datos se rechaza, y cancelar no deja nada.
        let data = Track { number: 3, start: 0, sectors: 10, audio: false };
        assert!(rip_track(&mut cd, &data, &dir.join("d.wav"), &AtomicBool::new(false), &mut |_| {}).is_err());
        assert!(rip_track(&mut cd, &track, &dir.join("c.wav"), &AtomicBool::new(true), &mut |_| {}).is_err());
        assert!(!dir.join("c.wav").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_names_are_made_safe_and_never_overwrite_an_existing_file() {
        assert_eq!(sanitize("Giselle: acto 1/2?"), "Giselle_ acto 1_2_");
        assert_eq!(sanitize("  ... "), "audio");
        let dir = tmpdir("names");
        std::fs::write(dir.join("a.wav"), b"x").unwrap();
        assert_eq!(unique_path(&dir, "a", "wav"), dir.join("a (2).wav"));
        std::fs::write(dir.join("a (2).wav"), b"x").unwrap();
        assert_eq!(unique_path(&dir, "a", "wav"), dir.join("a (3).wav"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ffmpeg_settings_follow_the_options() {
        let a = codec_args(&ConvertOptions { format: Format::Mp3, kbps: 192, sample_rate: Some(44_100), channels: Channels::Stereo, normalize: true });
        let s = a.join(" ");
        assert!(s.contains("libmp3lame") && s.contains("192k") && s.contains("-ar 44100") && s.contains("-ac 2") && s.contains("loudnorm"), "{s}");
        assert!(codec_args(&ConvertOptions { normalize: true, ..Default::default() }).join(" ").contains("-ar 48000"), "loudnorm no deja la frecuencia en 192 kHz");
        assert!(!codec_args(&ConvertOptions::default()).join(" ").contains("-ar"));
        assert_eq!(parse_duration("Input #0\n  Duration: 01:02:03.50, start: 0.0"), Some(3723.5));
    }

    #[test]
    fn converting_real_files_to_every_format_works_in_a_batch_with_a_failure_in_the_middle() {
        if !ffmpeg_available() {
            eprintln!("sin ffmpeg: se omite");
            return;
        }
        let dir = tmpdir("conv");
        let wav = crate::waveform::write_test_wav("conv-a", 2, 96_000, 48_000, |i, _| ((i as f32) * 0.05).sin() * 0.5);
        let junk = dir.join("roto.mp3");
        std::fs::write(&junk, b"esto no es audio").unwrap();
        for fmt in Format::ALL {
            let dest = dir.join(fmt.ext()).join(format!("{fmt:?}"));
            let job = convert(vec![wav.clone(), junk.clone(), wav.clone()], dest.clone(), ConvertOptions { format: fmt, ..Default::default() }, || {});
            let ev = wait(&job);
            let done: Vec<_> = ev.iter().filter_map(|e| if let Event::Done(i, p) = e { Some((*i, p.clone())) } else { None }).collect();
            assert_eq!(done.iter().map(|d| d.0).collect::<Vec<_>>(), vec![0, 2], "{fmt:?}: el archivo roto falla y los demás siguen: {ev:?}");
            assert!(ev.iter().any(|e| matches!(e, Event::Failed(1, _))), "{fmt:?}");
            assert!(ev.iter().any(|e| matches!(e, Event::Progress(0, f) if *f > 0.99)), "{fmt:?}: informa el avance");
            assert!(done[0].1.extension().unwrap() == fmt.ext());
            assert_ne!(done[0].1, done[1].1, "el segundo no pisa al primero");
            assert!(std::fs::read_dir(&dest).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().ends_with(".parcial")));
            // El resultado se puede leer con el decodificador propio del motor y dura lo mismo (2 s).
            let scan = qlab_engine::disk_streamer::scan_peaks(&done[0].1, 64, &AtomicBool::new(false)).unwrap().expect("se decodifica");
            assert!((scan.total_frames as f64 / f64::from(scan.sample_rate) - 2.0).abs() < 0.1, "{fmt:?}: {} frames a {} Hz", scan.total_frames, scan.sample_rate);
        }
        let _ = std::fs::remove_file(&wav);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ripping_a_fake_disc_can_convert_each_track_and_cancel_leaves_nothing_half_written() {
        let dir = tmpdir("ripjob");
        let tracks = vec![Track { number: 1, start: 0, sectors: 150, audio: true }, Track { number: 2, start: 150, sectors: 75, audio: true }];
        let items: Vec<RipItem> = tracks.iter().map(|t| RipItem { track: t.clone(), name: format!("Pista {:02}", t.number) }).collect();
        // Solo WAV: copia directa.
        let cd = Box::new(FakeCd { tracks: tracks.clone(), bad: vec![160], reads: 0 });
        let (job, damaged) = rip(cd, items.clone(), dir.join("wav"), ConvertOptions::default(), || {});
        let ev = wait(&job);
        assert_eq!(ev.iter().filter(|e| matches!(e, Event::Done(..))).count(), 2, "{ev:?}");
        assert_eq!(*damaged.lock().unwrap(), vec![(1, 1)], "avisa qué pista tuvo un sector ilegible");
        assert!(dir.join("wav/Pista 01.wav").is_file());
        if ffmpeg_available() {
            // A FLAC: el WAV intermedio se borra y queda solo el FLAC.
            let cd = Box::new(FakeCd { tracks: tracks.clone(), bad: vec![], reads: 0 });
            let (job, _) = rip(cd, items.clone(), dir.join("flac"), ConvertOptions { format: Format::Flac, ..Default::default() }, || {});
            let ev = wait(&job);
            assert_eq!(ev.iter().filter(|e| matches!(e, Event::Done(..))).count(), 2, "{ev:?}");
            let names: Vec<String> = std::fs::read_dir(dir.join("flac")).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
            assert!(names.iter().all(|n| n.ends_with(".flac")), "{names:?}");
        }
        // Cancelar antes de empezar: no queda nada.
        let cd = Box::new(FakeCd { tracks, bad: vec![], reads: 0 });
        let (job, _) = rip(cd, items, dir.join("cancel"), ConvertOptions::default(), || {});
        job.cancel();
        let ev = wait(&job);
        assert!(matches!(ev.last(), Some(Event::Finished { .. })));
        let n = std::fs::read_dir(dir.join("cancel")).map(|d| d.count()).unwrap_or(0);
        assert!(n <= 2, "como mucho lo copiado antes de cancelar");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
