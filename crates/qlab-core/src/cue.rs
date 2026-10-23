//! Modelo de cue: `AudioCue`, `FadeCue` y `GroupCue` sobre una base común (spec §1.2 y §1.4).
//!
//! Los datos editables son los DTOs de `qlab-protocol`; aquí se añade el estado de ejecución
//! (`CueMachine`) y la semántica de cada tipo.

use qlab_engine::rt_command::FadeMode;
use qlab_protocol::show_format::VideoFit;
use qlab_protocol::cue_dto::{AudioTimeAndLoopsData, ContinueMode, CueBasicsData, CueId, CueTriggersData};
use qlab_protocol::matrix::AudioLevelMatrixData;

use crate::clock::Timebase;
use crate::clock::Frames;
use crate::cue_state::{ActionEnd, CueMachine, CueState, CueTiming, SliceCursor, OPEN_END};
use crate::fade_processor::FadeCurve;

// ---------------------------------------------------------------------------------------------
// Datos por tipo
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct AudioCueData {
    pub time_loops: AudioTimeAndLoopsData,
    pub levels: AudioLevelMatrixData,
    /// Ecualizador paramétrico de 4 bandas (apagado por defecto).
    pub eq: qlab_protocol::cue_dto::AudioEqData,
}

/// Parámetros del motor (copiables, tamaño fijo) a partir de la configuración de la cue. Se corrigen a rango.
pub fn eq_params(eq: &qlab_protocol::cue_dto::AudioEqData) -> qlab_engine::biquad::EqParams {
    use qlab_engine::biquad::{BandParams, FilterKind};
    use qlab_protocol::cue_dto::EqFilterType as T;
    let eq = eq.clamped();
    let mut p = qlab_engine::biquad::EqParams::OFF;
    p.enabled = eq.enabled;
    for (dst, b) in p.bands.iter_mut().zip(eq.bands.iter()) {
        *dst = BandParams {
            kind: match b.filter_type {
                T::LowCut => FilterKind::LowCut,
                T::LowShelf => FilterKind::LowShelf,
                T::Peaking => FilterKind::Peaking,
                T::HighShelf => FilterKind::HighShelf,
                T::HighCut => FilterKind::HighCut,
            },
            freq_hz: b.frequency_hz,
            gain_db: b.gain_db,
            q: b.q_factor,
            enabled: b.enabled,
        };
    }
    p
}

impl AudioCueData {
    pub fn with_eq(mut self, eq: qlab_protocol::cue_dto::AudioEqData) -> Self {
        self.eq = eq.clamped();
        self
    }

    pub fn new(time_loops: AudioTimeAndLoopsData, levels: AudioLevelMatrixData) -> Self {
        Self { time_loops, levels, eq: Default::default() }
    }

    /// Duración estimada: `(fin − inicio) / rate × repeticiones`. `None` si hay bucle infinito o la
    /// región no está definida. La duración real la decide el motor al terminar la voz.
    pub fn estimated_duration_secs(&self) -> Option<f64> {
        let view = self.time_loops.playback_view();
        let t = &*view;
        if t.infinite_loop {
            return None;
        }
        let span = t.end_time_sec - t.start_time_sec;
        if !(span > 0.0) || !(t.rate > 0.0) {
            return None;
        }
        if !t.slices.is_empty() {
            // Con slices: [inicio, s1) una vez y cada tramo `play_count` veces; un tramo infinito no tiene fin.
            let cursor = self.slice_cursor()?;
            let mut frames: u128 = 0;
            for s in cursor.spans() {
                if s.play_count == crate::cue_state::SLICE_INFINITE {
                    return None;
                }
                frames += (s.end - s.start) as u128 * s.play_count as u128;
            }
            return Some(frames as f64 / t.file_sample_rate.max(1) as f64 / t.rate as f64);
        }
        Some(span / t.rate as f64 * t.play_count.max(1) as f64)
    }

    /// Programa de slices de la cue (posición inicial). `None` si los marcadores no son válidos.
    pub fn slice_cursor(&self) -> Option<SliceCursor> {
        let view = self.time_loops.playback_view();
        let t = &*view;
        let sr = t.file_sample_rate.max(1) as f64;
        let start = (t.start_time_sec.max(0.0) * sr).round() as u64;
        let end = if t.end_time_sec > 0.0 { (t.end_time_sec * sr).round() as u64 } else { OPEN_END };
        let markers: Vec<(u64, u32)> = t.slices.iter().map(|m| (m.frame, m.play_count)).collect();
        SliceCursor::new(start, end, &markers, t.play_count, t.infinite_loop, false).ok()
    }
}

