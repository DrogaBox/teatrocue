//! Máquina de estados de una cue (spec §1.1) con tiempos en frames exactos.
//!
//! ```text
//!            load            fire (pre>0)        pre cumplido        acción terminada
//!   Idle ───────▶ Armed ───────────────▶ PreWaiting ─────────▶ Running ──────────────▶ Completed
//!     └──────────────────── fire (pre=0) ─────────────────────────▲      │
//!                                                                         └─▶ PostWaiting ─▶ Completed
//!   PreWaiting / Running / PostWaiting ◀──── pause / resume ────▶ Paused
//! ```
//!
//! * `Armed` significa **cargada y lista** (el `Loaded` del diagrama). No es la propiedad `armed`
//!   (habilitada para GO) de `CueBasicsData`.
//! * `PostWaiting`: la acción terminó pero un `AutoContinue` aún espera su post-wait.
//! * El tail-out de un Stop no es un estado: la cue sigue `Running` hasta que el motor confirma el fin.
//!
//! Todas las marcas son instantes absolutos en frames, así que `pause`/`resume` solo desplazan las
//! marcas el tiempo retenido y no queda ningún temporizador que corregir.

use qlab_protocol::cue_dto::{ContinueMode, INFINITE_SLICE_LOOP, MAX_SLICES_PER_VOICE};

use crate::clock::{Frames, Timebase};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CueState {
    Idle,
    /// Cargada (pre-cargada) y lista para disparar.
    Armed,
    PreWaiting,
    Running,
    PostWaiting,
    Paused,
    Completed,
}

impl CueState {
    /// Ocupa el scheduler: ha sido disparada y no ha terminado.
    pub fn is_active(self) -> bool {
        matches!(self, Self::PreWaiting | Self::Running | Self::PostWaiting | Self::Paused)
    }

    pub fn can_fire(self) -> bool {
        matches!(self, Self::Idle | Self::Armed | Self::Completed)
    }
}

/// Cómo termina la acción de la cue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActionEnd {
    /// Por reloj, a `action` frames de empezar (fade, wait).
    Timed,
    /// Cuando alguien lo notifica (fin de la voz de audio, fin de los hijos de un grupo).
    External,
}

/// Tiempos de una cue, en frames. `post_wait` es el **efectivo**: solo cuenta con `AutoContinue`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CueTiming {
    pub pre_wait: Frames,
    /// Duración de la acción; `None` si no se conoce (bucle infinito, grupo).
    pub action: Option<Frames>,
    pub post_wait: Frames,
}

impl CueTiming {
    pub fn from_secs(tb: Timebase, pre: f64, action: Option<f64>, post: f64) -> Self {
        Self {
            pre_wait: tb.secs_to_frames(pre),
            action: action.map(|a| tb.secs_to_frames(a)),
            post_wait: tb.secs_to_frames(post),
        }
    }

    pub fn pre_wait_micros(&self, tb: Timebase) -> u64 {
        tb.frames_to_micros(self.pre_wait)
    }

    pub fn action_micros(&self, tb: Timebase) -> Option<u64> {
        self.action.map(|a| tb.frames_to_micros(a))
    }

    pub fn post_wait_micros(&self, tb: Timebase) -> u64 {
        tb.frames_to_micros(self.post_wait)
    }

    /// `pre_wait + acción + post_wait`, la duración total que usa un grupo Timeline.
    pub fn total(&self) -> Option<Frames> {
        self.action.map(|a| self.pre_wait + a.max(self.post_wait))
    }
}

/// Qué ocurrió en una llamada. Un mismo `tick` puede encadenar varios.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Transitions {
    /// Terminó el pre-wait: hay que arrancar la acción.
    pub action_started: bool,
    pub action_completed: bool,
    /// Toca disparar la cue siguiente (`AutoContinue` por reloj o `AutoFollow` por fin de acción).
    pub continue_due: bool,
    pub completed: bool,
}

impl Transitions {
    pub fn any(&self) -> bool {
        self.action_started || self.action_completed || self.continue_due || self.completed
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StateError {
    /// La cue ya está en marcha.
    AlreadyActive,
    /// La operación no aplica al estado actual.
    InvalidState(CueState),
}

impl std::fmt::Display for StateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyActive => write!(f, "la cue ya está en marcha"),
            Self::InvalidState(s) => write!(f, "operación no válida en el estado {s:?}"),
        }
    }
}

impl std::error::Error for StateError {}

#[derive(Clone, Debug)]
pub struct CueMachine {
    state: CueState,
    paused_from: CueState,
    timing: CueTiming,
    end: ActionEnd,
    mode: ContinueMode,
    /// Instante del disparo (se desplaza al reanudar).
    fired_at: Frames,
    paused_at: Frames,
    action_done: bool,
    action_end_at: Option<Frames>,
    continue_fired: bool,
}

impl Default for CueMachine {
    fn default() -> Self {
        Self::new()
    }
}

