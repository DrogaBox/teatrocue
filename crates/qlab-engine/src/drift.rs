//! Control de deriva de reloj entre dominios de audio (spec §4.1: "PI DriftController + Catmull-Rom
//! Resampler para multi-dispositivo").
//!
//! Dos interfaces con relojes de cristal distintos (o un dispositivo agregado) producen y consumen
//! audio a ritmos que difieren decenas de ppm. Si el consumidor lee a ritmo nominal, el búfer
//! intermedio se vacía o se desborda. El lazo cierra así:
//!
//! ```text
//!  dominio A (callback A) --push--> [FrameRing SPSC] --render--> dominio B (callback B)
//!                                        │ nivel de llenado                 ▲
//!                                        └──► DriftControllerPI ──ratio──► DriftResampler (Catmull-Rom)
//! ```
//!
//! Todo es wait-free: el anillo usa solo atómicos con Release/Acquire, el controlador es aritmética
//! sobre su propio estado y el resampler no asigna memoria. Ninguna parte toma bloqueos.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::disk_streamer::AlignedBuf;
use crate::mixer_simd::catmull_rom_4pt;
use crate::rt_command::MAX_OUTPUT_CHANNELS;

// ---------------------------------------------------------------------------------------------
// Controlador PI
// ---------------------------------------------------------------------------------------------

/// Parámetros del lazo. Las ganancias se derivan del ancho de banda y el amortiguamiento, no se
/// escriben a mano: ver [`pi_gains`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DriftConfig {
    /// Frecuencia de muestreo del dominio que consume (y ejecuta el controlador).
    pub sample_rate: f32,
    /// Frames que se quieren mantener en el búfer (margen contra la variación de tamaño de bloque).
    pub target_fill_frames: f32,
    /// Ancho de banda del lazo en régimen (Hz). Bajo: la deriva real es lenta y el nivel de llenado
    /// tiene mucho ruido por el tamaño de los bloques.
    pub track_bandwidth_hz: f32,
    /// Ancho de banda inicial, mientras engancha (Hz). Decae hacia `track_bandwidth_hz`.
    pub acquire_bandwidth_hz: f32,
    /// Constante de tiempo (s) con que el ancho de banda decae de `acquire` a `track`. La
    /// transición es continua, así que el ratio no da saltos.
    pub acquire_secs: f32,
    /// Amortiguamiento ζ (0,7 = respuesta rápida con poco rebasamiento; 1 = crítico).
    pub damping: f32,
    /// Constante de tiempo del filtro de un polo sobre el error medido.
    pub filter_tau_secs: f32,
    /// Corrección máxima del ratio (fracción; `1e-3` = ±1000 ppm).
    pub max_ratio_offset: f32,
}

impl DriftConfig {
    /// Valores por defecto para `sample_rate`: engancha a 0,4 Hz y baja a 0,1 Hz con una constante
    /// de 2 s, con un margen de 1024 frames y un tope de ±1000 ppm (los cristales de audio rondan
    /// ±100 ppm).
    pub fn new(sample_rate: f32) -> Self {
        Self {
            sample_rate,
            target_fill_frames: 1024.0,
            track_bandwidth_hz: 0.1,
            acquire_bandwidth_hz: 0.4,
            acquire_secs: 2.0,
            damping: 0.9,
            filter_tau_secs: 0.25,
            max_ratio_offset: 1.0e-3,
        }
    }
}

impl Default for DriftConfig {
    fn default() -> Self {
        Self::new(48_000.0)
    }
}

/// Ganancias `(kp, ki)` de un PI sobre la planta integradora `d(fill)/dt = deriva − fs·δ`, donde `δ`
/// es la corrección del ratio y `fill` el error en frames.
///
/// Con `δ = kp·e + ki·∫e`, el error obedece `ë + fs·kp·ė + fs·ki·e = 0`, de modo que
/// `ωn² = fs·ki` y `2ζωn = fs·kp`. Unidades: `kp` en 1/frame, `ki` en 1/(frame·s).
pub fn pi_gains(sample_rate: f32, bandwidth_hz: f32, damping: f32) -> (f32, f32) {
    let wn = core::f32::consts::TAU * bandwidth_hz;
    (2.0 * damping * wn / sample_rate, wn * wn / sample_rate)
}

