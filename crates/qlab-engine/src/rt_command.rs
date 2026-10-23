//! Comandos y eventos `Copy` de tamaño fijo entre el secuenciador y el callback de audio
//! (spec §4.2.1). Nada de aquí asigna memoria ni tiene destructores.

pub type VoiceId = u64;
pub type FadeId = u64;
pub type PatchId = u16;

pub const MAX_INPUT_CHANNELS: usize = 32;
pub const MAX_OUTPUT_CHANNELS: usize = 64;
pub const MAX_SLICES_PER_VOICE: usize = 64;
pub const MAX_FADES_PER_VOICE: usize = 8;
pub const MIN_VOLUME_DB: f32 = -120.0;
pub const MAX_VOLUME_DB: f32 = 12.0;
/// Puntos de una [`CurveTable`] (32 tramos).
pub const CURVE_POINTS: usize = 33;

/// `0.0` exacto para `db <= -120` o NaN; recorta a `+12 dB`.
#[inline(always)]
pub fn db_to_linear(db: f32) -> f32 {
    if db <= MIN_VOLUME_DB || db.is_nan() {
        0.0
    } else {
        10.0_f32.powf(db.min(MAX_VOLUME_DB) * 0.05)
    }
}

/// Curva pre-evaluada de 33 puntos en stack (132 bytes, `Copy`) para Bezier/S-curve/paramétrica.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CurveTable {
    pub samples: [f32; CURVE_POINTS],
}

impl CurveTable {
    pub const fn constant(value: f32) -> Self {
        Self { samples: [value; CURVE_POINTS] }
    }

    /// Tabla de `f(t)` con `t = i / 32`. Se construye en el hilo secuenciador, no en el RT.
    pub fn from_fn(mut f: impl FnMut(f32) -> f32) -> Self {
        let mut samples = [0.0; CURVE_POINTS];
        for (i, s) in samples.iter_mut().enumerate() {
            *s = f(i as f32 / (CURVE_POINTS - 1) as f32);
        }
        Self { samples }
    }

    /// Rampa lineal `0 → 1`.
    pub const LINEAR: Self = {
        let mut samples = [0.0; CURVE_POINTS];
        let mut i = 0;
        while i < CURVE_POINTS {
            samples[i] = i as f32 / (CURVE_POINTS - 1) as f32;
            i += 1;
        }
        Self { samples }
    };

    pub fn linear() -> Self {
        Self::LINEAR
    }

    /// Smoothstep `3t² - 2t³`.
    pub fn s_curve() -> Self {
        Self::from_fn(|t| t * t * (3.0 - 2.0 * t))
    }

    /// Interpolación cúbica Catmull-Rom de 4 puntos sobre la tabla, sin asignaciones.
    /// `t` se limita a `[0, 1]` (NaN → 0). Pasa por todos los puntos de la tabla y el resultado
    /// queda acotado por los dos puntos vecinos, para que un fade no rebase su objetivo.
    #[inline(always)]
    pub fn eval(&self, t: f32) -> f32 {
        let t = if t.is_nan() { 0.0 } else { t.clamp(0.0, 1.0) };
        let scaled = t * (CURVE_POINTS - 1) as f32;
        let idx = (scaled as usize).min(CURVE_POINTS - 2);
        let x = scaled - idx as f32;
        let y1 = self.samples[idx];
        let y2 = self.samples[idx + 1];
        // En los extremos se extrapola linealmente el punto que falta.
        let y0 = if idx == 0 { 2.0 * y1 - y2 } else { self.samples[idx - 1] };
        let y3 = if idx + 2 >= CURVE_POINTS { 2.0 * y2 - y1 } else { self.samples[idx + 2] };
        let a = -y0 + 3.0 * y1 - 3.0 * y2 + y3;
        let b = 2.0 * y0 - 5.0 * y1 + 4.0 * y2 - y3;
        let c = y2 - y0;
        let v = y1 + 0.5 * x * (c + x * (b + x * a));
        v.clamp(y1.min(y2), y1.max(y2))
    }