impl CueMachine {
    pub fn new() -> Self {
        Self {
            state: CueState::Idle,
            paused_from: CueState::Idle,
            timing: CueTiming::default(),
            end: ActionEnd::External,
            mode: ContinueMode::DoNotContinue,
            fired_at: 0,
            paused_at: 0,
            action_done: false,
            action_end_at: None,
            continue_fired: false,
        }
    }

    #[inline]
    pub fn state(&self) -> CueState {
        self.state
    }

    pub fn timing(&self) -> &CueTiming {
        &self.timing
    }

    pub fn fired_at(&self) -> Frames {
        self.fired_at
    }

    /// Instante en que empieza (o empezó) la acción.
    pub fn action_start(&self) -> Frames {
        self.fired_at + self.timing.pre_wait
    }

    /// Instante programado del disparo de la siguiente cue por `AutoContinue`.
    pub fn continue_at(&self) -> Option<Frames> {
        (self.mode == ContinueMode::AutoContinue).then(|| self.action_start() + self.timing.post_wait)
    }

    pub fn is_action_done(&self) -> bool {
        self.action_done
    }

    /// Idle → Armed (pre-carga). No hace nada en otros estados.
    pub fn load(&mut self) -> bool {
        if self.state == CueState::Idle {
            self.state = CueState::Armed;
            true
        } else {
            false
        }
    }

    /// Armed → Idle.
    pub fn unload(&mut self) {
        if self.state == CueState::Armed {
            self.state = CueState::Idle;
        }
    }

    /// Vuelve a `Idle` desde cualquier estado.
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Dispara la cue en el instante `now`. Con `ignore_pre_wait` salta directamente a la acción.
    pub fn fire(
        &mut self,
        now: Frames,
        mut timing: CueTiming,
        end: ActionEnd,
        mode: ContinueMode,
        ignore_pre_wait: bool,
    ) -> Result<Transitions, StateError> {
        if !self.state.can_fire() {
            return Err(StateError::AlreadyActive);
        }
        if ignore_pre_wait {
            timing.pre_wait = 0;
        }
        if mode != ContinueMode::AutoContinue {
            timing.post_wait = 0;
        }
        self.timing = timing;
        self.end = end;
        self.mode = mode;
        self.fired_at = now;
        self.action_done = false;
        self.action_end_at = None;
        self.continue_fired = false;
        self.state = CueState::PreWaiting;
        Ok(self.tick(now))
    }

    /// Avanza hasta `now`. Idempotente: llamarlo dos veces seguidas con el mismo `now` no repite nada.
    pub fn tick(&mut self, now: Frames) -> Transitions {
        let mut t = Transitions::default();
        if !matches!(self.state, CueState::PreWaiting | CueState::Running | CueState::PostWaiting) {
            return t;
        }
        let start = self.action_start();
        if self.state == CueState::PreWaiting {
            if now < start {
                return t;
            }
            self.state = CueState::Running;
            t.action_started = true;
        }
        if !self.action_done && self.end == ActionEnd::Timed {
            if let Some(d) = self.timing.action {
                if now >= start + d {
                    self.finish_action(start + d, true, &mut t);
                }
            }
        }
        if self.mode == ContinueMode::AutoContinue && !self.continue_fired && now >= start + self.timing.post_wait {
            self.continue_fired = true;
            t.continue_due = true;
        }
        self.settle(&mut t);
        t
    }

    /// Notifica el fin de una acción `External`. `natural` es `false` si fue una parada: entonces
    /// `AutoFollow` no dispara la siguiente cue.
    pub fn complete_action(&mut self, now: Frames, natural: bool) -> Transitions {
        let mut t = Transitions::default();
        if !matches!(self.state, CueState::Running | CueState::PostWaiting) || self.action_done {
            return t;
        }
        self.finish_action(now, natural, &mut t);
        self.settle(&mut t);
        t
    }

    fn finish_action(&mut self, at: Frames, natural: bool, t: &mut Transitions) {
        self.action_done = true;
        self.action_end_at = Some(at);
        t.action_completed = true;
        if self.mode == ContinueMode::AutoFollow && natural && !self.continue_fired {
            self.continue_fired = true;
            t.continue_due = true;
        }
    }

    /// Con la acción hecha: espera el post-wait pendiente o termina.
    fn settle(&mut self, t: &mut Transitions) {
        if !self.action_done || !matches!(self.state, CueState::Running | CueState::PostWaiting) {
            return;
        }
        if self.mode == ContinueMode::AutoContinue && !self.continue_fired {
            self.state = CueState::PostWaiting;
        } else {
            self.state = CueState::Completed;
            t.completed = true;
        }
    }

    /// Parada inmediata de una cue en marcha: pasa a `Completed` sin disparar la siguiente.
    pub fn stop(&mut self, now: Frames) -> Transitions {
        let mut t = Transitions::default();
        if !self.state.is_active() {
            return t;
        }
        if !self.action_done && self.state != CueState::PreWaiting {
            t.action_completed = true;
        }
        self.action_done = true;
        self.action_end_at.get_or_insert(now);
        self.continue_fired = true;
        self.state = CueState::Completed;
        t.completed = true;
        t
    }

