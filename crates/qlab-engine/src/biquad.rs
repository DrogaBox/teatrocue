//! Ecualizador paramétrico de 4 bandas por voz: filtros biquad IIR con las fórmulas del «Audio EQ Cookbook» de
//! Robert Bristow-Johnson (RBJ).
//!
//! Reglas de tiempo real: todo el estado vive en arrays de tamaño fijo dentro de la voz (cero asignaciones, cero
//! bloqueos) y el procesado se salta por completo (coste nulo) cuando el EQ o una banda están desactivados.
//!
//! * Forma directa I (`x1, x2, y1, y2`) con estado y coeficientes en `f64`: a 20 Hz los polos están muy cerca de
//!   la circunferencia unidad y en `f32` el ruido de cuantización se oye.
//! * Antizumbido: los parámetros (frecuencia, ganancia y Q) se suavizan hacia su objetivo (~10 ms) y los
//!   coeficientes se recalculan **una vez por trozo** de audio con los parámetros suavizados. Así nunca se
//!   interpolan coeficientes (que podría inestabilizar el filtro) y mover un control durante la reproducción no
//!   hace chasquidos.

use crate::rt_command::MAX_INPUT_CHANNELS;

pub const EQ_BANDS: usize = 4;
pub const EQ_MIN_HZ: f32 = 20.0;
pub const EQ_MAX_HZ: f32 = 20_000.0;
pub const EQ_MIN_GAIN_DB: f32 = -24.0;
pub const EQ_MAX_GAIN_DB: f32 = 12.0;
pub const EQ_MIN_Q: f32 = 0.1;
pub const EQ_MAX_Q: f32 = 10.0;
/// Constante de tiempo del suavizado de parámetros.
const SMOOTH_SECS: f32 = 0.010;
/// Por debajo de esta ganancia (dB) una banda de realce/shelf es la identidad y se salta.
const IDENTITY_GAIN_DB: f32 = 0.01;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FilterKind {
    /// Paso alto (recorta graves).
    LowCut,
    LowShelf,
    #[default]
    Peaking,
    HighShelf,
    /// Paso bajo (recorta agudos).
    HighCut,
}

