//! Voz en tiempo real con propiedad dividida (spec §4.2.2).
//!
//! * Campos atómicos o inmutables: los lee cualquier hilo (secuenciador, UI).
//! * [`VoiceRtState`] en un `UnsafeCell`: solo lo toca el hilo del callback de audio.

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};

use crate::disk_streamer::StreamingVoiceReader;
use crate::rt_command::{
    db_to_linear, CurveTable, DevampMode, FadeId, FadeMode, PatchId, VoiceId, MAX_FADES_PER_VOICE,
    MAX_INPUT_CHANNELS, MAX_OUTPUT_CHANNELS, MAX_SLICES_PER_VOICE, MIN_VOLUME_DB,
};

/// Constante de tiempo del suavizado de ganancias (anti-zipper).
pub const SLEW_TIME_SEC: f32 = 0.005;
/// Por debajo de esta diferencia la ganancia salta directamente al objetivo.
pub const SLEW_EPSILON: f32 = 1e-6;

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VoiceState {
    Idle = 0,
    Playing = 1,
    Paused = 2,
    TailingOut = 3,
    Stopped = 4,
}

impl VoiceState {
    #[inline]
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Playing,
            2 => Self::Paused,
            3 => Self::TailingOut,
            4 => Self::Stopped,
            _ => Self::Idle,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SliceSegment {
    pub start_frame: u64,
    pub end_frame: u64,
    /// `u32::MAX` = bucle infinito.
    pub play_count: u32,
    pub remaining_loops: u32,
}

impl SliceSegment {
    pub const INFINITE: u32 = u32::MAX;
    pub const EMPTY: Self = Self { start_frame: 0, end_frame: 0, play_count: 1, remaining_loops: 1 };

    pub const fn new(start_frame: u64, end_frame: u64, play_count: u32) -> Self {
        Self { start_frame, end_frame, play_count, remaining_loops: play_count }
    }