    pub fn pause(&mut self, now: Frames) -> Result<(), StateError> {
        match self.state {
            CueState::PreWaiting | CueState::Running | CueState::PostWaiting => {
                self.paused_from = self.state;
                self.paused_at = now;
                self.state = CueState::Paused;
                Ok(())
            }
            other => Err(StateError::InvalidState(other)),
        }
    }

    /// Reanuda: todas las marcas se desplazan el tiempo que estuvo en pausa.
    pub fn resume(&mut self, now: Frames) -> Result<(), StateError> {
        if self.state != CueState::Paused {
            return Err(StateError::InvalidState(self.state));
        }
        let held = now.saturating_sub(self.paused_at);
        self.fired_at += held;
        if let Some(e) = self.action_end_at.as_mut() {
            *e += held;
        }
        self.state = self.paused_from;
        Ok(())
    }

    /// Estado "visible" mientras está en pausa: el de antes de pausar.
    pub fn effective_state(&self) -> CueState {
        if self.state == CueState::Paused { self.paused_from } else { self.state }
    }

    /// Próximo instante en que `tick` hará algo por reloj.
    pub fn next_deadline(&self) -> Option<Frames> {
        let start = self.action_start();
        match self.state {
            CueState::PreWaiting => Some(start),
            CueState::Running | CueState::PostWaiting => {
                let mut best: Option<Frames> = None;
                let mut offer = |t: Frames| best = Some(best.map_or(t, |b| b.min(t)));
                if !self.action_done && self.end == ActionEnd::Timed {
                    if let Some(d) = self.timing.action {
                        offer(start + d);
                    }
                }
                if self.mode == ContinueMode::AutoContinue && !self.continue_fired {
                    offer(start + self.timing.post_wait);
                }
                best
            }
            _ => None,
        }
    }

    /// Instante de referencia: congelado mientras está en pausa.
    fn effective_now(&self, now: Frames) -> Frames {
        if self.state == CueState::Paused { self.paused_at.min(now) } else { now }
    }

    fn fired(&self) -> bool {
        !matches!(self.state, CueState::Idle | CueState::Armed)
    }

    /// Frames transcurridos del pre-wait (saturado en su duración).
    pub fn pre_wait_elapsed(&self, now: Frames) -> Frames {
        if !self.fired() {
            return 0;
        }
        self.effective_now(now).saturating_sub(self.fired_at).min(self.timing.pre_wait)
    }

    /// Frames transcurridos de la acción (se detiene al terminar).
    pub fn action_elapsed(&self, now: Frames) -> Frames {
        if !self.fired() {
            return 0;
        }
        let end = self.action_end_at.map_or(self.effective_now(now), |e| e.min(self.effective_now(now)));
        end.saturating_sub(self.action_start())
    }

    /// Frames transcurridos del post-wait efectivo (cuenta desde el inicio de la acción).
    pub fn post_wait_elapsed(&self, now: Frames) -> Frames {
        if !self.fired() || self.mode != ContinueMode::AutoContinue {
            return 0;
        }
        self.effective_now(now).saturating_sub(self.action_start()).min(self.timing.post_wait)
    }

    pub fn pre_wait_elapsed_micros(&self, now: Frames, tb: Timebase) -> u64 {
        tb.frames_to_micros(self.pre_wait_elapsed(now))
    }

    pub fn action_elapsed_micros(&self, now: Frames, tb: Timebase) -> u64 {
        tb.frames_to_micros(self.action_elapsed(now))
    }

    pub fn post_wait_elapsed_micros(&self, now: Frames, tb: Timebase) -> u64 {
        tb.frames_to_micros(self.post_wait_elapsed(now))
    }
}

// ---------------------------------------------------------------------------------------------
// Reproducción segmentada: slices y Devamp
// ---------------------------------------------------------------------------------------------

/// Tramo `[start, end)` del archivo (en frames de origen) que se reproduce `play_count` veces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SliceSpan {
    pub start: u64,
    pub end: u64,
    /// `SLICE_INFINITE` = bucle continuo hasta un Devamp.
    pub play_count: u32,
}