pub const PPM: f32 = 1.0e6;

/// Controlador proporcional-integral del ratio de remuestreo. Se llama una vez por callback.
#[derive(Clone, Debug)]
pub struct DriftControllerPI {
    cfg: DriftConfig,
    err_filtered: f32,
    have_error: bool,
    /// Término integral ya multiplicado por `ki` (adimensional): cambiar de ganancias no da saltos.
    i_term: f32,
    elapsed_secs: f32,
    ratio: f64,
    saturated: bool,
}

/// Nombre usado en la especificación.
pub type DriftController = DriftControllerPI;

impl DriftControllerPI {
    pub fn new(cfg: DriftConfig) -> Self {
        Self { cfg, err_filtered: 0.0, have_error: false, i_term: 0.0, elapsed_secs: 0.0, ratio: 1.0, saturated: false }
    }

    pub fn config(&self) -> &DriftConfig {
        &self.cfg
    }

    /// Vuelve al estado inicial (ratio 1, sin deriva estimada).
    pub fn reset(&mut self) {
        *self = Self::new(self.cfg);
    }

    /// Ratio actual: frames de entrada consumidos por frame de salida, respecto al nominal.
    #[inline]
    pub fn ratio(&self) -> f64 {
        self.ratio
    }

    /// Deriva de reloj estimada por el integrador, en ppm (positiva = el productor es más rápido).
    #[inline]
    pub fn clock_drift_ppm(&self) -> f32 {
        self.i_term * PPM
    }

    /// Corrección total aplicada ahora mismo, en ppm.
    #[inline]
    pub fn offset_ppm(&self) -> f32 {
        (self.ratio - 1.0) as f32 * PPM
    }

    /// `true` si la corrección está en el tope de `max_ratio_offset` (la deriva real lo supera).
    #[inline]
    pub fn is_saturated(&self) -> bool {
        self.saturated
    }

    /// Error filtrado respecto al objetivo, en frames.
    #[inline]
    pub fn filtered_error_frames(&self) -> f32 {
        self.err_filtered
    }

    /// Actualiza el lazo con el nivel de llenado medido (`fill_frames`) al inicio de un bloque de
    /// `block_frames` frames, y devuelve el ratio para ese bloque. Una medida no finita se ignora.
    pub fn update(&mut self, fill_frames: f32, block_frames: u32) -> f64 {
        if !fill_frames.is_finite() || block_frames == 0 {
            return self.ratio;
        }
        let cfg = &self.cfg;
        let dt = block_frames as f32 / cfg.sample_rate;
        let error = fill_frames - cfg.target_fill_frames;

        // Filtro de un polo: quita el diente de sierra que producen los bloques de ambos relojes.
        if self.have_error {
            let alpha = 1.0 - (-dt / cfg.filter_tau_secs).exp();
            self.err_filtered += alpha * (error - self.err_filtered);
        } else {
            self.err_filtered = error;
            self.have_error = true;
        }

        let decay = (-self.elapsed_secs / cfg.acquire_secs.max(1e-3)).exp();
        let bandwidth = cfg.track_bandwidth_hz + (cfg.acquire_bandwidth_hz - cfg.track_bandwidth_hz) * decay;
        let (kp, ki) = pi_gains(cfg.sample_rate, bandwidth, cfg.damping);

        let max = cfg.max_ratio_offset;
        // Anti-windup: el integrador nunca pide más de lo que se puede aplicar.
        self.i_term = (self.i_term + ki * self.err_filtered * dt).clamp(-max, max);
        let raw = kp * self.err_filtered + self.i_term;
        self.saturated = raw.abs() >= max;
        self.elapsed_secs += dt;
        self.ratio = 1.0 + raw.clamp(-max, max) as f64;
        self.ratio
    }
}

// ---------------------------------------------------------------------------------------------
// Anillo de frames entre dominios (SPSC, wait-free)
// ---------------------------------------------------------------------------------------------

