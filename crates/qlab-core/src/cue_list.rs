//! `CueList`: playhead, botón GO y encadenamientos (spec §1.1).
//!
//! La lista es un modelo puro y síncrono: no abre hilos ni toca el motor. Cada operación recibe el
//! instante `now` en frames y vuelca sus consecuencias en un `Vec<Effect>`; el [`Scheduler`]
//! (`crate::scheduler`) las traduce a `AudioCommand`.
//!
//! # Regla de GO (§1.1)
//! 1. Avanza el playhead sobre cues `dead` o `(!armed && skip_if_disarmed)`; si encuentra una cue
//!    `!armed` sin `skip_if_disarmed`, se detiene ahí (`Disarmed`).
//! 2. Calcula la siguiente posición **antes** de disparar y mueve el playhead de inmediato.
//! 3. Dispara la cue: nada bloquea; con pre-wait solo queda marcada y la acción llega por `tick`.
//! 4. Programa la pre-carga de la siguiente cue a los 0,15 s.
//! 5. `AutoContinue` dispara la siguiente a `pre + post` desde el disparo, en paralelo con el audio;
//!    `AutoFollow` lo hace cuando el motor confirma el fin natural de la voz.

use std::collections::HashMap;

use qlab_engine::rt_command::AudioStatus;
use qlab_protocol::cue_dto::{ContinueMode, CueBasicsData, CueId, CueTriggersData};

use crate::clock::{Frames, Timebase};
use crate::cue::{ControlAction, Cue, CueKind, GroupMode};
use crate::cue_state::{CueState, Transitions};

/// Retardo de la pre-carga de la cue que queda en el playhead.
pub const AUTO_LOAD_DELAY_SECS: f64 = 0.15;
/// Cota de pasadas por `tick`: cada una procesa los encadenamientos que provocó la anterior.
const MAX_TICK_PASSES: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TriggerSource {
    GoButton,
    AutoContinue,
    AutoFollow,
    Group,
    Api,
}

/// Consecuencias de una operación, para el scheduler y la UI.
#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    /// Pre-cargar el audio de la cue (el daemon crea la voz).
    Load { cue: CueId },
    /// Arrancar la voz de una cue de audio.
    StartAudio { cue: CueId, at_frames: Frames },
    StartFade { cue: CueId, at_frames: Frames },
    PauseVoice { cue: CueId },
    ResumeVoice { cue: CueId },
    StopVoice { cue: CueId, fade_frames: u32 },
    CancelFade { cue: CueId },
    /// Salir del slice actual al final de su pasada en curso y seguir sin saltos (`RequestDevamp`).
    Devamp { cue: CueId },
    PlayheadMoved { list: u64, position: Option<CueId> },
    /// Cambió el armado de una cue (desde una cue de control): la interfaz debe volver a leerla.
    ArmChanged { cue: CueId },
    /// Enviar un mensaje OSC (cue de red): lo ejecuta el daemon.
    SendOsc { cue: CueId, destination: String, message: String },
    StateChanged { cue: CueId, from: CueState, to: CueState },
    /// Evento del motor que no cambia el modelo (solo informativo).
    Engine(AudioStatus),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DevampResult {
    UnknownCue,
    /// No es una cue de audio en marcha.
    NotRunning,
    /// Ya hay un Devamp pendiente o el programa terminó.
    NothingToEscape,
    Requested,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SliceStatus {
    pub slice: usize,
    /// Pasadas que quedan del tramo (`u32::MAX` = infinito).
    pub loops_left: u32,
    /// Atrapada en un tramo infinito.
    pub vamping: bool,
    /// Hay un Devamp pedido que aún no se consumió (acaba la pasada en curso).
    pub devamping: bool,
    pub finished: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GoResult {
    /// Se disparó `cue`; el playhead quedó en `next_playhead`.
    Fired { cue: CueId, next_playhead: Option<CueId> },
    /// El playhead apunta a una cue desarmada sin `skip_if_disarmed`: no se dispara ni se avanza.
    Disarmed(CueId),
    /// La cue del playhead ya está en marcha.
    Busy(CueId),
    /// La cue del playhead estaba atrapada en un slice infinito: GO pidió el Devamp en vez de disparar.
    Devamped(CueId),
    /// No hay nada más que disparar.
    EndOfList,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListError {
    DuplicateId(CueId),
    UnknownCue(CueId),
    NotAGroup(CueId),
    /// La cue ya pertenece a un grupo o a la lista.
    AlreadyPlaced(CueId),
    /// La edición cambia el id o el tipo de la cue.
    KindMismatch(CueId),
    /// La cue (o alguna de sus hijas) está en marcha: hay que detenerla antes.
    Busy(CueId),
}

impl std::fmt::Display for ListError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DuplicateId(id) => write!(f, "ya existe la cue {}", id.0),
            Self::UnknownCue(id) => write!(f, "no existe la cue {}", id.0),
            Self::NotAGroup(id) => write!(f, "la cue {} no es un grupo", id.0),
            Self::AlreadyPlaced(id) => write!(f, "la cue {} ya está colocada", id.0),
            Self::KindMismatch(id) => write!(f, "la edición de la cue {} no puede cambiar su id ni su tipo", id.0),
            Self::Busy(id) => write!(f, "la cue {} está en marcha; detenla antes", id.0),
        }
    }
}

impl std::error::Error for ListError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Exec {
    /// Se puede disparar.
    Go,
    /// Se salta (dead, o desarmada con `skip_if_disarmed`).
    Skip,
    /// Desarmada sin `skip_if_disarmed`: detiene el avance.
    Halt,
}

pub struct CueList {
    id: u64,
    timebase: Timebase,
    order: Vec<CueId>,
    cues: HashMap<CueId, Cue>,
    parents: HashMap<CueId, CueId>,
    playback_position: Option<CueId>,
    /// Cues disparadas y sin terminar, en orden de activación.
    active: Vec<CueId>,
    pending_loads: Vec<(Frames, CueId)>,
}