pub const SLICE_INFINITE: u32 = INFINITE_SLICE_LOOP;
/// Final de programa desconocido (la cue no fija un final: lo decide el archivo).
pub const OPEN_END: u64 = u64::MAX;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SliceError {
    /// Marcadores no crecientes o fuera de la región.
    BadMarkers,
    TooManySlices,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SliceEvent {
    /// El cabezal volvió al inicio del tramo `slice`; quedan `loops_left` pasadas (infinito = `SLICE_INFINITE`).
    Repeated { slice: usize, loops_left: u32 },
    /// El cabezal pasó al tramo `to` (contiguo al anterior salvo el cierre del programa).
    Advanced { from: usize, to: usize },
    /// Se consumió un Devamp en el final del tramo `slice`.
    Devamped { slice: usize },
    /// El programa terminó.
    Finished,
}

/// Posición de reproducción dentro de un programa de slices, calculada sin audio.
///
/// Es el modelo lógico de lo que hace el motor en tiempo real (`resolve_boundaries`): al llegar al final
/// de un tramo, un Devamp pendiente se consume y pasa al siguiente tramo **sin saltar** (el siguiente
/// empieza donde termina el actual); si no hay Devamp, un tramo con repeticiones pendientes o infinito
/// vuelve a su inicio. Como es determinista, basta avanzarlo con los frames transcurridos.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SliceCursor {
    spans: Vec<SliceSpan>,
    idx: usize,
    loops_left: u32,
    pos: u64,
    devamp_pending: bool,
    program_loop: bool,
    finished: bool,
}

impl SliceCursor {
    /// Programa de `[start, end)` con marcadores `(frame, play_count)` ordenados: `[start, s1)` suena una
    /// vez y cada marcador abre un tramo hasta el siguiente (o `end`). Sin marcadores, un único tramo con
    /// `play_count` pasadas (o infinito). `program_loop` repite todo el programa al terminar.
    pub fn new(start: u64, end: u64, markers: &[(u64, u32)], play_count: u32, infinite_loop: bool, program_loop: bool) -> Result<Self, SliceError> {
        let mut spans = Vec::with_capacity(markers.len() + 1);
        if markers.is_empty() {
            spans.push(SliceSpan { start, end, play_count: if infinite_loop { SLICE_INFINITE } else { play_count.max(1) } });
        } else {
            if markers.len() + 1 > MAX_SLICES_PER_VOICE {
                return Err(SliceError::TooManySlices);
            }
            let mut prev = start;
            for &(frame, _) in markers {
                if frame <= prev || frame >= end {
                    return Err(SliceError::BadMarkers);
                }
                prev = frame;
            }
            spans.push(SliceSpan { start, end: markers[0].0, play_count: 1 });
            for (i, &(frame, count)) in markers.iter().enumerate() {
                spans.push(SliceSpan { start: frame, end: markers.get(i + 1).map_or(end, |n| n.0), play_count: count.max(1) });
            }
        }
        let loops_left = spans[0].play_count;
        Ok(Self { pos: start, spans, idx: 0, loops_left, devamp_pending: false, program_loop, finished: false })
    }

    pub fn spans(&self) -> &[SliceSpan] {
        &self.spans
    }

    /// Frame de origen que sonará a continuación.
    pub fn position(&self) -> u64 {
        self.pos
    }

    pub fn current_slice(&self) -> usize {
        self.idx
    }

    /// Pasadas que quedan del tramo actual contando la que está sonando (`SLICE_INFINITE` si es infinito).
    pub fn loops_left(&self) -> u32 {
        self.loops_left
    }

    pub fn is_finished(&self) -> bool {
        self.finished
    }

    pub fn devamp_pending(&self) -> bool {
        self.devamp_pending
    }

    /// Atrapado en un tramo infinito: sigue repitiéndose hasta que llegue un Devamp.
    pub fn is_vamping(&self) -> bool {
        !self.finished && !self.devamp_pending && self.spans[self.idx].play_count == SLICE_INFINITE
    }

    /// Pide salir del tramo actual al final de su pasada en curso, y seguir al tramo siguiente (o al final).
    /// `false` si no hay nada que escapar (programa terminado o ya pedido).
    pub fn request_devamp(&mut self) -> bool {
        if self.finished || self.devamp_pending {
            return false;
        }
        self.devamp_pending = true;
        true
    }

    /// Avanza `frames` de origen, anotando en `events` lo que ocurra en el camino.
    pub fn advance(&mut self, frames: u64, events: &mut Vec<SliceEvent>) {
        let mut remaining = frames;
        let mut guard = 0usize;
        while !self.finished && remaining > 0 {
            let span = self.spans[self.idx];
            let room = span.end.saturating_sub(self.pos);
            let take = room.min(remaining);
            self.pos += take;
            remaining -= take;
            if self.pos < span.end {
                break;
            }
            // Un tramo vacío no consume frames: el guardián evita girar para siempre sobre él.
            guard += 1;
            if guard > 4 * self.spans.len() + 4 && take == 0 {
                self.finish(events);
                break;
            }
            self.resolve_boundary(span, events);
        }
    }