    #[inline]
    pub const fn is_infinite(&self) -> bool {
        self.play_count == Self::INFINITE
    }
}

#[derive(Clone, Copy, Debug)]
pub struct CellFadeEntry {
    pub active: bool,
    pub row: u8,
    pub col: u8,
    pub start_db: f32,
    pub target_db: f32,
}

impl CellFadeEntry {
    pub const EMPTY: Self = Self { active: false, row: 0, col: 0, start_db: 0.0, target_db: 0.0 };
}

#[derive(Clone, Copy, Debug)]
pub struct MatrixFadeSlot {
    pub in_use: bool,
    pub fade_id: FadeId,
    pub mode: FadeMode,
    pub duration_frames: u64,
    pub elapsed_frames: u64,
    pub stop_target_when_done: bool,
    pub curve: CurveTable,
    pub cells_count: usize,
    pub cells: [CellFadeEntry; 64],
}

impl MatrixFadeSlot {
    pub const EMPTY: Self = Self {
        in_use: false,
        fade_id: 0,
        mode: FadeMode::Absolute,
        duration_frames: 0,
        elapsed_frames: 0,
        stop_target_when_done: false,
        curve: CurveTable::constant(0.0),
        cells_count: 0,
        cells: [CellFadeEntry::EMPTY; 64],
    };
}

/// Ganancia lineal de una celda. Una base de `-inf` sigue siendo silencio aunque un fade relativo
/// le sume dB.
#[inline(always)]
fn cell_linear(base_db: f32, rel_db: f32) -> f32 {
    if base_db <= MIN_VOLUME_DB {
        0.0
    } else {
        db_to_linear(base_db + rel_db)
    }
}

/// Coeficiente por frame del suavizado de 5 ms: `Δg = (target - current) * coeff`.
#[inline]
pub fn slew_coeff(sample_rate: f32) -> f32 {
    (1.0 / (SLEW_TIME_SEC * sample_rate)).min(1.0)
}

/// Matriz de ruteo en vivo `(1 + M) x (1 + N)`. Exclusiva del hilo de audio.
#[repr(C, align(64))]
pub struct AudioLevelMatrixRt {
    pub in_channels: usize,
    pub out_channels: usize,
    pub mute_mask: u64,
    pub solo_mask: u64,
    /// Nivel base/absoluto en dB por `[row][col]`.
    pub base_db: [[f32; MAX_OUTPUT_CHANNELS + 1]; MAX_INPUT_CHANNELS + 1],
    /// Offset acumulado en dB de fades relativos por `[row][col]`.
    pub rel_offset_db: [[f32; MAX_OUTPUT_CHANNELS + 1]; MAX_INPUT_CHANNELS + 1],
    /// Ganancia lineal objetivo del crosspoint efectivo `[in][out]`.
    pub target_crosspoint_lin: [[f32; MAX_OUTPUT_CHANNELS]; MAX_INPUT_CHANNELS],
    /// Ganancia lineal actual, suavizada a 5 ms `[in][out]`.
    pub current_crosspoint_lin: [[f32; MAX_OUTPUT_CHANNELS]; MAX_INPUT_CHANNELS],
    pub matrix_dirty: bool,
}

impl AudioLevelMatrixRt {
    /// Matriz uno-a-uno: masters y trims a 0 dB, diagonal a 0 dB, resto a `-inf`. Los objetivos y
    /// las ganancias actuales quedan ya calculados (sin rampa inicial).
    pub fn new(in_channels: usize, out_channels: usize) -> Self {
        let mut m = Self {
            in_channels: in_channels.clamp(1, MAX_INPUT_CHANNELS),
            out_channels: out_channels.clamp(1, MAX_OUTPUT_CHANNELS),
            mute_mask: 0,
            solo_mask: 0,
            base_db: [[MIN_VOLUME_DB; MAX_OUTPUT_CHANNELS + 1]; MAX_INPUT_CHANNELS + 1],
            rel_offset_db: [[0.0; MAX_OUTPUT_CHANNELS + 1]; MAX_INPUT_CHANNELS + 1],
            target_crosspoint_lin: [[0.0; MAX_OUTPUT_CHANNELS]; MAX_INPUT_CHANNELS],
            current_crosspoint_lin: [[0.0; MAX_OUTPUT_CHANNELS]; MAX_INPUT_CHANNELS],
            matrix_dirty: true,
        };
        for col in 0..=MAX_OUTPUT_CHANNELS {
            m.base_db[0][col] = 0.0;
        }
        for row in 0..=MAX_INPUT_CHANNELS {
            m.base_db[row][0] = 0.0;
        }
        for i in 1..=m.in_channels.min(m.out_channels) {
            m.base_db[i][i] = 0.0;
        }
        m.recompute_targets(1.0, 1.0);
        m.snap_to_targets();
        m
    }

    /// Recalcula `target_crosspoint_lin` cuando cambia un fader o avanza un `FadeCue`.
    ///
    /// Las ganancias de salida y de entrada se evalúan una vez por canal (no por celda) y las
    /// celdas silenciadas se saltan, así que `powf` solo se llama para crosspoints audibles. Sin
    /// asignaciones.
    #[inline(always)]
    pub fn recompute_targets(&mut self, duck_linear: f32, transport_env: f32) {
        let in_ch = self.in_channels.min(MAX_INPUT_CHANNELS);
        let out_ch = self.out_channels.min(MAX_OUTPUT_CHANNELS);

        let master_lin = cell_linear(self.base_db[0][0], self.rel_offset_db[0][0]);
        let global_scale = master_lin * duck_linear * transport_env;

        // Escala por salida, con mute/solo ya aplicado.
        let mut out_scale = [0.0f32; MAX_OUTPUT_CHANNELS];
        for (out_idx, scale) in out_scale.iter_mut().enumerate().take(out_ch) {
            let is_muted = ((self.mute_mask >> out_idx) & 1) != 0;
            let is_solo_blocked = self.solo_mask != 0 && ((self.solo_mask >> out_idx) & 1) == 0;
            if !(is_muted || is_solo_blocked) {
                let col = out_idx + 1;
                *scale = global_scale * cell_linear(self.base_db[0][col], self.rel_offset_db[0][col]);
            }
        }

        // Filas contiguas en memoria: entrada en el bucle externo.
        for in_idx in 0..in_ch {
            let row = in_idx + 1;
            let in_lin = cell_linear(self.base_db[row][0], self.rel_offset_db[row][0]);
            let base_row = &self.base_db[row];
            let rel_row = &self.rel_offset_db[row];
            let target_row = &mut self.target_crosspoint_lin[in_idx];
            for out_idx in 0..out_ch {
                let scale = out_scale[out_idx] * in_lin;
                target_row[out_idx] = if scale == 0.0 {
                    0.0
                } else {
                    scale * cell_linear(base_row[out_idx + 1], rel_row[out_idx + 1])
                };
            }
        }
        self.matrix_dirty = false;
    }

