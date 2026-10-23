//! Streaming de disco con Symphonia (spec §1.4.A y §2.2).
//!
//! * Archivos cortos (`<= 15 MiB` decodificados) se cargan enteros en RAM.
//! * Los largos tienen regiones **fijadas** (`StartPinned`: inicio de la región y de los slices con
//!   bucle, siempre residentes) y dos ventanas ping-pong **A/B** de `window_frames` frames que un
//!   pool de hilos de disco rellena por adelantado.
//!
//! El hilo de audio solo lee RAM: [`StreamingVoiceReader::maintain`] y [`ReaderView`] son lecturas y
//! escrituras atómicas, sin bloqueos, asignaciones, syscalls ni E/S. Los hilos de disco no esperan
//! en señales del hilo RT: sondean cada 2 ms las peticiones que este deja marcadas.
//!
//! # Protocolo de propiedad de cada ventana
//!
//! ```text
//! FREE --(RT)--> REQUESTED --(disco)--> LOADING --(disco)--> READY --(RT)--> FREE
//! ```
//!
//! Solo el hilo RT pasa `FREE→REQUESTED` y `READY→FREE`; solo el disco toca la ventana en
//! `REQUESTED`/`LOADING`. Así nadie lee y escribe el mismo búfer a la vez sin necesitar `unsafe`.

use std::fs::File;
use std::io::ErrorKind;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{Decoder, DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use crate::rt_command::MAX_INPUT_CHANNELS;

/// Frames por ventana ping-pong y por región fijada (~5,46 s a 48 kHz).
pub const DEFAULT_WINDOW_FRAMES: usize = 262_144;
/// Límite de residencia completa: 15 MiB decodificados.
pub const RESIDENT_LIMIT_BYTES: usize = 15 * 1024 * 1024;
/// Regiones fijadas por voz: el inicio de la región más los slices con bucle.
pub const MAX_PINNED_REGIONS: usize = 8;

const SLOT_FREE: u8 = 0;
const SLOT_REQUESTED: u8 = 1;
const SLOT_LOADING: u8 = 2;
const SLOT_READY: u8 = 3;

const POLL_INTERVAL: Duration = Duration::from_millis(2);

// ---------------------------------------------------------------------------------------------
// Errores y configuración
// ---------------------------------------------------------------------------------------------

#[derive(Debug)]
pub enum StreamError {
    Io(std::io::Error),
    Unsupported(String),
    Decode(String),
    NoAudioTrack,
    BadChannelCount(usize),
}

impl std::fmt::Display for StreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "error de E/S: {e}"),
            Self::Unsupported(s) => write!(f, "formato no soportado: {s}"),
            Self::Decode(s) => write!(f, "error de decodificación: {s}"),
            Self::NoAudioTrack => write!(f, "el archivo no tiene pista de audio"),
            Self::BadChannelCount(n) => write!(f, "{n} canales fuera de 1..={MAX_INPUT_CHANNELS}"),
        }
    }
}

impl std::error::Error for StreamError {}

impl From<std::io::Error> for StreamError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<SymError> for StreamError {
    fn from(e: SymError) -> Self {
        match e {
            SymError::IoError(e) => Self::Io(e),
            SymError::Unsupported(s) => Self::Unsupported(s.to_string()),
            other => Self::Decode(other.to_string()),
        }
    }
}