    fn resolve_boundary(&mut self, span: SliceSpan, events: &mut Vec<SliceEvent>) {
        if self.devamp_pending {
            self.devamp_pending = false;
            events.push(SliceEvent::Devamped { slice: self.idx });
        } else if span.start < span.end && (span.play_count == SLICE_INFINITE || self.loops_left > 1) {
            if span.play_count != SLICE_INFINITE {
                self.loops_left -= 1;
            }
            self.pos = span.start;
            events.push(SliceEvent::Repeated { slice: self.idx, loops_left: self.loops_left });
            return;
        }
        if self.idx + 1 < self.spans.len() {
            events.push(SliceEvent::Advanced { from: self.idx, to: self.idx + 1 });
            self.idx += 1;
            self.loops_left = self.spans[self.idx].play_count;
            self.pos = self.spans[self.idx].start;
        } else if self.program_loop {
            events.push(SliceEvent::Advanced { from: self.idx, to: 0 });
            self.idx = 0;
            self.loops_left = self.spans[0].play_count;
            self.pos = self.spans[0].start;
        } else {
            self.finish(events);
        }
    }

    fn finish(&mut self, events: &mut Vec<SliceEvent>) {
        self.finished = true;
        events.push(SliceEvent::Finished);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ContinueMode::*;

    const FS: u64 = 48_000;

    fn timing(pre: u64, action: Option<u64>, post: u64) -> CueTiming {
        CueTiming { pre_wait: pre, action, post_wait: post }
    }

    fn fire(m: &mut CueMachine, now: Frames, t: CueTiming, end: ActionEnd, mode: ContinueMode) -> Transitions {
        m.fire(now, t, end, mode, false).unwrap()
    }

    #[test]
    fn idle_load_unload_reset() {
        let mut m = CueMachine::new();
        assert_eq!(m.state(), CueState::Idle);
        assert!(m.load());
        assert_eq!(m.state(), CueState::Armed);
        assert!(!m.load());
        m.unload();
        assert_eq!(m.state(), CueState::Idle);
        m.load();
        fire(&mut m, 0, timing(0, Some(10), 0), ActionEnd::Timed, DoNotContinue);
        assert!(!m.load(), "no se carga una cue en marcha");
        m.reset();
        assert_eq!(m.state(), CueState::Idle);
    }

    #[test]
    fn no_pre_wait_starts_the_action_immediately() {
        let mut m = CueMachine::new();
        let t = fire(&mut m, 1_000, timing(0, None, 0), ActionEnd::External, DoNotContinue);
        assert!(t.action_started && !t.completed);
        assert_eq!(m.state(), CueState::Running);
        assert_eq!(m.action_start(), 1_000);
    }

    #[test]
    fn pre_wait_is_frame_exact() {
        let mut m = CueMachine::new();
        let pre = 3 * FS / 2; // 1,5 s = 72 000 frames
        let t = fire(&mut m, 10_000, timing(pre, None, 0), ActionEnd::External, DoNotContinue);
        assert!(!t.any());
        assert_eq!(m.state(), CueState::PreWaiting);
        assert_eq!(m.next_deadline(), Some(10_000 + 72_000));
        assert!(!m.tick(10_000 + 71_999).any(), "un frame antes sigue en pre-wait");
        assert_eq!(m.state(), CueState::PreWaiting);
        assert_eq!(m.pre_wait_elapsed(10_000 + 71_999), 71_999);
        let t = m.tick(10_000 + 72_000);
        assert!(t.action_started);
        assert_eq!(m.state(), CueState::Running);
        assert_eq!(m.pre_wait_elapsed(10_000 + 99_999), 72_000, "el pre-wait se satura");
        assert!(!m.tick(10_000 + 72_000).any(), "tick idempotente");
    }

    #[test]
    fn timed_action_completes_at_its_exact_frame() {
        let mut m = CueMachine::new();
        fire(&mut m, 0, timing(100, Some(500), 0), ActionEnd::Timed, DoNotContinue);
        assert!(m.tick(100).action_started);
        assert!(!m.tick(599).any());
        assert_eq!(m.action_elapsed(599), 499);
        let t = m.tick(600);
        assert!(t.action_completed && t.completed && !t.continue_due);
        assert_eq!(m.state(), CueState::Completed);
        assert_eq!(m.action_elapsed(10_000), 500, "el progreso queda congelado en la duración");
    }

    #[test]
    fn late_tick_still_reports_every_phase_once() {
        let mut m = CueMachine::new();
        fire(&mut m, 0, timing(100, Some(500), 0), ActionEnd::Timed, DoNotContinue);
        let t = m.tick(5_000); // el scheduler llegó tarde: todo ocurrió en este tick
        assert!(t.action_started && t.action_completed && t.completed);
        assert_eq!(m.action_elapsed(5_000), 500);
    }

    #[test]
    fn external_action_runs_until_notified() {
        let mut m = CueMachine::new();
        fire(&mut m, 0, timing(0, Some(480), 0), ActionEnd::External, DoNotContinue);
        assert!(!m.tick(1_000_000).any(), "la duración estimada no termina una acción externa");
        assert_eq!(m.state(), CueState::Running);
        assert_eq!(m.next_deadline(), None);
        let t = m.complete_action(2_000, true);
        assert!(t.action_completed && t.completed);
        assert_eq!(m.action_elapsed(9_999), 2_000);
        assert!(!m.complete_action(3_000, true).any());
    }

    #[test]
    fn auto_continue_fires_at_pre_plus_post_in_parallel_with_the_action() {
        let mut m = CueMachine::new();
        // Pre 1 s, post 2 s, acción larga (10 s): el siguiente dispara a los 3 s.
        fire(&mut m, 0, timing(FS, Some(10 * FS), 2 * FS), ActionEnd::Timed, AutoContinue);
        assert_eq!(m.continue_at(), Some(3 * FS));
        assert_eq!(m.next_deadline(), Some(FS));
        assert!(m.tick(FS).action_started);
        assert_eq!(m.next_deadline(), Some(3 * FS));
        assert!(!m.tick(3 * FS - 1).continue_due);
        let t = m.tick(3 * FS);
        assert!(t.continue_due && !t.completed, "sigue sonando");
        assert_eq!(m.state(), CueState::Running);
        assert_eq!(m.post_wait_elapsed(3 * FS), 2 * FS);
        let t = m.tick(11 * FS);
        assert!(t.action_completed && t.completed);
    }

    #[test]
    fn short_action_waits_in_post_waiting() {
        let mut m = CueMachine::new();
        fire(&mut m, 0, timing(0, Some(FS), 3 * FS), ActionEnd::Timed, AutoContinue);
        let t = m.tick(FS);
        assert!(t.action_completed && !t.completed && !t.continue_due);
        assert_eq!(m.state(), CueState::PostWaiting);
        assert_eq!(m.next_deadline(), Some(3 * FS));
        let t = m.tick(3 * FS);
        assert!(t.continue_due && t.completed);
        assert_eq!(m.state(), CueState::Completed);
    }

    #[test]
    fn auto_continue_with_zero_post_wait_continues_at_action_start() {
        let mut m = CueMachine::new();
        let t = fire(&mut m, 500, timing(0, None, 0), ActionEnd::External, AutoContinue);
        assert!(t.action_started && t.continue_due);
    }

    #[test]
    fn auto_follow_continues_only_on_natural_completion() {
        let mut m = CueMachine::new();
        fire(&mut m, 0, timing(0, None, 5 * FS), ActionEnd::External, AutoFollow);
        assert_eq!(m.timing().post_wait, 0, "AutoFollow fuerza post-wait 0");
        assert!(!m.tick(10 * FS).continue_due, "no hay disparo por reloj");
        let t = m.complete_action(10 * FS, true);
        assert!(t.continue_due && t.completed);

        let mut m = CueMachine::new();
        fire(&mut m, 0, timing(0, None, 0), ActionEnd::External, AutoFollow);
        let t = m.complete_action(100, false);
        assert!(!t.continue_due && t.completed, "una parada no encadena");
    }

    #[test]
    fn do_not_continue_ignores_post_wait() {
        let mut m = CueMachine::new();
        fire(&mut m, 0, timing(0, Some(10), 999), ActionEnd::Timed, DoNotContinue);
        assert_eq!(m.timing().post_wait, 0);
        let t = m.tick(10);
        assert!(t.completed && !t.continue_due);
    }

    #[test]
    fn pause_freezes_progress_and_resume_shifts_every_mark() {
        let mut m = CueMachine::new();
        fire(&mut m, 0, timing(1_000, Some(4_000), 2_000), ActionEnd::Timed, AutoContinue);
        assert!(m.tick(1_000).action_started);
        m.pause(1_500).unwrap(); // 500 frames de acción
        assert_eq!(m.state(), CueState::Paused);
        assert_eq!(m.effective_state(), CueState::Running);
        assert!(!m.tick(99_999).any(), "en pausa no pasa nada");
        assert_eq!(m.action_elapsed(99_999), 500);
        assert_eq!(m.next_deadline(), None);
        m.resume(11_500).unwrap(); // 10 000 frames retenidos
        assert_eq!(m.state(), CueState::Running);
        assert_eq!(m.action_elapsed(11_500), 500);
        assert_eq!(m.next_deadline(), Some(11_000 + 2_000), "el post-wait se desplazó");
        assert!(m.tick(13_000).continue_due);
        assert!(m.tick(15_000).completed);
    }

    #[test]
    fn pause_during_pre_wait() {
        let mut m = CueMachine::new();
        fire(&mut m, 0, timing(1_000, None, 0), ActionEnd::External, DoNotContinue);
        m.pause(400).unwrap();
        m.resume(5_400).unwrap();
        assert_eq!(m.pre_wait_elapsed(5_400), 400);
        assert!(!m.tick(5_999).any());
        assert!(m.tick(6_000).action_started);
        assert!(m.pause(10).is_ok());
        assert!(m.pause(20).is_err(), "no se pausa dos veces");
        assert!(CueMachine::new().resume(0).is_err());
    }

    #[test]
    fn stop_completes_without_continuing() {
        let mut m = CueMachine::new();
        fire(&mut m, 0, timing(0, None, 5_000), ActionEnd::External, AutoContinue);
        let t = m.stop(100);
        assert!(t.completed && !t.continue_due);
        assert_eq!(m.state(), CueState::Completed);
        assert!(!m.stop(200).any());
        let mut m = CueMachine::new();
        fire(&mut m, 0, timing(1_000, None, 0), ActionEnd::External, DoNotContinue);
        let t = m.stop(10);
        assert!(t.completed && !t.action_completed, "no llegó a empezar la acción");
    }

    #[test]
    fn refiring_and_double_fire() {
        let mut m = CueMachine::new();
        fire(&mut m, 0, timing(0, Some(10), 0), ActionEnd::Timed, DoNotContinue);
        assert_eq!(
            m.fire(5, timing(0, Some(10), 0), ActionEnd::Timed, DoNotContinue, false),
            Err(StateError::AlreadyActive)
        );
        m.tick(10);
        assert_eq!(m.state(), CueState::Completed);
        let t = fire(&mut m, 100, timing(0, Some(10), 0), ActionEnd::Timed, DoNotContinue);
        assert!(t.action_started);
        assert_eq!(m.action_elapsed(105), 5);
    }

    #[test]
    fn ignore_pre_wait_option() {
        let mut m = CueMachine::new();
        let t = m.fire(0, timing(5_000, None, 0), ActionEnd::External, DoNotContinue, true).unwrap();
        assert!(t.action_started);
        assert_eq!(m.timing().pre_wait, 0);
    }

    #[test]
    fn timing_helpers_in_micros() {
        let tb = Timebase::new(48_000);
        let t = CueTiming::from_secs(tb, 1.5, Some(0.25), 2.0);
        assert_eq!((t.pre_wait, t.action, t.post_wait), (72_000, Some(12_000), 96_000));
        assert_eq!(t.pre_wait_micros(tb), 1_500_000);
        assert_eq!(t.action_micros(tb), Some(250_000));
        assert_eq!(t.post_wait_micros(tb), 2_000_000);
        assert_eq!(t.total(), Some(72_000 + 96_000));
        assert_eq!(CueTiming::from_secs(tb, 0.0, None, 0.0).total(), None);
    }

    #[test]
    fn elapsed_in_micros_is_sample_exact_at_44k1() {
        let tb = Timebase::new(44_100);
        let mut m = CueMachine::new();
        fire(&mut m, 0, timing(44_100, None, 0), ActionEnd::External, DoNotContinue);
        assert_eq!(m.pre_wait_elapsed_micros(22_050, tb), 500_000);
        m.tick(44_100);
        assert_eq!(m.action_elapsed_micros(44_100 + 441, tb), 10_000);
        assert_eq!(m.post_wait_elapsed_micros(100, tb), 0);
    }
}

#[cfg(test)]
mod slice_tests {
    use super::*;