    /// Un frame de suavizado: acerca cada ganancia actual a su objetivo (`coeff` de [`slew_coeff`]).
    /// Por debajo de `SLEW_EPSILON` salta al objetivo para no arrastrar colas denormales.
    #[inline(always)]
    pub fn slew_step(&mut self, coeff: f32) {
        let in_ch = self.in_channels.min(MAX_INPUT_CHANNELS);
        let out_ch = self.out_channels.min(MAX_OUTPUT_CHANNELS);
        for in_idx in 0..in_ch {
            let target = &self.target_crosspoint_lin[in_idx][..out_ch];
            let current = &mut self.current_crosspoint_lin[in_idx][..out_ch];
            for (c, &t) in current.iter_mut().zip(target) {
                let d = t - *c;
                *c = if d.abs() > SLEW_EPSILON { *c + d * coeff } else { t };
            }
        }
    }

    /// Copia los objetivos a las ganancias actuales (sin rampa).
    pub fn snap_to_targets(&mut self) {
        self.current_crosspoint_lin = self.target_crosspoint_lin;
    }

    /// `true` si alguna ganancia activa aún no alcanzó su objetivo.
    pub fn is_settling(&self) -> bool {
        let out_ch = self.out_channels.min(MAX_OUTPUT_CHANNELS);
        (0..self.in_channels.min(MAX_INPUT_CHANNELS)).any(|i| {
            self.target_crosspoint_lin[i][..out_ch]
                .iter()
                .zip(&self.current_crosspoint_lin[i][..out_ch])
                .any(|(&t, &c)| (t - c).abs() > SLEW_EPSILON)
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VoiceError {
    TooManySlices(usize),
}

impl core::fmt::Display for VoiceError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::TooManySlices(n) => write!(f, "{n} segmentos superan el máximo de {MAX_SLICES_PER_VOICE}"),
        }
    }
}

impl std::error::Error for VoiceError {}

/// Estado mutable accedido SOLO por el hilo del callback de audio. Alineado a línea de caché para
/// no compartirla con los atómicos que lee la UI.
#[repr(C, align(64))]
pub struct VoiceRtState {
    pub cursor_frame: u64,
    pub sub_frame_frac: f64,
    pub current_slice_idx: usize,
    pub slice_count: usize,
    pub slices: [SliceSegment; MAX_SLICES_PER_VOICE],
    pub matrix: AudioLevelMatrixRt,
    pub active_fades: [MatrixFadeSlot; MAX_FADES_PER_VOICE],
    /// Envolvente de transporte (Pause/Resume/Stop): `start + (target - start) * curve(t)`.
    pub transport_fade_current: f32,
    pub transport_fade_target: f32,
    /// Progreso por frame (`1 / duración`); `0.0` = sin fade activo.
    pub transport_fade_step: f32,
    pub transport_fade_start: f32,
    /// Progreso `0..=1` del fade (f64 para que los fades largos no acumulen error).
    pub transport_fade_t: f64,
    pub transport_curve: CurveTable,
    /// Estado al que pasa la voz cuando el fade de transporte termina.
    pub transport_fade_action_on_zero: VoiceState,
    pub duck_current_lin: f32,
    pub duck_target_lin: f32,
    pub duck_step: f32,
    /// Ecualizador paramétrico de 4 bandas (estado de los filtros y parámetros suavizados).
    pub eq: crate::biquad::EqRt,
}

impl VoiceRtState {
    /// Construye el estado en el hilo secuenciador, antes de publicar la voz. Sin segmentos se usa
    /// uno solo `[cursor_frame, u64::MAX)` de una pasada.
    pub fn new(cursor_frame: u64, slices: &[SliceSegment], matrix: AudioLevelMatrixRt) -> Result<Self, VoiceError> {
        if slices.len() > MAX_SLICES_PER_VOICE {
            return Err(VoiceError::TooManySlices(slices.len()));
        }
        let mut arr = [SliceSegment::EMPTY; MAX_SLICES_PER_VOICE];
        let slice_count = if slices.is_empty() {
            arr[0] = SliceSegment::new(cursor_frame, u64::MAX, 1);
            1
        } else {
            for (dst, src) in arr.iter_mut().zip(slices) {
                *dst = SliceSegment { remaining_loops: src.play_count, ..*src };
            }
            slices.len()
        };
        Ok(Self {
            cursor_frame,
            sub_frame_frac: 0.0,
            current_slice_idx: 0,
            slice_count,
            slices: arr,
            matrix,
            active_fades: [MatrixFadeSlot::EMPTY; MAX_FADES_PER_VOICE],
            transport_fade_current: 1.0,
            transport_fade_target: 1.0,
            transport_fade_step: 0.0,
            transport_fade_start: 1.0,
            transport_fade_t: 0.0,
            transport_curve: CurveTable::LINEAR,
            transport_fade_action_on_zero: VoiceState::Stopped,
            duck_current_lin: 1.0,
            duck_target_lin: 1.0,
            duck_step: 0.0,
            eq: crate::biquad::EqRt::new(),
        })
    }
}

/// Datos inmutables de una voz.
#[derive(Clone, Copy, Debug)]
pub struct VoiceInit {
    pub id: VoiceId,
    pub cue_unique_id: u128,
    pub patch_id: PatchId,
    pub file_sample_rate: u32,
    pub start_frame: u64,
    pub end_frame: u64,
    pub infinite_loop: bool,
    pub rate: f32,
}

/// Voz compartida entre el secuenciador y el callback mediante `ArcSwap`.
#[repr(C, align(64))]
pub struct Voice {
    pub id: VoiceId,
    pub cue_unique_id: u128,
    pub patch_id: PatchId,
    pub file_sample_rate: u32,
    pub start_frame: u64,
    pub end_frame: u64,
    pub infinite_loop: AtomicBool,
    /// [`VoiceState`] como `u8`.
    pub state: AtomicU8,
    /// Posición publicada por el callback; la leen secuenciador y UI sin esperas.
    pub published_frame_pos: AtomicU64,
    pub published_loops_left: AtomicU32,
    /// Pico (`f32::to_bits`, siempre >= 0, así que el orden de los enteros es el de los reales) desde la última
    /// lectura con `take_peak`. Lo escribe el callback; lo recoge el secuenciador al publicar.
    pub peak_bits: AtomicU32,
    /// `f32::to_bits`.
    pub rate_bits: AtomicU32,
    /// [`DevampMode`] como `u8`.
    pub devamp_request: AtomicU8,
    pub devamp_trigger_next: AtomicBool,
    /// Lector pre-alojado (StartPinned + ping-pong A/B).
    pub reader: StreamingVoiceReader,
    /// Detecta accesos concurrentes a `rt` en compilaciones de depuración.
    rt_borrowed: AtomicBool,
    /// Estado exclusivo del hilo de callback de audio.
    rt: UnsafeCell<VoiceRtState>,
}

// SAFETY: `rt` solo lo accede el único hilo del callback de audio (`rt_mut`, `unsafe`, con
// detección de solapamiento en debug). Todos los demás campos son atómicos o inmutables, y
// `Voice` es `Send` automáticamente porque `VoiceRtState` lo es.
unsafe impl Sync for Voice {}

impl Voice {
    pub fn new(init: VoiceInit, rt: VoiceRtState) -> Self {
        Self::with_reader(init, rt, StreamingVoiceReader::new())
    }