impl CueList {
    pub fn new(id: u64, timebase: Timebase) -> Self {
        Self {
            id,
            timebase,
            order: Vec::new(),
            cues: HashMap::new(),
            parents: HashMap::new(),
            playback_position: None,
            active: Vec::new(),
            pending_loads: Vec::new(),
        }
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn timebase(&self) -> Timebase {
        self.timebase
    }

    // -- Construcción -----------------------------------------------------------------------

    /// Añade una cue al final del nivel superior. La primera cue añadida recibe el playhead.
    pub fn add_cue(&mut self, cue: Cue) -> Result<CueId, ListError> {
        let id = cue.id();
        if self.cues.contains_key(&id) {
            return Err(ListError::DuplicateId(id));
        }
        self.cues.insert(id, cue);
        self.order.push(id);
        if self.playback_position.is_none() {
            self.playback_position = Some(id);
        }
        Ok(id)
    }

    /// Añade una cue como hija de un grupo existente (no entra en el orden de nivel superior).
    pub fn add_child(&mut self, group: CueId, cue: Cue) -> Result<CueId, ListError> {
        let id = cue.id();
        match self.cues.get(&group).map(|g| &g.kind) {
            None => return Err(ListError::UnknownCue(group)),
            Some(CueKind::Group(_)) => {}
            Some(_) => return Err(ListError::NotAGroup(group)),
        }
        if self.cues.contains_key(&id) {
            return Err(ListError::DuplicateId(id));
        }
        self.cues.insert(id, cue);
        self.parents.insert(id, group);
        if let Some(CueKind::Group(g)) = self.cues.get_mut(&group).map(|g| &mut g.kind) {
            g.children.push(id);
        }
        Ok(id)
    }

    /// Inserta una cue en un contenedor: el grupo `parent`, o el del `after` si no se indica, o el nivel
    /// superior. Va justo después de `after` si este está en ese contenedor, y al final si no. La
    /// primera cue de nivel superior recibe el playhead.
    pub fn insert_cue(&mut self, cue: Cue, parent: Option<CueId>, after: Option<CueId>) -> Result<CueId, ListError> {
        let id = cue.id();
        if self.cues.contains_key(&id) {
            return Err(ListError::DuplicateId(id));
        }
        let container = match parent {
            Some(p) => Some(p),
            None => after.and_then(|a| self.parents.get(&a).copied()),
        };
        if let Some(g) = container {
            match self.cues.get(&g).map(|c| &c.kind) {
                None => return Err(ListError::UnknownCue(g)),
                Some(CueKind::Group(_)) => {}
                Some(_) => return Err(ListError::NotAGroup(g)),
            }
        }
        self.cues.insert(id, cue);
        match container {
            None => {
                let at = after.and_then(|a| self.order.iter().position(|&c| c == a)).map_or(self.order.len(), |i| i + 1);
                self.order.insert(at, id);
                if self.playback_position.is_none() {
                    self.playback_position = Some(id);
                }
            }
            Some(g) => {
                self.parents.insert(id, g);
                if let Some(CueKind::Group(data)) = self.cues.get_mut(&g).map(|c| &mut c.kind) {
                    let at = after.and_then(|a| data.children.iter().position(|&c| c == a)).map_or(data.children.len(), |i| i + 1);
                    data.children.insert(at, id);
                }
            }
        }
        Ok(id)
    }

    /// `id` y todas sus descendientes (en profundidad).
    pub fn subtree(&self, id: CueId) -> Vec<CueId> {
        let mut out = vec![id];
        let mut i = 0;
        while i < out.len() {
            if let Some(CueKind::Group(g)) = self.cues.get(&out[i]).map(|c| &c.kind) {
                out.extend(g.children.iter().copied());
            }
            i += 1;
        }
        out
    }

    /// Elimina la cue y sus hijas. Falla con `Busy` si alguna está en marcha. Si el playhead apuntaba a una
    /// de ellas pasa a la siguiente cue de nivel superior (o a la anterior, o a ninguna). Devuelve los ids
    /// eliminados.
    pub fn remove_cue(&mut self, id: CueId) -> Result<Vec<CueId>, ListError> {
        if !self.cues.contains_key(&id) {
            return Err(ListError::UnknownCue(id));
        }
        let removed = self.subtree(id);
        if let Some(&busy) = removed.iter().find(|r| self.cues.get(r).is_some_and(|c| c.state().is_active())) {
            return Err(ListError::Busy(busy));
        }
        let playhead_hit = self.playback_position.is_some_and(|p| removed.contains(&p));
        let replacement = if playhead_hit {
            let i = self.order.iter().position(|c| removed.contains(c));
            i.and_then(|i| self.order[i + 1..].iter().find(|c| !removed.contains(c)).or_else(|| self.order[..i].iter().rev().find(|c| !removed.contains(c))).copied())
        } else {
            None
        };
        if let Some(g) = self.parents.get(&id).copied() {
            if let Some(CueKind::Group(data)) = self.cues.get_mut(&g).map(|c| &mut c.kind) {
                data.children.retain(|&c| c != id);
            }
        }
        self.order.retain(|c| !removed.contains(c));
        for r in &removed {
            self.cues.remove(r);
            self.parents.remove(r);
        }
        self.active.retain(|c| !removed.contains(c));
        self.pending_loads.retain(|(_, c)| !removed.contains(c));
        if playhead_hit {
            self.playback_position = replacement;
        }
        Ok(removed)
    }

    /// Mueve la cue una posición hacia arriba (`delta < 0`) o hacia abajo (`delta > 0`) entre sus hermanas.
    /// `Ok(false)` si ya está en el extremo.
    pub fn move_cue(&mut self, id: CueId, delta: i32) -> Result<bool, ListError> {
        if !self.cues.contains_key(&id) {
            return Err(ListError::UnknownCue(id));
        }
        let parent = self.parents.get(&id).copied();
        let siblings: &mut Vec<CueId> = match parent {
            None => &mut self.order,
            Some(g) => match self.cues.get_mut(&g).map(|c| &mut c.kind) {
                Some(CueKind::Group(data)) => &mut data.children,
                _ => return Err(ListError::NotAGroup(g)),
            },
        };
        let i = siblings.iter().position(|&c| c == id).ok_or(ListError::UnknownCue(id))?;
        let j = i as i64 + delta.signum() as i64;
        if delta == 0 || j < 0 || j as usize >= siblings.len() {
            return Ok(false);
        }
        siblings.swap(i, j as usize);
        Ok(true)
    }

    pub fn cue(&self, id: CueId) -> Option<&Cue> {
        self.cues.get(&id)
    }

    pub fn cue_mut(&mut self, id: CueId) -> Option<&mut Cue> {
        self.cues.get_mut(&id)
    }

    /// Sustituye los datos editables de una cue sin tocar su estado de ejecución. No puede cambiar
    /// el id ni el tipo; en un grupo se conservan sus hijos y solo cambia el modo. Devuelve el tipo
    /// anterior para que quien llama compare (p. ej. los niveles) y avise al motor.
    pub fn update_cue(&mut self, id: CueId, basics: CueBasicsData, triggers: CueTriggersData, kind: CueKind) -> Result<CueKind, ListError> {
        let cue = self.cues.get_mut(&id).ok_or(ListError::UnknownCue(id))?;
        if basics.unique_id != id || std::mem::discriminant(&cue.kind) != std::mem::discriminant(&kind) {
            return Err(ListError::KindMismatch(id));
        }
        let mut new_kind = kind;
        match (&mut new_kind, &cue.kind) {
            (CueKind::Group(new), CueKind::Group(old)) => new.children = old.children.clone(),
            (CueKind::Fade(new), _) => new.target = basics.cue_target_id,
            (CueKind::Control(new), _) => new.target = basics.cue_target_id,
            _ => {}
        }
        cue.basics = basics;
        cue.triggers = triggers;
        Ok(std::mem::replace(&mut cue.kind, new_kind))
    }

    pub fn state(&self, id: CueId) -> Option<CueState> {
        self.cues.get(&id).map(Cue::state)
    }

    /// Cues de nivel superior, en orden.
    pub fn order(&self) -> &[CueId] {
        &self.order
    }

    pub fn parent_of(&self, id: CueId) -> Option<CueId> {
        self.parents.get(&id).copied()
    }

    pub fn active_ids(&self) -> &[CueId] {
        &self.active
    }

    pub fn playback_position(&self) -> Option<CueId> {
        self.playback_position
    }

    /// Mueve el playhead a una cue de nivel superior (independiente de la selección de la UI).
    pub fn set_playback_position(&mut self, position: Option<CueId>, out: &mut Vec<Effect>) -> Result<(), ListError> {
        if let Some(id) = position {
            if !self.order.contains(&id) {
                return Err(ListError::UnknownCue(id));
            }
        }
        self.set_playhead(position, out);
        Ok(())
    }

    // -- Navegación -------------------------------------------------------------------------

    fn classify(&self, id: CueId) -> Exec {
        match self.cues.get(&id) {
            None => Exec::Skip,
            Some(c) if c.dead => Exec::Skip,
            Some(c) if !c.basics.armed && c.basics.skip_if_disarmed => Exec::Skip,
            Some(c) if !c.basics.armed => Exec::Halt,
            Some(_) => Exec::Go,
        }
    }

    /// Siguiente cue de nivel superior tras `id`, sin saltar nada.
    pub fn next_playback_position_after(&self, id: CueId) -> Option<CueId> {
        let i = self.order.iter().position(|&c| c == id)?;
        self.order.get(i + 1).copied()
    }

    /// Primera cue ejecutable a partir de `from` (incluida), respetando saltos y paradas.
    fn first_executable_from(&self, from: Option<CueId>, siblings: &[CueId]) -> Option<CueId> {
        let mut cur = from;
        while let Some(id) = cur {
            match self.classify(id) {
                Exec::Go => return Some(id),
                Exec::Halt => return None,
                Exec::Skip => {
                    let i = siblings.iter().position(|&c| c == id)?;
                    cur = siblings.get(i + 1).copied();
                }
            }
        }
        None
    }

    fn chain_target_top(&self, id: CueId) -> Option<CueId> {
        self.first_executable_from(self.next_playback_position_after(id), &self.order)
    }

    /// Dentro de un grupo `FireFirstGoToNext` los hijos se encadenan; en los demás modos no.
    fn chain_target_in_group(&self, group: CueId, id: CueId) -> Option<CueId> {
        let CueKind::Group(g) = &self.cues.get(&group)?.kind else { return None };
        if g.mode != GroupMode::FireFirstGoToNext {
            return None;
        }
        let i = g.children.iter().position(|&c| c == id)?;
        self.first_executable_from(g.children.get(i + 1).copied(), &g.children)
    }

    fn set_playhead(&mut self, position: Option<CueId>, out: &mut Vec<Effect>) {
        if self.playback_position != position {
            self.playback_position = position;
            out.push(Effect::PlayheadMoved { list: self.id, position });
        }
    }

    fn schedule_load(&mut self, id: Option<CueId>, now: Frames) {
        if let Some(id) = id {
            let due = now + self.timebase.secs_to_frames(AUTO_LOAD_DELAY_SECS);
            self.pending_loads.push((due, id));
        }
    }

    // -- GO ---------------------------------------------------------------------------------

    /// Botón GO: dispara la cue del playhead sin bloquear y avanza el playhead de inmediato.
    pub fn fire_go(&mut self, now: Frames, out: &mut Vec<Effect>) -> GoResult {
        let Some(mut pos) = self.playback_position else { return GoResult::EndOfList };
        loop {
            match self.classify(pos) {
                Exec::Go => break,
                Exec::Halt => {
                    self.set_playhead(Some(pos), out);
                    return GoResult::Disarmed(pos);
                }
                Exec::Skip => match self.next_playback_position_after(pos) {
                    Some(next) => pos = next,
                    None => {
                        self.set_playhead(None, out);
                        return GoResult::EndOfList;
                    }
                },
            }
        }
        if !self.cues[&pos].state().can_fire() {
            // GO sobre una cue atrapada en un slice infinito la deja salir (Devamp) en lugar de ignorarse.
            let trapped = self.slice_status(pos, now).is_some_and(|st| st.vamping);
            return if trapped && self.devamp(pos, now, out) == DevampResult::Requested { GoResult::Devamped(pos) } else { GoResult::Busy(pos) };
        }
        // El playhead se mueve antes de disparar para que un AutoContinue inmediato encuentre su
        // destino como siguiente posición y lo arrastre.
        let next = self.next_playback_position_after(pos);
        self.set_playhead(next, out);
        self.schedule_load(next, now);
        self.trigger(pos, now, TriggerSource::GoButton, false, out);
        GoResult::Fired { cue: pos, next_playhead: next }
    }

    /// Devamp: la cue de audio en marcha sale de su slice actual cuando termina la pasada en curso y sigue
    /// con el siguiente tramo (o el final de la pista) sin saltos. Con un solo Devamp pendiente a la vez.
    pub fn devamp(&mut self, id: CueId, now: Frames, out: &mut Vec<Effect>) -> DevampResult {
        let tb = self.timebase;
        let Some(cue) = self.cues.get_mut(&id) else { return DevampResult::UnknownCue };
        if !cue.is_audio() || cue.state() != CueState::Running {
            return DevampResult::NotRunning;
        }
        let Some(cursor) = cue.slice_cursor_at(tb, now) else { return DevampResult::NotRunning };
        if !cursor.request_devamp() {
            return DevampResult::NothingToEscape;
        }
        out.push(Effect::Devamp { cue: id });
        DevampResult::Requested
    }

    /// Estado de slices de una cue de audio en marcha: `(tramo actual, pasadas restantes, atrapada)`.
    pub fn slice_status(&mut self, id: CueId, now: Frames) -> Option<SliceStatus> {
        let tb = self.timebase;
        let cue = self.cues.get_mut(&id)?;
        if cue.state() != CueState::Running {
            return None;
        }
        let c = cue.slice_cursor_at(tb, now)?;
        Some(SliceStatus { slice: c.current_slice(), loops_left: c.loops_left(), vamping: c.is_vamping(), devamping: c.devamp_pending(), finished: c.is_finished() })
    }

    /// Dispara una cue concreta (API/OSC), sin tocar el playhead.
    pub fn fire_cue(&mut self, id: CueId, now: Frames, out: &mut Vec<Effect>) -> bool {
        self.trigger(id, now, TriggerSource::Api, false, out)
    }

    // -- Núcleo de ejecución ------------------------------------------------------------------

    /// Aplica `f` a la cue y emite `StateChanged` si cambió de estado.
    fn mutate<R>(&mut self, id: CueId, out: &mut Vec<Effect>, f: impl FnOnce(&mut Cue) -> R) -> Option<R> {
        let cue = self.cues.get_mut(&id)?;
        let before = cue.state();
        let r = f(cue);
        let after = cue.state();
        if before != after {
            out.push(Effect::StateChanged { cue: id, from: before, to: after });
        }
        Some(r)
    }

    fn trigger(&mut self, id: CueId, now: Frames, _source: TriggerSource, ignore_pre_wait: bool, out: &mut Vec<Effect>) -> bool {
        let tb = self.timebase;
        let fired = self.mutate(id, out, |c| {
            let timing = c.timing(tb);
            let (end, mode) = (c.action_end(), c.continue_mode());
            c.stopping = false;
            c.slice_track = None;
            c.machine.fire(now, timing, end, mode, ignore_pre_wait)
        });
        let Some(Ok(t)) = fired else { return false };
        if !self.active.contains(&id) {
            self.active.push(id);
        }
        self.handle_transitions(id, t, now, out);
        true
    }

    fn handle_transitions(&mut self, id: CueId, t: Transitions, now: Frames, out: &mut Vec<Effect>) {
        if t.action_started {
            let at = self.cues[&id].machine.action_start();
            self.start_action(id, at, out);
        }
        if t.continue_due {
            self.fire_continue(id, now, out);
        }
        if t.completed {
            self.active.retain(|&c| c != id);
            if let Some(group) = self.parent_of(id) {
                self.check_group_done(group, now, out);
            }
        }
    }

    fn start_action(&mut self, id: CueId, at: Frames, out: &mut Vec<Effect>) {
        match &self.cues[&id].kind {
            CueKind::Audio(_) | CueKind::Video(_) => out.push(Effect::StartAudio { cue: id, at_frames: at }),
            CueKind::Fade(_) => out.push(Effect::StartFade { cue: id, at_frames: at }),
            CueKind::Control(_) => self.run_control(id, at, out),
            CueKind::Memo => {}
            CueKind::Osc(d) => out.push(Effect::SendOsc { cue: id, destination: d.destination.clone(), message: d.message.clone() }),
            CueKind::Group(_) => self.start_group(id, at, out),
        }
    }

    /// Ejecuta una cue de control sobre su objetivo. Un objetivo ausente o que no está en marcha no es un error:
    /// la cue simplemente no hace nada (en escena es preferible a un fallo).
    fn run_control(&mut self, id: CueId, at: Frames, out: &mut Vec<Effect>) {
        let CueKind::Control(d) = &self.cues[&id].kind else { return };
        let (action, Some(target)) = (d.action, d.target) else { return };
        if target == id {
            return;
        }
        let fade_frames = self.timebase.secs_to_frames(self.cues[&id].basics.action_duration).min(u32::MAX as u64) as u32;
        self.apply_control(action, target, at, fade_frames, out);
    }

    /// Aplica una acción de control sobre `target` (la usan las cues de control y el control remoto).
    pub fn apply_control(&mut self, action: ControlAction, target: CueId, at: Frames, fade_frames: u32, out: &mut Vec<Effect>) {
        if !self.cues.contains_key(&target) {
            return;
        }
        match action {
            ControlAction::Stop => self.stop_cue(target, at, fade_frames, out),
            ControlAction::Pause => self.pause_cue(target, at, out),
            ControlAction::Resume => self.resume_cue(target, at, out),
            ControlAction::Devamp => {
                self.devamp(target, at, out);
            }
            ControlAction::GoTo => {
                // Solo las cues de nivel superior pueden ser el playhead; otra cosa se ignora sin error.
                let _ = self.set_playback_position(Some(target), out);
            }
            ControlAction::Load => self.load_cue(target, out),
            ControlAction::Arm | ControlAction::Disarm => {
                let arm = action == ControlAction::Arm;
                if let Some(c) = self.cues.get_mut(&target) {
                    let needs_file = matches!(c.kind, CueKind::Audio(_) | CueKind::Video(_)) && c.basics.file_target.is_none();
                    if (arm && needs_file) || c.basics.armed == arm {
                        return;
                    }
                    c.basics.armed = arm;
                    out.push(Effect::ArmChanged { cue: target });
                }
            }
        }
    }

    fn start_group(&mut self, id: CueId, at: Frames, out: &mut Vec<Effect>) {
        let CueKind::Group(g) = &self.cues[&id].kind else { return };
        let mode = g.mode;
        let executable: Vec<CueId> = g.children.iter().copied().filter(|&c| self.classify(c) == Exec::Go).collect();
        let chosen: Vec<CueId> = match mode {
            GroupMode::FireFirstGoToNext => executable.into_iter().take(1).collect(),
            GroupMode::Timeline | GroupMode::FireAllSimultaneously => executable,
        };
        let ignore_pre = mode == GroupMode::FireAllSimultaneously;
        {
            let group = self.cues.get_mut(&id).expect("el grupo existe");
            group.group_running = chosen.clone();
            group.group_starting = true;
        }
        for child in chosen {
            if !self.trigger(child, at, TriggerSource::Group, ignore_pre, out) {
                if let Some(group) = self.cues.get_mut(&id) {
                    group.group_running.retain(|&c| c != child);
                }
            }
        }
        if let Some(group) = self.cues.get_mut(&id) {
            group.group_starting = false;
        }
        self.check_group_done(id, at, out);
    }

    /// El grupo termina cuando todos los hijos que disparó han terminado.
    fn check_group_done(&mut self, group: CueId, now: Frames, out: &mut Vec<Effect>) {
        let Some(g) = self.cues.get(&group) else { return };
        if g.group_starting || g.state() != CueState::Running || g.machine.is_action_done() {
            return;
        }
        let all_done = g.group_running.iter().all(|c| self.cues.get(c).map_or(true, |c| c.state() == CueState::Completed));
        if all_done {
            if let Some(t) = self.mutate(group, out, |c| c.machine.complete_action(now, true)) {
                self.handle_transitions(group, t, now, out);
            }
        }
    }

    /// Dispara la siguiente cue de la cadena (`AutoContinue` por reloj o `AutoFollow` por fin de voz).
    fn fire_continue(&mut self, id: CueId, now: Frames, out: &mut Vec<Effect>) {
        let cue = &self.cues[&id];
        let mode = cue.continue_mode();
        let when = if mode == ContinueMode::AutoContinue { cue.machine.continue_at().unwrap_or(now) } else { now };
        let source = if mode == ContinueMode::AutoContinue { TriggerSource::AutoContinue } else { TriggerSource::AutoFollow };
        let parent = self.parent_of(id);
        let target = match parent {
            Some(group) => self.chain_target_in_group(group, id),
            None => self.chain_target_top(id),
        };
        let Some(next) = target else { return };
        if !self.cues[&next].state().can_fire() {
            return; // ya la disparó alguien (GO manual): no se dispara dos veces
        }
        // Si el playhead esperaba en `next`, la cadena lo arrastra a la siguiente.
        if parent.is_none() && self.playback_position == Some(next) {
            let after = self.next_playback_position_after(next);
            self.set_playhead(after, out);
            self.schedule_load(after, when);
        }
        if let Some(group) = parent {
            if let Some(g) = self.cues.get_mut(&group) {
                g.group_running.push(next);
            }
        }
        self.trigger(next, when, source, false, out);
    }

    // -- Reloj ------------------------------------------------------------------------------

    /// Avanza todas las cues activas hasta `now`. Repite hasta que no haya más cambios, así una
    /// cadena de `AutoContinue` vencida por un tick tardío se resuelve entera.
    pub fn tick(&mut self, now: Frames, out: &mut Vec<Effect>) {
        let mut due = Vec::new();
        self.pending_loads.retain(|&(t, id)| {
            if t <= now {
                due.push(id);
                false
            } else {
                true
            }
        });
        for id in due {
            self.load_cue(id, out);
        }
        for _ in 0..MAX_TICK_PASSES {
            let mut progressed = false;
            for id in self.active.clone() {
                let Some(t) = self.mutate(id, out, |c| c.machine.tick(now)) else { continue };
                if t.any() {
                    progressed = true;
                    self.handle_transitions(id, t, now, out);
                }
            }
            if !progressed {
                break;
            }
        }
    }

    fn load_cue(&mut self, id: CueId, out: &mut Vec<Effect>) {
        if self.mutate(id, out, |c| c.machine.load()) == Some(true) {
            out.push(Effect::Load { cue: id });
        }
    }

    /// Próximo instante en que `tick` hará algo (para dormir exactamente hasta ahí).
    pub fn next_deadline(&self) -> Option<Frames> {
        let machines = self.active.iter().filter_map(|id| self.cues.get(id)?.machine.next_deadline());
        let loads = self.pending_loads.iter().map(|&(t, _)| t);
        machines.chain(loads).min()
    }

    // -- Eventos del motor ------------------------------------------------------------------

    pub fn on_audio_status(&mut self, now: Frames, status: AudioStatus, out: &mut Vec<Effect>) {
        match status {
            AudioStatus::VoiceCompleted { voice_id, natural_eof } => {
                let id = CueId(voice_id);
                let running = self.cues.get(&id).is_some_and(|c| c.is_audio() && c.state() == CueState::Running);
                if running {
                    if let Some(t) = self.mutate(id, out, |c| c.machine.complete_action(now, natural_eof)) {
                        self.handle_transitions(id, t, now, out);
                    }
                } else {
                    out.push(Effect::Engine(status));
                }
            }
            other => out.push(Effect::Engine(other)),
        }
    }

    // -- Pausa, reanudación y parada ----------------------------------------------------------

    /// Cues de nivel superior activas.
    fn active_top_level(&self) -> Vec<CueId> {
        self.active.iter().copied().filter(|id| !self.parents.contains_key(id)).collect()
    }

    pub fn pause_cue(&mut self, id: CueId, now: Frames, out: &mut Vec<Effect>) {
        if self.mutate(id, out, |c| c.machine.pause(now)) != Some(Ok(())) {
            return;
        }
        let cue = &self.cues[&id];
        let voice_started = cue.machine.effective_state() != CueState::PreWaiting;
        match &cue.kind {
            CueKind::Audio(_) | CueKind::Video(_) if voice_started => out.push(Effect::PauseVoice { cue: id }),
            CueKind::Group(_) => {
                for child in cue.group_running.clone() {
                    if self.state(child).is_some_and(|s| s.is_active() && s != CueState::Paused) {
                        self.pause_cue(child, now, out);
                    }
                }
            }
            // El fade ya enviado al motor no se puede pausar; ver la documentación del scheduler.
            _ => {}
        }
    }

    pub fn resume_cue(&mut self, id: CueId, now: Frames, out: &mut Vec<Effect>) {
        let was = self.cues.get(&id).map(|c| c.machine.effective_state());
        if self.mutate(id, out, |c| c.machine.resume(now)) != Some(Ok(())) {
            return;
        }
        let cue = &self.cues[&id];
        match &cue.kind {
            CueKind::Audio(_) | CueKind::Video(_) if was != Some(CueState::PreWaiting) => out.push(Effect::ResumeVoice { cue: id }),
            CueKind::Group(_) => {
                for child in cue.group_running.clone() {
                    if self.state(child) == Some(CueState::Paused) {
                        self.resume_cue(child, now, out);
                    }
                }
            }
            _ => {}
        }
    }

    /// Detiene una cue. El audio en marcha hace tail-out de `fade_frames` y la cue no termina
    /// hasta que el motor confirma el fin de la voz; el resto termina al instante.
    pub fn stop_cue(&mut self, id: CueId, now: Frames, fade_frames: u32, out: &mut Vec<Effect>) {
        let Some(cue) = self.cues.get(&id) else { return };
        let state = cue.state();
        if !state.is_active() {
            return;
        }
        let voice_started = cue.machine.effective_state() != CueState::PreWaiting;
        match &cue.kind {
            CueKind::Audio(_) | CueKind::Video(_) => {
                if voice_started && !cue.machine.is_action_done() {
                    out.push(Effect::StopVoice { cue: id, fade_frames });
                    if state == CueState::Paused {
                        // Una voz en pausa ya está en silencio: no habrá confirmación que esperar.
                        self.hard_stop(id, now, out);
                    } else if let Some(c) = self.cues.get_mut(&id) {
                        c.stopping = true;
                    }
                } else {
                    self.hard_stop(id, now, out);
                }
            }
            CueKind::Fade(_) => {
                out.push(Effect::CancelFade { cue: id });
                self.hard_stop(id, now, out);
            }
            CueKind::Control(_) | CueKind::Osc(_) | CueKind::Memo => self.hard_stop(id, now, out),
            CueKind::Group(_) => {
                for child in cue.group_running.clone() {
                    self.stop_cue(child, now, fade_frames, out);
                }
                self.hard_stop(id, now, out);
            }
        }
    }

    fn hard_stop(&mut self, id: CueId, now: Frames, out: &mut Vec<Effect>) {
        if let Some(t) = self.mutate(id, out, |c| c.machine.stop(now)) {
            self.handle_transitions(id, t, now, out);
        }
    }

    pub fn pause_all(&mut self, now: Frames, out: &mut Vec<Effect>) {
        for id in self.active_top_level() {
            self.pause_cue(id, now, out);
        }
    }

    pub fn resume_all(&mut self, now: Frames, out: &mut Vec<Effect>) {
        for id in self.active_top_level() {
            self.resume_cue(id, now, out);
        }
    }

    /// Panic / Stop All: detiene todo lo que esté en marcha.
    pub fn stop_all(&mut self, now: Frames, fade_frames: u32, out: &mut Vec<Effect>) {
        for id in self.active_top_level() {
            self.stop_cue(id, now, fade_frames, out);
        }
    }

    // -- Consultas --------------------------------------------------------------------------

    /// Duración de la línea de tiempo de un grupo: `max(pre + acción + post)` de sus hijos
    /// ejecutables. `None` si algún hijo tiene duración desconocida.
    pub fn group_timeline_frames(&self, group: CueId) -> Option<Frames> {
        let CueKind::Group(g) = &self.cues.get(&group)?.kind else { return None };
        let mut longest = 0;
        for &child in &g.children {
            if self.classify(child) != Exec::Go {
                continue;
            }
            let c = &self.cues[&child];
            let total = match &c.kind {
                CueKind::Group(_) => {
                    let inner = self.group_timeline_frames(child)?;
                    let t = c.timing(self.timebase);
                    t.pre_wait + inner.max(t.post_wait)
                }
                _ => c.timing(self.timebase).total()?,
            };
            longest = longest.max(total);
        }
        Some(longest)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::cue::{AudioCueData, FadeCueData, GroupCueData};
    use qlab_protocol::cue_dto::{AudioSliceMarker, AudioTimeAndLoopsData, CueBasicsData, INFINITE_SLICE_LOOP};
    use qlab_protocol::matrix::AudioLevelMatrixData;

    pub(crate) const FS: u64 = 48_000;

    pub(crate) fn tb() -> Timebase {
        Timebase::new(48_000)
    }

    pub(crate) fn audio_cue(id: u64, pre: f64, post: f64, mode: ContinueMode) -> Cue {
        let basics = CueBasicsData {
            unique_id: CueId(id),
            number: id.to_string(),
            pre_wait: pre,
            post_wait: post,
            continue_mode: mode,
            ..Default::default()
        };
        let data = AudioCueData::new(
            AudioTimeAndLoopsData { end_time_sec: 10.0, ..Default::default() },
            AudioLevelMatrixData::new_one_to_one(2, 2).unwrap(),
        );
        Cue::audio(basics, data)
    }

    pub(crate) fn fade_cue(id: u64, pre: f64, duration: f64, target: u64) -> Cue {
        let basics = CueBasicsData {
            unique_id: CueId(id),
            number: id.to_string(),
            pre_wait: pre,
            action_duration: duration,
            ..Default::default()
        };
        Cue::fade(basics, FadeCueData::new(CueId(target)))
    }

    pub(crate) fn group_cue(id: u64, mode: GroupMode) -> Cue {
        let basics = CueBasicsData { unique_id: CueId(id), number: id.to_string(), ..Default::default() };
        Cue::group(basics, GroupCueData::new(mode, vec![]))
    }

    fn list_of(cues: Vec<Cue>) -> CueList {
        let mut l = CueList::new(1, tb());
        for c in cues {
            l.add_cue(c).unwrap();
        }
        l
    }

    fn starts(out: &[Effect]) -> Vec<(u64, Frames)> {
        out.iter()
            .filter_map(|e| match *e {
                Effect::StartAudio { cue, at_frames } | Effect::StartFade { cue, at_frames } => Some((cue.0, at_frames)),
                _ => None,
            })
            .collect()
    }

    fn playheads(out: &[Effect]) -> Vec<Option<u64>> {
        out.iter()
            .filter_map(|e| match *e {
                Effect::PlayheadMoved { position, .. } => Some(position.map(|c| c.0)),
                _ => None,
            })
            .collect()
    }

    fn go(l: &mut CueList, now: Frames) -> (GoResult, Vec<Effect>) {
        let mut out = Vec::new();
        let r = l.fire_go(now, &mut out);
        (r, out)
    }

    fn tick(l: &mut CueList, now: Frames) -> Vec<Effect> {
        let mut out = Vec::new();
        l.tick(now, &mut out);
        out
    }

    use ContinueMode::*;

    #[test]
    fn go_fires_the_playhead_cue_and_advances_immediately() {
        let mut l = list_of(vec![audio_cue(1, 0.0, 0.0, DoNotContinue), audio_cue(2, 0.0, 0.0, DoNotContinue), audio_cue(3, 0.0, 0.0, DoNotContinue)]);
        assert_eq!(l.playback_position(), Some(CueId(1)));
        let (r, out) = go(&mut l, 1_000);
        assert_eq!(r, GoResult::Fired { cue: CueId(1), next_playhead: Some(CueId(2)) });
        assert_eq!(starts(&out), vec![(1, 1_000)]);
        assert_eq!(playheads(&out), vec![Some(2)]);
        assert_eq!(l.playback_position(), Some(CueId(2)));
        assert_eq!(l.state(CueId(1)), Some(CueState::Running));
        assert_eq!(l.state(CueId(2)), Some(CueState::Idle), "solo el playhead avanza; la siguiente espera su GO");
        let (r, _) = go(&mut l, 2_000);
        assert_eq!(r, GoResult::Fired { cue: CueId(2), next_playhead: Some(CueId(3)) });
        let (r, out) = go(&mut l, 3_000);
        assert_eq!(r, GoResult::Fired { cue: CueId(3), next_playhead: None });
        assert_eq!(playheads(&out), vec![None]);
        assert_eq!(l.playback_position(), None);
        assert_eq!(go(&mut l, 4_000).0, GoResult::EndOfList);
        assert_eq!(l.active_ids().len(), 3);
    }

    #[test]
    fn go_on_an_empty_list() {
        let mut l = CueList::new(1, tb());
        assert_eq!(go(&mut l, 0).0, GoResult::EndOfList);
    }

    #[test]
    fn pre_wait_defers_the_action_but_not_the_playhead() {
        let mut l = list_of(vec![audio_cue(1, 0.5, 0.0, DoNotContinue), audio_cue(2, 0.0, 0.0, DoNotContinue)]);
        let (_, out) = go(&mut l, 10_000);
        assert!(starts(&out).is_empty(), "con pre-wait no suena todavía");
        assert_eq!(l.playback_position(), Some(CueId(2)), "el playhead ya avanzó");
        assert_eq!(l.state(CueId(1)), Some(CueState::PreWaiting));
        assert_eq!(l.next_deadline(), Some(10_000 + 7_200), "primero vence la pre-carga de la siguiente cue");
        tick(&mut l, 10_000 + 7_200);
        assert_eq!(l.next_deadline(), Some(10_000 + 24_000));
        assert!(starts(&tick(&mut l, 10_000 + 23_999)).is_empty());
        let out = tick(&mut l, 10_000 + 24_000);
        assert_eq!(starts(&out), vec![(1, 34_000)]);
        assert_eq!(l.state(CueId(1)), Some(CueState::Running));
    }

    #[test]
    fn go_skips_dead_and_skip_if_disarmed_cues() {
        let mut dead = audio_cue(2, 0.0, 0.0, DoNotContinue);
        dead.dead = true;
        let mut skipped = audio_cue(3, 0.0, 0.0, DoNotContinue);
        skipped.basics.armed = false;
        skipped.basics.skip_if_disarmed = true;
        let mut l = list_of(vec![audio_cue(1, 0.0, 0.0, DoNotContinue), dead, skipped, audio_cue(4, 0.0, 0.0, DoNotContinue)]);
        go(&mut l, 0);
        assert_eq!(l.playback_position(), Some(CueId(2)), "el playhead se estaciona en la siguiente tal cual");
        let (r, out) = go(&mut l, 100);
        assert_eq!(r, GoResult::Fired { cue: CueId(4), next_playhead: None });
        assert_eq!(starts(&out), vec![(4, 100)]);
        assert_eq!(l.state(CueId(2)), Some(CueState::Idle));
        assert_eq!(l.state(CueId(3)), Some(CueState::Idle));
    }

    #[test]
    fn go_skipping_to_the_end_empties_the_playhead() {
        let mut dead = audio_cue(2, 0.0, 0.0, DoNotContinue);
        dead.dead = true;
        let mut l = list_of(vec![audio_cue(1, 0.0, 0.0, DoNotContinue), dead]);
        go(&mut l, 0);
        let (r, out) = go(&mut l, 10);
        assert_eq!(r, GoResult::EndOfList);
        assert_eq!(playheads(&out), vec![None]);
    }

    #[test]
    fn go_halts_on_a_disarmed_cue_without_skip() {
        let mut off = audio_cue(2, 0.0, 0.0, DoNotContinue);
        off.basics.armed = false;
        let mut l = list_of(vec![audio_cue(1, 0.0, 0.0, DoNotContinue), off, audio_cue(3, 0.0, 0.0, DoNotContinue)]);
        go(&mut l, 0);
        for t in [10, 20, 30] {
            let (r, out) = go(&mut l, t);
            assert_eq!(r, GoResult::Disarmed(CueId(2)));
            assert!(starts(&out).is_empty());
            assert_eq!(l.playback_position(), Some(CueId(2)));
        }
        l.cue_mut(CueId(2)).unwrap().basics.armed = true;
        assert!(matches!(go(&mut l, 40).0, GoResult::Fired { cue: CueId(2), .. }));
    }

    #[test]
    fn go_on_a_running_cue_is_busy_and_does_not_move_the_playhead() {
        let mut l = list_of(vec![audio_cue(1, 0.0, 0.0, DoNotContinue), audio_cue(2, 0.0, 0.0, DoNotContinue)]);
        go(&mut l, 0);
        let mut out = Vec::new();
        l.set_playback_position(Some(CueId(1)), &mut out).unwrap();
        let (r, out) = go(&mut l, 10);
        assert_eq!(r, GoResult::Busy(CueId(1)));
        assert!(out.is_empty());
        assert_eq!(l.playback_position(), Some(CueId(1)));
        assert!(l.set_playback_position(Some(CueId(99)), &mut Vec::new()).is_err());
    }

    #[test]
    fn auto_continue_chain_fires_on_pre_plus_post_and_drags_the_playhead() {
        // A(AC, post 1 s) → B(AC, post 0,5 s) → C
        let mut l = list_of(vec![
            audio_cue(1, 0.0, 1.0, AutoContinue),
            audio_cue(2, 0.0, 0.5, AutoContinue),
            audio_cue(3, 0.0, 0.0, DoNotContinue),
        ]);
        let (_, out) = go(&mut l, 0);
        assert_eq!(starts(&out), vec![(1, 0)]);
        assert_eq!(l.playback_position(), Some(CueId(2)));
        assert_eq!(l.next_deadline(), Some(7_200), "primero la pre-carga de B");
        tick(&mut l, 7_200);
        assert_eq!(l.next_deadline(), Some(FS), "el siguiente disparo es A → B");

        assert!(starts(&tick(&mut l, FS - 1)).is_empty());
        let out = tick(&mut l, FS);
        assert_eq!(starts(&out), vec![(2, FS)]);
        assert_eq!(playheads(&out), vec![Some(3)]);
        assert_eq!(l.state(CueId(1)), Some(CueState::Running), "A sigue sonando en paralelo");

        let out = tick(&mut l, FS + 24_000);
        assert_eq!(starts(&out), vec![(3, FS + 24_000)]);
        assert_eq!(playheads(&out), vec![None]);
        assert_eq!(go(&mut l, FS + 30_000).0, GoResult::EndOfList);
    }

    #[test]
    fn auto_continue_counts_the_pre_wait_and_uses_the_exact_scheduled_instant() {
        let mut l = list_of(vec![audio_cue(1, 1.0, 2.0, AutoContinue), audio_cue(2, 0.0, 0.0, DoNotContinue)]);
        go(&mut l, 5_000);
        tick(&mut l, 5_000 + 7_200); // pre-carga de B
        assert_eq!(l.next_deadline(), Some(5_000 + FS), "primero arranca la acción de A");
        tick(&mut l, 5_000 + FS);
        assert_eq!(l.next_deadline(), Some(5_000 + 3 * FS));
        // Tick muy tardío: el disparo se atribuye al instante programado, no al del tick.
        let out = tick(&mut l, 5_000 + 10 * FS);
        assert_eq!(starts(&out), vec![(2, 5_000 + 3 * FS)]);
    }

    #[test]
    fn a_late_tick_resolves_the_whole_chain() {
        let mut l = list_of(vec![
            audio_cue(1, 0.0, 1.0, AutoContinue),
            audio_cue(2, 0.0, 1.0, AutoContinue),
            audio_cue(3, 0.0, 1.0, AutoContinue),
            audio_cue(4, 0.0, 0.0, DoNotContinue),
        ]);
        go(&mut l, 0);
        let out = tick(&mut l, 100 * FS);
        assert_eq!(starts(&out), vec![(2, FS), (3, 2 * FS), (4, 3 * FS)]);
        assert_eq!(l.playback_position(), None);
    }

    #[test]
    fn auto_continue_with_zero_post_wait_fires_in_the_same_go() {
        let mut l = list_of(vec![audio_cue(1, 0.0, 0.0, AutoContinue), audio_cue(2, 0.0, 0.0, AutoContinue), audio_cue(3, 0.0, 0.0, DoNotContinue)]);
        let (_, out) = go(&mut l, 700);
        assert_eq!(starts(&out), vec![(1, 700), (2, 700), (3, 700)]);
        assert_eq!(l.playback_position(), None);
        assert_eq!(playheads(&out).last(), Some(&None));
    }

    #[test]
    fn auto_follow_waits_for_the_natural_end_reported_by_the_engine() {
        let mut l = list_of(vec![audio_cue(1, 0.0, 9.0, AutoFollow), audio_cue(2, 0.0, 0.0, DoNotContinue)]);
        go(&mut l, 0);
        assert!(starts(&tick(&mut l, 1_000 * FS)).is_empty(), "AutoFollow no depende del reloj ni del post-wait");
        let mut out = Vec::new();
        l.on_audio_status(123_456, AudioStatus::VoiceCompleted { voice_id: 1, natural_eof: true }, &mut out);
        assert_eq!(starts(&out), vec![(2, 123_456)]);
        assert_eq!(l.state(CueId(1)), Some(CueState::Completed));
        assert_eq!(l.playback_position(), None);
    }

    #[test]
    fn auto_follow_does_not_chain_after_a_stop() {
        let mut l = list_of(vec![audio_cue(1, 0.0, 0.0, AutoFollow), audio_cue(2, 0.0, 0.0, DoNotContinue)]);
        go(&mut l, 0);
        let mut out = Vec::new();
        l.on_audio_status(500, AudioStatus::VoiceCompleted { voice_id: 1, natural_eof: false }, &mut out);
        assert!(starts(&out).is_empty());
        assert_eq!(l.state(CueId(1)), Some(CueState::Completed));
        assert_eq!(l.playback_position(), Some(CueId(2)), "el playhead sigue esperando el GO");
    }

    #[test]
    fn auto_follow_chain_drags_the_playhead_and_pre_wait_applies_to_the_next() {
        let mut l = list_of(vec![
            audio_cue(1, 0.0, 0.0, AutoFollow),
            audio_cue(2, 0.25, 0.0, AutoFollow),
            audio_cue(3, 0.0, 0.0, DoNotContinue),
        ]);
        go(&mut l, 0);
        let mut out = Vec::new();
        l.on_audio_status(1_000, AudioStatus::VoiceCompleted { voice_id: 1, natural_eof: true }, &mut out);
        assert!(starts(&out).is_empty(), "B espera su pre-wait");
        assert_eq!(l.playback_position(), Some(CueId(3)));
        let out = tick(&mut l, 1_000 + 12_000);
        assert_eq!(starts(&out), vec![(2, 13_000)]);
        let mut out = Vec::new();
        l.on_audio_status(50_000, AudioStatus::VoiceCompleted { voice_id: 2, natural_eof: true }, &mut out);
        assert_eq!(starts(&out), vec![(3, 50_000)]);
    }

    #[test]
    fn chain_halts_at_a_disarmed_cue_and_skips_dead_ones() {
        let mut dead = audio_cue(2, 0.0, 0.0, DoNotContinue);
        dead.dead = true;
        let mut off = audio_cue(4, 0.0, 0.0, DoNotContinue);
        off.basics.armed = false;
        let mut l = list_of(vec![
            audio_cue(1, 0.0, 0.0, AutoContinue),
            dead,
            audio_cue(3, 0.0, 0.0, AutoContinue),
            off,
            audio_cue(5, 0.0, 0.0, DoNotContinue),
        ]);
        let (_, out) = go(&mut l, 0);
        assert_eq!(starts(&out), vec![(1, 0), (3, 0)], "salta la muerta; se detiene antes de la desarmada");
        assert_eq!(l.state(CueId(4)), Some(CueState::Idle));
        assert_eq!(l.state(CueId(5)), Some(CueState::Idle));
    }

    #[test]
    fn manual_go_before_the_chain_fires_does_not_double_trigger() {
        let mut l = list_of(vec![audio_cue(1, 0.0, 2.0, AutoContinue), audio_cue(2, 0.0, 0.0, DoNotContinue), audio_cue(3, 0.0, 0.0, DoNotContinue)]);
        go(&mut l, 0);
        let (r, _) = go(&mut l, FS / 2); // el operador adelanta a B con GO
        assert_eq!(r, GoResult::Fired { cue: CueId(2), next_playhead: Some(CueId(3)) });
        let out = tick(&mut l, 2 * FS);
        assert!(starts(&out).is_empty(), "el AutoContinue de A no vuelve a disparar B");
        assert_eq!(l.playback_position(), Some(CueId(3)));
    }

    #[test]
    fn pause_freezes_the_chain_and_resume_shifts_it() {
        let mut l = list_of(vec![audio_cue(1, 0.0, 2.0, AutoContinue), audio_cue(2, 0.0, 0.0, DoNotContinue)]);
        go(&mut l, 0);
        let mut out = Vec::new();
        l.pause_cue(CueId(1), FS, &mut out); // 1 s de los 2 s de post-wait
        assert!(out.contains(&Effect::PauseVoice { cue: CueId(1) }));
        assert_eq!(l.state(CueId(1)), Some(CueState::Paused));
        assert!(starts(&tick(&mut l, 100 * FS)).is_empty());
        assert_eq!(l.next_deadline(), None);
        let mut out = Vec::new();
        l.resume_cue(CueId(1), 11 * FS, &mut out);
        assert!(out.contains(&Effect::ResumeVoice { cue: CueId(1) }));
        assert_eq!(l.next_deadline(), Some(12 * FS), "queda 1 s tras reanudar");
        let out = tick(&mut l, 12 * FS);
        assert_eq!(starts(&out), vec![(2, 12 * FS)]);
    }

    #[test]
    fn pause_during_pre_wait_does_not_touch_the_voice() {
        let mut l = list_of(vec![audio_cue(1, 1.0, 0.0, DoNotContinue)]);
        go(&mut l, 0);
        let mut out = Vec::new();
        l.pause_cue(CueId(1), FS / 2, &mut out);
        assert!(!out.iter().any(|e| matches!(e, Effect::PauseVoice { .. })));
        let mut out = Vec::new();
        l.resume_cue(CueId(1), 10 * FS, &mut out);
        assert!(!out.iter().any(|e| matches!(e, Effect::ResumeVoice { .. })));
        assert_eq!(starts(&tick(&mut l, 10 * FS + FS / 2)), vec![(1, 10 * FS + FS / 2)]);
    }

    #[test]
    fn stop_audio_tails_out_until_the_engine_confirms() {
        let mut l = list_of(vec![audio_cue(1, 0.0, 0.0, AutoFollow), audio_cue(2, 0.0, 0.0, DoNotContinue)]);
        go(&mut l, 0);
        let mut out = Vec::new();
        l.stop_cue(CueId(1), 100, 240, &mut out);
        assert!(out.contains(&Effect::StopVoice { cue: CueId(1), fade_frames: 240 }));
        assert_eq!(l.state(CueId(1)), Some(CueState::Running));
        assert!(l.cue(CueId(1)).unwrap().stopping);
        let mut out = Vec::new();
        l.on_audio_status(340, AudioStatus::VoiceCompleted { voice_id: 1, natural_eof: false }, &mut out);
        assert_eq!(l.state(CueId(1)), Some(CueState::Completed));
        assert!(starts(&out).is_empty());
    }

    #[test]
    fn stop_during_pre_wait_and_stop_all() {
        let mut l = list_of(vec![
            audio_cue(1, 1.0, 0.0, DoNotContinue),
            audio_cue(2, 0.0, 0.0, DoNotContinue),
            fade_cue(3, 0.0, 5.0, 2),
        ]);
        go(&mut l, 0);
        go(&mut l, 10);
        go(&mut l, 20);
        let mut out = Vec::new();
        l.stop_all(100, 96, &mut out);
        assert_eq!(l.state(CueId(1)), Some(CueState::Completed), "sin voz: se corta al instante");
        assert_eq!(l.state(CueId(2)), Some(CueState::Running), "con voz: espera la confirmación");
        assert_eq!(l.state(CueId(3)), Some(CueState::Completed));
        assert!(out.contains(&Effect::StopVoice { cue: CueId(2), fade_frames: 96 }));
        assert!(out.contains(&Effect::CancelFade { cue: CueId(3) }));
        assert!(!starts(&tick(&mut l, 1_000 * FS)).iter().any(|&(c, _)| c == 1), "la cue parada no arranca después");
    }

    #[test]
    fn pause_all_and_resume_all() {
        let mut l = list_of(vec![audio_cue(1, 0.0, 0.0, DoNotContinue), audio_cue(2, 0.0, 0.0, DoNotContinue)]);
        go(&mut l, 0);
        go(&mut l, 0);
        let mut out = Vec::new();
        l.pause_all(100, &mut out);
        assert_eq!(out.iter().filter(|e| matches!(e, Effect::PauseVoice { .. })).count(), 2);
        let mut out = Vec::new();
        l.resume_all(200, &mut out);
        assert_eq!(out.iter().filter(|e| matches!(e, Effect::ResumeVoice { .. })).count(), 2);
        assert_eq!(l.state(CueId(1)), Some(CueState::Running));
    }

    #[test]
    fn next_cue_is_preloaded_after_the_auto_load_delay() {
        let mut l = list_of(vec![audio_cue(1, 0.0, 0.0, DoNotContinue), audio_cue(2, 0.0, 0.0, DoNotContinue)]);
        go(&mut l, 1_000);
        let delay = (AUTO_LOAD_DELAY_SECS * FS as f64) as u64; // 7200 frames
        assert_eq!(l.next_deadline(), Some(1_000 + delay));
        assert!(tick(&mut l, 1_000 + delay - 1).is_empty());
        let out = tick(&mut l, 1_000 + delay);
        assert!(out.contains(&Effect::Load { cue: CueId(2) }));
        assert_eq!(l.state(CueId(2)), Some(CueState::Armed));
        assert!(matches!(go(&mut l, 20_000).0, GoResult::Fired { cue: CueId(2), .. }), "se puede disparar desde Armed");
    }

    #[test]
    fn state_changes_are_reported() {
        let mut l = list_of(vec![fade_cue(1, 0.0, 1.0, 9)]);
        let (_, out) = go(&mut l, 0);
        assert!(out.contains(&Effect::StateChanged { cue: CueId(1), from: CueState::Idle, to: CueState::Running }));
        let out = tick(&mut l, FS);
        assert!(out.contains(&Effect::StateChanged { cue: CueId(1), from: CueState::Running, to: CueState::Completed }));
    }

    #[test]
    fn fade_cue_is_timed_and_completes_at_its_duration() {
        let mut l = list_of(vec![fade_cue(1, 0.5, 2.0, 9)]);
        let (_, out) = go(&mut l, 0);
        assert!(starts(&out).is_empty());
        let out = tick(&mut l, 24_000);
        assert_eq!(starts(&out), vec![(1, 24_000)]);
        assert_eq!(l.next_deadline(), Some(24_000 + 2 * FS));
        assert_eq!(l.state(CueId(1)), Some(CueState::Running));
        tick(&mut l, 24_000 + 2 * FS);
        assert_eq!(l.state(CueId(1)), Some(CueState::Completed));
        assert!(l.active_ids().is_empty());
    }

    #[test]
    fn short_audio_in_an_auto_continue_waits_in_post_waiting() {
        // Fade de 1 s con post-wait de 3 s: termina la acción y espera el resto.
        let mut c = fade_cue(1, 0.0, 1.0, 9);
        c.basics.continue_mode = AutoContinue;
        c.basics.post_wait = 3.0;
        let mut l = list_of(vec![c, audio_cue(2, 0.0, 0.0, DoNotContinue)]);
        go(&mut l, 0);
        tick(&mut l, FS);
        assert_eq!(l.state(CueId(1)), Some(CueState::PostWaiting));
        let out = tick(&mut l, 3 * FS);
        assert_eq!(starts(&out), vec![(2, 3 * FS)]);
        assert_eq!(l.state(CueId(1)), Some(CueState::Completed));
    }

    #[test]
    fn update_cue_replaces_data_but_keeps_state_and_children() {
        let mut l = list_of(vec![audio_cue(1, 0.0, 0.0, DoNotContinue), group_cue(5, GroupMode::Timeline)]);
        l.add_child(CueId(5), audio_cue(6, 0.0, 0.0, DoNotContinue)).unwrap();
        go(&mut l, 0);
        let running = l.cue(CueId(1)).unwrap().clone();
        let mut basics = running.basics.clone();
        basics.name = "renombrada".into();
        basics.post_wait = 3.0;
        let mut triggers = running.triggers.clone();
        triggers.duck_others_enabled = true;
        let mut kind = running.kind.clone();
        if let CueKind::Audio(a) = &mut kind {
            a.time_loops.rate = 2.0;
        }
        let old = l.update_cue(CueId(1), basics, triggers, kind).unwrap();
        assert!(matches!(old, CueKind::Audio(a) if a.time_loops.rate == 1.0));
        let c = l.cue(CueId(1)).unwrap().clone();
        assert_eq!((c.basics.name.as_str(), c.state()), ("renombrada", CueState::Running), "sigue corriendo");
        assert!(c.triggers.duck_others_enabled);
        assert!(matches!(&c.kind, CueKind::Audio(a) if a.time_loops.rate == 2.0));

        // Un grupo conserva a sus hijos aunque la edición traiga la lista vacía; solo cambia el modo.
        let g = l.cue(CueId(5)).unwrap().clone();
        let new_kind = CueKind::Group(GroupCueData::new(GroupMode::FireAllSimultaneously, vec![]));
        l.update_cue(CueId(5), g.basics.clone(), g.triggers.clone(), new_kind).unwrap();
        assert!(matches!(&l.cue(CueId(5)).unwrap().kind, CueKind::Group(g) if g.mode == GroupMode::FireAllSimultaneously && g.children == [CueId(6)]));

        // Errores: cambiar el tipo, el id, o editar una cue inexistente.
        assert_eq!(l.update_cue(CueId(1), c.basics.clone(), c.triggers.clone(), g.kind.clone()).err(), Some(ListError::KindMismatch(CueId(1))));
        let mut wrong_id = c.basics.clone();
        wrong_id.unique_id = CueId(77);
        assert_eq!(l.update_cue(CueId(1), wrong_id, c.triggers.clone(), c.kind.clone()).err(), Some(ListError::KindMismatch(CueId(1))));
        assert_eq!(l.update_cue(CueId(99), c.basics.clone(), c.triggers.clone(), c.kind.clone()).err(), Some(ListError::UnknownCue(CueId(99))));
    }

    #[test]
    fn update_cue_keeps_the_fade_target_in_sync_with_the_basics() {
        let mut l = list_of(vec![audio_cue(1, 0.0, 0.0, DoNotContinue), fade_cue(2, 0.0, 1.0, 1)]);
        let f = l.cue(CueId(2)).unwrap().clone();
        let mut basics = f.basics.clone();
        basics.cue_target_id = Some(CueId(1));
        l.update_cue(CueId(2), basics, f.triggers.clone(), f.kind.clone()).unwrap();
        assert!(matches!(&l.cue(CueId(2)).unwrap().kind, CueKind::Fade(d) if d.target == Some(CueId(1))));
    }

    #[test]
    fn duplicate_and_invalid_structure_errors() {
        let mut l = list_of(vec![audio_cue(1, 0.0, 0.0, DoNotContinue)]);
        assert_eq!(l.add_cue(audio_cue(1, 0.0, 0.0, DoNotContinue)), Err(ListError::DuplicateId(CueId(1))));
        assert_eq!(l.add_child(CueId(1), audio_cue(2, 0.0, 0.0, DoNotContinue)), Err(ListError::NotAGroup(CueId(1))));
        assert_eq!(l.add_child(CueId(9), audio_cue(2, 0.0, 0.0, DoNotContinue)), Err(ListError::UnknownCue(CueId(9))));
        l.add_cue(group_cue(5, GroupMode::Timeline)).unwrap();
        l.add_child(CueId(5), audio_cue(6, 0.0, 0.0, DoNotContinue)).unwrap();
        assert_eq!(l.add_child(CueId(5), audio_cue(6, 0.0, 0.0, DoNotContinue)), Err(ListError::DuplicateId(CueId(6))));
        assert_eq!(l.order(), &[CueId(1), CueId(5)], "los hijos no están en el nivel superior");
        assert_eq!(l.parent_of(CueId(6)), Some(CueId(5)));
        assert!(!ListError::UnknownCue(CueId(1)).to_string().is_empty());
    }

    // -- Grupos ---------------------------------------------------------------------------

    fn group_list(mode: GroupMode, children: Vec<Cue>) -> CueList {
        let mut l = list_of(vec![group_cue(10, mode), audio_cue(20, 0.0, 0.0, DoNotContinue)]);
        for c in children {
            l.add_child(CueId(10), c).unwrap();
        }
        l
    }

    fn complete(l: &mut CueList, voice: u64, now: Frames) -> Vec<Effect> {
        let mut out = Vec::new();
        l.on_audio_status(now, AudioStatus::VoiceCompleted { voice_id: voice, natural_eof: true }, &mut out);
        out
    }

    #[test]
    fn timeline_group_fires_all_children_with_their_own_pre_wait_offsets() {
        let mut l = group_list(
            GroupMode::Timeline,
            vec![audio_cue(11, 0.0, 0.0, DoNotContinue), audio_cue(12, 1.0, 0.0, DoNotContinue), fade_cue(13, 0.0, 2.0, 11)],
        );
        let (r, out) = go(&mut l, 1_000);
        assert_eq!(r, GoResult::Fired { cue: CueId(10), next_playhead: Some(CueId(20)) }, "el playhead sale del grupo");
        assert_eq!(starts(&out), vec![(11, 1_000), (13, 1_000)]);
        assert_eq!(l.state(CueId(10)), Some(CueState::Running));
        assert_eq!(l.state(CueId(12)), Some(CueState::PreWaiting));
        let out = tick(&mut l, 1_000 + FS);
        assert_eq!(starts(&out), vec![(12, 1_000 + FS)]);

        // El grupo no termina hasta que acaba el último hijo.
        tick(&mut l, 1_000 + 2 * FS);
        assert_eq!(l.state(CueId(13)), Some(CueState::Completed));
        complete(&mut l, 11, 5 * FS);
        assert_eq!(l.state(CueId(10)), Some(CueState::Running));
        complete(&mut l, 12, 7 * FS);
        assert_eq!(l.state(CueId(10)), Some(CueState::Completed));
        assert!(l.active_ids().is_empty());
    }

    #[test]
    fn simultaneous_group_ignores_child_pre_waits() {
        let mut l = group_list(
            GroupMode::FireAllSimultaneously,
            vec![audio_cue(11, 0.0, 0.0, DoNotContinue), audio_cue(12, 5.0, 0.0, DoNotContinue)],
        );
        let (_, out) = go(&mut l, 700);
        assert_eq!(starts(&out), vec![(11, 700), (12, 700)]);
    }

    #[test]
    fn group_pre_wait_delays_every_child() {
        let mut g = group_cue(10, GroupMode::Timeline);
        g.basics.pre_wait = 1.0;
        let mut l = list_of(vec![g]);
        l.add_child(CueId(10), audio_cue(11, 0.5, 0.0, DoNotContinue)).unwrap();
        let (_, out) = go(&mut l, 0);
        assert!(starts(&out).is_empty());
        let out = tick(&mut l, FS);
        assert_eq!(starts(&out), vec![], "el hijo arranca tras su propio pre-wait");
        let out = tick(&mut l, FS + FS / 2);
        assert_eq!(starts(&out), vec![(11, FS + FS / 2)]);
    }

    #[test]
    fn fire_first_group_starts_one_child_and_the_playhead_leaves_the_group() {
        let mut l = group_list(
            GroupMode::FireFirstGoToNext,
            vec![audio_cue(11, 0.0, 0.0, DoNotContinue), audio_cue(12, 0.0, 0.0, DoNotContinue)],
        );
        let (r, out) = go(&mut l, 0);
        assert_eq!(r, GoResult::Fired { cue: CueId(10), next_playhead: Some(CueId(20)) });
        assert_eq!(starts(&out), vec![(11, 0)], "solo el primer hijo");
        assert_eq!(l.state(CueId(12)), Some(CueState::Idle));
        complete(&mut l, 11, FS);
        assert_eq!(l.state(CueId(10)), Some(CueState::Completed), "sin cadena, termina con su único hijo");
        assert_eq!(l.state(CueId(12)), Some(CueState::Idle));
    }

    #[test]
    fn fire_first_group_chains_children_with_auto_continue() {
        let mut l = group_list(
            GroupMode::FireFirstGoToNext,
            vec![audio_cue(11, 0.0, 1.0, AutoContinue), audio_cue(12, 0.0, 1.0, AutoContinue), audio_cue(13, 0.0, 0.0, DoNotContinue)],
        );
        let (_, out) = go(&mut l, 0);
        assert_eq!(starts(&out), vec![(11, 0)]);
        let out = tick(&mut l, FS);
        assert_eq!(starts(&out), vec![(12, FS)]);
        let out = tick(&mut l, 2 * FS);
        assert_eq!(starts(&out), vec![(13, 2 * FS)]);
        assert_eq!(l.playback_position(), Some(CueId(20)), "la cadena interna no mueve el playhead");
        for v in [11, 12] {
            complete(&mut l, v, 3 * FS);
        }
        assert_eq!(l.state(CueId(10)), Some(CueState::Running), "falta el tercero");
        complete(&mut l, 13, 4 * FS);
        assert_eq!(l.state(CueId(10)), Some(CueState::Completed));
    }

    #[test]
    fn children_of_timeline_groups_do_not_chain() {
        let mut l = group_list(
            GroupMode::Timeline,
            vec![audio_cue(11, 0.0, 0.0, AutoContinue), audio_cue(12, 3.0, 0.0, DoNotContinue)],
        );
        let (_, out) = go(&mut l, 0);
        assert_eq!(starts(&out), vec![(11, 0)], "12 espera su propio pre-wait; 11 no la dispara");
        assert_eq!(l.state(CueId(12)), Some(CueState::PreWaiting));
    }

    #[test]
    fn empty_or_fully_disarmed_group_completes_immediately() {
        let mut l = group_list(GroupMode::Timeline, vec![]);
        go(&mut l, 0);
        assert_eq!(l.state(CueId(10)), Some(CueState::Completed));

        let mut off = audio_cue(11, 0.0, 0.0, DoNotContinue);
        off.basics.armed = false;
        let mut l = group_list(GroupMode::FireAllSimultaneously, vec![off]);
        let (_, out) = go(&mut l, 0);
        assert!(starts(&out).is_empty());
        assert_eq!(l.state(CueId(10)), Some(CueState::Completed));
    }

    #[test]
    fn group_completes_even_if_a_child_finishes_synchronously() {
        let mut l = group_list(GroupMode::Timeline, vec![fade_cue(11, 0.0, 0.0, 99), audio_cue(12, 0.0, 0.0, DoNotContinue)]);
        go(&mut l, 0);
        assert_eq!(l.state(CueId(11)), Some(CueState::Completed));
        assert_eq!(l.state(CueId(10)), Some(CueState::Running), "no termina mientras el segundo hijo suena");
        complete(&mut l, 12, 100);
        assert_eq!(l.state(CueId(10)), Some(CueState::Completed));
    }

    #[test]
    fn group_pause_resume_and_stop_propagate_to_children() {
        let mut l = group_list(GroupMode::Timeline, vec![audio_cue(11, 0.0, 0.0, DoNotContinue), audio_cue(12, 0.0, 0.0, DoNotContinue)]);
        go(&mut l, 0);
        let mut out = Vec::new();
        l.pause_cue(CueId(10), 100, &mut out);
        assert_eq!(out.iter().filter(|e| matches!(e, Effect::PauseVoice { .. })).count(), 2);
        assert_eq!(l.state(CueId(11)), Some(CueState::Paused));
        let mut out = Vec::new();
        l.resume_cue(CueId(10), 200, &mut out);
        assert_eq!(out.iter().filter(|e| matches!(e, Effect::ResumeVoice { .. })).count(), 2);
        let mut out = Vec::new();
        l.stop_cue(CueId(10), 300, 240, &mut out);
        assert_eq!(out.iter().filter(|e| matches!(e, Effect::StopVoice { .. })).count(), 2);
        assert_eq!(l.state(CueId(10)), Some(CueState::Completed));
    }

    #[test]
    fn group_timeline_duration() {
        let mut l = group_list(GroupMode::Timeline, vec![audio_cue(11, 0.0, 0.0, DoNotContinue), audio_cue(12, 4.0, 0.0, DoNotContinue)]);
        // Cada audio dura 10 s (región 0..10): el segundo empieza a los 4 s ⇒ 14 s.
        assert_eq!(l.group_timeline_frames(CueId(10)), Some(14 * FS));
        let mut inf = audio_cue(13, 0.0, 0.0, DoNotContinue);
        if let CueKind::Audio(a) = &mut inf.kind {
            a.time_loops.infinite_loop = true;
        }
        l.add_child(CueId(10), inf).unwrap();
        assert_eq!(l.group_timeline_frames(CueId(10)), None, "un hijo infinito deja la duración indefinida");
        assert_eq!(l.group_timeline_frames(CueId(20)), None, "no es un grupo");
    }

    #[test]
    fn engine_events_without_a_matching_running_cue_pass_through() {
        let mut l = list_of(vec![audio_cue(1, 0.0, 0.0, DoNotContinue)]);
        let mut out = Vec::new();
        let ev = AudioStatus::DiskBufferUnderrun { voice_id: 1, at_frame: 5 };
        l.on_audio_status(0, ev, &mut out);
        l.on_audio_status(0, AudioStatus::VoiceCompleted { voice_id: 77, natural_eof: true }, &mut out);
        assert_eq!(out.len(), 2);
        assert!(matches!(out[0], Effect::Engine(e) if e == ev));
    }
    // -- Slices y Devamp ---------------------------------------------------------------------

    /// Cue de audio de 10 s con un tramo infinito en [2 s, 4 s) y un tramo final de una pasada.
    fn vamp_cue(id: u64) -> Cue {
        let mut c = audio_cue(id, 0.0, 0.0, DoNotContinue);
        if let CueKind::Audio(a) = &mut c.kind {
            a.time_loops.slices = vec![AudioSliceMarker::new(96_000, INFINITE_SLICE_LOOP), AudioSliceMarker::new(192_000, 1)];
        }
        c
    }

    fn devamps(out: &[Effect]) -> usize {
        out.iter().filter(|e| matches!(e, Effect::Devamp { .. })).count()
    }

    #[test]
    fn a_running_cue_stays_trapped_in_its_infinite_slice() {
        let mut l = list_of(vec![vamp_cue(1)]);
        go(&mut l, 0);
        let sec = 48_000u64;
        // Mucho después del final natural (10 s), sigue en el tramo 1: no avanzó ni terminó.
        for t in [3, 7, 31, 600] {
            let st = l.slice_status(CueId(1), t * sec).expect("sigue sonando");
            assert!(st.vamping && !st.finished, "t = {t} s");
            assert_eq!(st.slice, 1);
        }
        // Antes de llegar al tramo infinito todavía no está atrapada.
        let mut l2 = list_of(vec![vamp_cue(1)]);
        go(&mut l2, 0);
        let st = l2.slice_status(CueId(1), sec).unwrap();
        assert_eq!((st.slice, st.vamping), (0, false));
    }

    #[test]
    fn devamp_releases_the_cue_once_and_it_continues_to_the_next_slice() {
        let mut l = list_of(vec![vamp_cue(1)]);
        go(&mut l, 0);
        let sec = 48_000u64;
        let mut out = Vec::new();
        // A los 5 s está en la 2.ª pasada del tramo [2 s, 4 s) (1 s dentro).
        assert!(l.slice_status(CueId(1), 5 * sec).unwrap().vamping);
        assert_eq!(l.devamp(CueId(1), 5 * sec, &mut out), DevampResult::Requested);
        assert_eq!(devamps(&out), 1);
        assert_eq!(l.devamp(CueId(1), 5 * sec, &mut out), DevampResult::NothingToEscape, "no se acumula");
        assert_eq!(devamps(&out), 1);
        // Termina la pasada en curso (hasta los 6 s) y entra en el tramo final sin volver atrás.
        let during = l.slice_status(CueId(1), 5 * sec + sec / 2).unwrap();
        assert_eq!(during.slice, 1, "aún no salió: acaba la pasada actual");
        let after = l.slice_status(CueId(1), 6 * sec + 1).unwrap();
        assert_eq!((after.slice, after.vamping), (2, false));
    }

    #[test]
    fn devamp_needs_a_running_audio_cue() {
        let mut l = list_of(vec![vamp_cue(1), vamp_cue(2), fade_cue(3, 0.0, 1.0, 1)]);
        let mut out = Vec::new();
        assert_eq!(l.devamp(CueId(1), 0, &mut out), DevampResult::NotRunning, "inactiva");
        assert_eq!(l.devamp(CueId(99), 0, &mut out), DevampResult::UnknownCue);
        assert_eq!(l.devamp(CueId(3), 0, &mut out), DevampResult::NotRunning, "no es de audio");
        assert!(out.is_empty());
    }

    #[test]
    fn go_on_a_vamping_cue_devamps_instead_of_being_busy() {
        let mut l = list_of(vec![vamp_cue(1), audio_cue(2, 0.0, 0.0, DoNotContinue)]);
        go(&mut l, 0);
        // El playhead vuelve a la cue que está sonando atrapada en su bucle.
        l.set_playback_position(Some(CueId(1)), &mut Vec::new()).unwrap();
        let (r, out) = go(&mut l, 5 * 48_000);
        assert_eq!(r, GoResult::Devamped(CueId(1)));
        assert_eq!(devamps(&out), 1);
        assert_eq!(l.playback_position(), Some(CueId(1)), "el playhead no se mueve");
        // Un segundo GO ya no tiene nada que escapar: sigue ocupada.
        let (r, out) = go(&mut l, 5 * 48_000 + 10);
        assert_eq!(r, GoResult::Busy(CueId(1)));
        assert_eq!(devamps(&out), 0);
    }

    #[test]
    fn go_on_a_running_cue_without_a_loop_is_still_busy() {
        let mut l = list_of(vec![audio_cue(1, 0.0, 0.0, DoNotContinue)]);
        go(&mut l, 0);
        l.set_playback_position(Some(CueId(1)), &mut Vec::new()).unwrap();
        let (r, out) = go(&mut l, 48_000);
        assert_eq!(r, GoResult::Busy(CueId(1)));
        assert_eq!(devamps(&out), 0);
    }

    #[test]
    fn a_new_trigger_restarts_the_slice_program() {
        let mut l = list_of(vec![vamp_cue(1)]);
        go(&mut l, 0);
        let mut out = Vec::new();
        assert!(l.slice_status(CueId(1), 30 * 48_000).unwrap().vamping);
        l.stop_all(31 * 48_000, 0, &mut out);
        l.on_audio_status(31 * 48_000, AudioStatus::VoiceCompleted { voice_id: 1, natural_eof: false }, &mut out);
        l.set_playback_position(Some(CueId(1)), &mut Vec::new()).unwrap();
        go(&mut l, 40 * 48_000);
        let st = l.slice_status(CueId(1), 40 * 48_000 + 48_000).unwrap();
        assert_eq!((st.slice, st.vamping), (0, false), "empieza de nuevo por el primer tramo");
    }

    #[test]
    fn estimated_duration_accounts_for_slices() {
        let mut c = audio_cue(1, 0.0, 0.0, DoNotContinue);
        let CueKind::Audio(a) = &mut c.kind else { unreachable!() };
        assert_eq!(a.estimated_duration_secs(), Some(10.0));
        a.time_loops.slices = vec![AudioSliceMarker::new(96_000, 3)]; // [0,2 s) ×1 + [2 s,10 s) ×3
        assert_eq!(a.estimated_duration_secs(), Some(2.0 + 8.0 * 3.0));
        a.time_loops.slices[0].play_count = INFINITE_SLICE_LOOP;
        assert_eq!(a.estimated_duration_secs(), None, "un tramo infinito no tiene duración");
    }
    // -- Estructura: insertar, borrar, mover ----------------------------------------------------

    fn ids(l: &CueList) -> Vec<u64> {
        l.order().iter().map(|c| c.0).collect()
    }

    fn children(l: &CueList, g: u64) -> Vec<u64> {
        match &l.cue(CueId(g)).unwrap().kind {
            CueKind::Group(d) => d.children.iter().map(|c| c.0).collect(),
            _ => panic!("no es un grupo"),
        }
    }

    #[test]
    fn insert_cue_goes_after_the_reference_or_at_the_end() {
        let mut l = list_of(vec![audio_cue(1, 0.0, 0.0, DoNotContinue), audio_cue(2, 0.0, 0.0, DoNotContinue)]);
        l.insert_cue(audio_cue(3, 0.0, 0.0, DoNotContinue), None, Some(CueId(1))).unwrap();
        l.insert_cue(audio_cue(4, 0.0, 0.0, DoNotContinue), None, None).unwrap();
        l.insert_cue(audio_cue(5, 0.0, 0.0, DoNotContinue), None, Some(CueId(99))).unwrap();
        assert_eq!(ids(&l), [1, 3, 2, 4, 5]);
        assert_eq!(l.insert_cue(audio_cue(3, 0.0, 0.0, DoNotContinue), None, None), Err(ListError::DuplicateId(CueId(3))));
        // En un lista vacía, la primera cue recibe el playhead.
        let mut e = list_of(vec![]);
        e.insert_cue(audio_cue(7, 0.0, 0.0, DoNotContinue), None, None).unwrap();
        assert_eq!(e.playback_position(), Some(CueId(7)));
    }

    #[test]
    fn insert_cue_into_a_group_and_next_to_a_child() {
        let mut l = list_of(vec![group_cue(5, GroupMode::Timeline), audio_cue(1, 0.0, 0.0, DoNotContinue)]);
        l.insert_cue(audio_cue(6, 0.0, 0.0, DoNotContinue), Some(CueId(5)), None).unwrap();
        l.insert_cue(audio_cue(8, 0.0, 0.0, DoNotContinue), Some(CueId(5)), None).unwrap();
        // `after` es hija del grupo: la nueva entra en el grupo, justo detrás.
        l.insert_cue(audio_cue(7, 0.0, 0.0, DoNotContinue), None, Some(CueId(6))).unwrap();
        assert_eq!(children(&l, 5), [6, 7, 8]);
        assert_eq!(ids(&l), [5, 1], "los hijos no aparecen en el nivel superior");
        assert_eq!(l.parent_of(CueId(7)), Some(CueId(5)));
        assert_eq!(l.insert_cue(audio_cue(9, 0.0, 0.0, DoNotContinue), Some(CueId(1)), None), Err(ListError::NotAGroup(CueId(1))));
        assert_eq!(l.insert_cue(audio_cue(9, 0.0, 0.0, DoNotContinue), Some(CueId(99)), None), Err(ListError::UnknownCue(CueId(99))));
    }

    #[test]
    fn remove_cue_drops_a_group_with_its_children_and_fixes_the_playhead() {
        let mut l = list_of(vec![audio_cue(1, 0.0, 0.0, DoNotContinue), group_cue(5, GroupMode::Timeline), audio_cue(2, 0.0, 0.0, DoNotContinue)]);
        l.add_child(CueId(5), audio_cue(6, 0.0, 0.0, DoNotContinue)).unwrap();
        l.set_playback_position(Some(CueId(5)), &mut Vec::new()).unwrap();
        let mut gone = l.remove_cue(CueId(5)).unwrap();
        gone.sort();
        assert_eq!(gone, [CueId(5), CueId(6)]);
        assert_eq!(ids(&l), [1, 2]);
        assert!(l.cue(CueId(6)).is_none() && l.parent_of(CueId(6)).is_none());
        assert_eq!(l.playback_position(), Some(CueId(2)), "el playhead pasa a la siguiente");
        // Si era la última, pasa a la anterior; y vaciando la lista queda en ninguna.
        l.set_playback_position(Some(CueId(2)), &mut Vec::new()).unwrap();
        l.remove_cue(CueId(2)).unwrap();
        assert_eq!(l.playback_position(), Some(CueId(1)));
        l.remove_cue(CueId(1)).unwrap();
        assert_eq!(l.playback_position(), None);
        assert_eq!(l.remove_cue(CueId(1)), Err(ListError::UnknownCue(CueId(1))));
    }

    #[test]
    fn remove_cue_refuses_running_cues_and_removes_a_child_from_its_group() {
        let mut l = list_of(vec![audio_cue(1, 0.0, 0.0, DoNotContinue), group_cue(5, GroupMode::Timeline)]);
        l.add_child(CueId(5), audio_cue(6, 0.0, 0.0, DoNotContinue)).unwrap();
        l.add_child(CueId(5), audio_cue(7, 0.0, 0.0, DoNotContinue)).unwrap();
        go(&mut l, 0);
        assert_eq!(l.remove_cue(CueId(1)), Err(ListError::Busy(CueId(1))));
        assert!(l.cue(CueId(1)).is_some(), "no se tocó nada");
        l.remove_cue(CueId(6)).unwrap();
        assert_eq!(children(&l, 5), [7]);
    }

    #[test]
    fn move_cue_swaps_with_siblings_and_stops_at_the_ends() {
        let mut l = list_of(vec![audio_cue(1, 0.0, 0.0, DoNotContinue), audio_cue(2, 0.0, 0.0, DoNotContinue), group_cue(5, GroupMode::Timeline)]);
        l.add_child(CueId(5), audio_cue(6, 0.0, 0.0, DoNotContinue)).unwrap();
        l.add_child(CueId(5), audio_cue(7, 0.0, 0.0, DoNotContinue)).unwrap();
        assert_eq!(l.move_cue(CueId(2), -1), Ok(true));
        assert_eq!(ids(&l), [2, 1, 5]);
        assert_eq!(l.move_cue(CueId(2), -1), Ok(false), "ya es la primera");
        assert_eq!(l.move_cue(CueId(5), 1), Ok(false), "ya es la última");
        assert_eq!(l.move_cue(CueId(1), 1), Ok(true));
        assert_eq!(ids(&l), [2, 5, 1]);
        // Dentro de un grupo se mueve entre sus hermanas, sin salir de él.
        assert_eq!(l.move_cue(CueId(7), -1), Ok(true));
        assert_eq!(children(&l, 5), [7, 6]);
        assert_eq!(l.move_cue(CueId(7), -1), Ok(false));
        assert_eq!(l.move_cue(CueId(99), 1), Err(ListError::UnknownCue(CueId(99))));
        assert_eq!(l.move_cue(CueId(1), 0), Ok(false));
    }
    // -- Vídeo: su audio suena por el motor igual que una cue de audio ------------------------------

    fn video_cue(id: u64) -> Cue {
        let a = audio_cue(id, 0.0, 0.0, DoNotContinue);
        let CueKind::Audio(data) = a.kind else { unreachable!() };
        Cue::video(a.basics, crate::cue::VideoCueData { audio: data, fit: qlab_protocol::show_format::VideoFit::Fit })
    }

    #[test]
    fn a_video_cue_drives_its_audio_track_through_the_same_voice_commands() {
        let mut l = list_of(vec![video_cue(1)]);
        assert!(l.cue(CueId(1)).unwrap().is_video() && l.cue(CueId(1)).unwrap().is_audio());
        let (r, out) = go(&mut l, 0);
        assert_eq!(r, GoResult::Fired { cue: CueId(1), next_playhead: None });
        assert_eq!(starts(&out), [(1, 0)], "arranca la voz de su pista de audio");
        // Pausa, reanudación y parada llegan a la voz; su duración sale del audio.
        let mut out = Vec::new();
        l.pause_cue(CueId(1), 100, &mut out);
        assert!(out.contains(&Effect::PauseVoice { cue: CueId(1) }));
        out.clear();
        l.resume_cue(CueId(1), 200, &mut out);
        assert!(out.contains(&Effect::ResumeVoice { cue: CueId(1) }));
        out.clear();
        l.stop_cue(CueId(1), 300, 240, &mut out);
        assert!(out.contains(&Effect::StopVoice { cue: CueId(1), fade_frames: 240 }));
        assert_eq!(l.cue(CueId(1)).unwrap().timing(l.timebase()).action, Some(480_000), "10 s de pista a 48 kHz");
    }

    #[test]
    fn a_video_cue_with_an_infinite_slice_can_be_devamped_and_completes_with_its_voice() {
        let mut v = video_cue(1);
        if let Some(a) = v.audio_data_mut() {
            a.time_loops.slices = vec![AudioSliceMarker::new(96_000, INFINITE_SLICE_LOOP)];
        }
        let mut l = list_of(vec![v]);
        go(&mut l, 0);
        assert!(l.slice_status(CueId(1), 30 * 48_000).unwrap().vamping);
        let mut out = Vec::new();
        assert_eq!(l.devamp(CueId(1), 30 * 48_000, &mut out), DevampResult::Requested);
        l.on_audio_status(31 * 48_000, AudioStatus::VoiceCompleted { voice_id: 1, natural_eof: true }, &mut out);
        assert_eq!(l.state(CueId(1)), Some(CueState::Completed));
    }
}

#[cfg(test)]
mod control_tests {
    use super::tests::{audio_cue, tb};
    use super::*;
    use crate::cue::{ControlAction, ControlCueData};
    use qlab_protocol::cue_dto::{CueBasicsData, CueId};
    use qlab_protocol::cue_dto::ContinueMode::DoNotContinue;

    fn control(id: u64, action: ControlAction, target: Option<u64>, secs: f64) -> Cue {
        let basics = CueBasicsData { unique_id: CueId(id), number: id.to_string(), action_duration: secs, cue_target_id: target.map(CueId), ..Default::default() };
        Cue::control(basics, ControlCueData { target: target.map(CueId), action })
    }

    fn list(cues: Vec<Cue>) -> CueList {
        let mut l = CueList::new(1, tb());
        for c in cues {
            l.add_cue(c).unwrap();
        }
        l
    }

    /// Dispara `n` cues seguidas con GO en instantes distintos y devuelve todos los efectos.
    fn go_n(l: &mut CueList, n: usize) -> Vec<Effect> {
        let mut out = Vec::new();
        for i in 0..n {
            l.fire_go(1_000 * (i as u64 + 1), &mut out);
        }
        out
    }

    #[test]
    fn a_stop_cue_stops_its_target_with_the_fade_out_it_was_given() {
        let mut l = list(vec![audio_cue(1, 0.0, 0.0, DoNotContinue), control(2, ControlAction::Stop, Some(1), 2.0)]);
        let out = go_n(&mut l, 2);
        assert!(out.iter().any(|e| matches!(e, Effect::StopVoice { cue, fade_frames } if *cue == CueId(1) && *fade_frames == 96_000)), "{out:?}");
        assert_eq!(l.state(CueId(2)), Some(CueState::Running), "la cue de control dura lo que su fundido");
    }

    #[test]
    fn pause_and_resume_cues_drive_the_target_voice() {
        let mut l = list(vec![audio_cue(1, 0.0, 0.0, DoNotContinue), control(2, ControlAction::Pause, Some(1), 0.0), control(3, ControlAction::Resume, Some(1), 0.0)]);
        let out = go_n(&mut l, 3);
        let kinds: Vec<&str> = out.iter().filter_map(|e| match e { Effect::PauseVoice { .. } => Some("pause"), Effect::ResumeVoice { .. } => Some("resume"), _ => None }).collect();
        assert_eq!(kinds, ["pause", "resume"]);
        assert_eq!(l.state(CueId(1)), Some(CueState::Running));
    }

    #[test]
    fn a_control_cue_without_a_usable_target_does_nothing_and_never_fails() {
        let mut l = list(vec![control(1, ControlAction::Stop, None, 0.0), control(2, ControlAction::Stop, Some(2), 0.0), control(3, ControlAction::Pause, Some(99), 0.0), audio_cue(4, 0.0, 0.0, DoNotContinue)]);
        let out = go_n(&mut l, 3);
        assert!(!out.iter().any(|e| matches!(e, Effect::StopVoice { .. } | Effect::PauseVoice { .. })), "{out:?}");
    }

    #[test]
    fn updating_a_control_cue_takes_the_target_from_the_inspector_field() {
        let mut l = list(vec![audio_cue(1, 0.0, 0.0, DoNotContinue), control(2, ControlAction::Stop, None, 0.0)]);
        let c = l.cue(CueId(2)).unwrap().clone();
        let basics = CueBasicsData { cue_target_id: Some(CueId(1)), ..c.basics.clone() };
        l.update_cue(CueId(2), basics, c.triggers.clone(), CueKind::Control(ControlCueData { target: None, action: ControlAction::Pause })).unwrap();
        assert!(matches!(&l.cue(CueId(2)).unwrap().kind, CueKind::Control(d) if d.target == Some(CueId(1)) && d.action == ControlAction::Pause));
    }
    #[test]
    fn goto_moves_the_playhead_and_load_preloads_the_target() {
        // 1 audio, 2 goto→1 (deja el playhead en 1 para repetir), 3 audio, 4 load→3.
        let mut l = list(vec![audio_cue(1, 0.0, 0.0, DoNotContinue), control(2, ControlAction::GoTo, Some(1), 0.0), audio_cue(3, 0.0, 0.0, DoNotContinue), control(4, ControlAction::Load, Some(3), 0.0)]);
        let mut out = Vec::new();
        l.fire_go(1_000, &mut out); // 1
        l.fire_go(2_000, &mut out); // 2: goto 1
        assert_eq!(l.playback_position(), Some(CueId(1)), "tras el Go to, el siguiente GO vuelve a disparar la cue 1");
        l.set_playback_position(Some(CueId(4)), &mut out).unwrap();
        out.clear();
        l.fire_go(3_000, &mut out); // 4: load 3
        assert!(out.iter().any(|e| matches!(e, Effect::Load { cue } if *cue == CueId(3))), "{out:?}");
    }

    #[test]
    fn arm_and_disarm_cues_change_the_target_and_announce_it() {
        let mut target = audio_cue(1, 0.0, 0.0, DoNotContinue);
        target.basics.armed = true;
        target.basics.file_target = Some(qlab_protocol::cue_dto::MediaTarget { absolute_path: "x.wav".into(), ..Default::default() });
        let mut l = list(vec![target, control(2, ControlAction::Disarm, Some(1), 0.0), control(3, ControlAction::Arm, Some(1), 0.0)]);
        let mut out = Vec::new();
        l.set_playback_position(Some(CueId(2)), &mut out).unwrap();
        out.clear();
        l.fire_go(1_000, &mut out);
        assert!(!l.cue(CueId(1)).unwrap().basics.armed);
        assert!(out.contains(&Effect::ArmChanged { cue: CueId(1) }));
        out.clear();
        l.fire_go(2_000, &mut out);
        assert!(l.cue(CueId(1)).unwrap().basics.armed && out.contains(&Effect::ArmChanged { cue: CueId(1) }));
        // Armar lo que ya está armado no avisa.
        l.set_playback_position(Some(CueId(3)), &mut out).unwrap();
        out.clear();
        l.fire_go(3_000, &mut out);
        assert!(!out.iter().any(|e| matches!(e, Effect::ArmChanged { .. })));
    }

    #[test]
    fn a_cue_without_a_file_cannot_be_armed_by_a_control_cue() {
        let mut bare = audio_cue(1, 0.0, 0.0, DoNotContinue);
        bare.basics.armed = false;
        let mut l = list(vec![control(2, ControlAction::Arm, Some(1), 0.0), bare]);
        let mut out = Vec::new();
        l.fire_go(1_000, &mut out);
        assert!(!l.cue(CueId(1)).unwrap().basics.armed, "sin archivo no se arma");
        assert!(!out.iter().any(|e| matches!(e, Effect::ArmChanged { .. })));
    }

}