    /// Lee `n` frames de uno en uno y devuelve la posición de origen de cada frame sonado.
    fn play(c: &mut SliceCursor, n: u64, ev: &mut Vec<SliceEvent>) -> Vec<u64> {
        (0..n)
            .map(|_| {
                let p = c.position();
                c.advance(1, ev);
                p
            })
            .collect()
    }

    fn cursor(markers: &[(u64, u32)]) -> SliceCursor {
        SliceCursor::new(0, 400, markers, 1, false, false).unwrap()
    }

    #[test]
    fn plain_program_plays_once_and_finishes() {
        let mut c = cursor(&[]);
        let mut ev = Vec::new();
        let seen = play(&mut c, 400, &mut ev);
        assert_eq!(seen, (0..400).collect::<Vec<_>>());
        assert!(c.is_finished());
        assert_eq!(ev, [SliceEvent::Finished]);
    }

    #[test]
    fn fixed_repeats_loop_back_exactly_n_times() {
        // [0,100) una vez; [100,200) tres veces; [200,400) una vez.
        let mut c = cursor(&[(100, 3), (200, 1)]);
        let mut ev = Vec::new();
        let seen = play(&mut c, 100 + 300 + 200, &mut ev);
        let mut expect: Vec<u64> = (0..100).collect();
        for _ in 0..3 {
            expect.extend(100..200);
        }
        expect.extend(200..400);
        assert_eq!(seen, expect);
        assert!(c.is_finished());
        assert_eq!(ev.iter().filter(|e| matches!(e, SliceEvent::Repeated { .. })).count(), 2);
    }