/// Celda de un `FadeCue`: `(row, col)` con 0 = master/trim, como en `AudioCommand::SetCrosspointDb`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FadeCell {
    pub row: u8,
    pub col: u8,
    pub target_db: f32,
    pub active: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FadeCueData {
    /// Cue de audio que se modula (`cue_target_id` del Inspector).
    pub target: Option<CueId>,
    pub mode: FadeMode,
    pub cells: Vec<FadeCell>,
    pub curve: FadeCurve,
    pub stop_target_when_done: bool,
    pub honor_gangs: bool,
}

impl FadeCueData {
    pub fn new(target: CueId) -> Self {
        Self {
            target: Some(target),
            mode: FadeMode::Absolute,
            cells: Vec::new(),
            curve: FadeCurve::default(),
            stop_target_when_done: false,
            honor_gangs: true,
        }
    }
}

/// Modos de ejecución de un grupo.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroupMode {
    /// Dispara todos los hijos armados a la vez; cada uno respeta su pre-wait como desfase dentro
    /// de la línea de tiempo del grupo. Dura `max(pre + acción + post)` de sus hijos.
    Timeline,
    /// Dispara solo el primer hijo armado y el playhead pasa a la cue siguiente **fuera** del
    /// grupo. Los demás hijos suenan solo si se encadenan con `AutoContinue`/`AutoFollow`.
    FireFirstGoToNext,
    /// Dispara todos los hijos armados en el mismo instante, ignorando sus pre-waits.
    FireAllSimultaneously,
}

#[derive(Clone, Debug, PartialEq)]
pub struct GroupCueData {
    pub mode: GroupMode,
    /// Hijos en orden. Existen en la `CueList` pero no figuran en su orden de nivel superior.
    pub children: Vec<CueId>,
}

impl GroupCueData {
    pub fn new(mode: GroupMode, children: Vec<CueId>) -> Self {
        Self { mode, children }
    }
}

/// Cue de vídeo: su pista de audio suena por el motor igual que una cue de audio (mismos tiempos, bucles,
/// slices y niveles) y la imagen la presenta la interfaz sincronizada con ese audio.
#[derive(Clone, Debug, PartialEq)]
pub struct VideoCueData {
    pub audio: AudioCueData,
    pub fit: VideoFit,
}

/// Qué hace una cue de control sobre su objetivo.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlAction {
    /// Detiene el objetivo (con fundido de salida de `action_duration` segundos).
    Stop,
    Pause,
    Resume,
    /// Saca la cue de audio de su slice infinito al final de la pasada en curso.
    Devamp,
    /// Mueve el playhead a la cue destino (la siguiente que dispare GO).
    GoTo,
    /// Precarga el audio de la cue destino para que arranque sin demora.
    Load,
    /// Arma la cue destino (un audio o vídeo sin archivo asignado no se puede armar).
    Arm,
    /// Desarma la cue destino: GO la salta o se detiene en ella según su opción.
    Disarm,
}

/// Cue de control: actúa sobre otra cue (`cue_target_id`) en el momento de dispararse.
#[derive(Clone, Debug, PartialEq)]
pub struct ControlCueData {
    pub target: Option<CueId>,
    pub action: ControlAction,
}

/// Cue de red: al dispararse envía un mensaje OSC por UDP a otro equipo (consola de luces, otro TeatroCue…).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OscCueData {
    /// `host:puerto`, p. ej. `192.168.0.20:8000`.
    pub destination: String,
    /// Dirección y argumentos en una línea, p. ej. `/cue/1/start` o `/fader 1 0.5 "hola"`.
    pub message: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum CueKind {
    Audio(AudioCueData),
    Video(VideoCueData),
    Fade(FadeCueData),
    Control(ControlCueData),
    Osc(OscCueData),
    /// Nota o espera: no hace nada, solo dura `action_duration` (útil con auto-continuar).
    Memo,
    Group(GroupCueData),
}