#[derive(Clone, Debug)]
pub struct StreamConfig {
    /// Inicio de la región reproducible: de aquí se fija `StartPinned`.
    pub start_frame: u64,
    /// Inicios de slices con `play_count > 1`: también quedan fijados (salto de bucle sin latencia).
    pub extra_pinned_starts: Vec<u64>,
    pub window_frames: usize,
    pub pinned_frames: usize,
    pub resident_limit_bytes: usize,
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            start_frame: 0,
            extra_pinned_starts: Vec::new(),
            window_frames: DEFAULT_WINDOW_FRAMES,
            pinned_frames: DEFAULT_WINDOW_FRAMES,
            resident_limit_bytes: RESIDENT_LIMIT_BYTES,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileInfo {
    pub sample_rate: u32,
    pub channels: usize,
    /// `None` si el contenedor no declara la duración.
    pub total_frames: Option<u64>,
    /// `true` si el archivo entero quedó en RAM.
    pub resident: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StreamStats {
    pub windows_loaded: u64,
    pub load_errors: u64,
}

// ---------------------------------------------------------------------------------------------
// Almacenamiento alineado a 64 bytes
// ---------------------------------------------------------------------------------------------

#[repr(align(64))]
struct Line([AtomicU32; 16]);

impl Line {
    fn new() -> Self {
        Line([const { AtomicU32::new(0) }; 16])
    }
}

/// Muestras `f32` en líneas de caché de 64 bytes. Se guardan como bits en atómicos `Relaxed`: el
/// orden lo dan los estados de ventana (Release/Acquire), y en x86 equivale a una carga normal.
pub(crate) struct AlignedBuf {
    lines: Box<[Line]>,
    len: usize,
}

impl AlignedBuf {
    pub(crate) fn new(len: usize) -> Self {
        let lines: Vec<Line> = (0..len.div_ceil(16)).map(|_| Line::new()).collect();
        Self { lines: lines.into_boxed_slice(), len }
    }

    #[inline(always)]
    pub(crate) fn load(&self, i: usize) -> f32 {
        f32::from_bits(self.lines[i >> 4].0[i & 15].load(Ordering::Relaxed))
    }

    #[inline(always)]
    pub(crate) fn store(&self, i: usize, v: f32) {
        self.lines[i >> 4].0[i & 15].store(v.to_bits(), Ordering::Relaxed);
    }

    fn fill_from(&self, data: &[f32]) {
        debug_assert!(data.len() <= self.len);
        for (i, &v) in data.iter().enumerate() {
            self.store(i, v);
        }
    }
}

struct Pinned {
    origin: u64,
    frames: u64,
    data: AlignedBuf,
}

struct Slot {
    state: AtomicU8,
    req_origin: AtomicU64,
    origin: AtomicU64,
    frames: AtomicU64,
    data: AlignedBuf,
}

impl Slot {
    fn new(len: usize) -> Self {
        Self {
            state: AtomicU8::new(SLOT_FREE),
            req_origin: AtomicU64::new(0),
            origin: AtomicU64::new(0),
            frames: AtomicU64::new(0),
            data: AlignedBuf::new(len),
        }
    }
}

struct Shared {
    channels: usize,
    sample_rate: u32,
    window_frames: u64,
    resident: bool,
    /// Primer frame que ya no existe (fin del archivo); `u64::MAX` si se desconoce.
    eof_frame: AtomicU64,
    closed: AtomicBool,
    pinned: Box<[Pinned]>,
    slots: [Slot; 2],
    windows_loaded: AtomicU64,
    load_errors: AtomicU64,
}

impl Shared {
    fn resident(channels: usize, sample_rate: u32, pinned: Vec<Pinned>, eof: u64) -> Self {
        Self {
            channels,
            sample_rate,
            window_frames: 0,
            resident: true,
            eof_frame: AtomicU64::new(eof),
            closed: AtomicBool::new(false),
            pinned: pinned.into_boxed_slice(),
            slots: [Slot::new(0), Slot::new(0)],
            windows_loaded: AtomicU64::new(0),
            load_errors: AtomicU64::new(0),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Lector (lado RT)
// ---------------------------------------------------------------------------------------------

/// Fuente de audio de una voz. `Voice` lo contiene; el hilo de audio solo usa `maintain` y `view`.
pub struct StreamingVoiceReader {
    shared: Arc<Shared>,
}

impl Default for StreamingVoiceReader {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for StreamingVoiceReader {
    fn drop(&mut self) {
        // El pool ve la marca y descarta el trabajo.
        self.shared.closed.store(true, Ordering::Release);
    }
}

impl std::fmt::Debug for StreamingVoiceReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamingVoiceReader")
            .field("channels", &self.shared.channels)
            .field("resident", &self.shared.resident)
            .finish()
    }
}

impl StreamingVoiceReader {
    /// Lector vacío: toda lectura devuelve silencio.
    pub fn new() -> Self {
        Self { shared: Arc::new(Shared::resident(0, 0, Vec::new(), 0)) }
    }

    /// Lector residente a partir de PCM `f32` entrelazado (`origin_frame` = frame de `data[0]`).
    pub fn from_interleaved(channels: usize, origin_frame: u64, data: Box<[f32]>) -> Self {
        if channels == 0 {
            return Self::new();
        }
        let frames = (data.len() / channels) as u64;
        let buf = AlignedBuf::new(data.len());
        buf.fill_from(&data);
        let pinned = vec![Pinned { origin: origin_frame, frames, data: buf }];
        Self { shared: Arc::new(Shared::resident(channels, 0, pinned, origin_frame + frames)) }
    }

    #[inline]
    pub fn channels(&self) -> usize {
        self.shared.channels
    }

    #[inline]
    pub fn sample_rate(&self) -> u32 {
        self.shared.sample_rate
    }

    /// `true` si todo el archivo está en RAM y no hay streaming.
    pub fn is_resident(&self) -> bool {
        self.shared.resident
    }

    pub fn stats(&self) -> StreamStats {
        StreamStats {
            windows_loaded: self.shared.windows_loaded.load(Ordering::Relaxed),
            load_errors: self.shared.load_errors.load(Ordering::Relaxed),
        }
    }

    /// Hilo RT, una vez por trozo: libera las ventanas ya recorridas y pide al disco las siguientes
    /// para mantener al menos una ventana de margen por delante de `cursor`. O(1), sin bloqueos.
    pub fn maintain(&self, cursor: u64) {
        let sh = &*self.shared;
        if sh.resident {
            return;
        }
        let window = sh.window_frames;

        for slot in &sh.slots {
            if slot.state.load(Ordering::Acquire) == SLOT_READY {
                let origin = slot.origin.load(Ordering::Relaxed);
                let end = origin + slot.frames.load(Ordering::Relaxed);
                // Pasada, o muy por delante tras un salto hacia atrás: ya no sirve.
                if cursor >= end || origin > cursor.saturating_add(window) {
                    slot.state.store(SLOT_FREE, Ordering::Release);
                }
            }
        }

        let eof = sh.eof_frame.load(Ordering::Relaxed);
        for _ in 0..2 {
            // Hasta dónde llega, sin huecos, lo cargado o pedido desde `cursor`.
            let mut frontier = cursor;
            for _ in 0..(MAX_PINNED_REGIONS + 2) {
                let mut next = frontier;
                for p in sh.pinned.iter() {
                    if p.origin <= frontier && frontier < p.origin + p.frames {
                        next = next.max(p.origin + p.frames);
                    }
                }
                for slot in &sh.slots {
                    let (origin, end) = match slot.state.load(Ordering::Acquire) {
                        SLOT_READY => {
                            let o = slot.origin.load(Ordering::Relaxed);
                            (o, o + slot.frames.load(Ordering::Relaxed))
                        }
                        SLOT_REQUESTED | SLOT_LOADING => {
                            let o = slot.req_origin.load(Ordering::Relaxed);
                            (o, o + window)
                        }
                        _ => continue,
                    };
                    if origin <= frontier && frontier < end {
                        next = next.max(end);
                    }
                }
                if next == frontier {
                    break;
                }
                frontier = next;
            }
            if frontier >= eof || frontier - cursor >= window {
                return;
            }
            let Some(slot) = sh.slots.iter().find(|s| s.state.load(Ordering::Acquire) == SLOT_FREE) else {
                return;
            };
            slot.req_origin.store(frontier, Ordering::Relaxed);
            slot.state.store(SLOT_REQUESTED, Ordering::Release);
        }
    }

    /// Instantánea de lo que se puede leer ahora. Las ventanas que pasen a `READY` después no
    /// aparecen hasta la siguiente vista; las que están en lectura nunca se reescriben.
    #[inline]
    pub fn view(&self) -> ReaderView<'_> {
        let sh = &*self.shared;
        let mut windows = [None, None];
        for (i, slot) in sh.slots.iter().enumerate() {
            if slot.state.load(Ordering::Acquire) == SLOT_READY {
                let origin = slot.origin.load(Ordering::Relaxed);
                let frames = slot.frames.load(Ordering::Relaxed);
                if frames > 0 {
                    windows[i] = Some(Window { origin, end: origin + frames, data: &slot.data });
                }
            }
        }
        ReaderView { channels: sh.channels, pinned: &sh.pinned, windows }
    }

    /// Atajo de `view().has_frame(frame)`.
    pub fn has_frame(&self, frame: u64) -> bool {
        self.view().has_frame(frame)
    }

    /// Atajo de `view().sample(frame, ch)`; en bucles usar `view()` una sola vez.
    pub fn sample(&self, frame: i64, ch: usize) -> f32 {
        self.view().sample(frame, ch)
    }
}

struct Window<'a> {
    origin: u64,
    end: u64,
    data: &'a AlignedBuf,
}

/// Vista de solo lectura sobre las regiones fijadas y las ventanas listas.
pub struct ReaderView<'a> {
    channels: usize,
    pinned: &'a [Pinned],
    windows: [Option<Window<'a>>; 2],
}

impl ReaderView<'_> {
    #[inline]
    pub fn channels(&self) -> usize {
        self.channels
    }

    #[inline]
    pub fn has_frame(&self, frame: u64) -> bool {
        self.pinned.iter().any(|p| frame >= p.origin && frame - p.origin < p.frames)
            || self.windows.iter().flatten().any(|w| frame >= w.origin && frame < w.end)
    }

    /// Muestra del canal `ch` en `frame`; `0.0` si no está cargada o `frame < 0`.
    #[inline(always)]
    pub fn sample(&self, frame: i64, ch: usize) -> f32 {
        if frame < 0 || ch >= self.channels {
            return 0.0;
        }
        let f = frame as u64;
        for p in self.pinned {
            if f >= p.origin && f - p.origin < p.frames {
                return p.data.load((f - p.origin) as usize * self.channels + ch);
            }
        }
        for w in self.windows.iter().flatten() {
            if f >= w.origin && f < w.end {
                return w.data.load((f - w.origin) as usize * self.channels + ch);
            }
        }
        0.0
    }
}

// ---------------------------------------------------------------------------------------------
// Sesión de decodificación (lado disco)
// ---------------------------------------------------------------------------------------------

struct Session {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn Decoder>,
    track_id: u32,
    channels: usize,
    sample_rate: u32,
    total_frames: Option<u64>,
    /// Índice del próximo frame que se consumirá (de `left` o de la siguiente decodificación).
    pos: u64,
    /// Frames por descartar tras una búsqueda imprecisa.
    skip: u64,
    left: Vec<f32>,
    left_off: usize,
    sbuf: Option<SampleBuffer<f32>>,
    ended: bool,
    /// Paquetes que no se pudieron decodificar y se cubrieron con silencio.
    skipped_packets: u64,
    path: std::path::PathBuf,
    /// Resultado de los últimos 128 paquetes (1 = falló): si fallan demasiados se pasa a `ffmpeg`.
    recent_bad: u128,
    /// Lector de respaldo: cuando está, es él quien entrega el audio.
    ff: Option<crate::ffmpeg_pcm::FfmpegPcm>,
    ff_tried: bool,
    /// Frame desde el que el audio del archivo está dañado (si se detectó): de ahí al final se entrega silencio.
    damaged_at: Option<u64>,
    /// Frame donde empezó la racha de paquetes fallidos en curso (el daño empieza ahí, no donde se confirmó).
    bad_run_start: Option<u64>,
    /// Frame desde el que se pasó a ffmpeg.
    ff_started_at: u64,
}

/// Archivos con audio dañado que ya se descubrieron en este proceso (ruta → frame desde el que están dañados). Así la forma de
/// onda, la reproducción y las recargas coinciden y no se vuelve a «escuchar» la zona rota (que sonaría a ruido).
fn damage_registry() -> &'static std::sync::Mutex<std::collections::HashMap<std::path::PathBuf, u64>> {
    static R: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<std::path::PathBuf, u64>>> = std::sync::OnceLock::new();
    R.get_or_init(Default::default)
}

fn known_damage(path: &Path) -> Option<u64> {
    damage_registry().lock().ok()?.get(path).copied()
}

fn remember_damage(path: &Path, frame: u64) {
    if let Ok(mut r) = damage_registry().lock() {
        r.insert(path.to_path_buf(), frame);
    }
}

/// Paquetes fallidos entre los últimos 32 a partir de los cuales el archivo se considera ilegible para Symphonia (un cuadro
/// suelto dañado no cuenta: se cubre con silencio y se sigue).
const FALLBACK_BAD_PACKETS: u32 = 8;
/// Frames que entrega el lector de respaldo por vuelta.
const FF_CHUNK_FRAMES: usize = 4_096;

