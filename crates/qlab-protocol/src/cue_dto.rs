//! DTOs de las 4 pestañas del Inspector de audio (spec §1.2).

use std::fmt;

use serde::{Deserialize, Serialize};

pub use crate::matrix::AudioLevelMatrixData;
use crate::matrix::MAX_VOLUME_DB;

/// Segmentos máximos por voz en el hilo RT; con `k` marcadores hay `k + 1` segmentos.
pub const MAX_SLICES_PER_VOICE: usize = 64;
/// `play_count` de un slice que se repite hasta recibir `Devamp`/`Stop`.
pub const INFINITE_SLICE_LOOP: u32 = u32::MAX;
pub const RATE_MIN: f32 = 0.03;
pub const RATE_MAX: f32 = 33.0;
/// Puntos de la envolvente integrada precompilada.
pub const FADE_TABLE_POINTS: usize = 256;

#[derive(Clone, Debug, PartialEq)]
pub enum DtoError {
    NegativeOrNonFinite(&'static str),
    RateOutOfRange(f32),
    PlayCountZero,
    EmptyRegion { start_frame: u64, end_frame: u64 },
    TooManySlices(usize),
    SlicesNotIncreasing,
    SliceOutsideRegion(u64),
    SliceLabelTooLong(u64),
    FadeCurveLen(usize),
    FadeCurveNotFinite,
    BadWallClock,
}

impl fmt::Display for DtoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NegativeOrNonFinite(n) => write!(f, "`{n}` debe ser finito y >= 0"),
            Self::RateOutOfRange(r) => write!(f, "rate {r} fuera de [{RATE_MIN}, {RATE_MAX}]"),
            Self::PlayCountZero => write!(f, "play_count debe ser >= 1"),
            Self::EmptyRegion { start_frame, end_frame } => {
                write!(f, "región vacía: start {start_frame} >= end {end_frame}")
            }
            Self::TooManySlices(n) => write!(f, "{n} marcadores superan el máximo de {}", MAX_SLICES_PER_VOICE - 1),
            Self::SlicesNotIncreasing => write!(f, "los marcadores de slice deben ser estrictamente crecientes"),
            Self::SliceOutsideRegion(fr) => write!(f, "el marcador {fr} está fuera de (start, end)"),
            Self::SliceLabelTooLong(fr) => write!(f, "el nombre del marcador {fr} supera los {MAX_SLICE_LABEL_CHARS} caracteres"),
            Self::FadeCurveLen(n) => write!(f, "fade_curve tiene {n} puntos; debe estar vacía o tener {FADE_TABLE_POINTS}"),
            Self::FadeCurveNotFinite => write!(f, "fade_curve contiene valores no finitos"),
            Self::BadWallClock => write!(f, "hora de reloj de pared inválida"),
        }
    }
}

impl std::error::Error for DtoError {}

fn check_secs(name: &'static str, v: f64) -> Result<(), DtoError> {
    if v.is_finite() && v >= 0.0 { Ok(()) } else { Err(DtoError::NegativeOrNonFinite(name)) }
}

/// Id de cue para el motor (`u64`). El show JSON usará UUID y se mapeará a este valor.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CueId(pub u64);