// ---------------------------------------------------------------------------------------------
// Cue
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Cue {
    pub basics: CueBasicsData,
    /// Pestaña Triggers. Se guarda y se edita; el motor aún no ejecuta hotkeys, MIDI, reloj ni
    /// fade/duck de otras cues.
    pub triggers: CueTriggersData,
    pub kind: CueKind,
    /// Cue omitida por el playhead (borrada lógicamente).
    pub dead: bool,
    /// Se pidió un Stop y se espera a que el motor confirme el fin de la voz.
    pub stopping: bool,
    pub(crate) machine: CueMachine,
    /// Hijos disparados por este grupo en la ejecución actual.
    pub(crate) group_running: Vec<CueId>,
    /// El grupo está disparando a sus hijos: no puede darse por terminado todavía.
    pub(crate) group_starting: bool,
    /// Seguimiento de la posición de slices de una cue de audio en marcha (ver `slice_cursor_at`).
    pub(crate) slice_track: Option<SliceTrack>,
}

/// Cursor de slices más los frames de origen ya consumidos.
#[derive(Clone, Debug)]
pub(crate) struct SliceTrack {
    pub cursor: SliceCursor,
    pub synced_src: u64,
}

impl Cue {
    pub fn new(basics: CueBasicsData, kind: CueKind) -> Self {
        Self { basics, triggers: CueTriggersData::default(), kind, dead: false, stopping: false, machine: CueMachine::new(), group_running: Vec::new(), group_starting: false, slice_track: None }
    }

    pub fn audio(basics: CueBasicsData, data: AudioCueData) -> Self {
        Self::new(basics, CueKind::Audio(data))
    }

    pub fn video(basics: CueBasicsData, data: VideoCueData) -> Self {
        Self::new(basics, CueKind::Video(data))
    }

    pub fn osc(basics: CueBasicsData, data: OscCueData) -> Self {
        Self::new(basics, CueKind::Osc(data))
    }

    pub fn memo(basics: CueBasicsData) -> Self {
        Self::new(basics, CueKind::Memo)
    }

    pub fn control(basics: CueBasicsData, data: ControlCueData) -> Self {
        Self::new(basics, CueKind::Control(data))
    }

    pub fn fade(basics: CueBasicsData, data: FadeCueData) -> Self {
        Self::new(basics, CueKind::Fade(data))
    }

    pub fn group(basics: CueBasicsData, data: GroupCueData) -> Self {
        Self::new(basics, CueKind::Group(data))
    }

    #[inline]
    pub fn id(&self) -> CueId {
        self.basics.unique_id
    }

    #[inline]
    pub fn state(&self) -> CueState {
        self.machine.state()
    }

    pub fn machine(&self) -> &CueMachine {
        &self.machine
    }

    pub fn continue_mode(&self) -> ContinueMode {
        self.basics.continue_mode
    }

    /// Post-wait efectivo: `AutoFollow` y `DoNotContinue` lo anulan.
    pub fn effective_post_wait_secs(&self) -> f64 {
        if self.basics.continue_mode == ContinueMode::AutoContinue { self.basics.post_wait } else { 0.0 }
    }

    pub(crate) fn action_end(&self) -> ActionEnd {
        match self.kind {
            CueKind::Fade(_) | CueKind::Control(_) | CueKind::Osc(_) | CueKind::Memo => ActionEnd::Timed,
            CueKind::Audio(_) | CueKind::Video(_) | CueKind::Group(_) => ActionEnd::External,
        }
    }

    /// Tiempos de la cue en frames. La duración de un audio es estimada (la acción termina cuando
    /// el motor lo notifica); la de un grupo se calcula en la `CueList`.
    pub fn timing(&self, tb: Timebase) -> CueTiming {
        let action = match &self.kind {
            CueKind::Audio(a) => a.estimated_duration_secs(),
            CueKind::Video(v) => v.audio.estimated_duration_secs(),
            CueKind::Fade(_) | CueKind::Control(_) | CueKind::Osc(_) | CueKind::Memo => Some(self.basics.action_duration),
            CueKind::Group(_) => None,
        };
        CueTiming::from_secs(tb, self.basics.pre_wait, action, self.effective_post_wait_secs())
    }

    /// Cursor de slices de la cue puesto al día hasta `now` (frames del show). Se crea al pedirlo por
    /// primera vez desde el inicio de la acción y se avanza con los frames de origen transcurridos
    /// (`transcurrido · rate · fs_archivo / fs_show`). `None` si no es audio o sus marcadores son inválidos.
    pub fn slice_cursor_at(&mut self, tb: Timebase, now: Frames) -> Option<&mut SliceCursor> {
        let a = match &self.kind {
            CueKind::Audio(a) => a,
            CueKind::Video(v) => &v.audio,
            _ => return None,
        };
        if self.slice_track.is_none() {
            self.slice_track = Some(SliceTrack { cursor: a.slice_cursor()?, synced_src: 0 });
        }
        let t = &a.time_loops;
        let ratio = t.rate.max(f32::MIN_POSITIVE) as f64 * t.file_sample_rate.max(1) as f64 / tb.sample_rate() as f64;
        let total_src = (self.machine.action_elapsed(now) as f64 * ratio) as u64;
        let track = self.slice_track.as_mut()?;
        let delta = total_src.saturating_sub(track.synced_src);
        track.synced_src = total_src.max(track.synced_src);
        track.cursor.advance(delta, &mut Vec::new());
        Some(&mut track.cursor)
    }