    pub fn with_reader(init: VoiceInit, rt: VoiceRtState, reader: StreamingVoiceReader) -> Self {
        Self {
            id: init.id,
            cue_unique_id: init.cue_unique_id,
            patch_id: init.patch_id,
            file_sample_rate: init.file_sample_rate,
            start_frame: init.start_frame,
            end_frame: init.end_frame,
            infinite_loop: AtomicBool::new(init.infinite_loop),
            state: AtomicU8::new(VoiceState::Idle as u8),
            published_frame_pos: AtomicU64::new(init.start_frame),
            published_loops_left: AtomicU32::new(0),
            peak_bits: AtomicU32::new(0),
            rate_bits: AtomicU32::new(init.rate.to_bits()),
            devamp_request: AtomicU8::new(DevampMode::None as u8),
            devamp_trigger_next: AtomicBool::new(false),
            reader,
            rt_borrowed: AtomicBool::new(false),
            rt: UnsafeCell::new(rt),
        }
    }

    #[inline]
    pub fn state(&self) -> VoiceState {
        VoiceState::from_u8(self.state.load(Ordering::Acquire))
    }

    #[inline]
    pub fn set_state(&self, state: VoiceState) {
        self.state.store(state as u8, Ordering::Release);
    }

    #[inline]
    pub fn rate(&self) -> f32 {
        f32::from_bits(self.rate_bits.load(Ordering::Relaxed))
    }