impl FilterKind {
    /// ¿Usa el parámetro de ganancia? (los cortes no).
    pub const fn uses_gain(self) -> bool {
        matches!(self, Self::LowShelf | Self::Peaking | Self::HighShelf)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BandParams {
    pub kind: FilterKind,
    pub freq_hz: f32,
    pub gain_db: f32,
    pub q: f32,
    pub enabled: bool,
}

impl BandParams {
    pub const FLAT: Self = Self { kind: FilterKind::Peaking, freq_hz: 1_000.0, gain_db: 0.0, q: 1.0, enabled: false };

    /// Parámetros dentro de rango (NaN y desbordes se corrigen).
    pub fn sanitized(mut self) -> Self {
        let fix = |v: f32, lo: f32, hi: f32, d: f32| if v.is_finite() { v.clamp(lo, hi) } else { d };
        self.freq_hz = fix(self.freq_hz, EQ_MIN_HZ, EQ_MAX_HZ, 1_000.0);
        self.gain_db = fix(self.gain_db, EQ_MIN_GAIN_DB, EQ_MAX_GAIN_DB, 0.0);
        self.q = fix(self.q, EQ_MIN_Q, EQ_MAX_Q, 1.0);
        self
    }

    /// ¿Esta banda deja la señal intacta? (desactivada, o realce/shelf con 0 dB).
    pub fn is_identity(&self) -> bool {
        !self.enabled || (self.kind.uses_gain() && self.gain_db.abs() < IDENTITY_GAIN_DB)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EqParams {
    pub enabled: bool,
    pub bands: [BandParams; EQ_BANDS],
}

impl EqParams {
    pub const OFF: Self = Self { enabled: false, bands: [BandParams::FLAT; EQ_BANDS] };
}

/// Coeficientes de un biquad normalizados (`a0 = 1`): `y = b0·x + b1·x1 + b2·x2 − a1·y1 − a2·y2`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Coeffs {
    pub b0: f64,
    pub b1: f64,
    pub b2: f64,
    pub a1: f64,
    pub a2: f64,
}

impl Coeffs {
    pub const IDENTITY: Self = Self { b0: 1.0, b1: 0.0, b2: 0.0, a1: 0.0, a2: 0.0 };

    /// Estabilidad: los dos polos dentro del círculo unidad (triángulo de estabilidad de un biquad).
    pub fn is_stable(&self) -> bool {
        self.a2.abs() < 1.0 && self.a1.abs() < 1.0 + self.a2
    }
}

/// Coeficientes RBJ de un filtro. `freq_hz`, `gain_db` y `q` se recortan a rango; la frecuencia además no pasa de
/// 0,45 × la de muestreo (cerca de Nyquist las fórmulas se degeneran).
pub fn coefficients(kind: FilterKind, sample_rate: f32, freq_hz: f32, gain_db: f32, q: f32) -> Coeffs {
    let sr = sample_rate.max(8_000.0) as f64;
    let f = (freq_hz.clamp(EQ_MIN_HZ, EQ_MAX_HZ) as f64).min(0.45 * sr);
    let q = q.clamp(EQ_MIN_Q, EQ_MAX_Q) as f64;
    let gain = gain_db.clamp(EQ_MIN_GAIN_DB, EQ_MAX_GAIN_DB) as f64;
    let w0 = 2.0 * std::f64::consts::PI * f / sr;
    let (sin, cos) = w0.sin_cos();
    let alpha = sin / (2.0 * q);
    let a = 10f64.powf(gain / 40.0);
    let (b0, b1, b2, a0, a1, a2);
    match kind {
        FilterKind::Peaking => {
            b0 = 1.0 + alpha * a;
            b1 = -2.0 * cos;
            b2 = 1.0 - alpha * a;
            a0 = 1.0 + alpha / a;
            a1 = -2.0 * cos;
            a2 = 1.0 - alpha / a;
        }
        FilterKind::LowShelf => {
            let t = 2.0 * a.sqrt() * alpha;
            b0 = a * ((a + 1.0) - (a - 1.0) * cos + t);
            b1 = 2.0 * a * ((a - 1.0) - (a + 1.0) * cos);
            b2 = a * ((a + 1.0) - (a - 1.0) * cos - t);
            a0 = (a + 1.0) + (a - 1.0) * cos + t;
            a1 = -2.0 * ((a - 1.0) + (a + 1.0) * cos);
            a2 = (a + 1.0) + (a - 1.0) * cos - t;
        }
        FilterKind::HighShelf => {
            let t = 2.0 * a.sqrt() * alpha;
            b0 = a * ((a + 1.0) + (a - 1.0) * cos + t);
            b1 = -2.0 * a * ((a - 1.0) + (a + 1.0) * cos);
            b2 = a * ((a + 1.0) + (a - 1.0) * cos - t);
            a0 = (a + 1.0) - (a - 1.0) * cos + t;
            a1 = 2.0 * ((a - 1.0) - (a + 1.0) * cos);
            a2 = (a + 1.0) - (a - 1.0) * cos - t;
        }
        FilterKind::LowCut => {
            b0 = (1.0 + cos) / 2.0;
            b1 = -(1.0 + cos);
            b2 = (1.0 + cos) / 2.0;
            a0 = 1.0 + alpha;
            a1 = -2.0 * cos;
            a2 = 1.0 - alpha;
        }
        FilterKind::HighCut => {
            b0 = (1.0 - cos) / 2.0;
            b1 = 1.0 - cos;
            b2 = (1.0 - cos) / 2.0;
            a0 = 1.0 + alpha;
            a1 = -2.0 * cos;
            a2 = 1.0 - alpha;
        }
    }
    Coeffs { b0: b0 / a0, b1: b1 / a0, b2: b2 / a0, a1: a1 / a0, a2: a2 / a0 }
}

/// Respuesta en frecuencia |H| en dB de un biquad a `freq_hz`.
pub fn response_db(c: &Coeffs, sample_rate: f32, freq_hz: f32) -> f32 {
    let w = 2.0 * std::f64::consts::PI * (freq_hz as f64) / (sample_rate.max(8_000.0) as f64);
    let (s1, c1) = w.sin_cos();
    let (s2, c2) = (2.0 * w).sin_cos();
    let num_re = c.b0 + c.b1 * c1 + c.b2 * c2;
    let num_im = -(c.b1 * s1 + c.b2 * s2);
    let den_re = 1.0 + c.a1 * c1 + c.a2 * c2;
    let den_im = -(c.a1 * s1 + c.a2 * s2);
    let mag2 = (num_re * num_re + num_im * num_im) / (den_re * den_re + den_im * den_im).max(1e-30);
    (10.0 * mag2.max(1e-30).log10()) as f32
}

/// Respuesta combinada (suma en dB) de las bandas activas de `eq` a `freq_hz`: la curva que dibuja la interfaz.
pub fn combined_response_db(eq: &EqParams, sample_rate: f32, freq_hz: f32) -> f32 {
    if !eq.enabled {
        return 0.0;
    }
    eq.bands.iter().map(|b| b.sanitized()).filter(|b| !b.is_identity()).map(|b| response_db(&coefficients(b.kind, sample_rate, b.freq_hz, b.gain_db, b.q), sample_rate, freq_hz)).sum()
}

#[derive(Clone, Copy, Default)]
struct BiquadState {
    x1: f64,
    x2: f64,
    y1: f64,
    y2: f64,
}

impl BiquadState {
    #[inline]
    fn run(&mut self, c: &Coeffs, buf: &mut [f32]) {
        let (mut x1, mut x2, mut y1, mut y2) = (self.x1, self.x2, self.y1, self.y2);
        for s in buf.iter_mut() {
            let x = *s as f64;
            let y = c.b0 * x + c.b1 * x1 + c.b2 * x2 - c.a1 * y1 - c.a2 * y2;
            x2 = x1;
            x1 = x;
            y2 = y1;
            y1 = y;
            *s = y as f32;
        }
        // Un estado no finito (entrada con NaN) no debe quedarse pegado para siempre.
        if !(y1.is_finite() && y2.is_finite()) {
            *self = Self::default();
        } else {
            *self = Self { x1, x2, y1, y2 };
        }
    }
}

/// Parámetros suavizados de una banda (lo que realmente suena en este momento).
#[derive(Clone, Copy)]
struct BandCur {
    /// ¿Se está procesando esta banda?
    running: bool,
    kind: FilterKind,
    ln_freq: f32,
    gain_db: f32,
    q: f32,
}

impl BandCur {
    const OFF: Self = Self { running: false, kind: FilterKind::Peaking, ln_freq: 6.907_755, gain_db: 0.0, q: 1.0 };
}

/// Estado de EQ de una voz: objetivo, parámetros suavizados y memoria de los filtros por canal de entrada.
pub struct EqRt {
    target: EqParams,
    cur: [BandCur; EQ_BANDS],
    states: [[BiquadState; EQ_BANDS]; MAX_INPUT_CHANNELS],
}

impl EqRt {
    pub fn new() -> Self {
        Self { target: EqParams::OFF, cur: [BandCur::OFF; EQ_BANDS], states: [[BiquadState::default(); EQ_BANDS]; MAX_INPUT_CHANNELS] }
    }

    /// Nuevo objetivo (comando `SetEq`). Los parámetros se corrigen a rango.
    pub fn set_target(&mut self, mut p: EqParams) {
        for b in p.bands.iter_mut() {
            *b = b.sanitized();
        }
        self.target = p;
    }

    /// Borra la memoria de los filtros (al reiniciar la reproducción desde cero).
    pub fn reset_state(&mut self) {
        self.states = [[BiquadState::default(); EQ_BANDS]; MAX_INPUT_CHANNELS];
    }

    /// ¿No hay nada que calcular? (EQ apagado y todas las bandas ya a cero): coste nulo.
    pub fn is_idle(&self) -> bool {
        !self.target.enabled && self.cur.iter().all(|c| !c.running)
    }

    /// Procesa `frames` muestras de los `channels` primeros canales en su sitio.
    pub fn process<const N: usize>(&mut self, src: &mut [[f32; N]], channels: usize, frames: usize, sample_rate: f32) {
        if self.is_idle() {
            return;
        }
        let frames = frames.min(N);
        let alpha = 1.0 - (-(frames as f32) / (sample_rate.max(8_000.0) * SMOOTH_SECS)).exp();
        let channels = channels.min(MAX_INPUT_CHANNELS).min(src.len());
        let mut any = false;
        let mut coeffs = [Coeffs::IDENTITY; EQ_BANDS];
        for b in 0..EQ_BANDS {
            let t = self.target.bands[b];
            let want = self.target.enabled && t.enabled && !(t.kind.uses_gain() && t.gain_db.abs() < IDENTITY_GAIN_DB);
            let cur = &mut self.cur[b];
            if want {
                if !cur.running || cur.kind != t.kind {
                    // Banda nueva (o de otro tipo): los realces nacen desde 0 dB y suben con la rampa; los cortes entran ya.
                    *cur = BandCur { running: true, kind: t.kind, ln_freq: t.freq_hz.ln(), gain_db: if t.kind.uses_gain() { 0.0 } else { t.gain_db }, q: t.q };
                    self.states.iter_mut().for_each(|s| s[b] = BiquadState::default());
                }
                cur.ln_freq += (t.freq_hz.ln() - cur.ln_freq) * alpha;
                cur.gain_db += (t.gain_db - cur.gain_db) * alpha;
                cur.q += (t.q - cur.q) * alpha;
            } else if cur.running {
                if cur.kind.uses_gain() && cur.gain_db.abs() >= IDENTITY_GAIN_DB {
                    // Se apaga con la rampa: la ganancia vuelve a 0 dB antes de saltar la banda.
                    cur.gain_db += (0.0 - cur.gain_db) * alpha;
                    if cur.gain_db.abs() < IDENTITY_GAIN_DB {
                        cur.running = false;
                    }
                } else {
                    cur.running = false;
                }
            }
            if cur.running {
                coeffs[b] = coefficients(cur.kind, sample_rate, cur.ln_freq.exp(), cur.gain_db, cur.q);
                any = true;
            }
        }
        if !any {
            return;
        }
        for (ch, buf) in src.iter_mut().take(channels).enumerate() {
            let buf = &mut buf[..frames];
            for b in 0..EQ_BANDS {
                if self.cur[b].running {
                    self.states[ch][b].run(&coeffs[b], buf);
                }
            }
        }
    }
}

impl Default for EqRt {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;

    fn db_at(kind: FilterKind, f0: f32, gain: f32, q: f32, f: f32) -> f32 {
        response_db(&coefficients(kind, SR, f0, gain, q), SR, f)
    }

    #[test]
    fn peaking_reaches_its_gain_at_the_center_and_is_flat_far_away() {
        for gain in [-24.0f32, -6.0, 3.0, 12.0] {
            assert!((db_at(FilterKind::Peaking, 1_000.0, gain, 1.0, 1_000.0) - gain).abs() < 0.05, "{gain} dB en el centro");
            assert!(db_at(FilterKind::Peaking, 1_000.0, gain, 4.0, 30.0).abs() < 0.2, "lejos del centro no hay efecto");
            assert!(db_at(FilterKind::Peaking, 1_000.0, gain, 4.0, 15_000.0).abs() < 0.5);
        }
        // Con 0 dB es la identidad exacta.
        let flat = coefficients(FilterKind::Peaking, SR, 800.0, 0.0, 2.0);
        for f in [30.0, 500.0, 5_000.0, 18_000.0] {
            assert!(response_db(&flat, SR, f).abs() < 1e-4);
        }
    }

    #[test]
    fn shelves_apply_their_gain_on_one_side_only() {
        // Low shelf +9 dB en 200 Hz: ~+9 por debajo, ~0 muy por encima, +4,5 (mitad) en la frecuencia de corte.
        assert!((db_at(FilterKind::LowShelf, 200.0, 9.0, 0.707, 25.0) - 9.0).abs() < 0.4);
        assert!(db_at(FilterKind::LowShelf, 200.0, 9.0, 0.707, 10_000.0).abs() < 0.2);
        assert!((db_at(FilterKind::LowShelf, 200.0, 9.0, 0.707, 200.0) - 4.5).abs() < 0.3);
        // High shelf −12 dB en 4 kHz.
        assert!((db_at(FilterKind::HighShelf, 4_000.0, -12.0, 0.707, 19_000.0) + 12.0).abs() < 0.6);
        assert!(db_at(FilterKind::HighShelf, 4_000.0, -12.0, 0.707, 60.0).abs() < 0.2);
    }

    #[test]
    fn cut_filters_are_minus_3_db_at_the_corner_for_butterworth_q() {
        let q = std::f32::consts::FRAC_1_SQRT_2;
        assert!((db_at(FilterKind::LowCut, 100.0, 0.0, q, 100.0) + 3.01).abs() < 0.05);
        assert!((db_at(FilterKind::HighCut, 8_000.0, 0.0, q, 8_000.0) + 3.01).abs() < 0.05);
        assert!(db_at(FilterKind::LowCut, 100.0, 0.0, q, 10.0) < -35.0, "12 dB/oct por debajo");
        assert!(db_at(FilterKind::LowCut, 100.0, 0.0, q, 5_000.0).abs() < 0.1);
        assert!(db_at(FilterKind::HighCut, 8_000.0, 0.0, q, 20_000.0) < -9.0);
        assert!(db_at(FilterKind::HighCut, 8_000.0, 0.0, q, 200.0).abs() < 0.1);
    }

    #[test]
    fn every_setting_in_the_allowed_range_gives_a_stable_filter() {
        let kinds = [FilterKind::LowCut, FilterKind::LowShelf, FilterKind::Peaking, FilterKind::HighShelf, FilterKind::HighCut];
        for sr in [32_000.0f32, 44_100.0, 48_000.0, 96_000.0] {
            for kind in kinds {
                for f in [20.0f32, 31.5, 100.0, 1_000.0, 8_000.0, 16_000.0, 20_000.0] {
                    for g in [-24.0f32, -12.0, 0.0, 6.0, 12.0] {
                        for q in [0.1f32, 0.707, 1.0, 4.0, 10.0] {
                            let c = coefficients(kind, sr, f, g, q);
                            assert!(c.is_stable(), "{kind:?} {f} Hz {g} dB Q{q} @ {sr}: {c:?}");
                            assert!([c.b0, c.b1, c.b2, c.a1, c.a2].iter().all(|v| v.is_finite()));
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn the_impulse_response_of_the_worst_cases_decays_and_stays_finite() {
        for (kind, f, g, q) in [(FilterKind::LowCut, 20.0, 0.0, 10.0), (FilterKind::Peaking, 20.0, 12.0, 10.0), (FilterKind::LowShelf, 20.0, 12.0, 0.1), (FilterKind::HighCut, 20_000.0, 0.0, 10.0), (FilterKind::Peaking, 20_000.0, -24.0, 10.0)] {
            let c = coefficients(kind, SR, f, g, q);
            let mut st = BiquadState::default();
            let mut buf = vec![0.0f32; 48_000 * 4];
            buf[0] = 1.0;
            st.run(&c, &mut buf);
            assert!(buf.iter().all(|v| v.is_finite()), "{kind:?}");
            let tail = buf[buf.len() - 4_800..].iter().fold(0.0f32, |m, v| m.max(v.abs()));
            assert!(tail < 1e-3, "{kind:?} {f} Hz Q{q}: la respuesta no decae ({tail})");
        }
    }

    fn sine(f: f32, n: usize, amp: f32) -> Vec<f32> {
        (0..n).map(|i| amp * (2.0 * std::f32::consts::PI * f * i as f32 / SR).sin()).collect()
    }

    fn peak_of(v: &[f32]) -> f32 {
        v.iter().fold(0.0f32, |m, s| m.max(s.abs()))
    }

    /// Procesa `x` (un canal) en trozos de 128 con el EQ dado; devuelve la salida.
    fn run_eq(eq: EqParams, x: &[f32]) -> Vec<f32> {
        let mut rt = EqRt::new();
        rt.set_target(eq);
        let mut out = Vec::with_capacity(x.len());
        for chunk in x.chunks(128) {
            let mut block = [[0.0f32; 128]; 2];
            block[0][..chunk.len()].copy_from_slice(chunk);
            rt.process(&mut block, 1, chunk.len(), SR);
            out.extend_from_slice(&block[0][..chunk.len()]);
        }
        out
    }

    fn eq_with(band: BandParams) -> EqParams {
        let mut e = EqParams::OFF;
        e.enabled = true;
        e.bands[0] = band;
        e
    }

    #[test]
    fn a_disabled_eq_or_band_leaves_the_signal_bit_for_bit_untouched() {
        let x = sine(440.0, 4_800, 0.7);
        assert_eq!(run_eq(EqParams::OFF, &x), x, "EQ apagado");
        let mut off_band = eq_with(BandParams { kind: FilterKind::Peaking, freq_hz: 440.0, gain_db: 12.0, q: 1.0, enabled: false });
        assert_eq!(run_eq(off_band, &x), x, "banda desactivada");
        off_band.bands[0].enabled = true;
        off_band.bands[0].gain_db = 0.0;
        assert_eq!(run_eq(off_band, &x), x, "realce de 0 dB = identidad");
        let mut rt = EqRt::new();
        assert!(rt.is_idle(), "sin EQ el coste es nulo");
        rt.set_target(eq_with(BandParams { enabled: true, gain_db: 6.0, ..BandParams::FLAT }));
        assert!(!rt.is_idle());
    }

    #[test]
    fn the_streaming_eq_matches_the_analytic_response_once_the_ramp_settles() {
        // Peaking +10 dB en 1 kHz aplicado a un seno de 1 kHz: la amplitud sube ~10 dB tras la rampa.
        let eq = eq_with(BandParams { kind: FilterKind::Peaking, freq_hz: 1_000.0, gain_db: 10.0, q: 1.0, enabled: true });
        let y = run_eq(eq, &sine(1_000.0, 48_000, 0.1));
        let gain = 20.0 * (peak_of(&y[40_000..]) / 0.1).log10();
        assert!((gain - 10.0).abs() < 0.2, "ganancia medida {gain} dB");
        // Low cut en 500 Hz: un seno de 50 Hz casi desaparece y uno de 5 kHz pasa intacto.
        let cut = eq_with(BandParams { kind: FilterKind::LowCut, freq_hz: 500.0, gain_db: 0.0, q: 0.707, enabled: true });
        assert!(peak_of(&run_eq(cut, &sine(50.0, 48_000, 0.5))[24_000..]) < 0.01);
        assert!((peak_of(&run_eq(cut, &sine(5_000.0, 48_000, 0.5))[24_000..]) - 0.5).abs() < 0.01);
    }

    #[test]
    fn moving_a_control_during_playback_ramps_without_jumps() {
        let x = sine(1_000.0, 48_000, 0.2);
        let mut rt = EqRt::new();
        let mut eq = eq_with(BandParams { kind: FilterKind::Peaking, freq_hz: 1_000.0, gain_db: 0.0, q: 1.0, enabled: true });
        rt.set_target(eq);
        let mut out = Vec::new();
        for (i, chunk) in x.chunks(128).enumerate() {
            if i == 100 {
                // De golpe a +12 dB y a otra frecuencia: la rampa lo suaviza.
                eq.bands[0].gain_db = 12.0;
                eq.bands[0].freq_hz = 1_500.0;
                rt.set_target(eq);
            }
            let mut block = [[0.0f32; 128]; 1];
            block[0][..chunk.len()].copy_from_slice(chunk);
            rt.process(&mut block, 1, chunk.len(), SR);
            out.extend_from_slice(&block[0][..chunk.len()]);
        }
        let max_step = out.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0f32, f32::max);
        // El seno de 1 kHz a 0,2 sube como mucho 2π·1000/48000·0,2·(ganancia 4×) ≈ 0,1 por muestra; un chasquido sería mucho mayor.
        assert!(max_step < 0.12, "salto brusco: {max_step}");
        assert!(out.iter().all(|v| v.is_finite()));
    }

    #[test]
    fn turning_a_boost_off_fades_it_out_and_returns_to_zero_cost() {
        let mut rt = EqRt::new();
        let mut eq = eq_with(BandParams { kind: FilterKind::Peaking, freq_hz: 1_000.0, gain_db: 12.0, q: 1.0, enabled: true });
        rt.set_target(eq);
        let mut block = [[0.0f32; 128]; 1];
        for _ in 0..200 {
            rt.process(&mut block, 1, 128, SR);
        }
        eq.enabled = false;
        rt.set_target(eq);
        for _ in 0..400 {
            rt.process(&mut block, 1, 128, SR);
        }
        assert!(rt.is_idle(), "tras la rampa de salida ya no cuesta nada");
    }

    #[test]
    fn nan_input_does_not_poison_the_filter_state_and_bad_parameters_are_clamped() {
        let eq = eq_with(BandParams { kind: FilterKind::HighCut, freq_hz: f32::NAN, gain_db: f32::INFINITY, q: -3.0, enabled: true });
        let mut rt = EqRt::new();
        rt.set_target(eq);
        let mut block = [[0.0f32; 128]; 1];
        block[0][5] = f32::NAN;
        rt.process(&mut block, 1, 128, SR);
        let mut clean = [[0.25f32; 128]; 1];
        for _ in 0..4 {
            rt.process(&mut clean, 1, 128, SR);
        }
        assert!(clean[0].iter().all(|v| v.is_finite()), "el estado se recupera tras un NaN");
    }

    #[test]
    fn combined_response_sums_only_the_active_bands() {
        let mut eq = EqParams::OFF;
        assert_eq!(combined_response_db(&eq, SR, 1_000.0), 0.0, "EQ apagado");
        eq.enabled = true;
        eq.bands[0] = BandParams { kind: FilterKind::Peaking, freq_hz: 1_000.0, gain_db: 6.0, q: 1.0, enabled: true };
        eq.bands[1] = BandParams { kind: FilterKind::Peaking, freq_hz: 1_000.0, gain_db: -2.0, q: 1.0, enabled: true };
        eq.bands[2] = BandParams { kind: FilterKind::Peaking, freq_hz: 1_000.0, gain_db: 9.0, q: 1.0, enabled: false };
        assert!((combined_response_db(&eq, SR, 1_000.0) - 4.0).abs() < 0.1);
    }
}

#[cfg(test)]
mod cost {
    use super::*;

    /// Medición de referencia (no es un criterio de aprobación estricto: solo vigila que el EQ no se dispare en coste).
    #[test]
    fn worst_case_eq_cost_per_voice_chunk_is_small_next_to_the_callback_budget() {
        let mut eq = EqParams::OFF;
        eq.enabled = true;
        let kinds = [FilterKind::LowCut, FilterKind::LowShelf, FilterKind::Peaking, FilterKind::HighShelf];
        for (i, k) in kinds.into_iter().enumerate() {
            eq.bands[i] = BandParams { kind: k, freq_hz: 200.0 * (i as f32 + 1.0), gain_db: 6.0, q: 1.0, enabled: true };
        }
        let mut rt = EqRt::new();
        rt.set_target(eq);
        let mut block = [[0.1f32; 128]; 2];
        for _ in 0..2_000 {
            rt.process(&mut block, 2, 128, 48_000.0);
        }
        let n = 20_000;
        let t = std::time::Instant::now();
        for _ in 0..n {
            rt.process(&mut block, 2, 128, 48_000.0);
        }
        let per_chunk_us = t.elapsed().as_secs_f64() * 1e6 / n as f64;
        eprintln!("EQ 4 bandas estéreo: {per_chunk_us:.2} µs por trozo de 128 frames (presupuesto del callback: 2667 µs)");
        // Con margen enorme: aun en una máquina lenta y en depuración debe quedar muy por debajo de un callback entero.
        assert!(per_chunk_us < 800.0, "{per_chunk_us} µs");
    }
}