    /// Interpolación lineal de la especificación original.
    #[inline(always)]
    pub fn eval_linear(&self, t: f32) -> f32 {
        let t = if t.is_nan() { 0.0 } else { t.clamp(0.0, 1.0) };
        let scaled = t * (CURVE_POINTS - 1) as f32;
        let idx = (scaled as usize).min(CURVE_POINTS - 2);
        let frac = scaled - idx as f32;
        self.samples[idx] + (self.samples[idx + 1] - self.samples[idx]) * frac
    }
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FadeMode {
    Absolute = 0,
    Relative = 1,
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DevampMode {
    None = 0,
    ContinueToNextSlice = 1,
    StopAtSliceEnd = 2,
}

impl DevampMode {
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::ContinueToNextSlice,
            2 => Self::StopAtSliceEnd,
            _ => Self::None,
        }
    }
}

/// Secuenciador → audio por anillo SPSC. `Copy`, sin punteros al heap.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AudioCommand {
    Play { voice_id: VoiceId, start_host_time_ns: u64 },
    /// Como `Play`, pero la voz entra con un fundido lineal de `fade_frames` (micro-fade que evita el clic de un salto
    /// a mitad de una pista, p. ej. al arrancar desde una memoria).
    PlayFadeIn { voice_id: VoiceId, start_host_time_ns: u64, fade_frames: u32 },
    /// `fade_frames` mínimo 240 (5 ms @ 48 kHz).
    Pause { voice_id: VoiceId, fade_frames: u32 },
    Resume { voice_id: VoiceId, fade_frames: u32 },
    /// 240 frames para Stop normal, 96 para HardStop, o los de panic.
    Stop { voice_id: VoiceId, fade_frames: u32, curve: CurveTable },
    Seek { voice_id: VoiceId, target_frame: u64 },
    /// `row`: 0 = master/salida, 1..=M entrada; `col`: 0 = master/entrada, 1..=N salida.
    SetCrosspointDb { voice_id: VoiceId, row: u8, col: u8, db: f32 },
    SetMuteSoloMask { voice_id: VoiceId, mute_mask: u64, solo_mask: u64 },
    SetRate { voice_id: VoiceId, rate: f32 },
    RequestDevamp { voice_id: VoiceId, mode: DevampMode, trigger_next_cue: bool },
    /// Inicia o actualiza una celda de un `FadeCue`.
    ///
    /// `duration_frames` es `u32` (hasta ~24 h a 48 kHz): con `u64` el comando pasaría de 160 bytes.
    BeginCellFade {
        voice_id: VoiceId,
        fade_id: FadeId,
        mode: FadeMode,
        row: u8,
        col: u8,
        target_db: f32,
        duration_frames: u32,
        curve: CurveTable,
        stop_target_when_done: bool,
    },
    CancelFade { voice_id: VoiceId, fade_id: FadeId },
    SetDuckEnvelope { voice_id: VoiceId, target_duck_linear: f32, slew_frames: u32 },
    /// Ecualizador paramétrico de la voz (objetivo; el callback suaviza la transición).
    SetEq { voice_id: VoiceId, eq: crate::biquad::EqParams },
    PanicAll { fade_frames: u32 },
}

/// Audio → secuenciador por anillo SPSC.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AudioStatus {
    VoiceStarted { voice_id: VoiceId, host_time_ns: u64 },
    VoiceCompleted { voice_id: VoiceId, natural_eof: bool },
    SliceDevamped { voice_id: VoiceId, slice_index: u16, trigger_next_cue: bool },
    FadeCompleted { voice_id: VoiceId, fade_id: FadeId, stopped_target: bool },
    DiskBufferUnderrun { voice_id: VoiceId, at_frame: u64 },
    SeamDetected { output_channel: u8, jump_delta: f32 },
}