    #[inline]
    pub fn set_rate(&self, rate: f32) {
        self.rate_bits.store(rate.to_bits(), Ordering::Relaxed);
    }

    #[inline]
    pub fn request_devamp(&self, mode: DevampMode, trigger_next: bool) {
        self.devamp_trigger_next.store(trigger_next, Ordering::Relaxed);
        self.devamp_request.store(mode as u8, Ordering::Release);
    }

    /// Consume la petición pendiente de devamp (hilo de audio).
    #[inline]
    pub fn take_devamp(&self) -> (DevampMode, bool) {
        let mode = DevampMode::from_u8(self.devamp_request.swap(DevampMode::None as u8, Ordering::AcqRel));
        (mode, self.devamp_trigger_next.load(Ordering::Relaxed))
    }

    /// Registra el pico de un trozo (callback de audio).
    #[inline]
    pub fn note_peak(&self, peak: f32) {
        if peak > 0.0 {
            self.peak_bits.fetch_max(peak.to_bits(), Ordering::Relaxed);
        }
    }

    /// Pico máximo desde la última llamada; lo deja a cero.
    pub fn take_peak(&self) -> f32 {
        f32::from_bits(self.peak_bits.swap(0, Ordering::Relaxed))
    }

    #[inline]
    pub fn publish_progress(&self, frame_pos: u64, loops_left: u32) {
        self.published_frame_pos.store(frame_pos, Ordering::Relaxed);
        self.published_loops_left.store(loops_left, Ordering::Relaxed);
    }

    /// Acceso mutable al estado de audio.
    ///
    /// # Safety
    /// Solo el hilo del callback de audio puede llamarlo, y sin otro [`RtStateGuard`] vivo de la
    /// misma voz. En compilaciones de depuración un solapamiento provoca panic.
    #[inline]
    pub unsafe fn rt_mut(&self) -> RtStateGuard<'_> {
        #[cfg(debug_assertions)]
        assert!(
            !self.rt_borrowed.swap(true, Ordering::Acquire),
            "Voice::rt_mut: acceso concurrente al estado RT de la voz {}",
            self.id
        );
        RtStateGuard { voice: self }
    }
}

/// Acceso exclusivo al [`VoiceRtState`] mientras viva.
pub struct RtStateGuard<'a> {
    voice: &'a Voice,
}

impl Deref for RtStateGuard<'_> {
    type Target = VoiceRtState;
    #[inline(always)]
    fn deref(&self) -> &VoiceRtState {
        // SAFETY: contrato de `Voice::rt_mut`.
        unsafe { &*self.voice.rt.get() }
    }
}