/// `n_frames` de los parámetros del códec en muestras. Algunos contenedores (Matroska) lo dan en unidades de su base
/// de tiempo (p. ej. milisegundos); si esa base no es `1/fs`, se convierte por la duración.
fn frames_from_param(n: u64, time_base: Option<symphonia::core::units::TimeBase>, sample_rate: Option<u32>) -> u64 {
    match (time_base, sample_rate) {
        (Some(tb), Some(sr)) if tb.numer != 1 || tb.denom != sr => (n as f64 * tb.numer as f64 / tb.denom as f64 * sr as f64).round() as u64,
        _ => n,
    }
}

impl Session {
    fn open(path: &Path) -> Result<Self, StreamError> {
        let file = File::open(path)?;
        let mss = MediaSourceStream::new(Box::new(file), Default::default());
        let mut hint = Hint::new();
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            hint.with_extension(ext);
        }
        let probed = symphonia::default::get_probe().format(
            &hint,
            mss,
            &FormatOptions { enable_gapless: true, ..Default::default() },
            &MetadataOptions::default(),
        )?;
        let format = probed.format;
        let track = format
            .tracks()
            .iter()
            .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
            .ok_or(StreamError::NoAudioTrack)?;
        let params = track.codec_params.clone();
        let track_id = track.id;
        let decoder = symphonia::default::get_codecs().make(&params, &DecoderOptions::default())?;