/// Anillo SPSC de frames entrelazados entre dos callbacks. El productor escribe desde el dominio A
/// y el consumidor ([`DriftResampler`]) lee desde el B. Los datos son atómicos `Relaxed`; el orden lo
/// dan `head` (Release/Acquire) y `tail`.
pub struct FrameRing {
    channels: usize,
    capacity: usize,
    mask: usize,
    data: AlignedBuf,
    /// Frames escritos en total (solo lo avanza el productor).
    head: AtomicU64,
    /// Frame más antiguo que el consumidor aún necesita (solo lo avanza el consumidor).
    tail: AtomicU64,
    dropped_frames: AtomicU64,
}

impl FrameRing {
    /// `capacity_frames` se redondea a la potencia de 2 siguiente (mínimo 16).
    pub fn new(channels: usize, capacity_frames: usize) -> Arc<Self> {
        let channels = channels.clamp(1, MAX_OUTPUT_CHANNELS);
        let capacity = capacity_frames.max(16).next_power_of_two();
        Arc::new(Self {
            channels,
            capacity,
            mask: capacity - 1,
            data: AlignedBuf::new(capacity * channels),
            head: AtomicU64::new(0),
            tail: AtomicU64::new(0),
            dropped_frames: AtomicU64::new(0),
        })
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    pub fn capacity_frames(&self) -> usize {
        self.capacity
    }

    /// Frames descartados por falta de espacio (desbordamiento).
    pub fn dropped_frames(&self) -> u64 {
        self.dropped_frames.load(Ordering::Relaxed)
    }

    /// Productor: escribe frames entrelazados y devuelve cuántos cupieron. Lo que no cabe se cuenta
    /// en [`FrameRing::dropped_frames`]; nunca bloquea.
    pub fn push(&self, interleaved: &[f32]) -> usize {
        let ch = self.channels;
        let frames = interleaved.len() / ch;
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        let free = self.capacity - (head - tail) as usize;
        let n = frames.min(free);
        for (f, frame) in interleaved.chunks_exact(ch).take(n).enumerate() {
            let base = ((head as usize + f) & self.mask) * ch;
            for (c, &s) in frame.iter().enumerate() {
                self.data.store(base + c, s);
            }
        }
        self.head.store(head + n as u64, Ordering::Release);
        if n < frames {
            self.dropped_frames.fetch_add((frames - n) as u64, Ordering::Relaxed);
        }
        n
    }

    /// Productor: frames que caben ahora sin descartar.
    pub fn free_frames(&self) -> usize {
        let head = self.head.load(Ordering::Relaxed);
        self.capacity - (head - self.tail.load(Ordering::Acquire)) as usize
    }

    /// Frames escritos hasta ahora.
    #[inline]
    pub fn head(&self) -> u64 {
        self.head.load(Ordering::Acquire)
    }

    /// Frames entre `read_pos` y la cabeza.
    #[inline]
    pub fn fill_from(&self, read_pos: u64) -> u64 {
        self.head().saturating_sub(read_pos)
    }

    /// Consumidor: muestra del frame absoluto `frame`, que debe estar entre `tail` y `head`.
    #[inline(always)]
    fn sample(&self, frame: u64, ch: usize) -> f32 {
        self.data.load((frame as usize & self.mask) * self.channels + ch)
    }

    /// Consumidor: libera el espacio anterior a `read_pos`, conservando un frame de historia para
    /// el vecino `y[-1]` del interpolador.
    #[inline]
    fn release_before(&self, read_pos: u64) {
        self.tail.store(read_pos.saturating_sub(1), Ordering::Release);
    }
}

// ---------------------------------------------------------------------------------------------
// Resampler dinámico
// ---------------------------------------------------------------------------------------------

/// Lee de un [`FrameRing`] y produce audio a la frecuencia del dominio local, con un ratio que
/// corrige el [`DriftControllerPI`] y el interpolador cúbico Catmull-Rom de 4 puntos.
pub struct DriftResampler {
    ring: Arc<FrameRing>,
    ctrl: DriftControllerPI,
    /// `f_entrada / f_salida` nominal.
    base_step: f64,
    read_pos: u64,
    frac: f64,
    started: bool,
    underruns: u64,
}

impl DriftResampler {
    /// `input_rate` es la frecuencia nominal del productor y `output_rate` la del consumidor.
    pub fn new(ring: Arc<FrameRing>, mut cfg: DriftConfig, input_rate: f32, output_rate: f32) -> Self {
        cfg.sample_rate = output_rate;
        Self {
            ring,
            ctrl: DriftControllerPI::new(cfg),
            base_step: input_rate as f64 / output_rate as f64,
            read_pos: 0,
            frac: 0.0,
            started: false,
            underruns: 0,
        }
    }

