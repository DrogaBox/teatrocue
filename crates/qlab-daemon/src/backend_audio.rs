//! Backend de audio de tiempo real (spec §2.3).
//!
//! * **Linux**: `cpal` sobre PipeWire nativo (con ALSA como respaldo, que `pipewire-alsa` redirige a
//!   PipeWire), con buffer fijo de baja latencia y `SCHED_FIFO` en el hilo de audio.
//! * **Windows**: `cpal` con WASAPI (o ASIO con la feature `asio`); con `exclusive = true` se intenta
//!   primero WASAPI en **modo exclusivo** por eventos (`wasapi_exclusive`) y, si el dispositivo lo
//!   rechaza, se cae a WASAPI compartido avisando.
//! * **Nulo**: un hilo que ejecuta el mismo callback a ritmo de reloj, sin dispositivo (CI, VM).
//!
//! La lógica del callback vive en [`RtRenderer`], común a todos los backends. No asigna memoria, no
//! toma bloqueos y no hace E/S: solo atómicos, el anillo SPSC y `process_rt_callback`.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use qlab_engine::alloc_guard::{violations_on_this_thread, RtScope};
use qlab_engine::drift::{DriftConfig, DriftResampler, FrameRing};
use qlab_engine::rt_command::{AudioCommand, AudioStatus, MAX_OUTPUT_CHANNELS};
use qlab_engine::voice::Voice;
use qlab_engine::process_rt_callback;
use ringbuf::{HeapCons, HeapProd};

use crate::health::{Epoch, Health};
use crate::rt_priority::{elevate_current_thread, ThreadRole};
use crate::shm::SharedMemory;

#[cfg(windows)]
mod wasapi_exclusive;

// ---------------------------------------------------------------------------------------------
// Configuración y errores
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendKind {
    /// Dispositivo real mediante `cpal` (y WASAPI exclusivo si se pide en Windows).
    Cpal,
    /// Sin dispositivo: un hilo ejecuta el callback al ritmo del reloj.
    Null,
}

#[derive(Clone, Debug)]
pub struct BackendConfig {
    pub kind: BackendKind,
    /// Nombre del host de `cpal` (`pipewire`, `alsa`, `wasapi`, `asio`, `jack`…); `None` = el predeterminado.
    pub host: Option<String>,
    /// Nombre del dispositivo (o, en modo exclusivo de Windows, su ID de endpoint).
    pub device: Option<String>,
    /// Dispositivo de salida **independiente** para los auriculares (PFL / audición), p. ej. el jack de la notebook o
    /// otra placa USB. `None` (o el mismo que `device`) = los auriculares usan canales extra de la placa principal.
    pub monitor_device: Option<String>,
    pub sample_rate: u32,
    pub channels: u16,
    pub buffer_frames: u32,
    /// Windows: pedir WASAPI en modo exclusivo.
    pub exclusive: bool,
    /// Prioridad `SCHED_FIFO` (Linux); en Windows se usa MMCSS "Pro Audio".
    pub rt_priority: u8,
    /// Vigilar asignaciones dentro del callback (requiere el asignador de `qlab_engine::alloc_guard`).
    pub rt_alloc_check: bool,
}

/// Canales internos que se añaden a la mezcla para el bus de auriculares cuando va a otra placa (un par estéreo).
pub const MONITOR_BUS_CHANNELS: usize = 2;
/// Frames que el bus de auriculares retiene entre las dos placas (≈ 0,34 s a 48 kHz: sobra para la deriva de reloj).
const MONITOR_RING_FRAMES: usize = 16_384;
/// Tamaño máximo de un trozo que procesa el callback cuando hay bus de auriculares (se reserva una vez, al abrir).
const TAP_CHUNK_FRAMES: usize = 4_096;

impl BackendConfig {
    /// El dispositivo de auriculares si es **otra placa** distinta de la principal.
    pub fn separate_monitor_device(&self) -> Option<&str> {
        let m = self.monitor_device.as_deref().map(str::trim).filter(|m| !m.is_empty())?;
        let same = self.device.as_deref().is_some_and(|d| d.trim().eq_ignore_ascii_case(m));
        (!same && usize::from(self.channels) + MONITOR_BUS_CHANNELS <= MAX_OUTPUT_CHANNELS).then_some(m)
    }

    /// Canales con los que se cargan las voces: los de la placa principal más el par de auriculares si va aparte.
    pub fn mix_channels(&self) -> usize {
        usize::from(self.channels) + if self.separate_monitor_device().is_some() { MONITOR_BUS_CHANNELS } else { 0 }
    }
}

impl Default for BackendConfig {
    fn default() -> Self {
        Self {
            kind: BackendKind::Cpal,
            host: None,
            device: None,
            monitor_device: None,
            sample_rate: 48_000,
            channels: 2,
            buffer_frames: 128,
            exclusive: false,
            rt_priority: 90,
            rt_alloc_check: cfg!(debug_assertions),
        }
    }
}

#[derive(Debug)]
pub enum BackendError {
    HostUnavailable(String),
    NoDevice(String),
    Unsupported(String),
    Stream(String),
}

impl std::fmt::Display for BackendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::HostUnavailable(s) => write!(f, "host de audio no disponible: {s}"),
            Self::NoDevice(s) => write!(f, "sin dispositivo de salida: {s}"),
            Self::Unsupported(s) => write!(f, "configuración no soportada: {s}"),
            Self::Stream(s) => write!(f, "error de stream: {s}"),
        }
    }
}

impl std::error::Error for BackendError {}

fn stream_err(e: cpal::Error) -> BackendError {
    BackendError::Stream(e.to_string())
}

/// Lo que se logró abrir.
#[derive(Clone, Debug, Default)]
pub struct BackendInfo {
    pub backend: String,
    pub device: String,
    pub sample_rate: u32,
    pub channels: u16,
    pub buffer_frames: u32,
    pub exclusive_active: bool,
    pub warnings: Vec<String>,
    /// Placa de auriculares abierta aparte (`None` = no hay o van en canales de la principal).
    pub monitor_device: Option<String>,
}

impl BackendInfo {
    pub fn latency_ms(&self) -> f64 {
        self.buffer_frames as f64 * 1000.0 / self.sample_rate.max(1) as f64
    }
}

// ---------------------------------------------------------------------------------------------
// Estado exclusivo del callback
// ---------------------------------------------------------------------------------------------