    #[test]
    fn an_infinite_slice_holds_the_playhead_inside_its_bounds() {
        let mut c = cursor(&[(100, SLICE_INFINITE)]);
        let mut ev = Vec::new();
        let seen = play(&mut c, 100 + 10_000, &mut ev);
        assert!(!c.is_finished() && c.is_vamping());
        assert_eq!(c.current_slice(), 1);
        // Tras el primer tramo, todo el audio sale de [100, 400) y el bucle vuelve siempre a 100 sin huecos.
        assert!(seen[100..].iter().all(|&p| (100..400).contains(&p)), "el cabezal no salió del tramo infinito");
        for w in seen[100..].windows(2) {
            assert!(w[1] == w[0] + 1 || (w[0] == 399 && w[1] == 100), "salto inesperado {} → {}", w[0], w[1]);
        }
        assert!(ev.iter().filter(|e| matches!(e, SliceEvent::Repeated { slice: 1, .. })).count() > 30);
        assert!(!ev.iter().any(|e| matches!(e, SliceEvent::Finished)));
    }

    #[test]
    fn devamp_escapes_at_the_end_of_the_current_pass_without_jumps() {
        let mut c = cursor(&[(100, SLICE_INFINITE), (300, 1)]);
        let mut ev = Vec::new();
        let mut seen = play(&mut c, 100 + 200 + 50, &mut ev); // un bucle completo y 50 frames del siguiente
        assert!(c.is_vamping());
        assert!(c.request_devamp());
        assert!(!c.request_devamp(), "un segundo Devamp no se acumula");
        seen.extend(play(&mut c, 250, &mut ev));
        // Termina la pasada en curso (hasta 299), sigue en 300 y acaba en 399: todo contiguo.
        let tail = &seen[350..];
        assert_eq!(&tail[..150], &(150..300).collect::<Vec<u64>>()[..], "completa la repetición actual");
        assert_eq!(&tail[150..250], &(300..400).collect::<Vec<u64>>()[..], "continúa sin saltos en el siguiente tramo");
        assert_eq!(tail.len(), 250);
        assert!(c.is_finished());
        assert!(ev.contains(&SliceEvent::Devamped { slice: 1 }));
        assert!(ev.contains(&SliceEvent::Advanced { from: 1, to: 2 }));
        // Ningún retroceso después del Devamp.
        assert!(tail.windows(2).all(|w| w[1] == w[0] + 1));
    }