    pub fn controller(&self) -> &DriftControllerPI {
        &self.ctrl
    }

    /// Cuántas veces se vació el búfer después de arrancar (cada una reinicia el cebado).
    pub fn underruns(&self) -> u64 {
        self.underruns
    }

    pub fn is_running(&self) -> bool {
        self.started
    }

    /// Posición de lectura fraccionaria en frames de entrada.
    pub fn read_position(&self) -> f64 {
        self.read_pos as f64 + self.frac
    }

    /// Rellena `out` (entrelazado, mismo número de canales que el anillo) y devuelve los frames de
    /// audio real escritos; el resto queda en silencio. Hasta reunir `target_fill_frames` produce
    /// silencio (cebado). Sin asignaciones ni bloqueos.
    pub fn render(&mut self, out: &mut [f32]) -> usize {
        out.fill(0.0);
        let ch = self.ring.channels;
        let frames = out.len() / ch;
        let head = self.ring.head();
        let target = self.ctrl.config().target_fill_frames.max(4.0) as u64;

        if !self.started {
            if head < target + 3 {
                return 0;
            }
            // Arrancar a `target` frames de la cabeza: lo anterior se descarta.
            self.read_pos = head - target;
            self.frac = 0.0;
            self.started = true;
        }

        let ratio = self.ctrl.update((head - self.read_pos) as f32, frames as u32);
        let step = self.base_step * ratio;
        let mut produced = 0;
        for frame_out in out.chunks_exact_mut(ch) {
            // Se necesitan los frames read_pos-1 ..= read_pos+2.
            if head < self.read_pos + 3 {
                self.started = false;
                self.underruns += 1;
                break;
            }
            let frac = self.frac as f32;
            for (c, o) in frame_out.iter_mut().enumerate() {
                let y_m1 = if self.read_pos == 0 { 0.0 } else { self.ring.sample(self.read_pos - 1, c) };
                *o = catmull_rom_4pt(
                    y_m1,
                    self.ring.sample(self.read_pos, c),
                    self.ring.sample(self.read_pos + 1, c),
                    self.ring.sample(self.read_pos + 2, c),
                    frac,
                );
            }
            self.frac += step;
            let advance = self.frac as u64;
            self.frac -= advance as f64;
            self.read_pos += advance;
            produced += 1;
        }
        self.ring.release_before(self.read_pos);
        produced
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alloc_guard::{assert_no_alloc, set_violation_policy, violations_on_this_thread, ViolationPolicy};

    const FS: f32 = 48_000.0;
    const BLOCK: usize = 256;

    /// Generador congruencial determinista en [-1, 1).
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> f64 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((self.0 >> 33) as f64 / (1u64 << 31) as f64) * 2.0 - 1.0
        }
    }

    struct Sim {
        resampler: DriftResampler,
        ring: Arc<FrameRing>,
        ratios: Vec<f64>,
        fills: Vec<f32>,
        worst_ripple: f32,
        unity_ratio_blocks: usize,
    }