impl DerefMut for RtStateGuard<'_> {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut VoiceRtState {
        // SAFETY: contrato de `Voice::rt_mut`.
        unsafe { &mut *self.voice.rt.get() }
    }
}

impl Drop for RtStateGuard<'_> {
    #[inline]
    fn drop(&mut self) {
        #[cfg(debug_assertions)]
        self.voice.rt_borrowed.store(false, Ordering::Release);
    }
}

#[cfg(test)]
pub(crate) fn test_voice(id: VoiceId) -> Voice {
    let matrix = AudioLevelMatrixRt::new(2, 2);
    let rt = VoiceRtState::new(0, &[], matrix).unwrap();
    Voice::new(
        VoiceInit {
            id,
            cue_unique_id: id as u128,
            patch_id: 0,
            file_sample_rate: 48_000,
            start_frame: 0,
            end_frame: 48_000,
            infinite_loop: false,
            rate: 1.0,
        },
        rt,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alloc_guard::{assert_no_alloc, set_violation_policy, violations_on_this_thread, ViolationPolicy};

    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn voice_is_send_sync_and_aligned() {
        assert_send_sync::<Voice>();
        assert_eq!(core::mem::align_of::<Voice>(), 64);
        assert_eq!(core::mem::align_of::<VoiceRtState>(), 64);
        assert_eq!(core::mem::size_of::<Voice>() % 64, 0);
    }

    #[test]
    fn matrix_defaults_one_to_one() {
        let m = AudioLevelMatrixRt::new(2, 3);
        assert_eq!(m.target_crosspoint_lin[0][0], 1.0);
        assert_eq!(m.target_crosspoint_lin[1][1], 1.0);
        assert_eq!(m.target_crosspoint_lin[0][1], 0.0);
        assert_eq!(m.target_crosspoint_lin[1][2], 0.0);
        assert_eq!(m.current_crosspoint_lin, m.target_crosspoint_lin);
        assert!(!m.matrix_dirty && !m.is_settling());
    }

    #[test]
    fn recompute_applies_masters_duck_transport_and_gates() {
        let mut m = AudioLevelMatrixRt::new(2, 2);
        m.base_db[0][0] = -6.0; // master
        m.base_db[0][2] = -6.0; // master salida 2
        m.base_db[1][0] = -6.0; // trim entrada 1
        m.base_db[1][1] = -3.0; // crosspoint 1->1
        m.rel_offset_db[2][2] = -6.0; // fade relativo en 2->2
        m.recompute_targets(0.5, 0.5);
        let lin = |db: f32| 10f32.powf(db / 20.0);
        let want_11 = lin(-6.0) * 0.25 * 1.0 * lin(-6.0) * lin(-3.0);
        assert!((m.target_crosspoint_lin[0][0] - want_11).abs() < 1e-6);
        let want_22 = lin(-6.0) * 0.25 * lin(-6.0) * 1.0 * lin(-6.0);
        assert!((m.target_crosspoint_lin[1][1] - want_22).abs() < 1e-6);
        // mute de la salida 1 (bit 0)
        m.mute_mask = 0b01;
        m.recompute_targets(1.0, 1.0);
        assert_eq!(m.target_crosspoint_lin[0][0], 0.0);
        assert!(m.target_crosspoint_lin[1][1] > 0.0);
        // solo en la salida 1: la 2 queda bloqueada, y el mute de la 1 gana
        m.mute_mask = 0;
        m.solo_mask = 0b01;
        m.recompute_targets(1.0, 1.0);
        assert!(m.target_crosspoint_lin[0][0] > 0.0);
        assert_eq!(m.target_crosspoint_lin[1][1], 0.0);
    }

    #[test]
    fn minus_inf_base_stays_silent_under_relative_boost() {
        let mut m = AudioLevelMatrixRt::new(2, 2);
        m.rel_offset_db[1][2] = 24.0; // base de 1->2 es -inf
        m.recompute_targets(1.0, 1.0);
        assert_eq!(m.target_crosspoint_lin[0][1], 0.0);
    }

    #[test]
    fn slew_reaches_target_in_about_5ms_constant() {
        let fs = 48_000.0;
        let coeff = slew_coeff(fs);
        assert!((coeff - 1.0 / 240.0).abs() < 1e-9);
        let mut m = AudioLevelMatrixRt::new(1, 1);
        m.base_db[1][1] = -20.0;
        m.recompute_targets(1.0, 1.0);
        let target = m.target_crosspoint_lin[0][0];
        assert!(m.is_settling());
        // Tras una constante de tiempo (240 frames) recorre ~63 % del salto.
        for _ in 0..240 {
            m.slew_step(coeff);
        }
        let frac = (m.current_crosspoint_lin[0][0] - 1.0) / (target - 1.0);
        assert!((frac - 0.632).abs() < 0.01, "frac = {frac}");
        for _ in 0..40_000 {
            m.slew_step(coeff);
        }
        assert_eq!(m.current_crosspoint_lin[0][0], target);
        assert!(!m.is_settling());
    }

    #[test]
    fn rt_math_does_not_allocate() {
        set_violation_policy(ViolationPolicy::CountOnly);
        let mut m = AudioLevelMatrixRt::new(8, 8);
        m.base_db[3][3] = -9.0;
        let before = violations_on_this_thread();
        assert_no_alloc(|| {
            for k in 0..100 {
                m.rel_offset_db[1][1] = -(k as f32);
                m.recompute_targets(0.9, 0.8);
                for _ in 0..64 {
                    m.slew_step(0.004);
                }
            }
        });
        assert_eq!(violations_on_this_thread(), before);
    }

    #[test]
    fn rt_state_slices() {
        let slices = [SliceSegment::new(0, 100, 2), SliceSegment::new(100, 200, SliceSegment::INFINITE)];
        let rt = VoiceRtState::new(0, &slices, AudioLevelMatrixRt::new(1, 1)).unwrap();
        assert_eq!(rt.slice_count, 2);
        assert_eq!(rt.slices[0].remaining_loops, 2);
        assert!(rt.slices[1].is_infinite());
        let default = VoiceRtState::new(10, &[], AudioLevelMatrixRt::new(1, 1)).unwrap();
        assert_eq!((default.slice_count, default.slices[0].start_frame), (1, 10));
        let too_many = [SliceSegment::EMPTY; MAX_SLICES_PER_VOICE + 1];
        assert_eq!(
            VoiceRtState::new(0, &too_many, AudioLevelMatrixRt::new(1, 1)).err(),
            Some(VoiceError::TooManySlices(65))
        );
    }

    #[test]
    fn rt_state_is_mutated_by_one_thread_and_atomics_read_by_others() {
        use std::sync::Arc;
        let voice = Arc::new(test_voice(7));
        let rt_voice = Arc::clone(&voice);
        let audio = std::thread::spawn(move || {
            // SAFETY: este es el único hilo "de audio" de la prueba.
            let mut rt = unsafe { rt_voice.rt_mut() };
            for i in 1..=1000u64 {
                rt.cursor_frame = i;
                rt_voice.publish_progress(rt.cursor_frame, 3);
            }
        });
        audio.join().unwrap();
        assert_eq!(voice.published_frame_pos.load(Ordering::Relaxed), 1000);
        assert_eq!(unsafe { voice.rt_mut() }.cursor_frame, 1000);
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "acceso concurrente")]
    fn overlapping_rt_access_panics_in_debug() {
        let v = test_voice(1);
        let _a = unsafe { v.rt_mut() };
        let _b = unsafe { v.rt_mut() };
    }

    #[test]
    fn atomic_helpers() {
        let v = test_voice(1);
        assert_eq!(v.state(), VoiceState::Idle);
        v.set_state(VoiceState::Playing);
        assert_eq!(v.state(), VoiceState::Playing);
        v.set_rate(2.0);
        assert_eq!(v.rate(), 2.0);
        v.request_devamp(DevampMode::StopAtSliceEnd, true);
        assert_eq!(v.take_devamp(), (DevampMode::StopAtSliceEnd, true));
        assert_eq!(v.take_devamp().0, DevampMode::None);
    }
}