    #[test]
    fn devamp_on_the_last_slice_runs_to_the_end_of_the_track() {
        let mut c = cursor(&[(100, SLICE_INFINITE)]);
        let mut ev = Vec::new();
        play(&mut c, 100 + 500, &mut ev);
        assert!(c.is_vamping());
        let at = c.position();
        c.request_devamp();
        let rest = play(&mut c, 400 - at, &mut ev);
        assert!(rest.windows(2).all(|w| w[1] == w[0] + 1), "sin saltos");
        assert!(c.is_finished());
        assert_eq!(ev.last(), Some(&SliceEvent::Finished));
        assert!(!c.is_vamping());
    }

    #[test]
    fn devamp_cuts_a_finite_repeat_short_and_one_call_matches_frame_by_frame() {
        let mut a = cursor(&[(100, 5)]);
        let mut b = a.clone();
        let (mut ea, mut eb) = (Vec::new(), Vec::new());
        a.advance(100 + 130, &mut ea);
        a.request_devamp();
        a.advance(10_000, &mut ea);
        play(&mut b, 100 + 130, &mut eb);
        b.request_devamp();
        play(&mut b, 1_000, &mut eb);
        assert_eq!(a, b);
        assert_eq!(ea, eb, "avanzar de golpe equivale a avanzar frame a frame");
        assert!(a.is_finished());
        assert_eq!(ea.iter().filter(|e| matches!(e, SliceEvent::Repeated { .. })).count(), 0, "el Devamp evita las 4 repeticiones restantes");
        assert!(ea.contains(&SliceEvent::Devamped { slice: 1 }));
    }

    #[test]
    fn program_loop_wraps_to_the_first_slice() {
        let mut c = SliceCursor::new(0, 200, &[(100, 1)], 1, false, true).unwrap();
        let mut ev = Vec::new();
        let seen = play(&mut c, 500, &mut ev);
        assert!(!c.is_finished());
        assert_eq!(seen[200], 0);
        assert_eq!(seen[300], 100);
    }

    #[test]
    fn bad_markers_are_rejected() {
        assert_eq!(SliceCursor::new(0, 400, &[(100, 1), (100, 1)], 1, false, false), Err(SliceError::BadMarkers));
        assert_eq!(SliceCursor::new(0, 400, &[(0, 1)], 1, false, false), Err(SliceError::BadMarkers));
        assert_eq!(SliceCursor::new(0, 400, &[(400, 1)], 1, false, false), Err(SliceError::BadMarkers));
        let many: Vec<(u64, u32)> = (1..=MAX_SLICES_PER_VOICE as u64).map(|f| (f, 1)).collect();
        assert_eq!(SliceCursor::new(0, 1_000, &many, 1, false, false), Err(SliceError::TooManySlices));
    }

    #[test]
    fn single_infinite_loop_without_markers_is_vamping_until_devamp() {
        let mut c = SliceCursor::new(0, 100, &[], 1, true, false).unwrap();
        let mut ev = Vec::new();
        c.advance(250, &mut ev);
        assert!(c.is_vamping());
        c.request_devamp();
        c.advance(49, &mut ev);
        assert!(!c.is_finished());
        c.advance(1, &mut ev);
        assert!(c.is_finished());
    }
}