// ---------------------------------------------------------------------------------------------
// Pestaña 1: Basics
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
#[serde(rename_all = "snake_case")]
pub enum ContinueMode {
    #[default]
    DoNotContinue = 0,
    AutoContinue = 1,
    AutoFollow = 2,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CueColor {
    #[default]
    None,
    Red,
    Orange,
    Green,
    Blue,
    Purple,
}

/// Referencia a un archivo multimedia (sustituye a `F53Alias`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaTarget {
    pub absolute_path: String,
    pub relative_path: Option<String>,
    /// SHA-256 en hexadecimal.
    pub sha256: Option<String>,
    pub byte_len: u64,
    pub mtime_unix_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CueBasicsData {
    pub unique_id: CueId,
    /// `"1"`, `"2.1"`, `"10A"`…
    pub number: String,
    pub name: String,
    /// Nombre calculado cuando `name` está vacío.
    pub default_name: String,
    pub notes: String,
    pub file_target: Option<MediaTarget>,
    pub cue_target_id: Option<CueId>,
    pub pre_wait: f64,
    pub action_duration: f64,
    /// Solo en memoria; no debe persistirse.
    #[serde(skip)]
    pub temp_duration: Option<f64>,
    pub post_wait: f64,
    pub continue_mode: ContinueMode,
    pub armed: bool,
    pub skip_if_disarmed: bool,
    pub auto_load: bool,
    pub flagged: bool,
    pub color: CueColor,
    pub second_color: Option<CueColor>,
}

impl Default for CueBasicsData {
    fn default() -> Self {
        Self {
            unique_id: CueId::default(),
            number: String::new(),
            name: String::new(),
            default_name: String::new(),
            notes: String::new(),
            file_target: None,
            cue_target_id: None,
            pre_wait: 0.0,
            action_duration: 0.0,
            temp_duration: None,
            post_wait: 0.0,
            continue_mode: ContinueMode::DoNotContinue,
            armed: true,
            skip_if_disarmed: false,
            auto_load: false,
            flagged: false,
            color: CueColor::None,
            second_color: None,
        }
    }
}

impl CueBasicsData {
    /// Nombre a mostrar: `name`, o `default_name` si está vacío.
    pub fn display_name(&self) -> &str {
        if self.name.is_empty() { &self.default_name } else { &self.name }
    }

