//! Estado de la UI que no depende de egui: filas de la tabla, selección, telemetría y vúmetros.

use std::collections::{HashMap, HashSet};

use qlab_protocol::ipc_msg::CueSummary;
use qlab_protocol::ipc_shm::{CuePhase, SharedTelemetryShm, ShowStatus, MAX_METER_CHANNELS, SLICE_DEVAMPING, SLICE_PROGRAM, SLICE_VAMPING};

/// Estado de una cue, tal como lo informa el daemon.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CueUiState {
    Idle,
    Armed,
    PreWaiting,
    Running,
    PostWaiting,
    Paused,
    Completed,
}

impl CueUiState {
    pub fn parse(s: &str) -> Self {
        match s {
            "Armed" => Self::Armed,
            "PreWaiting" => Self::PreWaiting,
            "Running" => Self::Running,
            "PostWaiting" => Self::PostWaiting,
            "Paused" => Self::Paused,
            "Completed" => Self::Completed,
            _ => Self::Idle,
        }
    }

    /// Está en marcha (se pinta destacada).
    pub fn is_active(self) -> bool {
        matches!(self, Self::PreWaiting | Self::Running | Self::PostWaiting | Self::Paused)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Row {
    /// Índice en `UiModel::summaries`.
    pub index: usize,
    pub depth: u8,
    pub has_children: bool,
}

#[derive(Default)]
pub struct UiModel {
    pub summaries: Vec<CueSummary>,
    rows: Vec<Row>,
    pub collapsed: HashSet<u64>,
    pub selected: Option<u64>,
    pub playhead: Option<u64>,
    pub show_name: String,
}

impl UiModel {
    pub fn set_summaries(&mut self, summaries: Vec<CueSummary>) {
        self.summaries = summaries;
        self.rebuild_rows();
        if let Some(sel) = self.selected {
            if self.summary(sel).is_none() {
                self.selected = None;
            }
        }
    }

    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    pub fn summary(&self, id: u64) -> Option<&CueSummary> {
        self.summaries.iter().find(|c| c.id == id)
    }

    pub fn state_of(&self, id: u64) -> CueUiState {
        self.summary(id).map_or(CueUiState::Idle, |c| CueUiState::parse(&c.state))
    }

    pub fn set_state(&mut self, id: u64, state: &str) {
        if let Some(c) = self.summaries.iter_mut().find(|c| c.id == id) {
            c.state = state.to_string();
        }
    }

    /// ¿Hay alguna cue sonando o contando tiempo? (las pausadas y terminadas no animan nada)
    pub fn any_running(&self) -> bool {
        self.summaries.iter().any(|c| matches!(CueUiState::parse(&c.state), CueUiState::Running | CueUiState::PreWaiting | CueUiState::PostWaiting))
    }

    pub fn toggle_collapsed(&mut self, id: u64) {
        if !self.collapsed.remove(&id) {
            self.collapsed.insert(id);
        }
        self.rebuild_rows();
    }

    fn rebuild_rows(&mut self) {
        let parents: HashMap<u64, Option<u64>> = self.summaries.iter().map(|c| (c.id, c.parent)).collect();
        let has_children: HashSet<u64> = self.summaries.iter().filter_map(|c| c.parent).collect();
        let hidden = |mut parent: Option<u64>| {
            // Un hijo se oculta si cualquiera de sus ancestros está contraído.
            let mut guard = 0;
            while let Some(p) = parent {
                if self.collapsed.contains(&p) {
                    return true;
                }
                parent = parents.get(&p).copied().flatten();
                guard += 1;
                if guard > 16 {
                    break;
                }
            }
            false
        };
        let depth_of = |id: u64| {
            let (mut d, mut cur) = (0u8, parents.get(&id).copied().flatten());
            while let Some(p) = cur {
                d += 1;
                cur = parents.get(&p).copied().flatten();
                if d > 16 {
                    break;
                }
            }
            d
        };
        self.rows = self
            .summaries
            .iter()
            .enumerate()
            .filter(|(_, c)| !hidden(c.parent))
            .map(|(index, c)| Row { index, depth: depth_of(c.id), has_children: has_children.contains(&c.id) })
            .collect();
    }

    /// Siguiente cue de nivel superior tras `id` (el destino del playhead tras un GO).
    pub fn next_top_level_after(&self, id: u64) -> Option<u64> {
        let top: Vec<u64> = self.summaries.iter().filter(|c| c.parent.is_none()).map(|c| c.id).collect();
        let anchor = self.summary(id).map(|c| c.parent.map_or(id, |_| self.top_ancestor(id)))?;
        let i = top.iter().position(|&c| c == anchor)?;
        top.get(i + 1).copied()
    }

    fn top_ancestor(&self, mut id: u64) -> u64 {
        for _ in 0..16 {
            match self.summary(id).and_then(|c| c.parent) {
                Some(p) => id = p,
                None => break,
            }
        }
        id
    }

    /// Posición `(índice, total)` de una cue entre sus hermanas (las que comparten padre), en el orden de la lista.
    pub fn sibling_position(&self, id: u64) -> Option<(usize, usize)> {
        let parent = self.summary(id)?.parent;
        let siblings: Vec<u64> = self.summaries.iter().filter(|c| c.parent == parent).map(|c| c.id).collect();
        Some((siblings.iter().position(|&s| s == id)?, siblings.len()))
    }

    /// Mueve la selección `delta` filas dentro de las visibles (sin salirse de la lista).
    pub fn select_relative(&mut self, delta: isize) -> Option<u64> {
        if self.rows.is_empty() {
            return None;
        }
        let ids: Vec<u64> = self.rows.iter().map(|r| self.summaries[r.index].id).collect();
        let current = self.selected.and_then(|s| ids.iter().position(|&i| i == s));
        let next = match current {
            None => if delta >= 0 { 0 } else { ids.len() - 1 },
            Some(i) => (i as isize + delta).clamp(0, ids.len() as isize - 1) as usize,
        };
        self.selected = Some(ids[next]);
        self.selected
    }
}

// ---------------------------------------------------------------------------------------------
// Telemetría y vúmetros
// ---------------------------------------------------------------------------------------------

/// Caída del vúmetro en dB/s.
const METER_FALL_DB_PER_SEC: f32 = 24.0;
/// Tiempo que se mantiene el marcador de pico.
const PEAK_HOLD_SECS: f32 = 1.5;
const CLIP_HOLD_SECS: f32 = 2.0;
/// Por debajo de esto un vúmetro se considera apagado (no pide repintado).
pub const METER_FLOOR_DB: f32 = -72.0;

pub fn lin_to_db(x: f32) -> f32 {
    if x <= 1e-6 { -120.0 } else { 20.0 * x.log10() }
}

#[derive(Clone, Copy, Debug)]
pub struct MeterState {
    pub level_db: f32,
    pub hold_db: f32,
    hold_left: f32,
    pub clip_left: f32,
}

impl Default for MeterState {
    fn default() -> Self {
        Self { level_db: -120.0, hold_db: -120.0, hold_left: 0.0, clip_left: 0.0 }
    }
}

impl MeterState {
    /// Incorpora el pico (lineal) consumido en este fotograma y avanza `dt` segundos.
    pub fn push(&mut self, peak_linear: f32, dt: f32) {
        let db = lin_to_db(peak_linear);
        self.level_db = db.max(self.level_db - METER_FALL_DB_PER_SEC * dt).max(-120.0);
        self.hold_left = (self.hold_left - dt).max(0.0);
        if db >= self.hold_db || self.hold_left == 0.0 {
            if db >= self.hold_db {
                self.hold_db = db;
                self.hold_left = PEAK_HOLD_SECS;
            } else {
                self.hold_db = (self.hold_db - METER_FALL_DB_PER_SEC * 0.6 * dt).max(self.level_db);
            }
        }
        self.clip_left = if peak_linear >= 1.0 { CLIP_HOLD_SECS } else { (self.clip_left - dt).max(0.0) };
    }

    pub fn is_visible(&self) -> bool {
        self.level_db > METER_FLOOR_DB || self.hold_db > METER_FLOOR_DB || self.clip_left > 0.0
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RunningInfo {
    pub phase_idle: bool,
    pub pre_wait_elapsed: f32,
    pub action_elapsed: f32,
    pub post_wait_elapsed: f32,
    pub phase: Option<CueUiPhase>,
    /// Tramo actual de una cue con slices o bucle (`None` si no tiene programa).
    pub slice: Option<u16>,
    /// Atrapada en un tramo infinito hasta recibir Devamp.
    pub vamping: bool,
    /// Devamp pedido: acaba la pasada en curso y sigue al tramo siguiente.
    pub devamping: bool,
    /// Frame real del archivo que lee el motor (retrocede en cada vuelta de un bucle).
    pub source_frame: u64,
}

/// Rótulo de la columna de estado para una cue con programa de slices.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SliceTag {
    /// Atrapada en un bucle infinito (parpadea).
    Vamp,
    /// Devamp en curso (fijo).
    Devamping,
}

impl SliceTag {
    pub fn label(self) -> &'static str {
        match self {
            Self::Vamp => "VAMP",
            Self::Devamping => "DEVAMP",
        }
    }
}

impl RunningInfo {
    /// Segundos del archivo donde está el cursor de reproducción. Con la posición real del motor
    /// (`source_frame`), el cursor vuelve al inicio del tramo en cada vuelta del bucle; si el daemon no la
    /// publica (aún no hay lectura), se cae a inicio + tiempo transcurrido × velocidad.
    pub fn cursor_secs(&self, file_sample_rate: u32, start_time_sec: f64, rate: f32) -> f64 {
        if self.source_frame > 0 {
            self.source_frame as f64 / file_sample_rate.max(1) as f64
        } else {
            start_time_sec + self.action_elapsed as f64 * rate as f64
        }
    }

    /// Qué rótulo mostrar: solo mientras la cue suena (no en pre-wait ni al terminar). Un Devamp
    /// pedido tiene prioridad sobre el bucle: el indicador cambia en cuanto se emite.
    pub fn slice_tag(&self) -> Option<SliceTag> {
        if self.phase != Some(CueUiPhase::Action) {
            return None;
        }
        if self.devamping {
            Some(SliceTag::Devamping)
        } else if self.vamping {
            Some(SliceTag::Vamp)
        } else {
            None
        }
    }
}

/// Parpadeo a 2 Hz derivado del reloj de egui.
pub fn blink_on(time_secs: f64) -> bool {
    (time_secs * 2.0).floor() as i64 % 2 == 0
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CueUiPhase {
    PreWait,
    Action,
    PostWait,
    TailingOut,
}

pub struct Telemetry {
    pub status: Option<ShowStatus>,
    pub meters: Vec<MeterState>,
    pub running: HashMap<u64, RunningInfo>,
    /// Nivel suavizado (dBFS) del vúmetro de cada cue que suena, con caída; ver [`Telemetry::cue_level_db`].
    cue_levels: HashMap<u64, f32>,
    /// Pico máximo (lineal) de cada cue desde el último fotograma (el daemon publica más a menudo que la UI).
    cue_peaks: HashMap<u64, f32>,
    last_version: u64,
}

/// Caída del vúmetro de una cue (dB por segundo).
pub const CUE_METER_FALL_DB_PER_SEC: f32 = 36.0;

impl Default for Telemetry {
    fn default() -> Self {
        Self { status: None, meters: vec![MeterState::default(); MAX_METER_CHANNELS], running: HashMap::new(), cue_levels: HashMap::new(), cue_peaks: HashMap::new(), last_version: 0 }
    }
}

impl Telemetry {
    pub fn playhead(&self) -> Option<u64> {
        self.status.as_ref().map(|s| s.active_playhead_cue_id as u64).filter(|&id| id != 0)
    }

    pub fn is_playing(&self) -> bool {
        self.status.as_ref().is_some_and(|s| s.is_playing)
    }

    pub fn xruns(&self) -> u64 {
        self.status.as_ref().map_or(0, |s| s.total_xruns)
    }

    pub fn cpu_permille(&self) -> u32 {
        self.status.as_ref().map_or(0, |s| s.callback_cpu_load_permille)
    }

    pub fn frozen(&self) -> bool {
        self.status.as_ref().is_some_and(|s| s.audio_freeze_detected)
    }

    /// Lee el seqlock y consume los medidores. Devuelve `true` si hay actividad que seguir dibujando
    /// (reproducción en curso o algún vúmetro todavía visible). `channels` limita los medidores usados.
    pub fn update(&mut self, shm: &SharedTelemetryShm, dt: f32, channels: usize) -> bool {
        let version = shm.status.version();
        if version != self.last_version || self.status.is_none() {
            if let Some(s) = shm.status.snapshot() {
                self.running.clear();
                for c in s.running_cues.iter().filter(|c| c.phase != CuePhase::Idle) {
                    self.running.insert(
                        c.cue_id,
                        RunningInfo {
                            phase_idle: false,
                            pre_wait_elapsed: c.pre_wait_elapsed_sec,
                            action_elapsed: c.action_elapsed_sec,
                            post_wait_elapsed: c.post_wait_elapsed_sec,
                            phase: Some(match c.phase {
                                CuePhase::PreWait => CueUiPhase::PreWait,
                                CuePhase::Action => CueUiPhase::Action,
                                CuePhase::PostWait => CueUiPhase::PostWait,
                                _ => CueUiPhase::TailingOut,
                            }),
                            slice: (c.slice_flags & SLICE_PROGRAM != 0).then_some(c.current_slice_idx),
                            vamping: c.slice_flags & SLICE_VAMPING != 0,
                            devamping: c.slice_flags & SLICE_DEVAMPING != 0,
                            source_frame: c.source_frame,
                        },
                    );
                }
                for c in s.running_cues.iter().filter(|c| c.phase != CuePhase::Idle && c.peak > 0.0) {
                    let p = self.cue_peaks.entry(c.cue_id).or_insert(0.0);
                    *p = p.max(c.peak);
                }
                self.status = Some(s);
                self.last_version = version;
            }
        }
        let n = channels.clamp(1, MAX_METER_CHANNELS);
        for (ch, m) in self.meters.iter_mut().enumerate() {
            let peak = if ch < n { shm.meters.consume_output(ch) } else { 0.0 };
            m.push(peak, dt);
        }
        self.advance_cue_levels(dt);
        self.is_playing() || self.meters.iter().any(MeterState::is_visible) || !self.cue_levels.is_empty()
    }

    /// Un fotograma de los vúmetros por cue: sube al pico recibido y cae a `CUE_METER_FALL_DB_PER_SEC`.
    fn advance_cue_levels(&mut self, dt: f32) {
        let peaks = std::mem::take(&mut self.cue_peaks);
        for (&id, &p) in &peaks {
            let db = lin_to_db(p);
            let e = self.cue_levels.entry(id).or_insert(METER_FLOOR_DB);
            *e = e.max(db);
        }
        let fall = CUE_METER_FALL_DB_PER_SEC * dt;
        self.cue_levels.retain(|id, level| {
            if !peaks.contains_key(id) {
                *level -= fall;
            }
            *level > METER_FLOOR_DB
        });
    }

    /// Nivel (dBFS) del vúmetro de una cue, o `None` si no suena ni le queda cola visible.
    pub fn cue_level_db(&self, id: u64) -> Option<f32> {
        self.cue_levels.get(&id).copied()
    }

    /// Huella de lo que cambia el dibujo cuando el daemon está en reposo, para despertar a la UI.
    pub fn activity_signature(shm: &SharedTelemetryShm, channels: usize) -> (u64, u64, bool, bool) {
        let s = shm.status.snapshot();
        let (playhead, running_hash, playing) = match &s {
            Some(s) => {
                let h = s.running_cues.iter().filter(|c| c.phase != CuePhase::Idle).fold(0u64, |acc, c| {
                    acc.wrapping_mul(31).wrapping_add(c.cue_id).wrapping_mul(31).wrapping_add(c.phase as u64).wrapping_mul(31).wrapping_add(c.slice_flags as u64).wrapping_mul(31).wrapping_add(c.current_slice_idx as u64)
                });
                (s.active_playhead_cue_id as u64, h, s.is_playing)
            }
            None => (0, 0, false),
        };
        let meters_active = (0..channels.clamp(1, MAX_METER_CHANNELS)).any(|ch| lin_to_db(shm.meters.peek_output(ch)) > METER_FLOOR_DB);
        (playhead, running_hash, playing, meters_active)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use qlab_protocol::cue_dto::{ContinueMode, CueColor};

    pub(crate) fn summary(id: u64, parent: Option<u64>, state: &str) -> CueSummary {
        CueSummary {
            id,
            number: id.to_string(),
            name: format!("cue {id}"),
            kind: if id >= 100 { "group" } else { "audio" }.into(),
            state: state.into(),
            armed: true,
            parent,
            target: String::new(),
            pre_wait: 0.0,
            post_wait: 0.0,
            duration: None,
            continue_mode: ContinueMode::DoNotContinue,
            flagged: false,
            color: CueColor::None,
            dead: false,
            memories: vec![],
            file_sample_rate: 48_000,
            start_override_secs: None,
        }
    }

    fn model() -> UiModel {
        let mut m = UiModel::default();
        m.set_summaries(vec![
            summary(1, None, "Idle"),
            summary(100, None, "Running"),
            summary(11, Some(100), "Running"),
            summary(12, Some(100), "Idle"),
            summary(2, None, "Idle"),
        ]);
        m
    }

    #[test]
    fn rows_have_depth_and_collapse() {
        let mut m = model();
        let flat = |m: &UiModel| m.rows().iter().map(|r| (m.summaries[r.index].id, r.depth, r.has_children)).collect::<Vec<_>>();
        assert_eq!(flat(&m), vec![(1, 0, false), (100, 0, true), (11, 1, false), (12, 1, false), (2, 0, false)]);
        m.toggle_collapsed(100);
        assert_eq!(flat(&m), vec![(1, 0, false), (100, 0, true), (2, 0, false)]);
        m.toggle_collapsed(100);
        assert_eq!(m.rows().len(), 5);
    }

    #[test]
    fn nested_groups_hide_with_any_collapsed_ancestor() {
        let mut m = UiModel::default();
        m.set_summaries(vec![summary(100, None, "Idle"), summary(101, Some(100), "Idle"), summary(5, Some(101), "Idle")]);
        assert_eq!(m.rows().iter().map(|r| r.depth).collect::<Vec<_>>(), vec![0, 1, 2]);
        m.toggle_collapsed(100);
        assert_eq!(m.rows().len(), 1);
    }

    #[test]
    fn selection_moves_and_clamps_over_visible_rows() {
        let mut m = model();
        assert_eq!(m.select_relative(1), Some(1));
        assert_eq!(m.select_relative(1), Some(100));
        m.toggle_collapsed(100);
        assert_eq!(m.select_relative(1), Some(2), "salta los hijos ocultos");
        assert_eq!(m.select_relative(10), Some(2), "no se sale por abajo");
        assert_eq!(m.select_relative(-10), Some(1));
        let mut empty = UiModel::default();
        assert_eq!(empty.select_relative(1), None);
        // Una selección que desaparece de la lista se limpia.
        m.selected = Some(2);
        m.set_summaries(vec![summary(1, None, "Idle")]);
        assert_eq!(m.selected, None);
    }

    #[test]
    fn next_top_level_skips_group_children() {
        let m = model();
        assert_eq!(m.next_top_level_after(1), Some(100));
        assert_eq!(m.next_top_level_after(100), Some(2));
        assert_eq!(m.next_top_level_after(11), Some(2), "desde un hijo: tras su grupo");
        assert_eq!(m.next_top_level_after(2), None);
        assert_eq!(m.next_top_level_after(999), None);
    }

    #[test]
    fn states_parse() {
        for (s, st) in [("Running", CueUiState::Running), ("PreWaiting", CueUiState::PreWaiting), ("Paused", CueUiState::Paused), ("raro", CueUiState::Idle)] {
            assert_eq!(CueUiState::parse(s), st);
        }
        assert!(CueUiState::PostWaiting.is_active() && !CueUiState::Completed.is_active() && !CueUiState::Armed.is_active());
        let mut m = model();
        assert!(m.any_running(), "la cue 100 del modelo de prueba está sonando");
        m.set_state(100, "Paused");
        m.set_state(11, "Completed");
        assert!(!m.any_running(), "pausada o terminada no anima nada");
        m.set_state(1, "Running");
        assert_eq!(m.state_of(1), CueUiState::Running);
        assert_eq!(m.state_of(404), CueUiState::Idle);
    }

    #[test]
    fn meter_ballistics() {
        let mut m = MeterState::default();
        assert!(!m.is_visible());
        m.push(1.0, 1.0 / 60.0);
        assert_eq!(m.level_db, 0.0);
        assert!(m.clip_left > 1.9);
        // Con silencio, cae a ~24 dB/s.
        for _ in 0..60 {
            m.push(0.0, 1.0 / 60.0);
        }
        assert!((m.level_db - (-24.0)).abs() < 0.5, "{}", m.level_db);
        assert!(m.hold_db > m.level_db, "el pico se mantiene");
        // Termina apagándose del todo: no pide repintados para siempre.
        for _ in 0..(10 * 60) {
            m.push(0.0, 1.0 / 60.0);
        }
        assert!(!m.is_visible(), "{m:?}");
        // Un pico más bajo que el nivel actual no lo baja.
        let mut m = MeterState::default();
        m.push(0.5, 0.016);
        m.push(0.1, 0.016);
        assert!(m.level_db > lin_to_db(0.1));
        assert_eq!(lin_to_db(0.0), -120.0);
        assert!((lin_to_db(0.5) + 6.02).abs() < 0.01);
    }

    #[test]
    fn telemetry_follows_the_shared_page_and_goes_idle() {
        let shm = Box::new(SharedTelemetryShm::new());
        shm.initialize();
        let mut t = Telemetry::default();
        assert!(!t.update(&shm, 0.016, 2), "reposo: sin actividad");
        assert_eq!((t.playhead(), t.is_playing(), t.xruns()), (None, false, 0));

        let mut st = ShowStatus { active_playhead_cue_id: 7, is_playing: true, total_xruns: 2, callback_cpu_load_permille: 120, ..Default::default() };
        st.running_cues[0] = qlab_protocol::ipc_shm::RunningCueTelemetry {
            cue_id: 3,
            phase: CuePhase::Action,
            action_elapsed_sec: 1.25,
            ..Default::default()
        };
        shm.status.publish(&st);
        shm.meters.report_output(0, 0.5);
        assert!(t.update(&shm, 0.016, 2));
        assert_eq!((t.playhead(), t.xruns(), t.cpu_permille()), (Some(7), 2, 120));
        assert_eq!(t.running[&3].action_elapsed, 1.25);
        assert!((t.meters[0].level_db + 6.0).abs() < 0.1);
        assert_eq!(t.meters[1].level_db, -120.0);

        // Todo se detiene: tras decaer los vúmetros deja de haber actividad.
        shm.status.publish(&ShowStatus::default());
        let mut frames = 0;
        while t.update(&shm, 0.016, 2) {
            frames += 1;
            assert!(frames < 600, "los vúmetros no se apagan");
        }
        assert!(t.running.is_empty() && t.playhead().is_none());
    }

    #[test]
    fn activity_signature_changes_with_playhead_running_cues_and_meters() {
        let shm = Box::new(SharedTelemetryShm::new());
        shm.initialize();
        shm.status.publish(&ShowStatus::default());
        let idle = Telemetry::activity_signature(&shm, 2);
        // Publicar de nuevo el mismo contenido (el daemon lo hace a 120 Hz) no cambia la huella.
        shm.status.publish(&ShowStatus::default());
        assert_eq!(Telemetry::activity_signature(&shm, 2), idle);
        let mut st = ShowStatus { active_playhead_cue_id: 4, ..Default::default() };
        shm.status.publish(&st);
        assert_ne!(Telemetry::activity_signature(&shm, 2), idle);
        st.is_playing = true;
        st.running_cues[0].cue_id = 4;
        st.running_cues[0].phase = CuePhase::PreWait;
        shm.status.publish(&st);
        let waiting = Telemetry::activity_signature(&shm, 2);
        st.running_cues[0].phase = CuePhase::Action;
        shm.status.publish(&st);
        assert_ne!(Telemetry::activity_signature(&shm, 2), waiting);
        // El progreso (segundos transcurridos) no entra en la huella: no despierta a la UI por sí solo.
        st.running_cues[0].action_elapsed_sec = 9.0;
        let same = Telemetry::activity_signature(&shm, 2);
        shm.status.publish(&st);
        assert_eq!(Telemetry::activity_signature(&shm, 2), same);
        shm.meters.report_output(1, 0.3);
        assert!(Telemetry::activity_signature(&shm, 2).3);
        assert!(!Telemetry::activity_signature(&shm, 1).3, "solo cuentan los canales usados");
        assert_eq!(shm.meters.peek_output(1), 0.3, "sondear no consume");
    }

    #[test]
    fn slice_tag_follows_vamping_then_devamping_only_while_the_action_runs() {
        let mut i = RunningInfo { phase: Some(CueUiPhase::Action), slice: Some(1), vamping: true, ..Default::default() };
        assert_eq!(i.slice_tag(), Some(SliceTag::Vamp));
        // El Devamp tiene prioridad aunque el cursor aún figure como atrapado.
        i.devamping = true;
        assert_eq!(i.slice_tag(), Some(SliceTag::Devamping));
        // Al consumirse el Devamp vuelve a su estado normal.
        i.devamping = false;
        i.vamping = false;
        assert_eq!(i.slice_tag(), None);
        // Fuera de la acción no hay rótulo.
        let pre = RunningInfo { phase: Some(CueUiPhase::PreWait), vamping: true, ..Default::default() };
        assert_eq!(pre.slice_tag(), None);
        assert_eq!((SliceTag::Vamp.label(), SliceTag::Devamping.label()), ("VAMP", "DEVAMP"));
    }

    #[test]
    fn blink_alternates_at_two_hertz() {
        assert!(blink_on(0.0) && blink_on(0.49));
        assert!(!blink_on(0.5) && !blink_on(0.99));
        assert!(blink_on(1.0));
    }

    #[test]
    fn telemetry_decodes_slice_flags_and_the_signature_reacts_to_them() {
        let shm = Box::new(SharedTelemetryShm::new());
        shm.initialize();
        let mut st = ShowStatus::default();
        st.running_cues[0] = qlab_protocol::ipc_shm::RunningCueTelemetry { cue_id: 5, phase: CuePhase::Action, current_slice_idx: 1, slice_flags: SLICE_PROGRAM | SLICE_VAMPING, ..Default::default() };
        st.is_playing = true;
        shm.status.publish(&st);
        let before = Telemetry::activity_signature(&shm, 2);
        let mut t = Telemetry::default();
        t.update(&shm, 0.016, 2);
        let info = t.running[&5];
        assert_eq!((info.slice, info.vamping, info.devamping), (Some(1), true, false));
        // Devamp: cambian los flags, no la fase ni el id; la huella debe despertar a la UI.
        st.running_cues[0].slice_flags = SLICE_PROGRAM | SLICE_DEVAMPING;
        shm.status.publish(&st);
        assert_ne!(before, Telemetry::activity_signature(&shm, 2));
        t.update(&shm, 0.016, 2);
        assert_eq!(t.running[&5].slice_tag(), Some(SliceTag::Devamping));
    }

    #[test]
    fn cursor_uses_the_real_source_position_and_wraps_with_the_loop() {
        let mut i = RunningInfo { phase: Some(CueUiPhase::Action), action_elapsed: 9.0, ..Default::default() };
        // Sin posición real: aproximación lineal (crece sin parar).
        assert_eq!(i.cursor_secs(48_000, 1.0, 2.0), 19.0);
        // Con posición real: es lo que lee el motor, aunque el tiempo transcurrido siga creciendo.
        i.source_frame = 96_000;
        assert_eq!(i.cursor_secs(48_000, 1.0, 2.0), 2.0);
        i.action_elapsed = 9.5;
        i.source_frame = 48_000; // el bucle volvió al inicio del tramo
        assert_eq!(i.cursor_secs(48_000, 1.0, 2.0), 1.0);
    }

    #[test]
    fn per_cue_meters_rise_with_the_peak_fall_smoothly_and_disappear() {
        let shm = Box::new(SharedTelemetryShm::new());
        shm.initialize();
        let mut st = ShowStatus::default();
        st.running_cues[0] = qlab_protocol::ipc_shm::RunningCueTelemetry { cue_id: 3, phase: CuePhase::Action, peak: 0.5, ..Default::default() };
        st.running_cues[1] = qlab_protocol::ipc_shm::RunningCueTelemetry { cue_id: 4, phase: CuePhase::Action, peak: 0.0, ..Default::default() };
        st.is_playing = true;
        shm.status.publish(&st);
        let mut t = Telemetry::default();
        assert!(t.update(&shm, 0.016, 2));
        let a = t.cue_level_db(3).unwrap();
        assert!((a - lin_to_db(0.5)).abs() < 0.01, "{a}");
        assert!(t.cue_level_db(4).is_none(), "una cue en silencio no dibuja vúmetro");
        // Sin picos nuevos el nivel cae a CUE_METER_FALL_DB_PER_SEC.
        t.update(&shm, 0.5, 2);
        let b = t.cue_level_db(3).unwrap();
        assert!((a - b - CUE_METER_FALL_DB_PER_SEC * 0.5).abs() < 0.01, "{a} → {b}");
        // Un pico más fuerte lo sube de golpe; con el tiempo desaparece del todo.
        st.running_cues[0].peak = 0.9;
        shm.status.publish(&st);
        t.update(&shm, 0.016, 2);
        assert!(t.cue_level_db(3).unwrap() > b);
        st.running_cues[0].phase = CuePhase::Idle;
        shm.status.publish(&st);
        for _ in 0..40 {
            t.update(&shm, 0.1, 2);
        }
        assert!(t.cue_level_db(3).is_none());
    }
}