    /// Productor a `48000·(1+drift)` Hz y consumidor a 48000 Hz, en bloques de 256 con jitter de
    /// llegada de ±0,5 ms. Tono de 1 kHz de verificación.
    fn simulate(drift_ppm: f64, seconds: f64, cfg: DriftConfig) -> Sim {
        let d = drift_ppm * 1e-6;
        let ring = FrameRing::new(2, 16_384);
        let mut resampler = DriftResampler::new(Arc::clone(&ring), cfg, FS, FS);
        let w = std::f64::consts::TAU * 1000.0 / FS as f64;
        let mut rng = Lcg(42);
        let mut pushed = 0u64;
        let mut out = vec![0.0f32; BLOCK * 2];
        let blocks = (seconds * FS as f64 / BLOCK as f64) as usize;
        let mut sim = Sim { resampler: DriftResampler::new(Arc::clone(&ring), cfg, FS, FS), ring: Arc::clone(&ring), ratios: Vec::new(), fills: Vec::new(), worst_ripple: 0.0, unity_ratio_blocks: 0 };
        let mut tmp = Vec::new();
        for b in 0..blocks {
            // El productor ha generado floor((t + jitter) * fs * (1 + d)) frames hasta este instante.
            let jitter_frames = rng.next() * 24.0;
            let produced = (((b * BLOCK) as f64 + jitter_frames).max(0.0) * (1.0 + d)) as u64;
            if produced > pushed {
                tmp.clear();
                for n in pushed..produced {
                    let s = (w * n as f64).sin() as f32;
                    tmp.extend_from_slice(&[s, -s]);
                }
                ring.push(&tmp);
                pushed = produced;
            }
            let was_running = resampler.is_running();
            // El controlador regula el nivel al inicio del bloque, antes de consumirlo.
            let fill_before = ring.fill_from(resampler.read_pos) as f32;
            let got = resampler.render(&mut out);
            if was_running && got == BLOCK {
                // Un seno puro cumple x[n+1] + x[n-1] = 2·cos(ω)·x[n]: detecta clics y saltos.
                let c = 2.0 * (w as f32).cos();
                for i in 1..BLOCK - 1 {
                    let r = (out[(i + 1) * 2] + out[(i - 1) * 2] - c * out[i * 2]).abs();
                    sim.worst_ripple = sim.worst_ripple.max(r);
                }
                // Canal 2 es el opuesto del 1.
                assert!((out[0] + out[1]).abs() < 1e-6);
            }
            if resampler.is_running() {
                sim.ratios.push(resampler.controller().ratio());
                sim.fills.push(fill_before);
                if (resampler.controller().ratio() - 1.0).abs() < 1e-9 {
                    sim.unity_ratio_blocks += 1;
                }
            }
        }
        sim.resampler = resampler;
        sim
    }

    fn mean(v: &[f64]) -> f64 {
        v.iter().sum::<f64>() / v.len() as f64
    }

    fn check_locks(drift_ppm: f64) {
        let sim = simulate(drift_ppm, 90.0, DriftConfig::new(FS));
        let n = sim.ratios.len();
        assert!(n > 15_000, "debe estar en marcha casi todo el tiempo ({n})");
        assert_eq!(sim.resampler.underruns(), 0, "vaciado del búfer con deriva {drift_ppm} ppm");
        assert_eq!(sim.ring.dropped_frames(), 0, "desbordamiento con deriva {drift_ppm} ppm");

        let last = n - (20.0 * FS as f64 / BLOCK as f64) as usize;
        let tail_ratio = mean(&sim.ratios[last..]);
        let err_ppm = (tail_ratio - 1.0 - drift_ppm * 1e-6) * 1e6;
        assert!(err_ppm.abs() < 10.0, "ratio final {:.1} ppm frente a {drift_ppm} ppm", (tail_ratio - 1.0) * 1e6);

        let fills = &sim.fills[n - (30.0 * FS as f64 / BLOCK as f64) as usize..];
        let target = DriftConfig::new(FS).target_fill_frames;
        let mean_dev = fills.iter().map(|f| (f - target).abs()).sum::<f32>() / fills.len() as f32;
        assert!(mean_dev < 20.0, "el nivel se aleja {mean_dev} frames del objetivo");

        // La deriva estimada por el integrador coincide con la real.
        let est = sim.resampler.controller().clock_drift_ppm() as f64;
        assert!((est - drift_ppm).abs() < 15.0, "deriva estimada {est} ppm");
        assert!(sim.worst_ripple < 3e-3, "salto/clic en la salida: {}", sim.worst_ripple);
    }

    #[test]
    fn locks_to_a_faster_producer() {
        check_locks(100.0);
    }

    #[test]
    fn locks_to_a_slower_producer() {
        check_locks(-250.0);
    }

    #[test]
    fn locks_to_a_large_drift() {
        check_locks(600.0);
    }

    #[test]
    fn matched_clocks_stay_near_unity() {
        let sim = simulate(0.0, 60.0, DriftConfig::new(FS));
        assert_eq!(sim.resampler.underruns(), 0);
        let tail = &sim.ratios[sim.ratios.len() / 2..];
        assert!((mean(tail) - 1.0).abs() < 1e-5);
    }