    /// Datos de audio de una cue que suena por el motor: una cue de audio o la pista de audio de un vídeo.
    pub fn audio_data(&self) -> Option<&AudioCueData> {
        match &self.kind {
            CueKind::Audio(a) => Some(a),
            CueKind::Video(v) => Some(&v.audio),
            _ => None,
        }
    }

    pub fn audio_data_mut(&mut self) -> Option<&mut AudioCueData> {
        match &mut self.kind {
            CueKind::Audio(a) => Some(a),
            CueKind::Video(v) => Some(&mut v.audio),
            _ => None,
        }
    }

    /// ¿Tiene una voz en el motor? (audio y vídeo)
    pub fn is_audio(&self) -> bool {
        self.audio_data().is_some()
    }

    pub fn is_video(&self) -> bool {
        matches!(self.kind, CueKind::Video(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qlab_protocol::matrix::AudioLevelMatrixData;

    pub(crate) fn basics(id: u64, number: &str) -> CueBasicsData {
        CueBasicsData { unique_id: CueId(id), number: number.into(), name: format!("cue {number}"), ..Default::default() }
    }

    fn audio_data(start: f64, end: f64) -> AudioCueData {
        AudioCueData::new(
            AudioTimeAndLoopsData { start_time_sec: start, end_time_sec: end, ..Default::default() },
            AudioLevelMatrixData::new_one_to_one(2, 2).unwrap(),
        )
    }

    #[test]
    fn estimated_duration() {
        let mut a = audio_data(1.0, 5.0);
        assert_eq!(a.estimated_duration_secs(), Some(4.0));
        a.time_loops.rate = 2.0;
        assert_eq!(a.estimated_duration_secs(), Some(2.0));
        a.time_loops.play_count = 3;
        assert_eq!(a.estimated_duration_secs(), Some(6.0));
        a.time_loops.infinite_loop = true;
        assert_eq!(a.estimated_duration_secs(), None);
        assert_eq!(audio_data(0.0, 0.0).estimated_duration_secs(), None);
    }

    #[test]
    fn effective_post_wait_follows_continue_mode() {
        let mut b = basics(1, "1");
        b.post_wait = 2.5;
        let mut c = Cue::audio(b, audio_data(0.0, 1.0));
        assert_eq!(c.effective_post_wait_secs(), 0.0);
        c.basics.continue_mode = ContinueMode::AutoContinue;
        assert_eq!(c.effective_post_wait_secs(), 2.5);
        c.basics.continue_mode = ContinueMode::AutoFollow;
        assert_eq!(c.effective_post_wait_secs(), 0.0);
    }

    #[test]
    fn timing_per_kind() {
        let tb = Timebase::new(48_000);
        let mut b = basics(1, "1");
        b.pre_wait = 0.5;
        b.post_wait = 1.0;
        b.continue_mode = ContinueMode::AutoContinue;
        let audio = Cue::audio(b.clone(), audio_data(0.0, 2.0));
        assert_eq!(audio.timing(tb), CueTiming { pre_wait: 24_000, action: Some(96_000), post_wait: 48_000 });
        assert_eq!(audio.action_end(), ActionEnd::External);

        b.action_duration = 3.0;
        let fade = Cue::fade(b.clone(), FadeCueData::new(CueId(9)));
        assert_eq!(fade.timing(tb).action, Some(144_000));
        assert_eq!(fade.action_end(), ActionEnd::Timed);

        let group = Cue::group(b, GroupCueData::new(GroupMode::Timeline, vec![]));
        assert_eq!(group.timing(tb).action, None);
        assert!(!group.is_audio());
    }

    #[test]
    fn constructors_expose_ids_and_state() {
        let c = Cue::audio(basics(42, "7"), audio_data(0.0, 1.0));
        assert_eq!(c.id(), CueId(42));
        assert_eq!(c.state(), CueState::Idle);
        assert!(!c.dead && !c.stopping);
    }
}
