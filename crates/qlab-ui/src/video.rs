//! Vídeo en la interfaz: decodificación con `ffmpeg` (subproceso, cuadros RGBA por una tubería), sincronización con
//! el reloj maestro de audio y almacén de reproductores por cue.
//!
//! **El audio manda.** El motor reproduce la pista de audio del archivo y publica en qué frame del archivo está
//! (`source_frame`); la imagen se limita a mostrar el cuadro que corresponde a ese instante. Si la interfaz va
//! con retraso los cuadros atrasados se descartan (no se ralentiza nada), y si el reloj salta (un bucle, un
//! cambio de región) el decodificador se reinicia en la nueva posición.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{sync_channel, Receiver, TryRecvError};
use std::sync::{Arc, Mutex, OnceLock};

use eframe::egui;

/// Cuadros que el decodificador puede ir por delante de lo mostrado (contrapresión sobre la tubería).
const READ_AHEAD: usize = 4;
/// El reloj retrocede más de esto respecto al cuadro actual: se reinicia el decodificador (vuelta de un bucle).
pub const BACKWARD_TOLERANCE: f64 = 0.25;
/// El reloj se adelanta más de esto al cuadro actual: la decodificación no da abasto; se salta con un reinicio.
pub const FORWARD_TOLERANCE: f64 = 1.0;
/// Tamaño máximo de la imagen decodificada (se escala en `ffmpeg`, no en la interfaz).
pub const MAX_WIDTH: u32 = 960;
pub const MAX_HEIGHT: u32 = 540;

#[derive(Clone, Debug, PartialEq)]
pub struct VideoInfo {
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub duration: f64,
}

impl VideoInfo {
    /// Dimensiones (pares) de la imagen decodificada: caben en `MAX_WIDTH × MAX_HEIGHT` conservando la proporción.
    pub fn decode_size(&self) -> (u32, u32) {
        let (w, h) = (self.width.max(2) as f64, self.height.max(2) as f64);
        let k = (MAX_WIDTH as f64 / w).min(MAX_HEIGHT as f64 / h).min(1.0);
        let even = |v: f64| ((v.round() as u32).max(2)) & !1;
        (even(w * k), even(h * k))
    }
}