    #[test]
    fn drift_beyond_the_limit_saturates_instead_of_diverging() {
        let sim = simulate(3000.0, 20.0, DriftConfig::new(FS));
        let ctrl = sim.resampler.controller();
        assert!(ctrl.is_saturated() || sim.resampler.underruns() > 0 || sim.ring.dropped_frames() > 0);
        let cap = DriftConfig::new(FS).max_ratio_offset as f64;
        assert!(sim.ratios.iter().all(|&r| (r - 1.0).abs() <= cap + 1e-12), "el ratio rebasó el tope");
    }

    #[test]
    fn gains_follow_the_second_order_design() {
        let (kp, ki) = pi_gains(FS, 0.1, 0.9);
        let wn = std::f32::consts::TAU * 0.1;
        assert!((ki * FS - wn * wn).abs() < 1e-6);
        assert!((kp * FS - 2.0 * 0.9 * wn).abs() < 1e-6);
        // Más ancho de banda, más ganancia.
        let (kp2, ki2) = pi_gains(FS, 0.4, 0.9);
        assert!(kp2 > kp && ki2 > ki);
    }

    #[test]
    fn linear_step_response_is_well_damped() {
        // Planta lineal ideal: d(fill)/dt = deriva − fs·δ, con el controlador real.
        let cfg = DriftConfig { filter_tau_secs: 0.01, ..DriftConfig::new(FS) };
        let mut ctrl = DriftControllerPI::new(cfg);
        let drift = 100e-6 * FS as f64; // frames/s
        let dt = BLOCK as f64 / FS as f64;
        let mut fill = cfg.target_fill_frames as f64;
        let mut worst = 0.0f64;
        for _ in 0..(40.0 / dt) as usize {
            let ratio = ctrl.update(fill as f32, BLOCK as u32);
            fill += (drift - (ratio - 1.0) * FS as f64) * dt;
            worst = worst.max((fill - cfg.target_fill_frames as f64).abs());
        }
        assert!((fill - cfg.target_fill_frames as f64).abs() < 1.0, "no converge: {fill}");
        assert!(worst < 20.0, "excursión de {worst} frames ante 100 ppm");
        assert!((ctrl.clock_drift_ppm() - 100.0).abs() < 1.0);
    }

    #[test]
    fn gain_schedule_is_continuous() {
        let cfg = DriftConfig::new(FS);
        let mut ctrl = DriftControllerPI::new(cfg);
        let mut prev = ctrl.update(cfg.target_fill_frames + 10.0, BLOCK as u32);
        let mut worst_jump = 0.0f64;
        for _ in 0..4000 {
            // Error constante de 10 frames sobre el objetivo, durante ~21 s (varias constantes de tiempo).
            let r = ctrl.update(cfg.target_fill_frames + 10.0, BLOCK as u32);
            worst_jump = worst_jump.max((r - prev).abs());
            prev = r;
        }
        // Un cambio brusco de ganancias daría ~7e-4; la deriva natural del lazo es de unos 5e-6 por bloque.
        assert!(worst_jump < 1e-5, "salto de {worst_jump} entre bloques consecutivos");
    }

    #[test]
    fn invalid_measurements_are_ignored() {
        let mut ctrl = DriftControllerPI::new(DriftConfig::default());
        let r = ctrl.update(2000.0, 256);
        assert_eq!(ctrl.update(f32::NAN, 256), r);
        assert_eq!(ctrl.update(f32::INFINITY, 256), r);
        assert_eq!(ctrl.update(10.0, 0), r);
        ctrl.reset();
        assert_eq!(ctrl.ratio(), 1.0);
        assert_eq!(ctrl.clock_drift_ppm(), 0.0);
    }