/// Lo que el callback necesita y nadie más toca.
pub struct RtAudioContext {
    pub voices: Arc<ArcSwap<Vec<Arc<Voice>>>>,
    pub cmd_rx: HeapCons<AudioCommand>,
    pub status_tx: HeapProd<AudioStatus>,
    pub tail: [f32; MAX_OUTPUT_CHANNELS],
}

impl RtAudioContext {
    pub fn new(voices: Arc<ArcSwap<Vec<Arc<Voice>>>>, cmd_rx: HeapCons<AudioCommand>, status_tx: HeapProd<AudioStatus>) -> Self {
        Self { voices, cmd_rx, status_tx, tail: [0.0; MAX_OUTPUT_CHANNELS] }
    }
}

/// Celda de acceso exclusivo **sin bloqueo**: `with` toma el contenido o devuelve `None` al instante.
/// Permite reiniciar el stream (otro callback) sin mover los anillos ni usar un `Mutex`.
pub struct RtCell<T> {
    busy: AtomicBool,
    value: UnsafeCell<T>,
}

// SAFETY: `busy` garantiza que como mucho un hilo accede a `value` a la vez.
unsafe impl<T: Send> Sync for RtCell<T> {}
unsafe impl<T: Send> Send for RtCell<T> {}

impl<T> RtCell<T> {
    pub fn new(value: T) -> Self {
        Self { busy: AtomicBool::new(false), value: UnsafeCell::new(value) }
    }

    #[inline]
    pub fn with<R>(&self, f: impl FnOnce(&mut T) -> R) -> Option<R> {
        if self.busy.swap(true, Ordering::Acquire) {
            return None;
        }
        struct Release<'a>(&'a AtomicBool);
        impl Drop for Release<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::Release);
            }
        }
        let _release = Release(&self.busy);
        // SAFETY: la bandera `busy` que acabamos de tomar excluye a cualquier otro acceso.
        Some(f(unsafe { &mut *self.value.get() }))
    }
}

/// Piezas compartidas con las que se construye un [`RtRenderer`] (también al reiniciar el stream).
#[derive(Clone)]
pub struct RendererParts {
    pub cell: Arc<RtCell<RtAudioContext>>,
    pub health: Arc<Health>,
    pub shm: Arc<SharedMemory>,
    pub epoch: Epoch,
    pub rt_priority: u8,
    pub alloc_check: bool,
    /// Anillo por el que el bus de auriculares pasa a la segunda placa (`None` = auriculares en la misma placa).
    pub monitor_tap: Option<Arc<FrameRing>>,
}

/// Bus de auriculares de una placa aparte: el callback principal mezcla `canales + 2` y envía el par extra al anillo.
struct MonitorTap {
    ring: Arc<FrameRing>,
    /// Mezcla entrelazada de `canales + 2` (reservada al abrir: el callback no asigna).
    scratch: Vec<f32>,
    /// Par de auriculares entrelazado.
    pair: Vec<f32>,
}

/// Constante de tiempo de la rampa del safe mute.
const SAFE_MUTE_RAMP_SECS: f32 = 0.010;

/// El callback de audio. Cada stream tiene el suyo.
pub struct RtRenderer {
    parts: RendererParts,
    channels: usize,
    sample_rate: u32,
    first_call: bool,
    last_start: Option<Instant>,
    last_period_ns: u64,
    cpu_smoothed: u32,
    mute_gain: f32,
    /// Ganancia lineal del volumen maestro (suavizada).
    master_gain: f32,
    tap: Option<MonitorTap>,
}

impl RtRenderer {
    pub fn new(parts: RendererParts, channels: usize, sample_rate: u32) -> Self {
        let channels = channels.max(1);
        let tap = parts.monitor_tap.clone().map(|ring| MonitorTap { ring, scratch: vec![0.0; TAP_CHUNK_FRAMES * (channels + MONITOR_BUS_CHANNELS)], pair: vec![0.0; TAP_CHUNK_FRAMES * MONITOR_BUS_CHANNELS] });
        Self { parts, channels, sample_rate: sample_rate.max(1), first_call: true, last_start: None, last_period_ns: 0, cpu_smoothed: 0, mute_gain: 1.0, master_gain: 1.0, tap }
    }