    pub fn validate(&self) -> Result<(), DtoError> {
        check_secs("pre_wait", self.pre_wait)?;
        check_secs("action_duration", self.action_duration)?;
        check_secs("post_wait", self.post_wait)?;
        if let Some(t) = self.temp_duration {
            check_secs("temp_duration", t)?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// Pestaña 2: Triggers
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyChord {
    pub key: String,
    /// Bits: 1 = Shift, 2 = Ctrl, 4 = Alt, 8 = Meta/Cmd.
    pub modifiers: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MidiCommand {
    NoteOn,
    NoteOff,
    ProgramChange,
    ControlChange,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MidiTriggerSpec {
    pub command: MidiCommand,
    /// Canal 0..=15; `-1` = cualquiera.
    pub channel: i8,
    pub byte1: u8,
    pub byte2_min: u8,
    pub byte2_max: u8,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SmpteTimecode {
    pub hours: u8,
    pub minutes: u8,
    pub seconds: u8,
    pub frames: u8,
    pub fps: u8,
    pub drop_frame: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
#[serde(rename_all = "snake_case")]
pub enum SecondTriggerAction {
    #[default]
    Nothing = 0,
    Panic = 1,
    Stop = 2,
    HardStop = 3,
    HardStopAndRestart = 4,
    Devamp = 5,
    PlaylistPlaysNext = 6,
    PlaylistPlaysPrevious = 7,
}

/// Alcance de "Fade & Stop Others" al disparar la cue.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
#[serde(rename_all = "snake_case")]
pub enum FadeOthersScope {
    #[default]
    Disabled = 0,
    Peers = 1,
    CueList = 2,
    All = 3,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CueTriggersData {
    pub use_hotkey: bool,
    pub hotkey: Option<KeyChord>,
    pub use_midi: bool,
    pub midi_trigger: Option<MidiTriggerSpec>,
    pub use_wall_clock: bool,
    pub wall_hms: (u8, u8, u8),
    /// Bit 0 = domingo … bit 6 = sábado.
    pub weekdays_mask: u8,
    pub use_timecode: bool,
    pub timecode_trigger: SmpteTimecode,
    pub second_trigger_on_release: bool,
    pub second_trigger_action: SecondTriggerAction,
    /// Fade & Stop Others: alcance del auto-fade al entrar en `Action`.
    pub fade_and_stop_others_mode: FadeOthersScope,
    pub fade_and_stop_others_time: f64,
    /// Duck / Boost Others.
    pub duck_others_enabled: bool,
    pub duck_level_db: f32,
    pub duck_time_sec: f64,
}

impl Default for CueTriggersData {
    fn default() -> Self {
        Self {
            use_hotkey: false,
            hotkey: None,
            use_midi: false,
            midi_trigger: None,
            use_wall_clock: false,
            wall_hms: (0, 0, 0),
            weekdays_mask: 0x7F,
            use_timecode: false,
            timecode_trigger: SmpteTimecode::default(),
            second_trigger_on_release: false,
            second_trigger_action: SecondTriggerAction::Nothing,
            fade_and_stop_others_mode: FadeOthersScope::Disabled,
            fade_and_stop_others_time: 0.0,
            duck_others_enabled: false,
            duck_level_db: -12.0,
            duck_time_sec: 0.0,
        }
    }
}

impl CueTriggersData {
    pub fn validate(&self) -> Result<(), DtoError> {
        check_secs("fade_and_stop_others_time", self.fade_and_stop_others_time)?;
        check_secs("duck_time_sec", self.duck_time_sec)?;
        let (h, m, s) = self.wall_hms;
        if h > 23 || m > 59 || s > 59 {
            return Err(DtoError::BadWallClock);
        }
        if !self.duck_level_db.is_finite() || self.duck_level_db > MAX_VOLUME_DB {
            return Err(DtoError::NegativeOrNonFinite("duck_level_db"));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// Pestaña 3: Time & Loops
// ---------------------------------------------------------------------------------------------

/// Marcador de slice: la región `[frame, siguiente)` se repite `play_count` veces.
///
/// `frame` es el desplazamiento en frames del archivo donde empieza el slice. La lista de marcadores
/// de `AudioTimeAndLoopsData::slices` va ordenada de forma estrictamente creciente (lo exige `validate`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioSliceMarker {
    pub frame: u64,
    /// `1..N` = repeticiones fijas; `INFINITE_SLICE_LOOP` = bucle continuo hasta un Devamp.
    pub play_count: u32,
    /// Nombre del tramo que empieza en este marcador (p. ej. «Entrada coro»). Solo informativo: no afecta
    /// a la reproducción ni obliga a recargar la voz.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub label: String,
}

/// Longitud máxima del nombre de un slice.
pub const MAX_SLICE_LABEL_CHARS: usize = 64;

/// Nombre corto del marcador de slice.
pub type SliceMarker = AudioSliceMarker;

impl AudioSliceMarker {
    pub const INFINITE: u32 = INFINITE_SLICE_LOOP;

    pub const fn new(frame: u64, play_count: u32) -> Self {
        Self { frame, play_count, label: String::new() }
    }

    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = label.into();
        self
    }

    #[inline]
    pub const fn is_infinite(&self) -> bool {
        self.play_count == INFINITE_SLICE_LOOP
    }

    /// Siguiente valor del selector 1× → 2× → ∞ → 1×; cualquier otro valor vuelve a 1×.
    pub const fn next_play_count(&self) -> u32 {
        match self.play_count {
            1 => 2,
            2 => INFINITE_SLICE_LOOP,
            _ => 1,
        }
    }
}

impl AudioTimeAndLoopsData {
    /// Los tiempos tal como se reproducen: con un «arrancar desde» válido (dentro de la región) el inicio pasa a ese punto
    /// y los slices anteriores se ignoran; si no, son los mismos datos sin copiar.
    pub fn playback_view(&self) -> std::borrow::Cow<'_, Self> {
        let Some(s) = self.start_override_secs else { return std::borrow::Cow::Borrowed(self) };
        let end = if self.end_time_sec > 0.0 { self.end_time_sec } else { f64::INFINITY };
        if !(s.is_finite() && s > self.start_time_sec && s < end) {
            return std::borrow::Cow::Borrowed(self);
        }
        let mut v = self.clone();
        v.start_time_sec = s;
        v.start_override_secs = None;
        v.memories.clear();
        let from = (s * self.file_sample_rate.max(1) as f64).round() as u64;
        v.slices.retain(|m| m.frame > from);
        std::borrow::Cow::Owned(v)
    }

    /// Inserta un marcador en `frame` manteniendo la lista ordenada. Devuelve su índice, o `None` si ya
    /// hay uno en ese frame o queda fuera de la región `(inicio, fin)`; el fin vale `file_frames`
    /// cuando la cue no fija uno.
    pub fn add_slice(&mut self, frame: u64, play_count: u32, file_frames: u64) -> Option<usize> {
        let sr = self.file_sample_rate.max(1) as f64;
        let start = (self.start_time_sec.max(0.0) * sr).round() as u64;
        let end = if self.end_time_sec > 0.0 { ((self.end_time_sec * sr).round() as u64).min(file_frames.max(1)) } else { file_frames };
        if frame <= start || (end > 0 && frame >= end) || self.slices.len() + 1 >= MAX_SLICES_PER_VOICE {
            return None;
        }
        match self.slices.binary_search_by_key(&frame, |m| m.frame) {
            Ok(_) => None,
            Err(i) => {
                self.slices.insert(i, AudioSliceMarker::new(frame, play_count.max(1)));
                Some(i)
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Ecualizador paramétrico de 4 bandas por cue
// ---------------------------------------------------------------------------------------------

pub const EQ_BANDS: usize = 4;
pub const EQ_MIN_HZ: f32 = 20.0;
pub const EQ_MAX_HZ: f32 = 20_000.0;
pub const EQ_MIN_GAIN_DB: f32 = -24.0;
pub const EQ_MAX_GAIN_DB: f32 = 12.0;
pub const EQ_MIN_Q: f32 = 0.1;
pub const EQ_MAX_Q: f32 = 10.0;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EqFilterType {
    /// Paso alto: recorta graves.
    LowCut,
    LowShelf,
    #[default]
    Peaking,
    HighShelf,
    /// Paso bajo: recorta agudos.
    HighCut,
}

impl EqFilterType {
    pub const ALL: [Self; 5] = [Self::LowCut, Self::LowShelf, Self::Peaking, Self::HighShelf, Self::HighCut];

    pub fn label(self) -> &'static str {
        match self {
            Self::LowCut => "Low Cut",
            Self::LowShelf => "Low Shelf",
            Self::Peaking => "Peaking",
            Self::HighShelf => "High Shelf",
            Self::HighCut => "High Cut",
        }
    }

    /// ¿Usa la ganancia? (los cortes no).
    pub fn uses_gain(self) -> bool {
        matches!(self, Self::LowShelf | Self::Peaking | Self::HighShelf)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EqBandData {
    pub filter_type: EqFilterType,
    /// 20 a 20 000 Hz.
    pub frequency_hz: f32,
    /// −24 a +12 dB (los cortes la ignoran).
    pub gain_db: f32,
    /// 0,1 a 10.
    pub q_factor: f32,
    pub enabled: bool,
}

impl Default for EqBandData {
    fn default() -> Self {
        Self { filter_type: EqFilterType::Peaking, frequency_hz: 1_000.0, gain_db: 0.0, q_factor: 1.0, enabled: true }
    }
}

impl EqBandData {
    /// Valores dentro de rango (NaN y desbordes se corrigen).
    pub fn clamped(mut self) -> Self {
        let fix = |v: f32, lo: f32, hi: f32, d: f32| if v.is_finite() { v.clamp(lo, hi) } else { d };
        self.frequency_hz = fix(self.frequency_hz, EQ_MIN_HZ, EQ_MAX_HZ, 1_000.0);
        self.gain_db = fix(self.gain_db, EQ_MIN_GAIN_DB, EQ_MAX_GAIN_DB, 0.0);
        self.q_factor = fix(self.q_factor, EQ_MIN_Q, EQ_MAX_Q, 1.0);
        self
    }
}

/// Ecualizador de una cue de audio o vídeo: 4 bandas en cascada. Apagado por defecto.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioEqData {
    pub enabled: bool,
    pub bands: [EqBandData; EQ_BANDS],
}

impl Default for AudioEqData {
    /// Apagado y plano: graves (low shelf 100 Hz), dos medios (500 Hz y 2 kHz) y agudos (high shelf 8 kHz), todos a 0 dB.
    fn default() -> Self {
        let band = |filter_type, frequency_hz, q_factor| EqBandData { filter_type, frequency_hz, gain_db: 0.0, q_factor, enabled: true };
        Self {
            enabled: false,
            bands: [band(EqFilterType::LowShelf, 100.0, 0.707), band(EqFilterType::Peaking, 500.0, 1.0), band(EqFilterType::Peaking, 2_000.0, 1.0), band(EqFilterType::HighShelf, 8_000.0, 0.707)],
        }
    }
}

impl AudioEqData {
    /// ¿Es el valor por defecto? (así los shows sin EQ no ganan campos al guardarse).
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    pub fn clamped(mut self) -> Self {
        for b in self.bands.iter_mut() {
            *b = b.clamped();
        }
        self
    }
}

/// «Memoria»: un punto del audio con nombre y nota (como las ubicaciones de memoria de Pro Tools). No afecta a la
/// reproducción: sirve para marcar y volver a encontrar pasajes del ensayo.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MemoryMark {
    /// Posición dentro del archivo, en segundos.
    pub secs: f64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
}

pub const MAX_MEMORIES: usize = 500;
pub const MAX_MEMORY_NAME_CHARS: usize = 64;
pub const MAX_MEMORY_NOTE_CHARS: usize = 1_000;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioTimeAndLoopsData {
    pub file_sample_rate: u32,
    pub start_time_sec: f64,
    pub end_time_sec: f64,
    /// Reproducciones de `[start_frame, end_frame)`; siempre >= 1.
    pub play_count: u32,
    pub infinite_loop: bool,
    pub slices: Vec<AudioSliceMarker>,
    /// Velocidad en `[0.03, 33.0]`.
    pub rate: f32,
    /// Deshabilitado en el MVP; se conserva por compatibilidad con archivos `.qlab5`.
    pub preserve_pitch: bool,
    pub do_integrated_fade: bool,
    pub lock_fade_to_cue: bool,
    /// Envolvente precompilada de 256 puntos, o vacía si no hay.
    pub fade_curve: Vec<f32>,
    /// Memorias (puntos con nombre y nota) marcadas sobre la onda; ordenadas por posición.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub memories: Vec<MemoryMark>,
    /// «Arrancar desde»: al dispararse la cue empieza en este punto (segundos del archivo) en vez de en `start_time_sec`.
    /// No toca la región ni los slices guardados: quitarlo devuelve la cue a como estaba.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_override_secs: Option<f64>,
}

impl Default for AudioTimeAndLoopsData {
    fn default() -> Self {
        Self {
            file_sample_rate: 48_000,
            start_time_sec: 0.0,
            end_time_sec: 0.0,
            play_count: 1,
            infinite_loop: false,
            slices: Vec::new(),
            rate: 1.0,
            preserve_pitch: false,
            do_integrated_fade: false,
            lock_fade_to_cue: false,
            fade_curve: Vec::new(),
            memories: Vec::new(),
            start_override_secs: None,
        }
    }
}

impl AudioTimeAndLoopsData {
    #[inline]
    pub fn start_frame(&self) -> u64 {
        (self.start_time_sec * self.file_sample_rate as f64).round() as u64
    }

    #[inline]
    pub fn end_frame(&self) -> u64 {
        (self.end_time_sec * self.file_sample_rate as f64).round() as u64
    }

    /// Avance de lectura por frame de salida: `rate * f_file / f_host`.
    #[inline]
    pub fn playback_step(&self, host_sample_rate: u32) -> f64 {
        self.rate as f64 * self.file_sample_rate as f64 / host_sample_rate as f64
    }

    /// `true` si la lectura puede copiar muestras sin interpolar.
    #[inline]
    pub fn is_unity_playback(&self, host_sample_rate: u32) -> bool {
        self.rate == 1.0 && self.file_sample_rate == host_sample_rate
    }

    /// Firma FNV-1a de 64 bits de la lista de slices (longitud, y `frame`/`play_count` de cada uno).
    pub fn slices_signature(&self) -> u64 {
        const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
        const PRIME: u64 = 0x0000_0100_0000_01b3;
        let mut h = OFFSET;
        let mut feed = |bytes: &[u8]| {
            for &b in bytes {
                h ^= b as u64;
                h = h.wrapping_mul(PRIME);
            }
        };
        feed(&(self.slices.len() as u64).to_le_bytes());
        for s in &self.slices {
            feed(&s.frame.to_le_bytes());
            feed(&s.play_count.to_le_bytes());
        }
        h
    }

    pub fn validate(&self) -> Result<(), DtoError> {
        check_secs("start_time_sec", self.start_time_sec)?;
        check_secs("end_time_sec", self.end_time_sec)?;
        if !(RATE_MIN..=RATE_MAX).contains(&self.rate) {
            return Err(DtoError::RateOutOfRange(self.rate));
        }
        if self.play_count == 0 {
            return Err(DtoError::PlayCountZero);
        }
        let (start, end) = (self.start_frame(), self.end_frame());
        if start >= end {
            return Err(DtoError::EmptyRegion { start_frame: start, end_frame: end });
        }
        if self.slices.len() >= MAX_SLICES_PER_VOICE {
            return Err(DtoError::TooManySlices(self.slices.len()));
        }
        let mut prev = start;
        for s in &self.slices {
            if s.frame <= start || s.frame >= end {
                return Err(DtoError::SliceOutsideRegion(s.frame));
            }
            if s.label.chars().count() > MAX_SLICE_LABEL_CHARS {
                return Err(DtoError::SliceLabelTooLong(s.frame));
            }
            if s.frame <= prev {
                return Err(DtoError::SlicesNotIncreasing);
            }
            if s.play_count == 0 {
                return Err(DtoError::PlayCountZero);
            }
            prev = s.frame;
        }
        if !self.fade_curve.is_empty() && self.fade_curve.len() != FADE_TABLE_POINTS {
            return Err(DtoError::FadeCurveLen(self.fade_curve.len()));
        }
        if self.fade_curve.iter().any(|v| !v.is_finite()) {
            return Err(DtoError::FadeCurveNotFinite);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// Pestaña 4: Audio Levels (ver `AudioLevelMatrixData`) y agregado
// ---------------------------------------------------------------------------------------------

/// Estado completo de una `AudioCue`: las 4 pestañas del Inspector.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AudioCueDto {
    pub basics: CueBasicsData,
    pub triggers: CueTriggersData,
    pub time_loops: AudioTimeAndLoopsData,
    pub levels: AudioLevelMatrixData,
}

impl AudioCueDto {
    pub fn validate(&self) -> Result<(), Box<dyn std::error::Error>> {
        self.basics.validate()?;
        self.triggers.validate()?;
        self.time_loops.validate()?;
        self.levels.validate()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn region() -> AudioTimeAndLoopsData {
        AudioTimeAndLoopsData { end_time_sec: 10.0, ..Default::default() }
    }

    #[test]
    fn frames_from_seconds() {
        let t = AudioTimeAndLoopsData { start_time_sec: 1.5, end_time_sec: 2.0, ..Default::default() };
        assert_eq!((t.start_frame(), t.end_frame()), (72_000, 96_000));
        assert!(t.is_unity_playback(48_000));
        assert!((t.playback_step(96_000) - 0.5).abs() < 1e-12);
    }

    #[test]
    fn add_slice_keeps_markers_sorted_and_inside_the_region() {
        let mut t = AudioTimeAndLoopsData { file_sample_rate: 48_000, ..Default::default() };
        let file = 480_000;
        assert_eq!(t.add_slice(96_000, 1, file), Some(0));
        assert_eq!(t.add_slice(48_000, 2, file), Some(0));
        assert_eq!(t.add_slice(144_000, INFINITE_SLICE_LOOP, file), Some(2));
        assert_eq!(t.slices.iter().map(|m| m.frame).collect::<Vec<_>>(), [48_000, 96_000, 144_000]);
        assert_eq!(t.add_slice(96_000, 1, file), None, "duplicado");
        assert_eq!(t.add_slice(0, 1, file), None, "en el inicio de la región");
        assert_eq!(t.add_slice(file, 1, file), None, "en el final del archivo");
        t.end_time_sec = 10.0;
        assert_eq!(t.validate(), Ok(()));
        t.end_time_sec = 2.0;
        assert_eq!(t.add_slice(100_000, 1, file), None, "después del final fijado");
    }

    #[test]
    fn slice_labels_are_optional_in_json_and_validated() {
        let m: AudioSliceMarker = serde_json::from_str(r#"{"frame":10,"play_count":2}"#).unwrap();
        assert_eq!(m.label, "", "los shows antiguos no traen nombre");
        assert_eq!(serde_json::to_string(&m).unwrap(), r#"{"frame":10,"play_count":2}"#, "sin nombre no se escribe el campo");
        let named = AudioSliceMarker::new(10, 2).with_label("Danza en puntas");
        assert_eq!(serde_json::from_str::<AudioSliceMarker>(&serde_json::to_string(&named).unwrap()).unwrap(), named);
        let mut t = AudioTimeAndLoopsData { file_sample_rate: 1_000, end_time_sec: 10.0, ..Default::default() };
        t.slices = vec![named.clone()];
        assert_eq!(t.validate(), Ok(()));
        // El nombre no cambia la firma que decide si hay que recargar la voz.
        let sig = t.slices_signature();
        t.slices[0].label = "otro".into();
        assert_eq!(sig, t.slices_signature());
        t.slices[0].label = "x".repeat(MAX_SLICE_LABEL_CHARS + 1);
        assert_eq!(t.validate(), Err(DtoError::SliceLabelTooLong(10)));
    }

    #[test]
    fn play_count_selector_cycles() {
        let mut m = SliceMarker::new(10, 1);
        let seq: Vec<u32> = (0..4).map(|_| { m.play_count = m.next_play_count(); m.play_count }).collect();
        assert_eq!(seq, [2, INFINITE_SLICE_LOOP, 1, 2]);
        assert!(SliceMarker::new(0, SliceMarker::INFINITE).is_infinite());
    }

    #[test]
    fn slices_validation_and_signature() {
        let mut t = region();
        t.slices = vec![
            AudioSliceMarker::new(48_000, 2),
            AudioSliceMarker::new(96_000, INFINITE_SLICE_LOOP),
        ];
        assert!(t.validate().is_ok());
        let sig = t.slices_signature();
        t.slices[1].play_count = 3;
        assert_ne!(sig, t.slices_signature());
        t.slices[1].frame = 48_000;
        assert_eq!(t.validate(), Err(DtoError::SlicesNotIncreasing));
        t.slices[1].frame = 480_000; // == end
        assert_eq!(t.validate(), Err(DtoError::SliceOutsideRegion(480_000)));
        t.slices = (1..=64).map(|i| AudioSliceMarker::new(i * 100, 1)).collect();
        assert_eq!(t.validate(), Err(DtoError::TooManySlices(64)));
        t.slices.pop();
        assert!(t.validate().is_ok()); // 63 marcadores = 64 segmentos
    }

    #[test]
    fn time_validation() {
        let mut t = region();
        t.rate = 0.01;
        assert!(matches!(t.validate(), Err(DtoError::RateOutOfRange(_))));
        t.rate = 1.0;
        t.play_count = 0;
        assert_eq!(t.validate(), Err(DtoError::PlayCountZero));
        t.play_count = 1;
        t.fade_curve = vec![0.0; 10];
        assert_eq!(t.validate(), Err(DtoError::FadeCurveLen(10)));
        assert!(matches!(AudioTimeAndLoopsData::default().validate(), Err(DtoError::EmptyRegion { .. })));
    }

    #[test]
    fn basics_and_triggers() {
        let mut b = CueBasicsData::default();
        b.default_name = "kick.wav".into();
        assert_eq!(b.display_name(), "kick.wav");
        b.pre_wait = -1.0;
        assert!(b.validate().is_err());
        let mut t = CueTriggersData::default();
        assert!(t.validate().is_ok());
        t.wall_hms = (24, 0, 0);
        assert_eq!(t.validate(), Err(DtoError::BadWallClock));
    }

    #[test]
    fn json_roundtrip_skips_temp_duration() {
        let mut dto = AudioCueDto {
            basics: CueBasicsData::default(),
            triggers: CueTriggersData::default(),
            time_loops: region(),
            levels: AudioLevelMatrixData::new_one_to_one(2, 2).unwrap(),
        };
        dto.basics.temp_duration = Some(3.0);
        dto.triggers.second_trigger_action = SecondTriggerAction::Devamp;
        let json = serde_json::to_string(&dto).unwrap();
        let back: AudioCueDto = serde_json::from_str(&json).unwrap();
        assert_eq!(back.basics.temp_duration, None);
        dto.basics.temp_duration = None;
        assert_eq!(dto, back);
        assert!(back.validate().is_ok());
    }
    #[test]
    fn a_start_override_moves_the_start_inside_the_region_and_drops_earlier_slices() {
        let mut t = AudioTimeAndLoopsData { file_sample_rate: 1_000, end_time_sec: 10.0, ..Default::default() };
        t.slices = vec![AudioSliceMarker::new(2_000, 2), AudioSliceMarker::new(6_000, INFINITE_SLICE_LOOP)];
        assert!(matches!(t.playback_view(), std::borrow::Cow::Borrowed(_)), "sin arranque no se copia");
        t.start_override_secs = Some(4.0);
        let v = t.playback_view();
        assert_eq!((v.start_time_sec, v.slices.len(), v.slices[0].frame), (4.0, 1, 6_000));
        assert_eq!(t.start_time_sec, 0.0, "los datos guardados no cambian");
        for bad in [Some(0.0), Some(-1.0), Some(10.0), Some(99.0), Some(f64::NAN), None] {
            t.start_override_secs = bad;
            assert!(matches!(t.playback_view(), std::borrow::Cow::Borrowed(_)), "{bad:?} se ignora");
        }
    }

}

// ---------------------------------------------------------------------------------------------
// Botonera de Carts (efectos rápidos)
// ---------------------------------------------------------------------------------------------

/// Cantidad de pads de la botonera (cuadrícula de 4×4).
pub const CART_PADS: usize = 16;
pub const CART_MIN_GAIN_DB: f32 = -60.0;
pub const CART_MAX_GAIN_DB: f32 = 12.0;

/// Cómo responde un pad a la pulsación.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CartMode {
    /// Dispara y, si ya suena, reinicia desde el principio.
    #[default]
    Retrigger,
    /// Alterna: la primera pulsación lo dispara y la siguiente lo detiene.
    Toggle,
    /// Suena mientras se mantiene pulsado.
    Hold,
}

impl CartMode {
    pub const ALL: [Self; 3] = [Self::Retrigger, Self::Toggle, Self::Hold];

    pub fn label(self) -> &'static str {
        match self {
            Self::Retrigger => "Disparar y reiniciar",
            Self::Toggle => "Alternar Play/Stop",
            Self::Hold => "Pulsar para reproducir",
        }
    }
}

/// Un pad: un efecto de audio con su color, volumen, atajo y modo. Sin `path` el pad está vacío.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CartPad {
    pub path: String,
    pub name: String,
    pub color: CueColor,
    pub gain_db: f32,
    /// Atajo de teclado: una tecla del teclado (`1`…`9`, `A`…`Z`, `F1`…`F12`); vacío = sin atajo.
    pub key: String,
    pub mode: CartMode,
}

impl Default for CartPad {
    fn default() -> Self {
        Self { path: String::new(), name: String::new(), color: CueColor::None, gain_db: 0.0, key: String::new(), mode: CartMode::default() }
    }
}

impl CartPad {
    pub fn is_empty(&self) -> bool {
        self.path.trim().is_empty()
    }

    /// Texto que se ve en el pad: el nombre, o el del archivo.
    pub fn title(&self) -> String {
        if !self.name.trim().is_empty() {
            return self.name.trim().to_string();
        }
        self.path.rsplit(['/', '\\']).next().unwrap_or("").rsplit_once('.').map_or_else(|| self.path.clone(), |(stem, _)| stem.to_string())
    }
}

/// Los pads de la botonera, guardados dentro del show.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CartBank {
    pub pads: Vec<CartPad>,
}

impl CartBank {
    /// Sin ningún pad con archivo no hace falta guardar nada.
    pub fn is_empty(&self) -> bool {
        self.pads.iter().all(CartPad::is_empty)
    }

    /// Exactamente [`CART_PADS`] pads, con volumen y atajo en rango (un JSON editado a mano no rompe la botonera).
    pub fn clamped(mut self) -> Self {
        self.pads.truncate(CART_PADS);
        self.pads.resize_with(CART_PADS, CartPad::default);
        for p in &mut self.pads {
            p.gain_db = if p.gain_db.is_finite() { p.gain_db.clamp(CART_MIN_GAIN_DB, CART_MAX_GAIN_DB) } else { 0.0 };
            p.key = p.key.trim().chars().take(4).collect::<String>().to_uppercase();
        }
        self
    }
}