/// Capacidad del anillo de comandos y del de estado (spec §2.2).
pub const COMMAND_RING_CAPACITY: usize = 1024;
pub const STATUS_RING_CAPACITY: usize = 2048;

const _: () = {
    assert!(core::mem::size_of::<CurveTable>() == 132);
    assert!(core::mem::size_of::<AudioCommand>() <= 160);
    assert!(core::mem::size_of::<AudioStatus>() <= 32);
    assert!(!core::mem::needs_drop::<AudioCommand>());
    assert!(!core::mem::needs_drop::<AudioStatus>());
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        println!(
            "AudioCommand = {} B, AudioStatus = {} B",
            core::mem::size_of::<AudioCommand>(),
            core::mem::size_of::<AudioStatus>()
        );
        assert!(core::mem::size_of::<AudioCommand>() <= 160);
    }

    #[test]
    fn db_matches_protocol() {
        for db in [-200.0f32, -120.0, -119.0, -60.0, -6.0, 0.0, 6.0, 12.0, 30.0, f32::NAN, f32::NEG_INFINITY] {
            // Fórmulas equivalentes (`/20` vs `*0.05`): pueden diferir en 1 ULP.
            let (a, b) = (db_to_linear(db), qlab_protocol::matrix::db_to_linear(db));
            assert!(a == b || (a - b).abs() <= 1e-5 * b.abs(), "db = {db}: {a} vs {b}");
        }
        assert_eq!(MIN_VOLUME_DB, qlab_protocol::matrix::MIN_VOLUME_DB);
        assert_eq!(MAX_VOLUME_DB, qlab_protocol::matrix::MAX_VOLUME_DB);
        assert_eq!(MAX_INPUT_CHANNELS, qlab_protocol::matrix::MAX_INPUT_CHANNELS);
        assert_eq!(MAX_OUTPUT_CHANNELS, qlab_protocol::matrix::MAX_OUTPUT_CHANNELS);
    }

    #[test]
    fn curve_hits_every_table_point() {
        let c = CurveTable::s_curve();
        for i in 0..CURVE_POINTS {
            let t = i as f32 / 32.0;
            assert!((c.eval(t) - c.samples[i]).abs() < 1e-5, "i = {i}");
        }
        assert_eq!(c.eval(0.0), 0.0);
        assert_eq!(c.eval(1.0), 1.0);
        assert_eq!(c.eval(-5.0), 0.0);
        assert_eq!(c.eval(5.0), 1.0);
        assert_eq!(c.eval(f32::NAN), 0.0);
    }

    #[test]
    fn cubic_is_close_to_smooth_function_and_bounded() {
        let c = CurveTable::s_curve();
        let mut prev = 0.0;
        for k in 0..=1000 {
            let t = k as f32 / 1000.0;
            let exact = t * t * (3.0 - 2.0 * t);
            let v = c.eval(t);
            assert!((v - exact).abs() < 5e-4, "t = {t}: {v} vs {exact}");
            assert!((0.0..=1.0).contains(&v));
            assert!(v >= prev - 1e-6, "no monótona en t = {t}");
            prev = v;
        }
        // En una curva lineal, ambas interpolaciones coinciden.
        let l = CurveTable::linear();
        for k in 0..=100 {
            let t = k as f32 / 100.0;
            assert!((l.eval(t) - t).abs() < 1e-5);
            assert!((l.eval_linear(t) - t).abs() < 1e-5);
        }
    }

    #[test]
    fn eval_does_not_allocate() {
        use crate::alloc_guard::{assert_no_alloc, set_violation_policy, violations_on_this_thread, ViolationPolicy};
        set_violation_policy(ViolationPolicy::CountOnly);
        let c = CurveTable::s_curve();
        let before = violations_on_this_thread();
        let mut acc = 0.0;
        assert_no_alloc(|| {
            for k in 0..1000 {
                acc += c.eval(k as f32 / 1000.0);
            }
        });
        assert!(acc > 0.0);
        assert_eq!(violations_on_this_thread(), before);
    }
}