pub struct Frame {
    /// Instante del cuadro dentro del archivo (segundos).
    pub pts: f64,
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

/// ¿Está `ffmpeg` en el PATH? (se comprueba una sola vez)
pub fn ffmpeg_available() -> bool {
    static OK: OnceLock<bool> = OnceLock::new();
    *OK.get_or_init(|| tool("ffmpeg").arg("-version").stdout(Stdio::null()).stderr(Stdio::null()).status().map(|s| s.success()).unwrap_or(false))
}

fn tool(name: &str) -> Command {
    let mut c = Command::new(name);
    c.stdin(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    c
}

/// Lee dimensiones, cuadros por segundo y duración con `ffprobe`.
pub fn probe(path: &Path) -> Result<VideoInfo, String> {
    let out = tool("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "stream=width,height,avg_frame_rate,duration:format=duration", "-of", "default=nw=1"])
        .arg(path)
        .output()
        .map_err(|e| format!("no se pudo ejecutar ffprobe: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    parse_probe(&String::from_utf8_lossy(&out.stdout))
}

pub fn parse_probe(text: &str) -> Result<VideoInfo, String> {
    let get = |k: &str| text.lines().find_map(|l| l.strip_prefix(k)?.strip_prefix('=')).map(str::trim);
    let num = |k: &str| get(k).and_then(|v| v.parse::<f64>().ok());
    let (w, h) = (num("width"), num("height"));
    let (Some(w), Some(h)) = (w, h) else { return Err("el archivo no tiene pista de vídeo".into()) };
    let fps = get("avg_frame_rate")
        .and_then(|v| {
            let (a, b) = v.split_once('/')?;
            let (a, b) = (a.parse::<f64>().ok()?, b.parse::<f64>().ok()?);
            (b > 0.0 && a > 0.0).then_some(a / b)
        })
        .unwrap_or(25.0);
    let duration = num("duration").or_else(|| text.lines().filter_map(|l| l.strip_prefix("duration=")?.trim().parse::<f64>().ok()).next()).unwrap_or(0.0);
    Ok(VideoInfo { width: w as u32, height: h as u32, fps: fps.clamp(1.0, 120.0), duration })
}

// ---------------------------------------------------------------------------------------------
// Origen de cuadros
// ---------------------------------------------------------------------------------------------

/// De dónde salen los cuadros (`ffmpeg` en producción; una lista en las pruebas).
pub trait FrameSource: Send {
    /// El siguiente cuadro ya decodificado, sin esperar.
    fn try_next(&mut self) -> Option<Frame>;
    /// ¿Se acabó el vídeo (o falló el decodificador)?
    fn finished(&self) -> bool;
}

/// Decodificación con `ffmpeg` desde `start` segundos. Un hilo lee los cuadros de la tubería; al estar el canal
/// limitado, `ffmpeg` se frena solo cuando la interfaz no consume.
pub struct FfmpegStream {
    rx: Receiver<Frame>,
    child: Arc<Mutex<Option<Child>>>,
    done: bool,
}

impl FfmpegStream {
    pub fn spawn(path: &Path, info: &VideoInfo, start: f64, single_frame: bool, wake: impl Fn() + Send + 'static) -> Result<Self, String> {
        let (w, h) = info.decode_size();
        let mut cmd = tool("ffmpeg");
        cmd.args(["-v", "error", "-nostdin", "-ss", &format!("{:.3}", start.max(0.0)), "-i"]).arg(path);
        cmd.args(["-an", "-sn", "-vf", &format!("fps={:.4},scale={w}:{h}:flags=bilinear", info.fps), "-pix_fmt", "rgba"]);
        if single_frame {
            cmd.args(["-frames:v", "1"]);
        }
        cmd.args(["-f", "rawvideo", "pipe:1"]).stdout(Stdio::piped()).stderr(Stdio::null());
        let mut child = cmd.spawn().map_err(|e| if e.kind() == std::io::ErrorKind::NotFound { "no se encontró ffmpeg en el PATH".to_string() } else { e.to_string() })?;
        let mut stdout = child.stdout.take().ok_or("sin salida de ffmpeg")?;
        let child = Arc::new(Mutex::new(Some(child)));
        let (tx, rx) = sync_channel::<Frame>(READ_AHEAD);
        let (fps, size) = (info.fps, (w as usize) * (h as usize) * 4);
        std::thread::Builder::new()
            .name("qlab-video-reader".into())
            .spawn(move || {
                let mut n = 0u64;
                loop {
                    let mut buf = vec![0u8; size];
                    if stdout.read_exact(&mut buf).is_err() {
                        break;
                    }
                    let frame = Frame { pts: start + n as f64 / fps, width: w as usize, height: h as usize, rgba: buf };
                    n += 1;
                    if tx.send(frame).is_err() {
                        break; // el reproductor se descartó
                    }
                    wake();
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Self { rx, child, done: false })
    }
}

impl FrameSource for FfmpegStream {
    fn try_next(&mut self) -> Option<Frame> {
        match self.rx.try_recv() {
            Ok(f) => Some(f),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                self.done = true;
                None
            }
        }
    }

    fn finished(&self) -> bool {
        self.done
    }
}

impl Drop for FfmpegStream {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.lock().ok().and_then(|mut g| g.take()) {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Sincronización con el reloj de audio
// ---------------------------------------------------------------------------------------------

/// Lo ocurrido en un paso de sincronización.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SyncReport {
    /// Cuadros decodificados que se saltaron sin mostrarse por ir el reloj por delante.
    pub dropped: usize,
    /// Se reinició el decodificador en otra posición.
    pub restarted: bool,
    /// Hay un cuadro nuevo que subir a la textura.
    pub changed: bool,
}

/// Qué cuadro toca mostrar según el reloj de audio, con descarte de los atrasados y reinicio cuando el reloj salta.
pub struct SyncedPlayer {
    source: Option<Box<dyn FrameSource>>,
    /// Crea un origen de cuadros desde un instante; el `bool` pide un solo cuadro (miniatura) en lugar de un flujo.
    make: Box<dyn FnMut(f64, bool) -> Result<Box<dyn FrameSource>, String> + Send>,
    /// El origen actual es una miniatura de un solo cuadro (no sirve para reproducir).
    source_is_still: bool,
    current: Option<Frame>,
    pending: Option<Frame>,
    pub total_dropped: usize,
    pub restarts: usize,
    pub error: Option<String>,
}

impl SyncedPlayer {
    pub fn new(make: impl FnMut(f64, bool) -> Result<Box<dyn FrameSource>, String> + Send + 'static) -> Self {
        Self { source: None, source_is_still: false, make: Box::new(make), current: None, pending: None, total_dropped: 0, restarts: 0, error: None }
    }

    pub fn current(&self) -> Option<&Frame> {
        self.current.as_ref()
    }

    fn restart(&mut self, at: f64, still: bool) {
        self.pending = None;
        match (self.make)(at, still) {
            Ok(s) => {
                self.source = Some(s);
                self.source_is_still = still;
                self.error = None;
            }
            Err(e) => {
                self.source = None;
                self.error = Some(e);
            }
        }
        self.restarts += 1;
    }

    /// Un paso con el reloj de audio en `clock` (segundos del archivo).
    pub fn sync(&mut self, clock: f64) -> SyncReport {
        let mut report = SyncReport::default();
        let off = self.current.as_ref().is_some_and(|c| clock + BACKWARD_TOLERANCE < c.pts || clock > c.pts + FORWARD_TOLERANCE && self.pending.as_ref().is_none_or(|p| clock > p.pts + FORWARD_TOLERANCE));
        if self.source.is_none() && self.error.is_none() || self.source_is_still || off {
            self.restart(clock, false);
            report.restarted = true;
        }
        let Some(source) = self.source.as_mut() else { return report };
        let mut newest: Option<Frame> = None;
        loop {
            let next = self.pending.take().or_else(|| source.try_next());
            match next {
                Some(f) if f.pts <= clock => {
                    if newest.is_some() {
                        report.dropped += 1;
                    }
                    newest = Some(f);
                }
                Some(f) => {
                    self.pending = Some(f);
                    break;
                }
                None => break,
            }
        }
        if let Some(f) = newest {
            // El cuadro anterior que seguía mostrado y ya no sirve también cuenta como descartado si hubo saltos.
            self.current = Some(f);
            report.changed = true;
        }
        self.total_dropped += report.dropped;
        report
    }

    /// Cuadro fijo en `at` (vista previa sin reproducción): decodifica uno solo y se queda con él.
    pub fn still(&mut self, at: f64) -> bool {
        if self.current.is_some() || self.error.is_some() {
            return false;
        }
        if self.source.is_none() {
            self.restart(at, true);
        }
        if let Some(s) = self.source.as_mut() {
            // La decodificación es asíncrona: el cuadro llega en unos milisegundos; se reintenta en el siguiente paso.
            if let Some(f) = s.try_next() {
                self.current = Some(f);
                return true;
            }
        }
        false
    }

    /// Reintento de la vista previa fija: recoge el cuadro cuando llega.
    pub fn poll_still(&mut self) -> bool {
        if self.current.is_some() {
            return false;
        }
        if let Some(f) = self.source.as_mut().and_then(|s| s.try_next()) {
            self.current = Some(f);
            return true;
        }
        false
    }

    pub fn forget_frame(&mut self) {
        self.current = None;
        self.pending = None;
        self.source = None;
    }
}

// ---------------------------------------------------------------------------------------------
// Reproductor con textura y almacén
// ---------------------------------------------------------------------------------------------

/// Metadatos de una cue de vídeo que hacen falta para dibujarla.
#[derive(Clone, Debug, PartialEq)]
pub struct VideoMeta {
    pub path: PathBuf,
    /// Frecuencia de muestreo del audio del archivo (el reloj `source_frame` va en esos frames).
    pub sample_rate: u32,
    pub start_time_sec: f64,
    pub fit: qlab_protocol::show_format::VideoFit,
}

pub struct VideoPlayer {
    pub meta: VideoMeta,
    pub info: Option<VideoInfo>,
    player: Option<SyncedPlayer>,
    pub texture: Option<egui::TextureHandle>,
    pub error: Option<String>,
}

impl VideoPlayer {
    pub fn new(ctx: &egui::Context, meta: VideoMeta) -> Self {
        let mut p = Self { meta, info: None, player: None, texture: None, error: None };
        p.open(ctx);
        p
    }

    fn open(&mut self, ctx: &egui::Context) {
        if !ffmpeg_available() {
            self.error = Some("Vista previa no disponible: no se encontró ffmpeg (el audio suena igual)".into());
            return;
        }
        match probe(&self.meta.path) {
            Ok(info) => {
                let (path, wake_ctx, info2) = (self.meta.path.clone(), ctx.clone(), info.clone());
                self.player = Some(SyncedPlayer::new(move |at, single| {
                    let wake = wake_ctx.clone();
                    FfmpegStream::spawn(&path, &info2, at, single, move || wake.request_repaint()).map(|s| Box::new(s) as Box<dyn FrameSource>)
                }));
                self.info = Some(info);
            }
            Err(e) => self.error = Some(format!("No se pudo leer el vídeo: {e}")),
        }
    }

    /// Cuadro que corresponde al reloj de audio (`clock` en segundos del archivo).
    pub fn sync(&mut self, ctx: &egui::Context, clock: f64) -> SyncReport {
        let Some(p) = self.player.as_mut() else { return SyncReport::default() };
        let r = p.sync(clock);
        self.error = p.error.clone().or(self.error.take());
        if r.changed {
            self.upload(ctx);
        }
        r
    }

    /// Miniatura sin reproducción: el cuadro del instante `at`.
    pub fn show_still(&mut self, ctx: &egui::Context, at: f64) {
        let Some(p) = self.player.as_mut() else { return };
        let got = p.still(at) || p.poll_still();
        self.error = p.error.clone().or(self.error.take());
        if got {
            self.upload(ctx);
        }
    }

    /// Vuelve a la miniatura (al terminar o parar la cue): se descarta lo reproducido.
    pub fn reset_to_still(&mut self) {
        if let Some(p) = self.player.as_mut() {
            p.forget_frame();
        }
    }

    fn upload(&mut self, ctx: &egui::Context) {
        let Some(f) = self.player.as_ref().and_then(|p| p.current()) else { return };
        let image = egui::ColorImage::from_rgba_unmultiplied([f.width, f.height], &f.rgba);
        match &mut self.texture {
            Some(t) => t.set(image, egui::TextureOptions::LINEAR),
            None => self.texture = Some(ctx.load_texture(format!("video-{}", self.meta.path.display()), image, egui::TextureOptions::LINEAR)),
        }
    }

    /// Instante (en el archivo) del cuadro que se está mostrando.
    pub fn shown_pts(&self) -> Option<f64> {
        self.player.as_ref().and_then(|p| p.current()).map(|f| f.pts)
    }

    pub fn stats(&self) -> (usize, usize) {
        self.player.as_ref().map_or((0, 0), |p| (p.total_dropped, p.restarts))
    }
}

/// Reproductores por cue (a lo sumo los que suenan más el seleccionado).
#[derive(Default)]
pub struct VideoHub {
    pub players: HashMap<u64, VideoPlayer>,
}

impl VideoHub {
    pub fn player(&mut self, ctx: &egui::Context, id: u64, meta: &VideoMeta) -> &mut VideoPlayer {
        // Cambiar solo el ajuste (`fit`) no reinicia el decodificador; cambiar de archivo, frecuencia o inicio sí.
        let stale = self.players.get(&id).is_some_and(|p| p.meta.path != meta.path || p.meta.sample_rate != meta.sample_rate || p.meta.start_time_sec != meta.start_time_sec);
        if stale {
            self.players.remove(&id);
        }
        let p = self.players.entry(id).or_insert_with(|| VideoPlayer::new(ctx, meta.clone()));
        p.meta.fit = meta.fit;
        p
    }

    pub fn keep_only(&mut self, ids: &[u64]) {
        self.players.retain(|id, _| ids.contains(id));
    }

    pub fn texture_of(&self, id: u64) -> Option<&egui::TextureHandle> {
        self.players.get(&id)?.texture.as_ref()
    }
}

/// Rectángulo donde se dibuja una imagen `img` (w, h) dentro de `area` según el ajuste, y la zona de la textura (uv).
pub fn fit_rect(area: egui::Rect, img: (f32, f32), fit: qlab_protocol::show_format::VideoFit) -> (egui::Rect, egui::Rect) {
    use qlab_protocol::show_format::VideoFit;
    let full_uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
    if img.0 <= 0.0 || img.1 <= 0.0 || area.width() <= 0.0 || area.height() <= 0.0 {
        return (area, full_uv);
    }
    let (ka, ki) = (area.width() / area.height(), img.0 / img.1);
    match fit {
        VideoFit::Stretch => (area, full_uv),
        VideoFit::Fit => {
            let size = if ki > ka { egui::vec2(area.width(), area.width() / ki) } else { egui::vec2(area.height() * ki, area.height()) };
            (egui::Rect::from_center_size(area.center(), size), full_uv)
        }
        VideoFit::Fill => {
            // Se llena el área y se recorta la textura por los lados que sobran.
            let uv = if ki > ka {
                let w = ka / ki;
                egui::Rect::from_min_max(egui::pos2((1.0 - w) / 2.0, 0.0), egui::pos2((1.0 + w) / 2.0, 1.0))
            } else {
                let h = ki / ka;
                egui::Rect::from_min_max(egui::pos2(0.0, (1.0 - h) / 2.0), egui::pos2(1.0, (1.0 + h) / 2.0))
            };
            (area, uv)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qlab_protocol::show_format::VideoFit;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Origen de pruebas: cuadros a 25 fps desde `start`, todos disponibles de golpe.
    struct Fake {
        frames: VecDeque<Frame>,
    }

    fn fake(start: f64, count: usize) -> Box<dyn FrameSource> {
        Box::new(Fake { frames: (0..count).map(|n| Frame { pts: start + n as f64 / 25.0, width: 2, height: 2, rgba: vec![n as u8; 16] }).collect() })
    }

    impl FrameSource for Fake {
        fn try_next(&mut self) -> Option<Frame> {
            self.frames.pop_front()
        }
        fn finished(&self) -> bool {
            self.frames.is_empty()
        }
    }

    fn player(starts: Arc<Mutex<Vec<f64>>>) -> SyncedPlayer {
        SyncedPlayer::new(move |at, _| {
            starts.lock().unwrap().push(at);
            Ok(fake(at, 250))
        })
    }

    #[test]
    fn the_frame_shown_is_the_latest_one_not_after_the_audio_clock_and_late_frames_are_dropped() {
        let starts = Arc::new(Mutex::new(vec![]));
        let mut p = player(Arc::clone(&starts));
        // Primer paso con el reloj en 0,2 s: se decodifican desde 0,2 (reinicio inicial) y se muestra el de 0,2.
        let r = p.sync(0.2);
        assert!(r.restarted && r.changed);
        assert_eq!(*starts.lock().unwrap(), [0.2]);
        assert!((p.current().unwrap().pts - 0.2).abs() < 1e-9);
        // La interfaz «se atrasa»: el reloj avanza 0,5 s de golpe; los cuadros intermedios se descartan.
        let r = p.sync(0.7);
        assert_eq!(r.dropped, 11, "de 0,24 a 0,68 son 12 cuadros; se muestra el último (0,68) y se saltan 11");
        assert!((p.current().unwrap().pts - 0.68).abs() < 1e-9, "{}", p.current().unwrap().pts);
        assert!(!r.restarted, "mientras el decodificador va por delante no hace falta reiniciar");
        // Sin avance del reloj no hay cuadro nuevo ni descartes.
        let r = p.sync(0.7);
        assert_eq!((r.changed, r.dropped), (false, 0));
        assert_eq!(p.total_dropped, 11);
    }

    #[test]
    fn a_backward_jump_of_the_clock_restarts_the_decoder_at_the_new_position() {
        let starts = Arc::new(Mutex::new(vec![]));
        let mut p = player(Arc::clone(&starts));
        p.sync(2.0);
        p.sync(2.5);
        // Un bucle de audio devuelve el reloj a 1,0 s: el vídeo salta con él (no se queda en 2,5).
        let r = p.sync(1.0);
        assert!(r.restarted);
        assert!((p.current().unwrap().pts - 1.0).abs() < 1e-9);
        assert_eq!(*starts.lock().unwrap(), [2.0, 1.0]);
        // Pequeñas oscilaciones hacia atrás (jitter del reloj) no reinician.
        let r = p.sync(0.9);
        assert!(!r.restarted && !r.changed);
    }

    #[test]
    fn a_decoder_that_falls_far_behind_is_restarted_instead_of_replaying_old_frames() {
        let count = Arc::new(AtomicUsize::new(0));
        let c2 = Arc::clone(&count);
        // Origen que solo entrega 5 cuadros por reinicio: se «agota» y el reloj se aleja.
        let mut p = SyncedPlayer::new(move |at, _| {
            c2.fetch_add(1, Ordering::Relaxed);
            Ok(fake(at, 5))
        });
        p.sync(0.0);
        p.sync(0.2);
        assert_eq!(count.load(Ordering::Relaxed), 1);
        let r = p.sync(3.0); // el último cuadro es ~0,16 s: el reloj va 2,8 s por delante
        assert!(r.restarted && count.load(Ordering::Relaxed) == 2);
        assert!((p.current().unwrap().pts - 3.0).abs() < 1e-9);
    }

    #[test]
    fn an_unavailable_decoder_reports_the_error_once_and_does_not_spin() {
        let n = Arc::new(AtomicUsize::new(0));
        let n2 = Arc::clone(&n);
        let mut p = SyncedPlayer::new(move |_, _| {
            n2.fetch_add(1, Ordering::Relaxed);
            Err("no se encontró ffmpeg".into())
        });
        for t in [0.0, 0.1, 0.2, 5.0] {
            p.sync(t);
        }
        assert_eq!(n.load(Ordering::Relaxed), 1, "no reintenta en cada fotograma");
        assert_eq!(p.error.as_deref(), Some("no se encontró ffmpeg"));
        assert!(p.current().is_none());
    }

    #[test]
    fn still_frames_are_decoded_once_for_the_preview_and_playback_replaces_the_still_source() {
        let starts = Arc::new(Mutex::new(Vec::<(f64, bool)>::new()));
        let s2 = Arc::clone(&starts);
        // El origen de miniatura entrega un cuadro con retraso (decodificación asíncrona).
        let mut p = SyncedPlayer::new(move |at, still| {
            s2.lock().unwrap().push((at, still));
            Ok(if still { Box::new(Delayed { left: 3, frame: Some(Frame { pts: at, width: 2, height: 2, rgba: vec![0; 16] }) }) as Box<dyn FrameSource> } else { fake(at, 100) })
        });
        assert!(!p.still(1.5), "el cuadro aún no llegó");
        // Los reintentos no reinician el decodificador (si no, nunca llegaría a decodificar nada).
        assert!(!p.still(1.5) && !p.poll_still());
        assert!(p.poll_still());
        assert!((p.current().unwrap().pts - 1.5).abs() < 1e-9);
        assert!(!p.still(9.0), "ya hay miniatura: no se vuelve a decodificar");
        assert_eq!(*starts.lock().unwrap(), [(1.5, true)], "una sola decodificación para la miniatura");
        // Al reproducir se sustituye el origen de un cuadro por un flujo normal.
        let r = p.sync(1.6);
        assert!(r.restarted && r.changed);
        assert_eq!(starts.lock().unwrap().last(), Some(&(1.6, false)));
    }

    /// Origen que tarda unas consultas en entregar su único cuadro.
    struct Delayed {
        left: usize,
        frame: Option<Frame>,
    }

    impl FrameSource for Delayed {
        fn try_next(&mut self) -> Option<Frame> {
            if self.left > 0 {
                self.left -= 1;
                return None;
            }
            self.frame.take()
        }
        fn finished(&self) -> bool {
            self.frame.is_none()
        }
    }

    #[test]
    fn decode_size_keeps_the_aspect_ratio_inside_the_limits_with_even_sides() {
        let v = |w, h| VideoInfo { width: w, height: h, fps: 25.0, duration: 1.0 };
        assert_eq!(v(1920, 1080).decode_size(), (960, 540));
        assert_eq!(v(320, 180).decode_size(), (320, 180), "no se amplía");
        assert_eq!(v(1080, 1920).decode_size(), (304, 540), "vertical");
        assert_eq!(v(1000, 563).decode_size().0 % 2, 0);
        assert_eq!(v(3, 3).decode_size(), (2, 2));
    }

    #[test]
    fn ffprobe_output_is_parsed_with_fallbacks() {
        let ok = parse_probe("width=1280\nheight=720\navg_frame_rate=30000/1001\nduration=12.5\n").unwrap();
        assert_eq!((ok.width, ok.height), (1280, 720));
        assert!((ok.fps - 29.97).abs() < 0.01 && (ok.duration - 12.5).abs() < 1e-9);
        // Sin duración de pista (mkv) se usa la del contenedor; sin fps, 25.
        let mkv = parse_probe("width=640\nheight=360\navg_frame_rate=0/0\nduration=N/A\nduration=3.0\n").unwrap();
        assert_eq!((mkv.fps, mkv.duration), (25.0, 3.0));
        assert!(parse_probe("duration=3.0\n").is_err(), "sin pista de vídeo");
    }

    #[test]
    fn fit_modes_place_the_image_inside_the_screen() {
        let screen = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1920.0, 1080.0));
        // 4:3 en pantalla 16:9: Fit deja barras laterales; Fill recorta arriba y abajo; Stretch llena.
        let (r, uv) = fit_rect(screen, (640.0, 480.0), VideoFit::Fit);
        assert_eq!((r.width(), r.height(), uv.width()), (1440.0, 1080.0, 1.0));
        assert_eq!(r.center(), screen.center());
        let (r, uv) = fit_rect(screen, (640.0, 480.0), VideoFit::Fill);
        assert_eq!(r, screen);
        assert!((uv.height() - 0.75).abs() < 1e-6 && uv.width() == 1.0, "{uv:?}");
        let (r, uv) = fit_rect(screen, (640.0, 480.0), VideoFit::Stretch);
        assert_eq!((r, uv.width(), uv.height()), (screen, 1.0, 1.0));
        // Vídeo más ancho que la pantalla: Fit deja barras arriba y abajo.
        let (r, _) = fit_rect(screen, (2560.0, 800.0), VideoFit::Fit);
        assert_eq!((r.width(), r.height()), (1920.0, 600.0));
    }
}