        let mut s = Self {
            format,
            decoder,
            track_id,
            channels: params.channels.map_or(0, |c| c.count()),
            sample_rate: params.sample_rate.unwrap_or(0),
            total_frames: params.n_frames.map(|n| frames_from_param(n, params.time_base, params.sample_rate)),
            pos: 0,
            skip: 0,
            left: Vec::new(),
            left_off: 0,
            sbuf: None,
            ended: false,
            skipped_packets: 0,
            path: path.to_path_buf(),
            recent_bad: 0,
            ff: None,
            ff_tried: false,
            damaged_at: known_damage(path),
            bad_run_start: None,
            ff_started_at: 0,
        };
        if s.channels == 0 || s.sample_rate == 0 {
            // Algunos códecs solo conocen el formato al decodificar el primer paquete.
            s.fill_left()?;
            if s.channels == 0 || s.sample_rate == 0 {
                return Err(StreamError::Decode("no se pudo determinar el formato de audio".into()));
            }
        }
        if !(1..=MAX_INPUT_CHANNELS).contains(&s.channels) {
            return Err(StreamError::BadChannelCount(s.channels));
        }
        Ok(s)
    }

    /// Decodifica el siguiente paquete a `left`. Marca `ended` al llegar al final.
    fn fill_left(&mut self) -> Result<(), StreamError> {
        if self.ff.is_some() {
            return self.fill_left_ffmpeg();
        }
        loop {
            let packet = match self.format.next_packet() {
                Ok(p) => p,
                Err(SymError::IoError(e)) if e.kind() == ErrorKind::UnexpectedEof => {
                    self.ended = true;
                    return Ok(());
                }
                Err(SymError::ResetRequired) => {
                    self.ended = true;
                    return Ok(());
                }
                Err(e) => return Err(e.into()),
            };
            if packet.track_id() != self.track_id {
                continue;
            }
            match self.decoder.decode(&packet) {
                Ok(decoded) => {
                    self.recent_bad <<= 1;
                    let spec = *decoded.spec();
                    if self.channels == 0 {
                        self.channels = spec.channels.count();
                        self.sample_rate = spec.rate;
                    } else if spec.channels.count() != self.channels {
                        return Err(StreamError::Decode("cambió el número de canales".into()));
                    }
                    let capacity = decoded.capacity() as u64;
                    if self.sbuf.as_ref().map_or(true, |b| (b.capacity() as u64) < capacity * spec.channels.count() as u64) {
                        self.sbuf = Some(SampleBuffer::<f32>::new(capacity, spec));
                    }
                    let sbuf = self.sbuf.as_mut().expect("creado arriba");
                    sbuf.copy_interleaved_ref(decoded);
                    self.left.clear();
                    self.left.extend_from_slice(sbuf.samples());
                    self.left_off = 0;
                    if self.left.is_empty() {
                        continue;
                    }
                    return Ok(());
                }
                // Paquete corrupto o con una función que el decodificador no implementa (p. ej. un «coupling channel element» de AAC,
                // que aparece en algún punto de archivos largos): no se pierde el archivo entero. Se sustituye por silencio de la
                // misma duración, así lo que viene después sigue en su sitio (si se omitiera, todo se adelantaría).
                Err(SymError::DecodeError(_) | SymError::Unsupported(_) | SymError::IoError(_)) => {
                    self.skipped_packets += 1;
                    if self.recent_bad & 0xFFFF_FFFF == 0 {
                        self.bad_run_start = Some(self.pos);
                    }
                    self.recent_bad = (self.recent_bad << 1) | 1;
                    // Si fallan demasiados paquetes seguidos el archivo está dañado para este decodificador: se sigue con ffmpeg.
                    if (self.recent_bad & 0xFFFF_FFFF).count_ones() >= FALLBACK_BAD_PACKETS {
                        if self.start_ffmpeg() {
                            return self.fill_left_ffmpeg();
                        }
                        // Sin ffmpeg: seguir «decodificando» datos rotos daría ráfagas de ruido; mejor silencio hasta el final.
                        self.mark_damaged_from(self.bad_run_start.unwrap_or(self.pos));
                        self.left.clear();
                        self.left_off = 0;
                        return Ok(());
                    }
                    // Tras un fallo a mitad de un cuadro el estado interno del decodificador (AAC) queda a medias: se reinicia.
                    self.decoder.reset();
                    let frames = packet.dur.clamp(1, 1 << 16) as usize;
                    self.left.clear();
                    self.left.resize(frames * self.channels.max(1), 0.0);
                    self.left_off = 0;
                    return Ok(());
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// Pasa a leer con `ffmpeg` desde el frame en curso. `false` si no hay `ffmpeg` o no arrancó (se sigue con Symphonia).
    fn start_ffmpeg(&mut self) -> bool {
        if self.ff_tried || !crate::ffmpeg_pcm::available() {
            self.ff_tried = true;
            return false;
        }
        self.ff_tried = true;
        match crate::ffmpeg_pcm::FfmpegPcm::spawn(&self.path, self.pos, self.channels, self.sample_rate) {
            Ok(r) => {
                self.ff = Some(r);
                self.ff_started_at = self.pos;
                self.left.clear();
                self.left_off = 0;
                self.skip = 0;
                self.ended = false;
                true
            }
            Err(_) => false,
        }
    }

    fn fill_left_ffmpeg(&mut self) -> Result<(), StreamError> {
        let Some(ff) = self.ff.as_mut() else { return Ok(()) };
        self.left.clear();
        self.left_off = 0;
        let n = ff.read(FF_CHUNK_FRAMES, &mut self.left).map_err(StreamError::from)?;
        if n == 0 {
            // ffmpeg también se detuvo: si el archivo declara más duración, el resto está dañado y suena en silencio. Si no llegó
            // a entregar nada, el daño empieza donde empezó la racha de paquetes fallidos.
            let nothing = self.pos == self.ff_started_at;
            self.ff = None;
            self.mark_damaged_from(if nothing { self.bad_run_start.unwrap_or(self.pos) } else { self.pos });
        }
        Ok(())
    }

    /// Anota (y recuerda para todo el proceso) que el audio está dañado desde el frame en curso, si falta más de medio segundo.
    fn mark_damaged_from(&mut self, from: u64) {
        let from = from.min(self.pos);
        if self.damaged_at.is_none() && self.total_frames.is_some_and(|t| from + u64::from(self.sample_rate) / 2 < t) {
            self.damaged_at = Some(from);
            remember_damage(&self.path, from);
        }
    }

    fn seek(&mut self, ts: u64) -> Result<(), StreamError> {
        self.left.clear();
        self.left_off = 0;
        self.ended = false;
        self.skip = 0;
        // En la zona dañada no hay nada que decodificar: solo se coloca la posición (el silencio se produce en `decode_into`).
        if self.damaged_at.is_some_and(|d| ts >= d) {
            self.pos = ts;
            return Ok(());
        }
        if let Some(ff) = self.ff.as_mut() {
            ff.seek(ts)?;
            self.pos = ts;
            return Ok(());
        }
        match self.format.seek(SeekMode::Accurate, SeekTo::TimeStamp { ts, track_id: self.track_id }) {
            Ok(seeked) => {
                self.decoder.reset();
                if seeked.actual_ts > ts {
                    return Err(StreamError::Decode("la búsqueda superó el destino".into()));
                }
                self.pos = seeked.actual_ts;
                self.skip = ts - seeked.actual_ts;
                Ok(())
            }
            // Más allá del final: no hay nada que leer.
            Err(SymError::SeekError(_)) => {
                self.pos = ts;
                self.ended = true;
                Ok(())
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Decodifica hasta `count` frames desde `start` a `out` (entrelazado). Devuelve los obtenidos,
    /// menos de `count` solo al llegar al final del archivo.
    fn decode_into(&mut self, start: u64, count: u64, out: &mut Vec<f32>) -> Result<u64, StreamError> {
        out.clear();
        if start != self.pos {
            self.seek(start)?;
        }
        let ch = self.channels;
        let want = count as usize * ch;
        while out.len() < want {
            // Zona dañada: silencio hasta la duración declarada.
            if self.damaged_at.is_some_and(|d| self.pos >= d) {
                let remaining = self.total_frames.map_or(0, |t| t.saturating_sub(self.pos));
                let n = remaining.min(((want - out.len()) / ch) as u64) as usize;
                if n == 0 {
                    break;
                }
                out.resize(out.len() + n * ch, 0.0);
                self.pos += n as u64;
                self.left.clear();
                self.left_off = 0;
                continue;
            }
            let avail = (self.left.len() - self.left_off) / ch;
            if avail > 0 {
                if self.skip > 0 {
                    let s = self.skip.min(avail as u64);
                    self.left_off += s as usize * ch;
                    self.pos += s;
                    self.skip -= s;
                    continue;
                }
                let mut take = avail.min((want - out.len()) / ch);
                // Lo que quede de la zona sana se entrega entero; lo de después (si llegara algo) se descarta.
                if let Some(d) = self.damaged_at {
                    take = take.min(d.saturating_sub(self.pos).max(0) as usize);
                    if take == 0 {
                        self.left.clear();
                        self.left_off = 0;
                        continue;
                    }
                }
                out.extend_from_slice(&self.left[self.left_off..self.left_off + take * ch]);
                self.left_off += take * ch;
                self.pos += take as u64;
                continue;
            }
            if self.ended {
                break;
            }
            self.fill_left()?;
        }
        Ok((out.len() / ch) as u64)
    }
}

// ---------------------------------------------------------------------------------------------
// Etiquetas del archivo (título, artista)
// ---------------------------------------------------------------------------------------------

/// Título y artista que trae el archivo en sus etiquetas (ID3, MP4, Vorbis, FLAC, RIFF INFO…), si los trae.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MediaTags {
    pub title: Option<String>,
    pub artist: Option<String>,
}

/// Lee solo las etiquetas de `path` (no decodifica audio). Archivos sin etiquetas o ilegibles devuelven etiquetas vacías.
pub fn media_tags(path: &Path) -> MediaTags {
    use symphonia::core::meta::{MetadataRevision, StandardTagKey};
    let Ok(file) = File::open(path) else { return MediaTags::default() };
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let Ok(mut probed) = symphonia::default::get_probe().format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default()) else {
        return MediaTags::default();
    };
    let mut tags = MediaTags::default();
    let mut take = |rev: &MetadataRevision| {
        for t in rev.tags() {
            let v = t.value.to_string().trim().to_string();
            if v.is_empty() {
                continue;
            }
            match t.std_key {
                Some(StandardTagKey::TrackTitle) if tags.title.is_none() => tags.title = Some(v),
                Some(StandardTagKey::Artist) if tags.artist.is_none() => tags.artist = Some(v),
                _ => {}
            }
        }
    };
    // Etiquetas del contenedor (MP4, Ogg, FLAC…) y, antes, las que van delante del audio (ID3v2 en un MP3).
    if let Some(rev) = probed.format.metadata().current() {
        take(rev);
    }
    if let Some(mut m) = probed.metadata.get() {
        if let Some(rev) = m.skip_to_latest() {
            take(rev);
        }
    }
    tags
}

// ---------------------------------------------------------------------------------------------
// Picos de un archivo de cualquier duración (forma de onda)
// ---------------------------------------------------------------------------------------------

/// Resultado de [`scan_peaks`]: mínimo y máximo por columna de todo el archivo.
#[derive(Clone, Debug, PartialEq)]
pub struct PeakScan {
    pub sample_rate: u32,
    pub channels: usize,
    pub total_frames: u64,
    pub min: Vec<f32>,
    pub max: Vec<f32>,
    /// Paquetes del archivo que el decodificador no pudo leer y se dibujan como silencio.
    pub skipped_packets: u64,
    /// Frame desde el que el audio del archivo está dañado (el resto se dibuja y suena en silencio), si se detectó.
    pub damaged_from_frame: Option<u64>,
}

/// Frames por bloque fino al barrer: el archivo se recorre una sola vez, en orden, y solo se guardan dos `f32` por bloque
/// (unos 4 MB para 3 horas a 48 kHz), de modo que la memoria no depende de la duración. Después se reducen a `buckets`.
const SCAN_BLOCK_FRAMES: u64 = 1_024; // tope del bloque fino
/// Frames que se piden al decodificador por vuelta.
const SCAN_CHUNK_FRAMES: u64 = 16_384;

/// Recorre `path` de principio a fin **en streaming** (sin cargarlo en RAM) y lo reduce a `buckets` pares mínimo/máximo
/// (decimación por picos: no se pierde ningún transitorio). Sirve para obras de horas. Es síncrono y largo: llamar desde
/// un hilo. `cancel` permite abandonarlo (devuelve `Ok(None)`).
pub fn scan_peaks(path: &Path, buckets: usize, cancel: &std::sync::atomic::AtomicBool) -> Result<Option<PeakScan>, StreamError> {
    let mut s = Session::open(path)?;
    let (channels, sample_rate) = (s.channels, s.sample_rate);
    // Con la duración conocida el bloque se ajusta para que haya unos 4 por columna (archivos cortos: más finos; obras de horas: 1024).
    let block = s.total_frames.map_or(SCAN_BLOCK_FRAMES, |t| (t / (buckets.max(1) as u64 * 4)).clamp(1, SCAN_BLOCK_FRAMES));
    let mut blocks_min: Vec<f32> = Vec::new();
    let mut blocks_max: Vec<f32> = Vec::new();
    let (mut lo, mut hi, mut in_block) = (f32::MAX, f32::MIN, 0u64);
    let mut total = 0u64;
    let mut buf = Vec::new();
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let got = s.decode_into(total, SCAN_CHUNK_FRAMES, &mut buf)?;
        for frame in buf.chunks_exact(channels).take(got as usize) {
            for &x in frame {
                lo = lo.min(x);
                hi = hi.max(x);
            }
            in_block += 1;
            if in_block == block {
                blocks_min.push(lo);
                blocks_max.push(hi);
                (lo, hi, in_block) = (f32::MAX, f32::MIN, 0);
            }
        }
        total += got;
        if got < SCAN_CHUNK_FRAMES {
            break;
        }
    }
    if in_block > 0 {
        blocks_min.push(lo);
        blocks_max.push(hi);
    }
    if total == 0 {
        return Err(StreamError::Decode("el archivo no tiene muestras".into()));
    }
    let n = blocks_min.len();
    let buckets = buckets.max(1).min(n);
    let (mut min, mut max) = (vec![0.0f32; buckets], vec![0.0f32; buckets]);
    for b in 0..buckets {
        let (a, z) = (n * b / buckets, (n * (b + 1) / buckets).max(n * b / buckets + 1).min(n));
        min[b] = blocks_min[a..z].iter().copied().fold(f32::MAX, f32::min);
        max[b] = blocks_max[a..z].iter().copied().fold(f32::MIN, f32::max);
    }
    Ok(Some(PeakScan { sample_rate, channels, total_frames: total, min, max, skipped_packets: s.skipped_packets, damaged_from_frame: s.damaged_at }))
}

/// Picos de un tramo del archivo: de `start_frame` a `start_frame + frames`, en `buckets` columnas mínimo/máximo. Es el
/// «zoom» de la forma de onda: salta al tramo y decodifica solo eso (una ventana de segundos en una obra de horas es
/// inmediata). Las columnas pasadas del final del archivo quedan en silencio. `Ok(None)` si se cancela.
pub fn scan_peaks_range(path: &Path, start_frame: u64, frames: u64, buckets: usize, cancel: &std::sync::atomic::AtomicBool) -> Result<Option<(Vec<f32>, Vec<f32>)>, StreamError> {
    let mut s = Session::open(path)?;
    let channels = s.channels;
    let (frames, buckets) = (frames.max(1), buckets.max(1));
    let (mut min, mut max) = (vec![f32::MAX; buckets], vec![f32::MIN; buckets]);
    let mut done = 0u64;
    let mut buf = Vec::new();
    while done < frames {
        if cancel.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let want = SCAN_CHUNK_FRAMES.min(frames - done);
        let got = s.decode_into(start_frame + done, want, &mut buf)?;
        if got == 0 {
            break;
        }
        for (i, frame) in buf.chunks_exact(channels).take(got as usize).enumerate() {
            let b = (((done + i as u64) * buckets as u64) / frames).min(buckets as u64 - 1) as usize;
            for &x in frame {
                min[b] = min[b].min(x);
                max[b] = max[b].max(x);
            }
        }
        done += got;
        if got < want {
            break;
        }
    }
    // Columnas sin datos (más allá del final): silencio.
    for b in 0..buckets {
        if min[b] > max[b] {
            (min[b], max[b]) = (0.0, 0.0);
        }
    }
    Ok(Some((min, max)))
}

// ---------------------------------------------------------------------------------------------
// Pool de hilos de disco
// ---------------------------------------------------------------------------------------------

struct StreamJob {
    shared: Arc<Shared>,
    session: Mutex<Session>,
}

struct PoolInner {
    jobs: Mutex<Vec<Arc<StreamJob>>>,
    shutdown: AtomicBool,
}

/// Pool `qlab-disk-streamer`: `N = max(2, cores / 2)` hilos por defecto.
pub struct DiskStreamerPool {
    inner: Arc<PoolInner>,
    threads: Vec<JoinHandle<()>>,
}

impl DiskStreamerPool {
    pub fn default_thread_count() -> usize {
        std::thread::available_parallelism().map_or(2, |n| n.get() / 2).max(2)
    }

    pub fn with_default_threads() -> Self {
        Self::new(Self::default_thread_count())
    }

    pub fn new(threads: usize) -> Self {
        let inner = Arc::new(PoolInner { jobs: Mutex::new(Vec::new()), shutdown: AtomicBool::new(false) });
        let threads = (0..threads.max(1))
            .map(|i| {
                let inner = Arc::clone(&inner);
                std::thread::Builder::new()
                    .name(format!("qlab-disk-streamer-{i}"))
                    .spawn(move || worker_loop(&inner))
                    .expect("no se pudo crear un hilo de disco")
            })
            .collect();
        Self { inner, threads }
    }

    /// Lee solo la cabecera de `path` (frecuencia, canales y duración) sin decodificar nada útil.
    pub fn probe(path: &Path) -> Result<FileInfo, StreamError> {
        let s = Session::open(path)?;
        Ok(FileInfo { sample_rate: s.sample_rate, channels: s.channels, total_frames: s.total_frames, resident: false })
    }

    /// Streams con trabajo pendiente o abiertos todavía.
    pub fn active_jobs(&self) -> usize {
        let mut jobs = self.inner.jobs.lock().expect("pool envenenado");
        jobs.retain(|j| !j.shared.closed.load(Ordering::Acquire));
        jobs.len()
    }

    /// Abre `path` y prepara el lector. Decodifica las regiones fijadas (o el archivo entero si es
    /// corto) en el hilo que llama, así que se invoca desde la pre-carga, nunca desde el hilo RT.
    pub fn open(&self, path: &Path, cfg: &StreamConfig) -> Result<(StreamingVoiceReader, FileInfo), StreamError> {
        let mut sess = Session::open(path)?;
        let (channels, sample_rate, total) = (sess.channels, sess.sample_rate, sess.total_frames);
        let mut scratch = Vec::new();

        let resident = total.is_some_and(|t| (t as usize).saturating_mul(channels * 4) <= cfg.resident_limit_bytes);
        if resident {
            let total = total.unwrap_or(0);
            let got = sess.decode_into(0, total, &mut scratch)?;
            let buf = AlignedBuf::new(scratch.len());
            buf.fill_from(&scratch);
            let pinned = vec![Pinned { origin: 0, frames: got, data: buf }];
            let shared = Shared::resident(channels, sample_rate, pinned, got);
            let info = FileInfo { sample_rate, channels, total_frames: Some(got), resident: true };
            return Ok((StreamingVoiceReader { shared: Arc::new(shared) }, info));
        }

        let window = cfg.window_frames.max(64);
        let mut starts = vec![cfg.start_frame];
        for &s in &cfg.extra_pinned_starts {
            if !starts.contains(&s) && starts.len() < MAX_PINNED_REGIONS {
                starts.push(s);
            }
        }
        let limit = total.unwrap_or(u64::MAX);
        let mut pinned = Vec::with_capacity(starts.len());
        for start in starts {
            let count = (cfg.pinned_frames.max(64) as u64).min(limit.saturating_sub(start));
            let got = if count == 0 { 0 } else { sess.decode_into(start, count, &mut scratch)? };
            let buf = AlignedBuf::new(scratch.len());
            buf.fill_from(&scratch);
            pinned.push(Pinned { origin: start, frames: got, data: buf });
        }
        let shared = Arc::new(Shared {
            channels,
            sample_rate,
            window_frames: window as u64,
            resident: false,
            eof_frame: AtomicU64::new(limit),
            closed: AtomicBool::new(false),
            pinned: pinned.into_boxed_slice(),
            slots: [Slot::new(window * channels), Slot::new(window * channels)],
            windows_loaded: AtomicU64::new(0),
            load_errors: AtomicU64::new(0),
        });
        self.inner
            .jobs
            .lock()
            .expect("pool envenenado")
            .push(Arc::new(StreamJob { shared: Arc::clone(&shared), session: Mutex::new(sess) }));
        let info = FileInfo { sample_rate, channels, total_frames: total, resident: false };
        Ok((StreamingVoiceReader { shared }, info))
    }
}

impl Drop for DiskStreamerPool {
    fn drop(&mut self) {
        self.inner.shutdown.store(true, Ordering::Release);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

fn worker_loop(inner: &PoolInner) {
    let mut scratch: Vec<f32> = Vec::new();
    while !inner.shutdown.load(Ordering::Acquire) {
        let jobs: Vec<Arc<StreamJob>> = {
            let mut guard = inner.jobs.lock().expect("pool envenenado");
            guard.retain(|j| !j.shared.closed.load(Ordering::Acquire));
            guard.clone()
        };
        let mut worked = false;
        for job in &jobs {
            if job.shared.closed.load(Ordering::Acquire) {
                continue;
            }
            // Otro hilo ya atiende este archivo: la sesión de decodificación es secuencial.
            let Ok(mut sess) = job.session.try_lock() else { continue };
            for (i, slot) in job.shared.slots.iter().enumerate() {
                if slot
                    .state
                    .compare_exchange(SLOT_REQUESTED, SLOT_LOADING, Ordering::Acquire, Ordering::Relaxed)
                    .is_ok()
                {
                    fill_slot(&job.shared, &mut sess, i, &mut scratch);
                    worked = true;
                }
            }
        }
        if !worked {
            std::thread::sleep(POLL_INTERVAL);
        }
    }
}

fn fill_slot(sh: &Shared, sess: &mut Session, index: usize, scratch: &mut Vec<f32>) {
    let slot = &sh.slots[index];
    let origin = slot.req_origin.load(Ordering::Relaxed);
    let eof = sh.eof_frame.load(Ordering::Relaxed);
    let want = sh.window_frames.min(eof.saturating_sub(origin));
    let got = if want == 0 {
        0
    } else {
        match sess.decode_into(origin, want, scratch) {
            Ok(got) => got,
            Err(_) => {
                // Se trata como fin de archivo para no reintentar sin parar.
                sh.load_errors.fetch_add(1, Ordering::Relaxed);
                scratch.clear();
                0
            }
        }
    };
    if got < want {
        sh.eof_frame.fetch_min(origin + got, Ordering::Relaxed);
    }
    slot.data.fill_from(scratch);
    slot.origin.store(origin, Ordering::Relaxed);
    slot.frames.store(got, Ordering::Relaxed);
    sh.windows_loaded.fetch_add(1, Ordering::Relaxed);
    slot.state.store(SLOT_READY, Ordering::Release);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alloc_guard::{assert_no_alloc, set_violation_policy, violations_on_this_thread, ViolationPolicy};
    use std::path::PathBuf;
    use std::time::Instant;

    const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

    /// Valor determinista y exactamente representable en `f32`.
    fn pattern(frame: u64, ch: usize) -> f32 {
        (((frame * 7 + ch as u64 * 131) % 4001) as f32 - 2000.0) / 2048.0
    }

    /// WAV `f32` IEEE (sin pérdidas) con `frames` frames de `pattern`.
    fn write_wav(name: &str, channels: usize, frames: u64, rate: u32) -> PathBuf {
        let mut data = Vec::with_capacity(frames as usize * channels * 4);
        for f in 0..frames {
            for c in 0..channels {
                data.extend_from_slice(&pattern(f, c).to_le_bytes());
            }
        }
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&3u16.to_le_bytes()); // IEEE float
        wav.extend_from_slice(&(channels as u16).to_le_bytes());
        wav.extend_from_slice(&rate.to_le_bytes());
        wav.extend_from_slice(&(rate * channels as u32 * 4).to_le_bytes());
        wav.extend_from_slice(&(channels as u16 * 4).to_le_bytes());
        wav.extend_from_slice(&32u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
        wav.extend_from_slice(&data);
        let path = std::env::temp_dir().join(format!("qlab-test-{}-{name}.wav", std::process::id()));
        std::fs::write(&path, wav).unwrap();
        path
    }

    /// WAV `f32` mono de `secs` segundos cuya amplitud sube linealmente de 0 a 1 (para saber qué tramo se está mirando).
    fn write_ramp_wav(name: &str, secs: u64, rate: u32) -> PathBuf {
        let frames = secs * u64::from(rate);
        let mut data = Vec::with_capacity(frames as usize * 4);
        for f in 0..frames {
            data.extend_from_slice(&((f as f32) / (frames as f32)).to_le_bytes());
        }
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&3u16.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&rate.to_le_bytes());
        wav.extend_from_slice(&(rate * 4).to_le_bytes());
        wav.extend_from_slice(&4u16.to_le_bytes());
        wav.extend_from_slice(&32u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
        wav.extend_from_slice(&data);
        let path = std::env::temp_dir().join(format!("qlab-test-{}-{name}.wav", std::process::id()));
        std::fs::write(&path, wav).unwrap();
        path
    }

    fn streaming_cfg(window: usize, pinned: usize) -> StreamConfig {
        StreamConfig { window_frames: window, pinned_frames: pinned, resident_limit_bytes: 0, ..Default::default() }
    }

    /// Espera (sondeando como lo haría el callback) a que `frame` esté disponible.
    fn wait_for(reader: &StreamingVoiceReader, cursor: u64, frame: u64) {
        let t0 = Instant::now();
        loop {
            reader.maintain(cursor);
            if reader.view().has_frame(frame) {
                return;
            }
            assert!(t0.elapsed() < Duration::from_secs(5), "el disco no entregó el frame {frame} (cursor {cursor})");
            std::thread::sleep(Duration::from_micros(200));
        }
    }

    #[test]
    fn short_wav_is_fully_resident_and_exact() {
        let path = write_wav("resident", 2, 10_000, 48_000);
        let pool = DiskStreamerPool::new(2);
        let (reader, info) = pool.open(&path, &StreamConfig::default()).unwrap();
        assert_eq!(info, FileInfo { sample_rate: 48_000, channels: 2, total_frames: Some(10_000), resident: true });
        assert!(reader.is_resident());
        let v = reader.view();
        for f in 0..10_000u64 {
            for c in 0..2 {
                assert_eq!(v.sample(f as i64, c), pattern(f, c), "frame {f} canal {c}");
            }
        }
        assert!(!v.has_frame(10_000));
        assert_eq!(v.sample(-1, 0), 0.0);
        assert_eq!(pool.active_jobs(), 0, "un archivo residente no necesita trabajo de disco");
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn streaming_playback_through_ping_pong_windows_is_bit_exact() {
        let total = 40_000u64;
        let path = write_wav("stream", 2, total, 48_000);
        let pool = DiskStreamerPool::new(2);
        let (reader, info) = pool.open(&path, &streaming_cfg(4096, 2048)).unwrap();
        assert!(!info.resident && !reader.is_resident());
        let mut cursor = 0u64;
        while cursor < total {
            // Una "ráfaga" de callback: mantener, vista, leer 128 frames.
            wait_for(&reader, cursor, cursor);
            reader.maintain(cursor);
            let v = reader.view();
            let n = 128.min(total - cursor);
            for f in cursor..cursor + n {
                assert!(v.has_frame(f), "hueco en {f}");
                for c in 0..2 {
                    assert_eq!(v.sample(f as i64, c), pattern(f, c), "frame {f} canal {c}");
                }
            }
            cursor += n;
        }
        let stats = reader.stats();
        assert!(stats.windows_loaded >= (total - 2048) / 4096, "{stats:?}");
        assert_eq!(stats.load_errors, 0);
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn pinned_start_gives_zero_latency_and_survives_loop_wrap() {
        let total = 40_000u64;
        let path = write_wav("pinned", 1, total, 48_000);
        let pool = DiskStreamerPool::new(2);
        let (reader, _) = pool.open(&path, &streaming_cfg(4096, 2048)).unwrap();
        // Sin esperar al disco: el inicio ya está.
        assert!(reader.view().has_frame(0) && reader.view().has_frame(2047));
        assert!(!reader.view().has_frame(2048));
        // Reproducir hasta bien entrado el archivo y volver al inicio (bucle).
        let mut cursor = 0;
        while cursor < 20_000 {
            wait_for(&reader, cursor, cursor);
            cursor += 500;
        }
        reader.maintain(0); // el bucle salta de vuelta
        let v = reader.view();
        assert!(v.has_frame(0), "el inicio fijado debe estar disponible tras el salto");
        assert_eq!(v.sample(100, 0), pattern(100, 0));
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn extra_pinned_slice_starts() {
        let path = write_wav("slices", 1, 40_000, 48_000);
        let pool = DiskStreamerPool::new(2);
        let cfg = StreamConfig { extra_pinned_starts: vec![30_000], ..streaming_cfg(4096, 1000) };
        let (reader, _) = pool.open(&path, &cfg).unwrap();
        let v = reader.view();
        assert!(v.has_frame(30_000) && v.has_frame(30_999) && !v.has_frame(31_000));
        assert_eq!(v.sample(30_500, 0), pattern(30_500, 0));
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn seek_far_ahead_is_served_by_the_disk_threads() {
        let total = 100_000u64;
        let path = write_wav("seek", 2, total, 48_000);
        let pool = DiskStreamerPool::new(2);
        let (reader, _) = pool.open(&path, &streaming_cfg(4096, 2048)).unwrap();
        for target in [70_001u64, 12_345, 99_000] {
            wait_for(&reader, target, target);
            let v = reader.view();
            for f in target..(target + 200).min(total) {
                for c in 0..2 {
                    assert_eq!(v.sample(f as i64, c), pattern(f, c), "tras salto a {target}: frame {f}");
                }
            }
        }
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn end_of_file_stops_requests_and_reports_no_frames_past_it() {
        let total = 9_000u64;
        let path = write_wav("eof", 1, total, 48_000);
        let pool = DiskStreamerPool::new(2);
        let (reader, _) = pool.open(&path, &streaming_cfg(2048, 1024)).unwrap();
        let mut cursor = 0;
        while cursor < total {
            wait_for(&reader, cursor, cursor);
            cursor += 256;
        }
        let loads = reader.stats().windows_loaded;
        std::thread::sleep(Duration::from_millis(30));
        for _ in 0..50 {
            reader.maintain(total - 1);
        }
        std::thread::sleep(Duration::from_millis(30));
        assert!(reader.stats().windows_loaded <= loads + 1, "no debe pedir ventanas más allá del final");
        assert!(!reader.view().has_frame(total));
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn flac_and_mp3_decode_like_the_wav_reference() {
        let pool = DiskStreamerPool::new(2);
        let cfg = StreamConfig::default();
        let wav = Path::new(FIXTURES).join("tone.wav");
        let (w, wi) = pool.open(&wav, &cfg).unwrap();
        assert_eq!((wi.sample_rate, wi.channels, wi.total_frames), (44_100, 1, Some(44_100)));

        // FLAC es sin pérdidas: idéntico bit a bit al WAV de origen.
        let (f, fi) = pool.open(&Path::new(FIXTURES).join("tone.flac"), &cfg).unwrap();
        assert_eq!((fi.sample_rate, fi.channels, fi.total_frames), (44_100, 1, Some(44_100)));
        let (vw, vf) = (w.view(), f.view());
        for i in 0..44_100i64 {
            assert_eq!(vw.sample(i, 0), vf.sample(i, 0), "FLAC difiere en {i}");
        }

        // MP3 con pérdidas: misma forma de onda dentro de una tolerancia, tras el retardo del codificador.
        let (m, mi) = pool.open(&Path::new(FIXTURES).join("tone.mp3"), &cfg).unwrap();
        assert_eq!((mi.sample_rate, mi.channels), (44_100, 1));
        let frames = mi.total_frames.expect("lame escribe cabecera Xing");
        assert!((43_000..=46_500).contains(&frames), "duración inesperada: {frames}");
        let vm = m.view();
        let peak = |v: &ReaderView<'_>, n: i64| (0..n).map(|i| v.sample(i, 0).abs()).fold(0.0f32, f32::max);
        let (pm, pw) = (peak(&vm, frames as i64), peak(&vw, 44_100));
        assert!((pm - pw).abs() < 0.05 * pw, "amplitud MP3 {pm} frente a {pw} del WAV");
        let crossings = (1..frames as i64).filter(|&i| (vm.sample(i - 1, 0) < 0.0) != (vm.sample(i, 0) < 0.0)).count();
        assert!((860..=900).contains(&crossings), "440 Hz ⇒ ~880 cruces por cero, hay {crossings}");
    }

    #[test]
    fn streamed_flac_seeks_accurately() {
        let pool = DiskStreamerPool::new(2);
        let cfg = StreamConfig { window_frames: 4096, pinned_frames: 1024, resident_limit_bytes: 0, ..Default::default() };
        let (reference, _) = pool.open(&Path::new(FIXTURES).join("tone.wav"), &StreamConfig::default()).unwrap();
        let (streamed, info) = pool.open(&Path::new(FIXTURES).join("tone.flac"), &cfg).unwrap();
        assert!(!info.resident);
        for target in [20_000u64, 5_000, 43_000] {
            wait_for(&streamed, target, target);
            let (a, b) = (reference.view(), streamed.view());
            for f in target..target + 300 {
                assert_eq!(a.sample(f as i64, 0), b.sample(f as i64, 0), "salto a {target}, frame {f}");
            }
        }
    }

    #[test]
    fn errors_are_reported_not_panicked() {
        let pool = DiskStreamerPool::new(1);
        let missing = pool.open(Path::new("/no/existe/nada.wav"), &StreamConfig::default());
        assert!(matches!(missing, Err(StreamError::Io(_))));
        let junk = std::env::temp_dir().join(format!("qlab-test-{}-junk.wav", std::process::id()));
        std::fs::write(&junk, vec![0x5Au8; 4096]).unwrap();
        assert!(pool.open(&junk, &StreamConfig::default()).is_err());
        std::fs::remove_file(junk).ok();
    }

    #[test]
    fn rt_side_never_allocates_or_blocks() {
        set_violation_policy(ViolationPolicy::CountOnly);
        let total = 30_000u64;
        let path = write_wav("rt", 2, total, 48_000);
        let pool = DiskStreamerPool::new(2);
        let (reader, _) = pool.open(&path, &streaming_cfg(4096, 2048)).unwrap();
        let before = violations_on_this_thread();
        let mut acc = 0.0f32;
        let mut cursor = 0u64;
        while cursor < total {
            // Dentro del ámbito RT solo se mantiene y se lee; la espera por el disco queda fuera.
            let ready = assert_no_alloc(|| {
                reader.maintain(cursor);
                reader.view().has_frame(cursor)
            });
            if !ready {
                std::thread::sleep(Duration::from_micros(200));
                continue;
            }
            assert_no_alloc(|| {
                let v = reader.view();
                for f in cursor..(cursor + 64).min(total) {
                    acc += v.sample(f as i64, 0) + v.sample(f as i64, 1);
                }
            });
            cursor += 64;
        }
        assert!(acc.is_finite());
        assert_eq!(violations_on_this_thread(), before, "la ruta RT del lector asignó o liberó memoria");
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn closed_readers_are_pruned_and_pool_shuts_down() {
        let path = write_wav("prune", 1, 30_000, 48_000);
        let pool = DiskStreamerPool::new(2);
        let (a, _) = pool.open(&path, &streaming_cfg(2048, 1024)).unwrap();
        let (b, _) = pool.open(&path, &streaming_cfg(2048, 1024)).unwrap();
        assert_eq!(pool.active_jobs(), 2);
        drop(a);
        assert_eq!(pool.active_jobs(), 1);
        drop(b);
        assert_eq!(pool.active_jobs(), 0);
        drop(pool); // debe terminar sin colgarse
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn many_concurrent_streams() {
        let total = 60_000u64;
        let path = write_wav("many", 2, total, 48_000);
        let pool = DiskStreamerPool::new(3);
        let readers: Vec<_> = (0..16).map(|_| pool.open(&path, &streaming_cfg(4096, 1024)).unwrap().0).collect();
        let mut cursor = 0u64;
        while cursor < total {
            for r in &readers {
                wait_for(r, cursor, cursor);
                let v = r.view();
                assert_eq!(v.sample(cursor as i64, 1), pattern(cursor, 1));
            }
            cursor += 1000;
        }
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn mixer_plays_a_streamed_file_without_underruns() {
        use crate::rt_command::{AudioCommand, AudioStatus, MAX_OUTPUT_CHANNELS};
        use crate::voice::{AudioLevelMatrixRt, Voice, VoiceInit, VoiceRtState};
        use crate::voice_pool::VoicePool;
        use ringbuf::traits::{Consumer, Producer, Split};
        use ringbuf::HeapRb;

        set_violation_policy(ViolationPolicy::CountOnly);
        let total = 30_000u64;
        let path = write_wav("mixer", 2, total, 48_000);
        let disk = DiskStreamerPool::new(2);
        let (reader, _) = disk.open(&path, &streaming_cfg(4096, 2048)).unwrap();
        let rt = VoiceRtState::new(0, &[], AudioLevelMatrixRt::new(2, 2)).unwrap();
        let voice = Arc::new(Voice::with_reader(
            VoiceInit {
                id: 1,
                cue_unique_id: 1,
                patch_id: 0,
                file_sample_rate: 48_000,
                start_frame: 0,
                end_frame: total,
                infinite_loop: false,
                rate: 1.0,
            },
            rt,
            reader,
        ));
        let mut pool = VoicePool::new(4);
        pool.insert_voice(Arc::clone(&voice));
        let handle = pool.rt_handle();
        drop(handle.load());
        let (mut cmd_tx, mut cmd_rx) = HeapRb::<AudioCommand>::new(16).split();
        let (mut st_tx, mut st_rx) = HeapRb::<AudioStatus>::new(256).split();
        let mut tail = [0.0f32; MAX_OUTPUT_CHANNELS];
        assert!(cmd_tx.try_push(AudioCommand::Play { voice_id: 1, start_host_time_ns: 0 }).is_ok());

        let mut pos = 0u64;
        let mut out = vec![0.0f32; 256 * 2];
        let before = violations_on_this_thread();
        while pos < total {
            // Entre callbacks el disco tiene tiempo de responder, como pasa con el hardware real.
            let t0 = Instant::now();
            loop {
                voice.reader.maintain(pos);
                if voice.reader.view().has_frame((pos + 260).min(total - 1)) {
                    break;
                }
                assert!(t0.elapsed() < Duration::from_secs(5), "disco lento en {pos}");
                std::thread::sleep(Duration::from_micros(200));
            }
            assert_no_alloc(|| {
                crate::mixer_simd::process_rt_callback(&mut out, 2, 48_000, &handle, &mut cmd_rx, &mut st_tx, &mut tail)
            });
            for f in 0..256u64.min(total - pos) {
                for c in 0..2 {
                    assert_eq!(out[f as usize * 2 + c], pattern(pos + f, c), "frame {}", pos + f);
                }
            }
            pos += 256;
        }
        assert_eq!(violations_on_this_thread(), before);
        let mut events = Vec::new();
        while let Some(e) = st_rx.try_pop() {
            events.push(e);
        }
        assert!(!events.iter().any(|e| matches!(e, AudioStatus::DiskBufferUnderrun { .. })), "{events:?}");
        assert!(events.contains(&AudioStatus::VoiceCompleted { voice_id: 1, natural_eof: true }));
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn aligned_buffers_are_cache_line_aligned() {
        let b = AlignedBuf::new(100);
        assert_eq!(b.lines.as_ptr() as usize % 64, 0);
        b.store(99, 1.5);
        assert_eq!(b.load(99), 1.5);
        assert_eq!(b.lines.len(), 7);
    }
    /// Genera un vídeo de prueba (imagen + tono) con `ffmpeg`; `None` si no está instalado.
    fn make_test_video(name: &str, container: &str, audio_codec: &str) -> Option<PathBuf> {
        let path = std::env::temp_dir().join(format!("qlab-engine-{}-{name}.{container}", std::process::id()));
        let ok = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc=duration=1:size=160x90:rate=25", "-f", "lavfi", "-i", "sine=frequency=440:duration=1:sample_rate=44100", "-c:v", "mpeg4", "-c:a", audio_codec, "-shortest"])
            .arg(&path)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        ok.then_some(path)
    }

    #[test]
    fn the_audio_track_of_video_containers_is_played_without_converting() {
        for (name, container, codec) in [("mp4-aac", "mp4", "aac"), ("mov-aac", "mov", "aac"), ("mkv-mp3", "mkv", "libmp3lame")] {
            let Some(path) = make_test_video(name, container, codec) else {
                eprintln!("ffmpeg no está instalado: se omite {name}");
                return;
            };
            let info = DiskStreamerPool::probe(&path).unwrap_or_else(|e| panic!("{name}: {e:?}"));
            assert_eq!((info.sample_rate, info.channels > 0), (44_100, true), "{name}");
            let total = info.total_frames.unwrap_or(0);
            assert!((40_000..=48_000).contains(&(total as i64)), "{name}: {total} frames ≈ 1 s");
            // Se decodifica de verdad: hay señal (el tono) y no solo silencio.
            let pool = DiskStreamerPool::new(1);
            let (reader, _) = pool.open(&path, &StreamConfig::default()).unwrap_or_else(|e| panic!("{name}: {e:?}"));
            let view = reader.view();
            let peak = (10_000..14_000).map(|f| view.sample(f, 0).abs()).fold(0.0f32, f32::max);
            assert!(peak > 0.1, "{name}: pico {peak}");
            let _ = std::fs::remove_file(&path);
        }
    }

    #[test]
    fn container_durations_are_converted_to_sample_frames() {
        use symphonia::core::units::TimeBase;
        assert_eq!(frames_from_param(1025, Some(TimeBase::new(1, 1_000)), Some(44_100)), 45_202, "milisegundos → muestras");
        assert_eq!(frames_from_param(48_000, Some(TimeBase::new(1, 48_000)), Some(48_000)), 48_000, "base 1/fs: ya son muestras");
        assert_eq!(frames_from_param(7, None, Some(48_000)), 7);
        assert_eq!(frames_from_param(7, Some(TimeBase::new(1, 1_000)), None), 7);
    }

    /// Genera con ffmpeg un `.m4a` (AAC) de `secs` segundos con un tono y devuelve su ruta; `None` si no hay ffmpeg.
    fn make_aac(tag: &str, secs: u32) -> Option<PathBuf> {
        if !crate::ffmpeg_pcm::available() {
            return None;
        }
        let path = std::env::temp_dir().join(format!("qlab-aac-{}-{tag}.m4a", std::process::id()));
        let st = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
            .arg(format!("sine=frequency=440:duration={secs}:sample_rate=48000"))
            .args(["-ac", "2", "-c:a", "aac", "-b:a", "128k"])
            .arg(&path)
            .status()
            .ok()?;
        st.success().then_some(path)
    }

    /// Rellena con basura determinista el tramo `[from, to)` (fracciones) del contenido (`mdat`) del MP4: simula una captura dañada.
    fn corrupt_mdat(path: &Path, from: f64, to: f64) {
        let mut data = std::fs::read(path).unwrap();
        let (mut at, mut range) = (0usize, None);
        while at + 8 <= data.len() {
            let size = u32::from_be_bytes(data[at..at + 4].try_into().unwrap()) as usize;
            if &data[at + 4..at + 8] == b"mdat" {
                range = Some((at + 8, at + size));
                break;
            }
            at += size.max(8);
        }
        let (a, b) = range.expect("el MP4 tiene mdat");
        let (lo, hi) = (a + ((b - a) as f64 * from) as usize, a + ((b - a) as f64 * to) as usize);
        let mut x = 0x9E37_79B9u32;
        for byte in &mut data[lo..hi] {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            *byte = (x >> 24) as u8;
        }
        std::fs::write(path, data).unwrap();
    }

    #[test]
    fn a_damaged_aac_file_gives_the_whole_waveform_marks_where_it_breaks_and_the_rest_is_silent_not_noise() {
        let Some(path) = make_aac("damaged", 30) else { return eprintln!("ffmpeg no está instalado: se omite") };
        // Sano: sin zona dañada.
        let ok = scan_peaks(&path, 300, &AtomicBool::new(false)).unwrap().unwrap();
        assert_eq!(ok.damaged_from_frame, None);
        assert!(ok.max.iter().all(|&m| m > 0.05), "el tono se ve en toda la pista");
        // Se destruye la segunda mitad del archivo.
        corrupt_mdat(&path, 0.5, 1.0);
        let t0 = std::time::Instant::now();
        let p = scan_peaks(&path, 300, &AtomicBool::new(false)).expect("un archivo dañado ya no se rechaza").unwrap();
        assert!(t0.elapsed() < std::time::Duration::from_secs(30));
        // La duración total sigue siendo la declarada (≈ 30 s) y la zona dañada se detecta cerca de la mitad.
        assert!((p.total_frames as i64 - 30 * 48_000).abs() < 3_000, "{}", p.total_frames);
        let from = p.damaged_from_frame.expect("se detecta el punto donde se rompe") as f64 / 48_000.0;
        assert!((13.0..=17.5).contains(&from), "el daño empieza hacia los 15 s y se detectó en {from}");
        let n = p.max.len();
        // Primera parte: señal real. Resto: silencio exacto (no ruido de datos rotos).
        assert!(p.max[..n / 4].iter().all(|&m| m > 0.05), "la primera parte sigue sonando");
        let after = (from / 30.0 * n as f64).ceil() as usize + 2;
        assert!(p.max[after.min(n - 1)..].iter().all(|&m| m == 0.0) && p.min[after.min(n - 1)..].iter().all(|&m| m == 0.0), "después del daño no hay ruido");
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn reading_a_damaged_aac_with_the_streamer_never_panics_and_keeps_positions_after_the_damage() {
        let Some(path) = make_aac("streamer", 20) else { return eprintln!("ffmpeg no está instalado: se omite") };
        corrupt_mdat(&path, 0.4, 0.7);
        let mut s = Session::open(&path).unwrap();
        let mut buf = Vec::new();
        let total = s.total_frames.expect("duración declarada");
        // Se lee todo seguido y también a saltos (el reproductor salta a regiones y memorias).
        let mut got = 0;
        loop {
            let n = s.decode_into(got, 8_192, &mut buf).unwrap();
            got += n;
            if n < 8_192 {
                break;
            }
        }
        assert!((got as i64 - total as i64).abs() < 3_000, "se entregó la duración completa: {got} de {total}");
        let n = s.decode_into(total / 5, 2_000, &mut buf).unwrap();
        assert_eq!(n, 2_000);
        assert!(buf.iter().any(|&x| x.abs() > 0.05), "al saltar a una parte sana suena");
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn a_zoomed_range_gives_finer_peaks_than_the_whole_file_and_zero_past_the_end() {
        // 4 s de rampa: el pico crece con el tiempo; un tramo de 0,5 s en el segundo 3 tiene picos entre 0,75 y 0,875.
        let path = write_ramp_wav("range-ramp", 4, 48_000);
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let (min, max) = scan_peaks_range(&path, 3 * 48_000, 24_000, 50, &cancel).unwrap().unwrap();
        assert_eq!((min.len(), max.len()), (50, 50));
        assert!(max[0] > 0.74 && max[0] < 0.8, "{}", max[0]);
        assert!(max[49] > 0.85 && max[49] < 0.88, "{}", max[49]);
        assert!(max.windows(2).all(|w| w[1] >= w[0] - 1e-6), "la rampa crece columna a columna");
        // Un tramo que pasa del final: lo que sobra es silencio.
        let (_, max) = scan_peaks_range(&path, 3 * 48_000 + 24_000, 96_000, 10, &cancel).unwrap().unwrap();
        assert!(max[0] > 0.8 && max[9] == 0.0, "{max:?}");
        assert!(scan_peaks_range(&path, 0, 1000, 10, &std::sync::atomic::AtomicBool::new(true)).unwrap().is_none());
        let _ = std::fs::remove_file(path);
    }
}