    #[test]
    fn resampler_primes_then_reprimes_after_underrun() {
        let ring = FrameRing::new(1, 4096);
        let cfg = DriftConfig { target_fill_frames: 64.0, ..DriftConfig::new(FS) };
        let mut r = DriftResampler::new(Arc::clone(&ring), cfg, FS, FS);
        let mut out = vec![1.0f32; 32];
        assert_eq!(r.render(&mut out), 0, "sin datos: silencio");
        assert!(out.iter().all(|&s| s == 0.0));
        ring.push(&(0..200).map(|i| i as f32).collect::<Vec<_>>());
        assert_eq!(r.render(&mut out), 32);
        assert!(r.is_running());
        // Rampa lineal: la salida es una rampa con paso 1 (ratio ~1).
        assert!(out.windows(2).all(|w| (w[1] - w[0] - 1.0).abs() < 0.05));
        // Se agota: reprime.
        let mut big = vec![0.0; 4096];
        r.render(&mut big);
        assert_eq!(r.underruns(), 1);
        assert!(!r.is_running());
    }

    #[test]
    fn different_nominal_rates_resample() {
        // Productor a 44100 → consumidor a 48000: step nominal 0,91875.
        let ring = FrameRing::new(1, 8192);
        let cfg = DriftConfig { target_fill_frames: 2000.0, ..DriftConfig::new(48_000.0) };
        let mut r = DriftResampler::new(Arc::clone(&ring), cfg, 44_100.0, 48_000.0);
        let w = std::f64::consts::TAU * 1000.0 / 44_100.0;
        ring.push(&(0..4000).map(|n| (w * n as f64).sin() as f32).collect::<Vec<_>>());
        let mut out = vec![0.0f32; 1000];
        assert_eq!(r.render(&mut out), 1000);
        let w_out = std::f32::consts::TAU * 1000.0 / 48_000.0;
        let c = 2.0 * w_out.cos();
        for i in 1..999 {
            assert!((out[i + 1] + out[i - 1] - c * out[i]).abs() < 4e-3, "i = {i}");
        }
    }

    #[test]
    fn render_and_push_do_not_allocate() {
        set_violation_policy(ViolationPolicy::CountOnly);
        let ring = FrameRing::new(2, 8192);
        let cfg = DriftConfig { target_fill_frames: 512.0, ..DriftConfig::default() };
        let mut r = DriftResampler::new(Arc::clone(&ring), cfg, FS, FS);
        let chunk = vec![0.25f32; 2 * 256];
        let mut out = vec![0.0f32; 2 * 256];
        let before = violations_on_this_thread();
        assert_no_alloc(|| {
            for _ in 0..200 {
                ring.push(&chunk);
                r.render(&mut out);
            }
        });
        assert_eq!(violations_on_this_thread(), before);
        assert!(r.is_running());
    }

    #[test]
    fn ring_overflow_is_counted_not_blocking() {
        let ring = FrameRing::new(1, 16);
        assert_eq!(ring.capacity_frames(), 16);
        assert_eq!(ring.push(&[0.0; 40]), 16);
        assert_eq!(ring.dropped_frames(), 24);
        assert_eq!(ring.push(&[0.0; 4]), 0);
    }

    #[test]
    fn spsc_threads_deliver_every_frame_in_order() {
        use std::sync::atomic::AtomicBool;
        const N: u64 = 400_000;
        let ring = FrameRing::new(2, 1024);
        let done = AtomicBool::new(false);
        std::thread::scope(|s| {
            s.spawn(|| {
                let mut n = 0u64;
                while n < N {
                    let upto = (n + 100).min(N);
                    let block: Vec<f32> = (n..upto).flat_map(|i| [i as f32, -(i as f32)]).collect();
                    while ring.free_frames() < block.len() / 2 {
                        std::hint::spin_loop();
                    }
                    assert_eq!(ring.push(&block), block.len() / 2);
                    n = upto;
                }
                done.store(true, Ordering::Release);
            });
            let mut pos = 0u64;
            while pos < N {
                let head = ring.head();
                while pos < head {
                    assert_eq!(ring.sample(pos, 0), pos as f32);
                    assert_eq!(ring.sample(pos, 1), -(pos as f32));
                    pos += 1;
                    // `tail` conserva un frame de historia, como el resampler.
                    ring.release_before(pos);
                }
                if done.load(Ordering::Acquire) && pos >= ring.head() {
                    break;
                }
            }
            assert_eq!(pos, N);
        });
        assert_eq!(ring.dropped_frames(), 0);
    }
}