    /// Rellena `data` (entrelazado, `channels` canales). Invocado por el hilo de audio.
    pub fn render(&mut self, data: &mut [f32]) {
        let started = Instant::now();
        let p = &self.parts;
        let frames = data.len() / self.channels;
        let period_ns = frames as u64 * 1_000_000_000 / self.sample_rate as u64;

        if self.first_call {
            self.first_call = false;
            let ok = elevate_current_thread(ThreadRole::Audio, p.rt_priority).is_ok();
            p.health.audio_priority_ok.store(ok, Ordering::Relaxed);
            // `ArcSwap::load` prepara estado por hilo la primera vez: se hace fuera del ámbito vigilado.
            p.cell.with(|ctx| drop(ctx.voices.load()));
        }

        p.health.audio_heartbeat_ns.store(p.epoch.ns_of(started), Ordering::Release);
        p.health.audio_callbacks.fetch_add(1, Ordering::Relaxed);
        if let Some(prev) = self.last_start {
            if self.last_period_ns > 0 {
                let gap = started.duration_since(prev).as_nanos() as u64;
                let jitter = gap.abs_diff(self.last_period_ns);
                p.health.jitter_max_ns.fetch_max(jitter, Ordering::Relaxed);
                p.health.jitter_total_ns.fetch_add(jitter, Ordering::Relaxed);
                p.health.jitter_samples.fetch_add(1, Ordering::Relaxed);
            }
            if period_ns > 0 && started.duration_since(prev).as_nanos() as u64 > period_ns * 2 {
                p.health.xruns.fetch_add(1, Ordering::Relaxed);
            }
        }
        self.last_start = Some(started);
        self.last_period_ns = period_ns;

        let violations_before = violations_on_this_thread();
        let scope = p.alloc_check.then(RtScope::enter);
        let (channels, rate) = (self.channels, self.sample_rate);
        if let Some(tap) = self.tap.as_mut() {
            // Auriculares en otra placa: se mezcla `canales + 2`; los primeros salen por este stream y el par extra va
            // al anillo que lee el stream de la otra placa.
            let inner = channels + MONITOR_BUS_CHANNELS;
            let mut done = 0;
            while done < frames {
                let n = (frames - done).min(TAP_CHUNK_FRAMES);
                let buf = &mut tap.scratch[..n * inner];
                let rendered = p.cell.with(|ctx| {
                    process_rt_callback(buf, inner, rate, &ctx.voices, &mut ctx.cmd_rx, &mut ctx.status_tx, &mut ctx.tail);
                });
                if rendered.is_none() {
                    buf.fill(0.0);
                }
                apply_safe_mute(&p.health, &mut self.mute_gain, inner, rate, buf);
                apply_master(&p.health, &mut self.master_gain, inner, rate, buf);
                for (f, frame) in buf.chunks_exact(inner).enumerate() {
                    data[(done + f) * channels..(done + f + 1) * channels].copy_from_slice(&frame[..channels]);
                    tap.pair[f * 2] = frame[channels];
                    tap.pair[f * 2 + 1] = frame[channels + 1];
                }
                tap.ring.push(&tap.pair[..n * 2]);
                done += n;
            }
        } else {
            let rendered = p.cell.with(|ctx| {
                process_rt_callback(data, channels, rate, &ctx.voices, &mut ctx.cmd_rx, &mut ctx.status_tx, &mut ctx.tail);
            });
            if rendered.is_none() {
                // Otro callback tiene el contexto (reinicio en curso): silencio.
                data.fill(0.0);
            }
            apply_safe_mute(&p.health, &mut self.mute_gain, self.channels, self.sample_rate, data);
            apply_master(&p.health, &mut self.master_gain, self.channels, self.sample_rate, data);
        }
        report_peaks(&p.shm, self.channels, data);
        drop(scope);
        let new_violations = violations_on_this_thread() - violations_before;
        if new_violations > 0 {
            p.health.rt_alloc_violations.fetch_add(new_violations, Ordering::Relaxed);
        }

        if period_ns > 0 {
            let used = started.elapsed().as_nanos() as u64;
            let permille = (used.saturating_mul(1_000) / period_ns).min(u32::MAX as u64) as u32;
            self.cpu_smoothed = (self.cpu_smoothed * 7 + permille) / 8;
            p.health.cpu_permille.store(self.cpu_smoothed, Ordering::Relaxed);
            p.health.peak_cpu_permille.fetch_max(permille, Ordering::Relaxed);
            p.health.callback_max_ns.fetch_max(used, Ordering::Relaxed);
            p.health.callback_total_ns.fetch_add(used, Ordering::Relaxed);
            if used > period_ns {
                p.health.callback_over_budget.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

/// Rampa hacia 0 (o de vuelta a 1) según `Health::safe_mute`; sin coste cuando no hay mute.
#[inline]
fn apply_safe_mute(health: &Health, gain: &mut f32, channels: usize, sample_rate: u32, data: &mut [f32]) {
    let target = if health.safe_mute.load(Ordering::Relaxed) { 0.0 } else { 1.0 };
    if *gain == 1.0 && target == 1.0 {
        return;
    }
    let step = 1.0 / (SAFE_MUTE_RAMP_SECS * sample_rate as f32);
    let mut g = *gain;
    for frame in data.chunks_exact_mut(channels) {
        g = if g < target { (g + step).min(target) } else { (g - step).max(target) };
        for s in frame {
            *s *= g;
        }
    }
    *gain = g;
}

/// Ganancia lineal de un volumen maestro en décimas de dB.
pub fn master_linear(db_x10: i32) -> f32 {
    if db_x10 <= crate::health::MASTER_MUTE_X10 {
        0.0
    } else {
        10f32.powf(db_x10.min(crate::health::MASTER_MAX_X10) as f32 / 200.0)
    }
}

/// Volumen maestro: aplica la ganancia con un suavizado de ~10 ms para que moverlo no deje chasquidos; sin coste
/// cuando está en 0 dB.
#[inline]
fn apply_master(health: &Health, gain: &mut f32, channels: usize, sample_rate: u32, data: &mut [f32]) {
    let target = master_linear(health.master_db_x10.load(Ordering::Relaxed));
    if (*gain - target).abs() < 1e-6 && target == 1.0 {
        return;
    }
    let coef = 1.0 - (-1.0 / (SAFE_MUTE_RAMP_SECS * sample_rate.max(1) as f32)).exp();
    let mut g = *gain;
    // Los auriculares (PFL) quedan fuera del maestro: se escucha lo mismo aunque la sala esté a −20 dB.
    let mon = health.monitor_first.load(Ordering::Relaxed) as usize;
    let (mon_lo, mon_hi) = if mon > 0 { (mon - 1, mon + 1) } else { (usize::MAX, usize::MAX) };
    for frame in data.chunks_exact_mut(channels) {
        g += (target - g) * coef;
        for (c, s) in frame.iter_mut().enumerate() {
            if c < mon_lo || c >= mon_hi {
                *s *= g;
            }
        }
    }
    // Al llegar (casi) al objetivo se fija exacto: así el 0 dB vuelve a ser «sin coste».
    *gain = if (g - target).abs() < 1e-5 { target } else { g };
}

/// Pico por canal del bloque → medidores por consumo de la memoria compartida.
#[inline]
fn report_peaks(shm: &SharedMemory, channels: usize, data: &[f32]) {
    let n = channels.min(MAX_OUTPUT_CHANNELS);
    let mut peaks = [0.0f32; MAX_OUTPUT_CHANNELS];
    for frame in data.chunks_exact(channels) {
        for (peak, &s) in peaks.iter_mut().zip(frame).take(n) {
            *peak = peak.max(s.abs());
        }
    }
    let meters = &shm.telemetry().meters;
    for (ch, &peak) in peaks.iter().enumerate().take(n) {
        meters.report_output(ch, peak);
    }
}

// ---------------------------------------------------------------------------------------------
// Backend
// ---------------------------------------------------------------------------------------------

enum Running {
    Stopped,
    Cpal(#[allow(dead_code)] cpal::Stream),
    Null(#[allow(dead_code)] NullStream),
    #[cfg(windows)]
    Exclusive(#[allow(dead_code)] wasapi_exclusive::ExclusiveStream),
}

pub struct AudioBackend {
    cfg: BackendConfig,
    parts: RendererParts,
    running: Running,
    /// Stream de la segunda placa (auriculares), si la hay.
    monitor_stream: Option<cpal::Stream>,
    info: BackendInfo,
}

impl AudioBackend {
    /// Abre el stream y empieza a sonar (silencio hasta que lleguen comandos).
    pub fn start(cfg: BackendConfig, ctx: RtAudioContext, health: Arc<Health>, shm: Arc<SharedMemory>, epoch: Epoch) -> Result<Self, BackendError> {
        let parts = RendererParts {
            cell: Arc::new(RtCell::new(ctx)),
            health,
            shm,
            epoch,
            rt_priority: cfg.rt_priority,
            alloc_check: cfg.rt_alloc_check,
            monitor_tap: cfg.separate_monitor_device().map(|_| FrameRing::new(MONITOR_BUS_CHANNELS, MONITOR_RING_FRAMES)),
        };
        let mut backend = Self { cfg, parts, running: Running::Stopped, monitor_stream: None, info: BackendInfo::default() };
        backend.open()?;
        Ok(backend)
    }

    pub fn info(&self) -> &BackendInfo {
        &self.info
    }

    pub fn config(&self) -> &BackendConfig {
        &self.cfg
    }

    pub fn is_running(&self) -> bool {
        !matches!(self.running, Running::Stopped)
    }

    /// Para y vuelve a abrir el stream con la misma configuración (recuperación tras un congelamiento).
    pub fn restart(&mut self) -> Result<(), BackendError> {
        self.stop();
        self.open()
    }

    pub fn stop(&mut self) {
        self.parts.health.audio_running.store(false, Ordering::Release);
        self.running = Running::Stopped; // soltar el stream detiene el callback
        self.monitor_stream = None;
    }

    fn open(&mut self) -> Result<(), BackendError> {
        let h = &self.parts.health;
        h.audio_heartbeat_ns.store(self.parts.epoch.now_ns(), Ordering::Release);
        let (running, mut info) = match self.cfg.kind {
            BackendKind::Null => self.open_null(),
            BackendKind::Cpal => self.open_device()?,
        };
        // Auriculares en otra placa: segundo stream alimentado por el bus de monitoreo. Si no abre, el programa sigue
        // con la sala y la audición avisa en lugar de callar sin explicación.
        self.monitor_stream = None;
        if let (Some(name), Some(ring), BackendKind::Cpal) = (self.cfg.separate_monitor_device(), self.parts.monitor_tap.clone(), self.cfg.kind) {
            match self.open_monitor_stream(name, ring, info.sample_rate) {
                Ok((stream, device, warns)) => {
                    self.monitor_stream = Some(stream);
                    info.monitor_device = Some(device);
                    info.warnings.extend(warns);
                }
                Err(e) => {
                    self.parts.health.monitor_first.store(0, Ordering::Relaxed);
                    info.warnings.push(format!("No se pudo abrir la salida de auriculares «{name}»: {e}. La audición (PFL) queda desactivada."));
                }
            }
        }
        self.running = running;
        self.info = info;
        h.audio_heartbeat_ns.store(self.parts.epoch.now_ns(), Ordering::Release);
        h.audio_running.store(true, Ordering::Release);
        Ok(())
    }

    fn open_null(&self) -> (Running, BackendInfo) {
        let renderer = RtRenderer::new(self.parts.clone(), self.cfg.channels as usize, self.cfg.sample_rate);
        let stream = NullStream::spawn(renderer, self.cfg.buffer_frames as usize, self.cfg.channels as usize, self.cfg.sample_rate);
        let info = BackendInfo {
            backend: "null".into(),
            device: "dispositivo virtual".into(),
            sample_rate: self.cfg.sample_rate,
            channels: self.cfg.channels,
            buffer_frames: self.cfg.buffer_frames,
            ..Default::default()
        };
        (Running::Null(stream), info)
    }

    /// Abre la placa de auriculares. Corre a su propia frecuencia y con su propio reloj: el audio llega por un anillo y
    /// un remuestreador Catmull-Rom con control PI de deriva lo adapta (ver `qlab_engine::drift`).
    fn open_monitor_stream(&self, wanted: &str, ring: Arc<FrameRing>, main_rate: u32) -> Result<(cpal::Stream, String, Vec<String>), BackendError> {
        let mut warnings = Vec::new();
        let device = find_output_device(self.cfg.host.as_deref(), wanted)?;
        let name = device.description().map(|d| d.name().to_string()).unwrap_or_else(|_| wanted.to_string());
        let ranges: Vec<_> = device.supported_output_configs().map_err(stream_err)?.filter(|r| r.sample_format() == cpal::SampleFormat::F32).collect();
        // Preferencia: estéreo; luego, la frecuencia de la placa principal si la admite.
        let range = ranges
            .iter()
            .filter(|r| r.channels() >= 1)
            .min_by_key(|r| {
                let rate_ok = r.min_sample_rate() <= main_rate && main_rate <= r.max_sample_rate();
                (r.channels() != 2, !rate_ok, r.channels())
            })
            .ok_or_else(|| BackendError::Unsupported(format!("«{name}» no ofrece salida f32")))?;
        let rate = main_rate.clamp(range.min_sample_rate(), range.max_sample_rate());
        if rate != main_rate {
            warnings.push(format!("los auriculares «{name}» corren a {rate} Hz y la sala a {main_rate} Hz: se remuestrea el audio de los auriculares"));
        }
        let buffer_size = match range.buffer_size() {
            cpal::SupportedBufferSize::Range { min, max } => cpal::BufferSize::Fixed(self.cfg.buffer_frames.clamp(*min, *max)),
            cpal::SupportedBufferSize::Unknown => cpal::BufferSize::Default,
        };
        let channels = range.channels() as usize;
        let config = cpal::StreamConfig { channels: range.channels(), sample_rate: rate, buffer_size };
        let mut drift = DriftConfig::new(rate as f32);
        // Margen de relleno: unos cuatro bloques del buffer pedido (el mínimo que aguanta la variación de tamaño).
        drift.target_fill_frames = (self.cfg.buffer_frames as f32 * 4.0).max(512.0);
        let mut resampler = DriftResampler::new(ring, drift, main_rate as f32, rate as f32);
        let mut scratch = vec![0.0f32; TAP_CHUNK_FRAMES * MONITOR_BUS_CHANNELS];
        let (rt_priority, mut first) = (self.cfg.rt_priority, true);
        let stream = device
            .build_output_stream::<f32, _, _>(
                config,
                move |data: &mut [f32], _| {
                    if first {
                        first = false;
                        let _ = elevate_current_thread(ThreadRole::Audio, rt_priority);
                    }
                    let frames = data.len() / channels;
                    let mut done = 0;
                    while done < frames {
                        let n = (frames - done).min(TAP_CHUNK_FRAMES);
                        let pair = &mut scratch[..n * MONITOR_BUS_CHANNELS];
                        resampler.render(pair);
                        for (f, lr) in pair.chunks_exact(2).enumerate() {
                            let out = &mut data[(done + f) * channels..(done + f + 1) * channels];
                            match channels {
                                1 => out[0] = 0.5 * (lr[0] + lr[1]),
                                _ => {
                                    out[0] = lr[0];
                                    out[1] = lr[1];
                                    out[2..].fill(0.0);
                                }
                            }
                        }
                        done += n;
                    }
                },
                move |err: cpal::Error| eprintln!("[audio] error del stream de auriculares: {err}"),
                None,
            )
            .map_err(stream_err)?;
        stream.play().map_err(stream_err)?;
        Ok((stream, name, warnings))
    }

    fn open_device(&self) -> Result<(Running, BackendInfo), BackendError> {
        let mut warnings = Vec::new();

        #[cfg(windows)]
        if self.cfg.exclusive {
            let parts = self.parts.clone();
            match wasapi_exclusive::ExclusiveStream::start(&self.cfg, move |channels, rate| RtRenderer::new(parts, channels, rate)) {
                Ok((stream, info)) => return Ok((Running::Exclusive(stream), info)),
                Err(e) => warnings.push(format!("WASAPI exclusivo no disponible ({e}); se usa el modo compartido")),
            }
        }
        #[cfg(not(windows))]
        if self.cfg.exclusive {
            warnings.push("el modo exclusivo solo existe en WASAPI (Windows); se ignora".into());
        }

        let host = select_host(self.cfg.host.as_deref())?;
        let host_name = host.id().name();
        let device = select_device(&host, self.cfg.device.as_deref())?;
        let device_name = device.description().map(|d| d.name().to_string()).unwrap_or_else(|_| "dispositivo sin nombre".into());
        let config = pick_config(&device, &self.cfg, &mut warnings)?;

        let mut renderer = RtRenderer::new(self.parts.clone(), config.channels as usize, config.sample_rate);
        let health_err = Arc::clone(&self.parts.health);
        let stream = device
            .build_output_stream::<f32, _, _>(
                config,
                move |data: &mut [f32], _| renderer.render(data),
                move |err: cpal::Error| {
                    if err.kind() == cpal::ErrorKind::Xrun {
                        health_err.xruns.fetch_add(1, Ordering::Relaxed);
                    }
                    eprintln!("[audio] error del stream: {err}");
                },
                None,
            )
            .map_err(stream_err)?;
        stream.play().map_err(stream_err)?;

        let buffer_frames = match config.buffer_size {
            cpal::BufferSize::Fixed(n) => n,
            cpal::BufferSize::Default => stream.buffer_size().unwrap_or(self.cfg.buffer_frames),
        };
        let info = BackendInfo {
            backend: format!("cpal/{host_name}"),
            device: device_name,
            sample_rate: config.sample_rate,
            channels: config.channels,
            buffer_frames,
            exclusive_active: false,
            warnings,
            monitor_device: None,
        };
        Ok((Running::Cpal(stream), info))
    }
}

impl Drop for AudioBackend {
    fn drop(&mut self) {
        self.stop();
    }
}

fn select_host(name: Option<&str>) -> Result<cpal::Host, BackendError> {
    match name {
        None => Ok(cpal::default_host()),
        Some(n) => {
            let id: cpal::HostId = n.parse().map_err(|_| {
                let names: Vec<_> = cpal::available_hosts().iter().map(|h| h.name()).collect();
                BackendError::HostUnavailable(format!("`{n}` no existe; hosts disponibles: {}", names.join(", ")))
            })?;
            cpal::host_from_id(id).map_err(|e| BackendError::HostUnavailable(e.to_string()))
        }
    }
}

/// Busca una salida por nombre en el host indicado y, si no está ahí, en cualquier otro host del sistema.
fn find_output_device(host_name: Option<&str>, wanted: &str) -> Result<cpal::Device, BackendError> {
    let host = select_host(host_name)?;
    match select_device(&host, Some(wanted)) {
        Ok(d) => Ok(d),
        Err(first) => {
            let w = wanted.to_lowercase();
            for id in cpal::available_hosts() {
                let Ok(h) = cpal::host_from_id(id) else { continue };
                let Ok(devices) = h.output_devices() else { continue };
                for d in devices {
                    let n = d.description().map(|d| d.name().to_string()).unwrap_or_default();
                    if n.to_lowercase() == w || (!w.is_empty() && n.to_lowercase().contains(&w)) {
                        return Ok(d);
                    }
                }
            }
            Err(first)
        }
    }
}

fn select_device(host: &cpal::Host, name: Option<&str>) -> Result<cpal::Device, BackendError> {
    match name {
        None => host.default_output_device().ok_or_else(|| BackendError::NoDevice("no hay dispositivo de salida predeterminado".into())),
        Some(wanted) => {
            let devices = host.output_devices().map_err(stream_err)?;
            let mut names = Vec::new();
            for d in devices {
                let n = d.description().map(|d| d.name().to_string()).unwrap_or_default();
                if n.eq_ignore_ascii_case(wanted) || (!wanted.is_empty() && n.to_lowercase().contains(&wanted.to_lowercase())) {
                    return Ok(d);
                }
                names.push(n);
            }
            Err(BackendError::NoDevice(format!("`{wanted}` no existe; salidas: {}", names.join(" | "))))
        }
    }
}

/// Elige `f32` con los canales y la frecuencia pedidos y un buffer fijo dentro del rango soportado.
fn pick_config(device: &cpal::Device, cfg: &BackendConfig, warnings: &mut Vec<String>) -> Result<cpal::StreamConfig, BackendError> {
    let ranges: Vec<_> = device.supported_output_configs().map_err(stream_err)?.collect();
    let range = ranges
        .iter()
        .find(|r| {
            r.sample_format() == cpal::SampleFormat::F32
                && r.channels() == cfg.channels
                && r.min_sample_rate() <= cfg.sample_rate
                && cfg.sample_rate <= r.max_sample_rate()
        })
        .ok_or_else(|| {
            let offered: Vec<_> = ranges
                .iter()
                .map(|r| format!("{}ch {:?} {}-{} Hz", r.channels(), r.sample_format(), r.min_sample_rate(), r.max_sample_rate()))
                .collect();
            BackendError::Unsupported(format!(
                "el dispositivo no ofrece f32, {} canales, {} Hz. Ofrece: {}",
                cfg.channels,
                cfg.sample_rate,
                offered.join("; ")
            ))
        })?;
    let buffer_size = match range.buffer_size() {
        cpal::SupportedBufferSize::Range { min, max } => {
            let frames = cfg.buffer_frames.clamp(*min, *max);
            if frames != cfg.buffer_frames {
                warnings.push(format!("buffer de {} frames fuera de [{min}, {max}]: se usa {frames}", cfg.buffer_frames));
            }
            cpal::BufferSize::Fixed(frames)
        }
        cpal::SupportedBufferSize::Unknown => cpal::BufferSize::Fixed(cfg.buffer_frames),
    };
    Ok(cpal::StreamConfig { channels: cfg.channels, sample_rate: cfg.sample_rate, buffer_size })
}

/// Lista `(host, dispositivo)` de salida para diagnóstico (`--list-devices`).
pub fn list_output_devices() -> Vec<(String, String)> {
    let mut out = Vec::new();
    for id in cpal::available_hosts() {
        let Ok(host) = cpal::host_from_id(id) else { continue };
        let Ok(devices) = host.output_devices() else { continue };
        for d in devices {
            out.push((id.name().to_string(), d.description().map(|d| d.name().to_string()).unwrap_or_default()));
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Backend nulo
// ---------------------------------------------------------------------------------------------

/// Hilo que invoca el callback a ritmo de reloj, con plazos absolutos para no acumular deriva.
pub struct NullStream {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl NullStream {
    fn spawn(mut renderer: RtRenderer, frames: usize, channels: usize, sample_rate: u32) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let period = Duration::from_secs_f64(frames as f64 / sample_rate as f64);
        let join = std::thread::Builder::new()
            .name("qlab-null-audio".into())
            .spawn(move || {
                let mut buf = vec![0.0f32; frames * channels];
                let mut next = Instant::now();
                while !flag.load(Ordering::Acquire) {
                    renderer.render(&mut buf);
                    next += period;
                    let now = Instant::now();
                    if next > now {
                        std::thread::sleep(next - now);
                    } else if now - next > period * 8 {
                        next = now; // se atrasó mucho: no recuperar a ráfagas
                    }
                }
            })
            .expect("no se pudo crear el hilo de audio nulo");
        Self { stop, join: Some(join) }
    }
}

impl Drop for NullStream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qlab_engine::disk_streamer::StreamingVoiceReader;
    use qlab_engine::rt_command::{COMMAND_RING_CAPACITY, STATUS_RING_CAPACITY};
    use qlab_engine::voice::{AudioLevelMatrixRt, VoiceInit, VoiceRtState};
    use qlab_engine::voice_pool::VoicePool;
    use ringbuf::traits::{Producer, Split};
    use ringbuf::HeapRb;

    #[allow(dead_code)]
    pub(crate) struct Rig {
        pub parts: RendererParts,
        pub health: Arc<Health>,
        pub shm: Arc<SharedMemory>,
        pub cmd_tx: HeapProd<AudioCommand>,
        pub st_rx: HeapCons<AudioStatus>,
        pub pool: VoicePool,
    }

    pub(crate) fn rig(tag: &str, alloc_check: bool) -> Rig {
        qlab_engine::alloc_guard::set_violation_policy(qlab_engine::alloc_guard::ViolationPolicy::CountOnly);
        let health = Arc::new(Health::default());
        let shm = Arc::new(SharedMemory::create(&format!("qlab_test_{}_{tag}", std::process::id())).unwrap());
        let pool = VoicePool::new(8);
        let (cmd_tx, cmd_rx) = HeapRb::<AudioCommand>::new(COMMAND_RING_CAPACITY).split();
        let (st_tx, st_rx) = HeapRb::<AudioStatus>::new(STATUS_RING_CAPACITY).split();
        let ctx = RtAudioContext::new(pool.rt_handle(), cmd_rx, st_tx);
        let parts = RendererParts {
            cell: Arc::new(RtCell::new(ctx)),
            health: Arc::clone(&health),
            shm: Arc::clone(&shm),
            epoch: Epoch::now(),
            rt_priority: 90,
            alloc_check,
            monitor_tap: None,
        };
        Rig { parts, health, shm, cmd_tx, st_rx, pool }
    }

    pub(crate) fn const_voice(id: u64, level: f32, frames: usize) -> Arc<Voice> {
        let reader = StreamingVoiceReader::from_interleaved(2, 0, vec![level; frames * 2].into_boxed_slice());
        let rt = VoiceRtState::new(0, &[], AudioLevelMatrixRt::new(2, 2)).unwrap();
        Arc::new(Voice::with_reader(
            VoiceInit { id, cue_unique_id: id as u128, patch_id: 0, file_sample_rate: 48_000, start_frame: 0, end_frame: frames as u64, infinite_loop: false, rate: 1.0 },
            rt,
            reader,
        ))
    }

    #[test]
    fn rt_cell_gives_exclusive_wait_free_access() {
        let cell = RtCell::new(5u32);
        assert_eq!(cell.with(|v| { *v += 1; *v }), Some(6));
        // Reentrancia: el acceso interior falla al instante en vez de bloquearse.
        let nested = cell.with(|_| cell.with(|v| *v));
        assert_eq!(nested, Some(None));
        assert_eq!(cell.with(|v| *v), Some(6), "se libera al terminar");
        let cell = Arc::new(RtCell::new(0u64));
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let c = Arc::clone(&cell);
                std::thread::spawn(move || (0..20_000).filter(|_| c.with(|v| *v += 1).is_some()).count() as u64)
            })
            .collect();
        let done: u64 = handles.into_iter().map(|h| h.join().unwrap()).sum();
        assert_eq!(cell.with(|v| *v), Some(done), "ningún incremento se pierde ni se duplica");
    }

    /// Voz estéreo constante cuyas dos entradas van a los canales `first..first+2` de una mezcla de `out` canales.
    fn routed_voice(id: u64, level: f32, frames: usize, out: usize, first: usize) -> Arc<Voice> {
        use qlab_protocol::matrix::MIN_VOLUME_DB;
        let reader = StreamingVoiceReader::from_interleaved(2, 0, vec![level; frames * 2].into_boxed_slice());
        let mut m = AudioLevelMatrixRt::new(2, out);
        for i in 1..=2 {
            for o in 1..=out {
                m.base_db[i][o] = if o == first + i { 0.0 } else { MIN_VOLUME_DB };
            }
        }
        m.matrix_dirty = true;
        m.recompute_targets(1.0, 1.0);
        m.snap_to_targets();
        let rt = VoiceRtState::new(0, &[], m).unwrap();
        Arc::new(Voice::with_reader(
            VoiceInit { id, cue_unique_id: id as u128, patch_id: 0, file_sample_rate: 48_000, start_frame: 0, end_frame: frames as u64, infinite_loop: false, rate: 1.0 },
            rt,
            reader,
        ))
    }

    #[test]
    fn the_headphone_bus_of_another_card_goes_to_the_ring_not_to_the_room_and_ignores_the_master_volume() {
        let mut r = rig("tap", true);
        let ring = FrameRing::new(MONITOR_BUS_CHANNELS, MONITOR_RING_FRAMES);
        let mut parts = r.parts.clone();
        parts.monitor_tap = Some(Arc::clone(&ring));
        r.health.monitor_first.store(2 + 1, Ordering::Relaxed); // la sala son 2 canales: el par de auriculares es el 3-4 interno
        r.pool.insert_voice(routed_voice(1, 0.5, 96_000, 4, 2));
        assert!(r.cmd_tx.try_push(AudioCommand::Play { voice_id: 1, start_host_time_ns: 0 }).is_ok());
        r.health.master_db_x10.store(-600, Ordering::Relaxed); // la sala en silencio total
        let mut renderer = RtRenderer::new(parts, 2, 48_000);
        let mut room = vec![0.0f32; 480 * 2];
        for _ in 0..20 {
            renderer.render(&mut room);
        }
        assert!(room.iter().all(|&s| s == 0.0), "la sala no oye la audición (y además está a −inf)");
        assert_eq!(ring.head(), 20 * 480, "cada bloque pasa entero al anillo");
        // La otra placa corre a 44,1 kHz: el remuestreador con control de deriva reconstruye el valor constante.
        let mut drift = DriftConfig::new(44_100.0);
        drift.target_fill_frames = 1_024.0;
        let mut rs = DriftResampler::new(Arc::clone(&ring), drift, 48_000.0, 44_100.0);
        let mut out = vec![0.0f32; 441 * 2];
        let produced = rs.render(&mut out);
        assert!(produced > 400, "{produced}");
        assert!(out[..produced * 2].iter().all(|&s| (s - 0.5).abs() < 1e-3), "el maestro en −inf no baja los auriculares");
        assert_eq!(ring.dropped_frames(), 0);
        assert_eq!(r.health.rt_alloc_violations.load(Ordering::Relaxed), 0, "el callback no asigna memoria con el bus de auriculares");
    }

    #[test]
    fn a_monitor_device_counts_as_separate_only_when_it_differs_from_the_main_one_and_fits() {
        let mut c = BackendConfig { channels: 2, ..Default::default() };
        assert_eq!((c.separate_monitor_device(), c.mix_channels()), (None, 2));
        c.monitor_device = Some("Realtek High Definition Audio".into());
        assert_eq!((c.separate_monitor_device(), c.mix_channels()), (Some("Realtek High Definition Audio"), 4));
        c.device = Some("realtek high definition audio".into());
        assert_eq!((c.separate_monitor_device(), c.mix_channels()), (None, 2), "el mismo dispositivo: auriculares por canales de la misma placa");
        c.device = Some("Focusrite USB".into());
        c.monitor_device = Some("   ".into());
        assert_eq!(c.separate_monitor_device(), None);
        c.monitor_device = Some("Auriculares USB".into());
        c.channels = 63;
        assert_eq!(c.separate_monitor_device(), None, "no caben 2 canales más en el máximo del motor");
    }

    #[test]
    fn renderer_plays_reports_meters_and_beats() {
        let mut r = rig("render", true);
        let v = const_voice(1, 0.5, 4_800);
        r.pool.insert_voice(Arc::clone(&v));
        assert!(r.cmd_tx.try_push(AudioCommand::Play { voice_id: 1, start_host_time_ns: 0 }).is_ok());
        let mut renderer = RtRenderer::new(r.parts.clone(), 2, 48_000);
        let mut buf = vec![0.0f32; 128 * 2];
        renderer.render(&mut buf);
        assert!(buf.iter().all(|&s| (s - 0.5).abs() < 1e-6));
        assert_eq!(r.shm.telemetry().meters.consume_output(0), 0.5);
        assert_eq!(r.shm.telemetry().meters.consume_output(1), 0.5);
        assert_eq!(r.shm.telemetry().meters.consume_output(2), 0.0);
        assert_eq!(r.health.audio_callbacks.load(Ordering::Relaxed), 1);
        assert!(r.health.audio_heartbeat_ns.load(Ordering::Relaxed) > 0);
        assert_eq!(r.health.rt_alloc_violations.load(Ordering::Relaxed), 0, "el callback no debe asignar");
        renderer.render(&mut buf);
        assert_eq!(r.health.audio_callbacks.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn safe_mute_ramps_to_silence_and_back() {
        let mut r = rig("mute", true);
        r.pool.insert_voice(const_voice(1, 0.5, 48_000));
        assert!(r.cmd_tx.try_push(AudioCommand::Play { voice_id: 1, start_host_time_ns: 0 }).is_ok());
        let mut renderer = RtRenderer::new(r.parts.clone(), 2, 48_000);
        let mut buf = vec![0.0f32; 128 * 2];
        renderer.render(&mut buf);
        r.health.safe_mute.store(true, Ordering::Release);
        let mut all = Vec::new();
        for _ in 0..6 {
            renderer.render(&mut buf);
            all.extend(buf.chunks(2).map(|f| f[0]));
        }
        // Rampa lineal de 10 ms (480 frames): monótona, sin saltos, y silencio total después.
        assert!(all.windows(2).all(|w| w[1] <= w[0] + 1e-6));
        let max_step = all.windows(2).map(|w| (w[0] - w[1]).abs()).fold(0.0f32, f32::max);
        assert!(max_step < 0.5 / 480.0 * 1.5, "paso {max_step}");
        assert!(all[480..].iter().all(|&s| s == 0.0));
        assert_eq!(r.shm.telemetry().meters.consume_output(0) > 0.0, true);
        r.shm.telemetry().meters.consume_output(0);
        renderer.render(&mut buf);
        assert_eq!(r.shm.telemetry().meters.consume_output(0), 0.0, "ya mudo");

        r.health.safe_mute.store(false, Ordering::Release);
        for _ in 0..5 {
            renderer.render(&mut buf);
        }
        assert!((buf[0] - 0.5).abs() < 1e-6, "vuelve al nivel original");
    }

    #[test]
    fn contended_context_yields_silence_not_a_block() {
        let r = rig("contend", false);
        let mut renderer = RtRenderer::new(r.parts.clone(), 2, 48_000);
        let mut buf = vec![1.0f32; 64];
        let cell = Arc::clone(&r.parts.cell);
        cell.with(|_| renderer.render(&mut buf)); // el contexto está ocupado
        assert!(buf.iter().all(|&s| s == 0.0));
    }

    #[test]
    fn gaps_between_callbacks_count_as_xruns() {
        let r = rig("xrun", false);
        let mut renderer = RtRenderer::new(r.parts.clone(), 2, 48_000);
        let mut buf = vec![0.0f32; 64 * 2]; // periodo de 1,33 ms
        renderer.render(&mut buf);
        std::thread::sleep(Duration::from_millis(20));
        renderer.render(&mut buf);
        assert_eq!(r.health.xruns.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn null_backend_runs_at_the_clock_rate_and_restarts() {
        let r = rig("null", true);
        let cfg = BackendConfig { kind: BackendKind::Null, buffer_frames: 480, rt_alloc_check: true, ..Default::default() };
        let ctx = RtAudioContext::new(r.pool.rt_handle(), {
            // El contexto de `rig` ya fue consumido por sus `parts`; este backend crea el suyo.
            let (_tx, rx) = HeapRb::<AudioCommand>::new(8).split();
            rx
        }, {
            let (tx, _rx) = HeapRb::<AudioStatus>::new(8).split();
            tx
        });
        let mut backend = AudioBackend::start(cfg, ctx, Arc::clone(&r.health), Arc::clone(&r.shm), Epoch::now()).unwrap();
        assert_eq!(backend.info().backend, "null");
        assert!((backend.info().latency_ms() - 10.0).abs() < 1e-9);
        assert!(r.health.audio_running.load(Ordering::Acquire) && backend.is_running());
        std::thread::sleep(Duration::from_millis(305));
        let n = r.health.audio_callbacks.load(Ordering::Relaxed);
        assert!((25..=36).contains(&n), "~30 callbacks de 10 ms en 300 ms, hubo {n}");
        let hb = r.health.audio_heartbeat_ns.load(Ordering::Acquire);
        backend.restart().unwrap();
        std::thread::sleep(Duration::from_millis(60));
        assert!(r.health.audio_heartbeat_ns.load(Ordering::Acquire) > hb, "el reinicio vuelve a latir");
        backend.stop();
        assert!(!r.health.audio_running.load(Ordering::Acquire) && !backend.is_running());
        let after = r.health.audio_callbacks.load(Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(40));
        assert_eq!(r.health.audio_callbacks.load(Ordering::Relaxed), after, "detenido de verdad");
        let _ = r.pool.len();
    }

    #[test]
    fn bad_host_and_device_names_are_reported() {
        let err = select_host(Some("no-existe")).err().expect("host inexistente");
        assert!(err.to_string().contains("no-existe"), "{err}");
        assert!(matches!(err, BackendError::HostUnavailable(_)));
        assert!(!list_output_devices().iter().any(|(_, d)| d.is_empty() && false));
    }

    /// Prueba con hardware real: `cargo test -p qlab-daemon -- --ignored real_device`.
    #[test]
    #[ignore = "abre el dispositivo de audio predeterminado"]
    fn real_device_opens_and_ticks() {
        let mut r = rig("real", true);
        let cfg = BackendConfig::default();
        let (cmd_tx, cmd_rx) = HeapRb::<AudioCommand>::new(8).split();
        let (st_tx, _st_rx) = HeapRb::<AudioStatus>::new(8).split();
        let _ = cmd_tx;
        let ctx = RtAudioContext::new(r.pool.rt_handle(), cmd_rx, st_tx);
        match AudioBackend::start(cfg, ctx, Arc::clone(&r.health), Arc::clone(&r.shm), Epoch::now()) {
            Ok(backend) => {
                println!("backend: {:?}", backend.info());
                std::thread::sleep(Duration::from_millis(500));
                let n = r.health.audio_callbacks.load(Ordering::Relaxed);
                println!("callbacks en 500 ms: {n}, prioridad RT: {}", r.health.audio_priority_ok.load(Ordering::Relaxed));
                assert!(n > 10);
            }
            Err(e) => println!("sin dispositivo utilizable: {e}"),
        }
        let _ = &mut r;
    }
    #[test]
    fn the_master_volume_is_unity_at_zero_db_and_ramps_smoothly_to_its_target() {
        assert_eq!(master_linear(0), 1.0);
        assert!((master_linear(-60) - 0.5011872).abs() < 1e-5, "-6,0 dB ≈ 0,501");
        assert!((master_linear(60) - 1.9952623).abs() < 1e-5, "+6,0 dB ≈ 1,995");
        assert_eq!((master_linear(-600), master_linear(-9_999)), (0.0, 0.0), "−60 dB o menos = silencio");
        let health = Health::default();
        let mut gain = 1.0f32;
        let mut block = vec![1.0f32; 2 * 4_800]; // 100 ms estéreo a 48 kHz
        apply_master(&health, &mut gain, 2, 48_000, &mut block);
        assert!(block.iter().all(|&s| s == 1.0) && gain == 1.0, "a 0 dB no se toca la señal");
        health.master_db_x10.store(-200, Ordering::Relaxed); // −20 dB
        apply_master(&health, &mut gain, 2, 48_000, &mut block);
        assert!(block[0] < 1.0 && block[0] > 0.5, "la primera muestra baja solo un poco (rampa): {}", block[0]);
        assert!(block.windows(4).all(|w| w[2] <= w[0] + 1e-7), "la rampa es monótona");
        let mut again = vec![1.0f32; 2 * 4_800];
        apply_master(&health, &mut gain, 2, 48_000, &mut again);
        assert!((again[again.len() - 1] - 0.1).abs() < 0.005, "tras 200 ms llega a −20 dB (0,1): {}", again[again.len() - 1]);
        health.master_db_x10.store(-600, Ordering::Relaxed);
        for _ in 0..10 {
            let mut b = vec![1.0f32; 2 * 4_800];
            apply_master(&health, &mut gain, 2, 48_000, &mut b);
        }
        assert_eq!(gain, 0.0, "silencio total");
    }

}
