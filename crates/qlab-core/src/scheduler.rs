//! Despachador del hilo de control (`qlab-sequencer`): el *tick loop* (spec §1.1 y §2.1).
//!
//! Corre **fuera del hilo RT**. Cada tick:
//! 1. reintenta los comandos que no cupieron en el anillo SPSC,
//! 2. drena los `AudioStatus` que dejó el callback y avanza el modelo (`AutoFollow`, fin de cues),
//! 3. avanza el reloj de la [`CueList`] (pre-waits, `AutoContinue`, fades, cargas),
//! 4. traduce los efectos a `AudioCommand` y los empuja al anillo hacia el motor.
//!
//! Los comandos nunca se pierden ni se reordenan: si el anillo está lleno esperan en un backlog.
//!
//! # Precisión
//! Los eventos se calculan al frame exacto (`at_frames`), pero se envían en el tick en que vencen. El
//! bucle [`Scheduler::run`] duerme hasta el siguiente vencimiento y, como máximo, un periodo de
//! tick (1 ms), de modo que el retraso de despacho es de microsegundos. El motor aplica los comandos
//! al inicio del siguiente bloque de audio.
//!
//! # Límite conocido
//! Los fades de matriz ya enviados al motor no se pueden pausar: pausar un `FadeCue` detiene su
//! contador en el modelo pero el motor sigue evaluando la rampa.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use qlab_engine::rt_command::{AudioCommand, AudioStatus, CurveTable, DevampMode};
use qlab_protocol::cue_dto::CueId;
use qlab_protocol::ipc_shm::{
    CuePhase, HotControlEvent, HotControlRing, RunningCueTelemetry, SeqlockShowStatus, ShowStatus, MAX_RUNNING_CUES, SLICE_DEVAMPING, SLICE_PROGRAM, SLICE_VAMPING,
};
use ringbuf::traits::{Consumer, Producer};

use crate::clock::{Frames, Micros, MonotonicClock, Timebase};
use crate::cue::CueKind;
use crate::cue_list::{CueList, Effect, GoResult};
use crate::cue_state::CueState;
use crate::fade_processor::{voice_of, FadeProcessor};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SchedulerConfig {
    pub sample_rate: u32,
    /// Instante del reloj monotónico que corresponde al frame 0 del show.
    pub origin_micros: Micros,
    /// Periodo máximo de dormida del bucle.
    pub tick_period_us: u64,
    /// Fade de un Stop normal y de Stop All (5 ms @ 48 kHz).
    pub stop_fade_frames: u32,
    /// Fade de Panic (suave).
    pub panic_fade_frames: u32,
    /// Fade de un Hard Stop (2 ms @ 48 kHz).
    pub hard_stop_fade_frames: u32,
    pub pause_fade_frames: u32,
    /// Tope de la cola de eventos pendientes de recoger; al llenarse se descartan los más antiguos.
    pub max_pending_events: usize,
}

impl SchedulerConfig {
    pub fn new(sample_rate: u32) -> Self {
        Self {
            sample_rate,
            origin_micros: 0,
            tick_period_us: 1_000,
            stop_fade_frames: 240,
            panic_fade_frames: sample_rate,
            hard_stop_fade_frames: 96,
            pause_fade_frames: 240,
            max_pending_events: 8_192,
        }
    }
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self::new(crate::clock::DEFAULT_SAMPLE_RATE)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SchedulerStats {
    pub ticks: u64,
    pub commands_sent: u64,
    /// Veces que el anillo estaba lleno y un comando pasó al backlog.
    pub ring_full_events: u64,
    pub max_backlog: usize,
    pub events_dropped: u64,
}

pub struct Scheduler<P, C>
where
    P: Producer<Item = AudioCommand>,
    C: Consumer<Item = AudioStatus>,
{
    cfg: SchedulerConfig,
    timebase: Timebase,
    list: CueList,
    fades: FadeProcessor,
    cmd_tx: P,
    status_rx: C,
    backlog: VecDeque<AudioCommand>,
    events: VecDeque<Effect>,
    scratch: Vec<Effect>,
    stats: SchedulerStats,
}

impl<P, C> Scheduler<P, C>
where
    P: Producer<Item = AudioCommand>,
    C: Consumer<Item = AudioStatus>,
{
    pub fn new(cfg: SchedulerConfig, list: CueList, cmd_tx: P, status_rx: C) -> Self {
        Self {
            timebase: Timebase::new(cfg.sample_rate),
            cfg,
            list,
            fades: FadeProcessor::new(),
            cmd_tx,
            status_rx,
            backlog: VecDeque::new(),
            events: VecDeque::new(),
            scratch: Vec::new(),
            stats: SchedulerStats::default(),
        }
    }

    pub fn list(&self) -> &CueList {
        &self.list
    }

    pub fn list_mut(&mut self) -> &mut CueList {
        &mut self.list
    }

    pub fn stats(&self) -> SchedulerStats {
        self.stats
    }

    pub fn timebase(&self) -> Timebase {
        self.timebase
    }

    pub fn pending_commands(&self) -> usize {
        self.backlog.len()
    }

    /// Frames del show en el instante `now_micros` del reloj monotónico.
    pub fn frames_at(&self, now_micros: Micros) -> Frames {
        self.timebase.micros_to_frames(now_micros.saturating_sub(self.cfg.origin_micros))
    }

    // -- Entradas ---------------------------------------------------------------------------

    /// Botón GO. No bloquea: dispara la cue del playhead, adelanta el playhead y encola los comandos.
    pub fn go(&mut self, now_micros: Micros) -> GoResult {
        let now = self.frames_at(now_micros);
        let mut effects = std::mem::take(&mut self.scratch);
        let result = self.list.fire_go(now, &mut effects);
        self.dispatch(&mut effects);
        self.scratch = effects;
        result
    }

    /// Un paso del bucle: backlog, estado del motor, reloj del modelo y despacho.
    pub fn tick(&mut self, now_micros: Micros) {
        self.stats.ticks += 1;
        let now = self.frames_at(now_micros);
        self.flush_backlog();

        let mut effects = std::mem::take(&mut self.scratch);
        while let Some(status) = self.status_rx.try_pop() {
            self.list.on_audio_status(now, status, &mut effects);
        }
        self.list.tick(now, &mut effects);
        self.dispatch(&mut effects);
        self.scratch = effects;
    }

    /// Parada suave de todo (Panic).
    pub fn panic(&mut self, now_micros: Micros) {
        self.stop_with(now_micros, self.cfg.panic_fade_frames);
    }

    /// Parada inmediata (Hard Stop, doble pulsación de Panic).
    pub fn hard_stop_all(&mut self, now_micros: Micros) {
        self.stop_with(now_micros, self.cfg.hard_stop_fade_frames);
    }

    pub fn stop_all(&mut self, now_micros: Micros) {
        self.stop_with(now_micros, self.cfg.stop_fade_frames);
    }

    fn stop_with(&mut self, now_micros: Micros, fade_frames: u32) {
        let now = self.frames_at(now_micros);
        let mut effects = std::mem::take(&mut self.scratch);
        self.list.stop_all(now, fade_frames, &mut effects);
        self.dispatch(&mut effects);
        self.scratch = effects;
    }

    /// Devamp de la cue `id` (ver `CueList::devamp`).
    pub fn devamp(&mut self, id: CueId, now_micros: Micros) -> crate::cue_list::DevampResult {
        let now = self.frames_at(now_micros);
        let mut effects = std::mem::take(&mut self.scratch);
        let result = self.list.devamp(id, now, &mut effects);
        self.dispatch(&mut effects);
        self.scratch = effects;
        result
    }

    /// Acción de control sobre una cue concreta (control remoto): Stop usa el fundido estándar.
    pub fn cue_action(&mut self, id: CueId, action: crate::cue::ControlAction, now_micros: Micros) {
        let now = self.frames_at(now_micros);
        let mut effects = std::mem::take(&mut self.scratch);
        let fade = self.cfg.stop_fade_frames;
        self.list.apply_control(action, id, now, fade, &mut effects);
        self.dispatch(&mut effects);
        self.scratch = effects;
    }

    pub fn pause_all(&mut self, now_micros: Micros) {
        let now = self.frames_at(now_micros);
        let mut effects = std::mem::take(&mut self.scratch);
        self.list.pause_all(now, &mut effects);
        self.dispatch(&mut effects);
        self.scratch = effects;
    }

    pub fn resume_all(&mut self, now_micros: Micros) {
        let now = self.frames_at(now_micros);
        let mut effects = std::mem::take(&mut self.scratch);
        self.list.resume_all(now, &mut effects);
        self.dispatch(&mut effects);
        self.scratch = effects;
    }

    /// Aplica un gesto de latencia crítica (GO, Panic, StopAll, arrastre de fader).
    pub fn apply_hot_event(&mut self, event: HotControlEvent, now_micros: Micros) {
        match event {
            HotControlEvent::Go => {
                self.go(now_micros);
            }
            HotControlEvent::Panic => self.panic(now_micros),
            HotControlEvent::StopAll => self.stop_all(now_micros),
            HotControlEvent::LiveCrosspointDrag { cue_num_id, row, col, db } => {
                self.send(AudioCommand::SetCrosspointDb { voice_id: cue_num_id, row, col, db });
            }
        }
    }

    /// Atiende lo que la UI dejó en el anillo caliente de memoria compartida.
    pub fn poll_hot_ring(&mut self, ring: &HotControlRing, now_micros: Micros) -> usize {
        let mut handled = 0;
        while let Some(event) = ring.pop() {
            handled += 1;
            self.apply_hot_event(event, now_micros);
        }
        handled
    }

    /// Encola un comando suelto para el motor (ediciones en vivo: niveles, velocidad…), conservando
    /// el orden con el resto.
    pub fn send_command(&mut self, cmd: AudioCommand) {
        self.send(cmd);
    }

    /// Mueve el playhead (independiente de la selección de la UI).
    pub fn set_playhead(&mut self, cue: Option<CueId>) -> Result<(), crate::cue_list::ListError> {
        let mut effects = std::mem::take(&mut self.scratch);
        let result = self.list.set_playback_position(cue, &mut effects);
        self.dispatch(&mut effects);
        self.scratch = effects;
        result
    }

    /// Sustituye la lista de cues (carga de un show) y devuelve la anterior. Los fades en vuelo del
    /// show anterior se olvidan; parar el audio que sigue sonando es cosa de quien llama.
    pub fn replace_list(&mut self, list: CueList) -> CueList {
        self.fades = FadeProcessor::new();
        std::mem::replace(&mut self.list, list)
    }

    // -- Salidas ----------------------------------------------------------------------------

    /// Mueve los eventos pendientes (playhead, estados, cargas…) a `out`.
    pub fn drain_events(&mut self, out: &mut Vec<Effect>) {
        out.extend(self.events.drain(..));
    }

    /// Próximo instante del reloj monotónico en que hay algo que hacer.
    pub fn next_deadline_micros(&self) -> Option<Micros> {
        self.list.next_deadline().map(|f| self.cfg.origin_micros + self.timebase.frames_to_micros(f))
    }

    /// Estado para el segmento de memoria compartida (playhead y cues en marcha con su progreso).
    pub fn show_status(&mut self, now_micros: Micros) -> ShowStatus {
        let now = self.frames_at(now_micros);
        let tb = self.timebase;
        let mut status = ShowStatus {
            active_playhead_cue_id: self.list.playback_position().map_or(0, |c| c.0 as u128),
            ..ShowStatus::default()
        };
        let mut slot = 0;
        let active: Vec<CueId> = self.list.active_ids().to_vec();
        for id in active {
            let has_program = self.list.cue(id).and_then(|c| c.audio_data()).is_some_and(|a| !a.time_loops.slices.is_empty() || a.time_loops.infinite_loop);
            let slice = if has_program { self.list.slice_status(id, now) } else { None };
            let Some(cue) = self.list.cue(id) else { continue };
            let m = cue.machine();
            let phase = match m.effective_state() {
                CueState::PreWaiting => CuePhase::PreWait,
                CueState::Running if cue.stopping => CuePhase::TailingOut,
                CueState::Running => CuePhase::Action,
                CueState::PostWaiting => CuePhase::PostWait,
                _ => continue,
            };
            if m.state() != CueState::Paused {
                status.is_playing = true;
            }
            if slot < MAX_RUNNING_CUES {
                status.running_cues[slot] = RunningCueTelemetry {
                    cue_id: id.0,
                    phase,
                    pre_wait_elapsed_sec: tb.frames_to_secs(m.pre_wait_elapsed(now)) as f32,
                    action_elapsed_sec: tb.frames_to_secs(m.action_elapsed(now)) as f32,
                    post_wait_elapsed_sec: tb.frames_to_secs(m.post_wait_elapsed(now)) as f32,
                    current_slice_idx: slice.map_or(0, |s| s.slice as u16),
                    loops_remaining: slice.map_or(0, |s| s.loops_left),
                    source_frame: 0, // lo rellena el daemon con la posición real del motor
                    peak: 0.0,       // ídem, con el pico de la voz
                    slice_flags: slice.map_or(0, |s| SLICE_PROGRAM | if s.vamping { SLICE_VAMPING } else { 0 } | if s.devamping { SLICE_DEVAMPING } else { 0 }),
                };
                slot += 1;
            }
        }
        status
    }

    pub fn publish_status(&mut self, shm: &SeqlockShowStatus, now_micros: Micros) {
        shm.publish(&self.show_status(now_micros));
    }

    /// Bucle del hilo de control: atiende la UI, avanza el modelo y duerme hasta el siguiente
    /// vencimiento (como mucho un periodo de tick). Termina cuando `stop` se pone a `true`.
    pub fn run(&mut self, clock: &dyn MonotonicClock, hot_ring: Option<&HotControlRing>, stop: &AtomicBool) {
        while !stop.load(Ordering::Acquire) {
            let now = clock.now_micros();
            if let Some(ring) = hot_ring {
                self.poll_hot_ring(ring, now);
            }
            self.tick(now);
            let period = self.cfg.tick_period_us.max(1);
            let wait = self.next_deadline_micros().map_or(period, |d| d.saturating_sub(clock.now_micros()).min(period));
            if wait > 150 {
                std::thread::sleep(Duration::from_micros(wait - 100));
            } else {
                std::hint::spin_loop();
            }
        }
    }

    // -- Traducción efecto → comando ----------------------------------------------------------

    fn dispatch(&mut self, effects: &mut Vec<Effect>) {
        for effect in effects.drain(..) {
            match effect {
                Effect::StartAudio { cue, at_frames } => {
                    // El EQ de la cue se aplica antes de que suene la primera muestra.
                    if let Some(eq) = self.list.cue(cue).and_then(|c| c.audio_data()).map(|a| a.eq).filter(|e| e.enabled) {
                        self.send(AudioCommand::SetEq { voice_id: voice_of(cue), eq: crate::cue::eq_params(&eq) });
                    }
                    let host_ns = self.cfg.origin_micros * 1_000 + self.timebase.frames_to_nanos(at_frames);
                    // Un arranque desde una memoria cae a mitad de la pista: entra con un micro-fade de 50 ms (sin clic).
                    let from_memory = self.list.cue(cue).and_then(|c| c.audio_data()).is_some_and(|a| a.time_loops.start_override_secs.is_some());
                    if from_memory {
                        let fade_frames = (self.cfg.sample_rate / 20).max(1);
                        self.send(AudioCommand::PlayFadeIn { voice_id: voice_of(cue), start_host_time_ns: host_ns, fade_frames });
                    } else {
                        self.send(AudioCommand::Play { voice_id: voice_of(cue), start_host_time_ns: host_ns });
                    }
                }
                Effect::StartFade { cue, .. } => self.start_fade(cue),
                Effect::PauseVoice { cue } => {
                    self.send(AudioCommand::Pause { voice_id: voice_of(cue), fade_frames: self.cfg.pause_fade_frames });
                }
                Effect::ResumeVoice { cue } => {
                    self.send(AudioCommand::Resume { voice_id: voice_of(cue), fade_frames: self.cfg.pause_fade_frames });
                }
                Effect::Devamp { cue } => {
                    self.send(AudioCommand::RequestDevamp { voice_id: voice_of(cue), mode: DevampMode::ContinueToNextSlice, trigger_next_cue: false });
                }
                Effect::StopVoice { cue, fade_frames } => {
                    self.send(AudioCommand::Stop { voice_id: voice_of(cue), fade_frames, curve: CurveTable::LINEAR });
                }
                Effect::CancelFade { cue } => {
                    let mut cmds = Vec::new();
                    self.fades.cancel(cue, &mut cmds);
                    for c in cmds {
                        self.send(c);
                    }
                }
                other => {
                    if let Effect::StateChanged { cue, to: CueState::Completed, .. } = other {
                        self.fades.forget(cue);
                    }
                    self.push_event(other);
                }
            }
        }
    }

    fn start_fade(&mut self, cue: CueId) {
        let Some(c) = self.list.cue(cue) else { return };
        let CueKind::Fade(data) = &c.kind else { return };
        let duration = c.timing(self.timebase).action.unwrap_or(0);
        let matrix = data.target.and_then(|t| self.list.cue(t)?.audio_data()).map(|a| &a.levels);
        let mut cmds = Vec::new();
        self.fades.start(cue, data, duration, matrix, &mut cmds);
        for cmd in cmds {
            self.send(cmd);
        }
    }

    /// Envía un comando conservando el orden: si hay backlog, detrás de él.
    fn send(&mut self, cmd: AudioCommand) {
        if self.backlog.is_empty() {
            match self.cmd_tx.try_push(cmd) {
                Ok(()) => self.stats.commands_sent += 1,
                Err(cmd) => {
                    self.stats.ring_full_events += 1;
                    self.backlog.push_back(cmd);
                }
            }
        } else {
            self.backlog.push_back(cmd);
        }
        self.stats.max_backlog = self.stats.max_backlog.max(self.backlog.len());
    }

    fn flush_backlog(&mut self) {
        while let Some(&cmd) = self.backlog.front() {
            if self.cmd_tx.try_push(cmd).is_err() {
                break;
            }
            self.backlog.pop_front();
            self.stats.commands_sent += 1;
        }
    }

    fn push_event(&mut self, e: Effect) {
        if self.events.len() >= self.cfg.max_pending_events {
            self.events.pop_front();
            self.stats.events_dropped += 1;
        }
        self.events.push_back(e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::{ManualClock, SystemClock};
    use crate::cue::{FadeCell, GroupMode};
    use crate::cue_list::tests::{audio_cue, fade_cue, group_cue};
    use qlab_engine::rt_command::{FadeMode, COMMAND_RING_CAPACITY, STATUS_RING_CAPACITY};
    use qlab_protocol::cue_dto::ContinueMode::*;
    use ringbuf::traits::Split;
    use ringbuf::{HeapCons, HeapProd, HeapRb};

    type Sched = Scheduler<HeapProd<AudioCommand>, HeapCons<AudioStatus>>;

    struct Rig {
        sched: Sched,
        cmd_rx: HeapCons<AudioCommand>,
        st_tx: HeapProd<AudioStatus>,
    }

    fn rig_with_capacity(cues: Vec<crate::cue::Cue>, cmd_capacity: usize) -> Rig {
        let mut list = CueList::new(1, Timebase::new(48_000));
        for c in cues {
            list.add_cue(c).unwrap();
        }
        let (cmd_tx, cmd_rx) = HeapRb::<AudioCommand>::new(cmd_capacity).split();
        let (st_tx, st_rx) = HeapRb::<AudioStatus>::new(STATUS_RING_CAPACITY).split();
        Rig { sched: Scheduler::new(SchedulerConfig::new(48_000), list, cmd_tx, st_rx), cmd_rx, st_tx }
    }

    fn rig(cues: Vec<crate::cue::Cue>) -> Rig {
        rig_with_capacity(cues, COMMAND_RING_CAPACITY)
    }

    fn drain(rx: &mut HeapCons<AudioCommand>) -> Vec<AudioCommand> {
        let mut v = Vec::new();
        while let Some(c) = rx.try_pop() {
            v.push(c);
        }
        v
    }

    const SEC: Micros = 1_000_000;

    #[test]
    fn a_cue_armed_from_a_memory_starts_with_a_50_ms_fade_in_and_a_plain_cue_does_not() {
        let plain = audio_cue(1, 0.0, 0.0, DoNotContinue);
        let mut from_memory = audio_cue(2, 0.0, 0.0, DoNotContinue);
        if let crate::cue::CueKind::Audio(a) = &mut from_memory.kind {
            a.time_loops.start_override_secs = Some(4.0);
        } else {
            panic!("la cue de prueba es de audio");
        }
        let mut r = rig(vec![plain, from_memory]);
        r.sched.go(SEC);
        assert!(matches!(drain(&mut r.cmd_rx)[..], [AudioCommand::Play { voice_id: 1, .. }]));
        r.sched.go(2 * SEC);
        assert!(matches!(drain(&mut r.cmd_rx)[..], [AudioCommand::PlayFadeIn { voice_id: 2, fade_frames: 2_400, .. }]), "50 ms a 48 kHz");
    }

    #[test]
    fn go_sends_play_without_blocking_and_advances_the_playhead() {
        let mut r = rig(vec![audio_cue(1, 0.0, 0.0, DoNotContinue), audio_cue(2, 0.0, 0.0, DoNotContinue)]);
        let res = r.sched.go(SEC);
        assert_eq!(res, GoResult::Fired { cue: CueId(1), next_playhead: Some(CueId(2)) });
        assert_eq!(drain(&mut r.cmd_rx), vec![AudioCommand::Play { voice_id: 1, start_host_time_ns: 1_000_000_000 }]);
        assert_eq!(r.sched.list().playback_position(), Some(CueId(2)));
        let mut ev = Vec::new();
        r.sched.drain_events(&mut ev);
        assert!(ev.contains(&Effect::PlayheadMoved { list: 1, position: Some(CueId(2)) }));
        assert!(matches!(r.sched.go(2 * SEC), GoResult::Fired { cue: CueId(2), next_playhead: None }));
        assert_eq!(r.sched.go(3 * SEC), GoResult::EndOfList);
    }

    #[test]
    fn devamp_sends_a_request_devamp_command_to_the_voice() {
        use qlab_protocol::cue_dto::{AudioSliceMarker, INFINITE_SLICE_LOOP};
        let mut c = audio_cue(1, 0.0, 0.0, DoNotContinue);
        if let CueKind::Audio(a) = &mut c.kind {
            a.time_loops.slices = vec![AudioSliceMarker::new(48_000, INFINITE_SLICE_LOOP)];
        }
        let mut r = rig(vec![c]);
        r.sched.go(SEC);
        drain(&mut r.cmd_rx);
        assert_eq!(r.sched.devamp(CueId(1), 5 * SEC), crate::cue_list::DevampResult::Requested);
        assert_eq!(drain(&mut r.cmd_rx), vec![AudioCommand::RequestDevamp { voice_id: 1, mode: DevampMode::ContinueToNextSlice, trigger_next_cue: false }]);
        // El segundo Devamp no genera otro comando.
        assert_eq!(r.sched.devamp(CueId(1), 5 * SEC), crate::cue_list::DevampResult::NothingToEscape);
        assert!(drain(&mut r.cmd_rx).is_empty());
    }

    #[test]
    fn show_status_flags_vamping_and_devamping_cues() {
        use qlab_protocol::cue_dto::{AudioSliceMarker, INFINITE_SLICE_LOOP};
        let mut c = audio_cue(1, 0.0, 0.0, DoNotContinue);
        if let CueKind::Audio(a) = &mut c.kind {
            a.time_loops.slices = vec![AudioSliceMarker::new(48_000, INFINITE_SLICE_LOOP)];
        }
        let mut r = rig(vec![c, audio_cue(2, 0.0, 0.0, DoNotContinue)]);
        r.sched.go(0);
        r.sched.go(0);
        // Antes del tramo infinito: tiene programa pero aún no está atrapada.
        let s = r.sched.show_status(SEC / 2);
        assert_eq!(s.running_cues[0].slice_flags, SLICE_PROGRAM);
        assert_eq!(s.running_cues[1].slice_flags, 0, "una cue sin slices no lleva flags");
        // Entra en el tramo infinito a 1 s y queda atrapada.
        let s = r.sched.show_status(3 * SEC);
        let c0 = s.running_cues[0];
        assert_eq!((c0.slice_flags, c0.current_slice_idx, c0.loops_remaining), (SLICE_PROGRAM | SLICE_VAMPING, 1, u32::MAX));
        // El Devamp cambia el indicador de inmediato.
        assert_eq!(r.sched.devamp(CueId(1), 3 * SEC), crate::cue_list::DevampResult::Requested);
        let c0 = r.sched.show_status(3 * SEC).running_cues[0];
        assert_eq!(c0.slice_flags, SLICE_PROGRAM | SLICE_DEVAMPING);
        // Una cue sin slices ni bucle nunca muestra indicador, aunque reciba un Devamp.
        r.sched.devamp(CueId(2), 3 * SEC);
        assert_eq!(r.sched.show_status(3 * SEC).running_cues[1].slice_flags, 0);
    }

    #[test]
    fn host_time_includes_the_clock_origin() {
        let mut list = CueList::new(1, Timebase::new(48_000));
        list.add_cue(audio_cue(1, 0.0, 0.0, DoNotContinue)).unwrap();
        let (cmd_tx, mut cmd_rx) = HeapRb::<AudioCommand>::new(8).split();
        let (_st_tx, st_rx) = HeapRb::<AudioStatus>::new(8).split();
        let cfg = SchedulerConfig { origin_micros: 5 * SEC, ..SchedulerConfig::new(48_000) };
        let mut s = Scheduler::new(cfg, list, cmd_tx, st_rx);
        s.go(6 * SEC);
        assert_eq!(s.frames_at(6 * SEC), 48_000);
        assert_eq!(drain(&mut cmd_rx), vec![AudioCommand::Play { voice_id: 1, start_host_time_ns: 6_000_000_000 }]);
    }

    #[test]
    fn pre_wait_is_dispatched_by_the_tick_at_the_right_microsecond() {
        let mut r = rig(vec![audio_cue(1, 0.5, 0.0, DoNotContinue)]);
        r.sched.go(0);
        assert!(drain(&mut r.cmd_rx).is_empty(), "GO con pre-wait no envía nada todavía");
        assert_eq!(r.sched.next_deadline_micros(), Some(500_000));
        // 1 frame son 20,8 µs y el reloj se redondea al frame más cercano: 10 µs antes sigue sin vencer.
        r.sched.tick(489_000);
        assert!(drain(&mut r.cmd_rx).is_empty());
        r.sched.tick(500_000);
        assert_eq!(drain(&mut r.cmd_rx), vec![AudioCommand::Play { voice_id: 1, start_host_time_ns: 500_000_000 }]);
        r.sched.tick(500_001);
        assert!(drain(&mut r.cmd_rx).is_empty(), "no se repite");
        assert_eq!(r.sched.next_deadline_micros(), None);
    }

    #[test]
    fn auto_continue_chain_is_dispatched_over_time() {
        let mut r = rig(vec![
            audio_cue(1, 0.0, 1.0, AutoContinue),
            audio_cue(2, 0.0, 0.5, AutoContinue),
            audio_cue(3, 0.0, 0.0, DoNotContinue),
        ]);
        r.sched.go(0);
        let mut got = drain(&mut r.cmd_rx);
        for t in (0..2_000).map(|ms| ms * 1_000) {
            r.sched.tick(t);
            got.extend(drain(&mut r.cmd_rx));
        }
        let plays: Vec<_> = got
            .iter()
            .map(|c| match c {
                AudioCommand::Play { voice_id, start_host_time_ns } => (*voice_id, *start_host_time_ns),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(plays, vec![(1, 0), (2, 1_000_000_000), (3, 1_500_000_000)]);
    }

    #[test]
    fn auto_follow_uses_the_engine_status_not_the_clock() {
        let mut r = rig(vec![audio_cue(1, 0.0, 0.0, AutoFollow), audio_cue(2, 0.0, 0.0, DoNotContinue)]);
        r.sched.go(0);
        drain(&mut r.cmd_rx);
        r.sched.tick(60 * SEC);
        assert!(drain(&mut r.cmd_rx).is_empty());
        assert!(r.st_tx.try_push(AudioStatus::VoiceCompleted { voice_id: 1, natural_eof: true }).is_ok());
        r.sched.tick(61 * SEC);
        assert_eq!(drain(&mut r.cmd_rx), vec![AudioCommand::Play { voice_id: 2, start_host_time_ns: 61_000_000_000 }]);
    }

    #[test]
    fn fade_cue_becomes_cell_fade_commands_for_the_target_voice() {
        let mut fade = fade_cue(2, 0.0, 2.0, 1);
        if let CueKind::Fade(d) = &mut fade.kind {
            d.cells = vec![FadeCell { row: 0, col: 0, target_db: -30.0, active: true }, FadeCell { row: 1, col: 1, target_db: -12.0, active: true }];
            d.stop_target_when_done = true;
            d.mode = FadeMode::Absolute;
        }
        let mut r = rig(vec![audio_cue(1, 0.0, 0.0, DoNotContinue), fade]);
        r.sched.go(0);
        r.sched.go(SEC);
        let cmds = drain(&mut r.cmd_rx);
        assert!(matches!(cmds[0], AudioCommand::Play { voice_id: 1, .. }));
        let fades: Vec<_> = cmds[1..].to_vec();
        assert_eq!(fades.len(), 2);
        for (cmd, db) in fades.iter().zip([-30.0f32, -12.0]) {
            match *cmd {
                AudioCommand::BeginCellFade { voice_id, mode, target_db, duration_frames, stop_target_when_done, curve, .. } => {
                    assert_eq!((voice_id, mode, target_db, duration_frames), (1, FadeMode::Absolute, db, 96_000));
                    assert!(stop_target_when_done);
                    assert_eq!(curve, crate::fade_processor::FadeCurve::SCurve.to_table());
                }
                other => panic!("{other:?}"),
            }
        }
        assert!(r.sched.fades.is_active(CueId(2)));
        r.sched.tick(3 * SEC);
        assert!(!r.sched.fades.is_active(CueId(2)), "al terminar el fade se olvida");
    }

    #[test]
    fn stopping_a_running_fade_cancels_it_in_the_engine() {
        let mut fade = fade_cue(2, 0.0, 10.0, 1);
        if let CueKind::Fade(d) = &mut fade.kind {
            d.cells = vec![FadeCell { row: 1, col: 1, target_db: -20.0, active: true }];
        }
        let mut r = rig(vec![audio_cue(1, 0.0, 0.0, DoNotContinue), fade]);
        r.sched.go(0);
        r.sched.go(0);
        drain(&mut r.cmd_rx);
        r.sched.stop_all(SEC);
        let cmds = drain(&mut r.cmd_rx);
        assert!(cmds.iter().any(|c| matches!(c, AudioCommand::CancelFade { voice_id: 1, .. })));
        assert!(cmds.contains(&AudioCommand::Stop { voice_id: 1, fade_frames: 240, curve: CurveTable::LINEAR }));
    }

    #[test]
    fn pause_resume_stop_and_panic_commands() {
        let mut r = rig(vec![audio_cue(1, 0.0, 0.0, DoNotContinue), audio_cue(2, 0.0, 0.0, DoNotContinue)]);
        r.sched.go(0);
        r.sched.go(0);
        drain(&mut r.cmd_rx);
        r.sched.pause_all(SEC);
        r.sched.resume_all(2 * SEC);
        r.sched.panic(3 * SEC);
        let cmds = drain(&mut r.cmd_rx);
        assert_eq!(cmds.iter().filter(|c| matches!(c, AudioCommand::Pause { fade_frames: 240, .. })).count(), 2);
        assert_eq!(cmds.iter().filter(|c| matches!(c, AudioCommand::Resume { .. })).count(), 2);
        assert_eq!(cmds.iter().filter(|c| matches!(c, AudioCommand::Stop { fade_frames: 48_000, .. })).count(), 2, "Panic dura 1 s");

        let mut r = rig(vec![audio_cue(1, 0.0, 0.0, DoNotContinue)]);
        r.sched.go(0);
        drain(&mut r.cmd_rx);
        r.sched.hard_stop_all(SEC);
        assert!(drain(&mut r.cmd_rx).contains(&AudioCommand::Stop { voice_id: 1, fade_frames: 96, curve: CurveTable::LINEAR }));
    }

    #[test]
    fn commands_keep_their_order_when_the_ring_is_full() {
        let cues = (1..=6).map(|i| audio_cue(i, 0.0, 0.0, DoNotContinue)).collect();
        let mut r = rig_with_capacity(cues, 2);
        for i in 0..6 {
            r.sched.go(i * 1_000);
        }
        let stats = r.sched.stats();
        assert!(stats.ring_full_events > 0 && r.sched.pending_commands() == 4, "{stats:?}");
        let mut order = Vec::new();
        for t in 0..20 {
            for c in drain(&mut r.cmd_rx) {
                if let AudioCommand::Play { voice_id, .. } = c {
                    order.push(voice_id);
                }
            }
            r.sched.tick(10_000 + t * 1_000);
        }
        assert_eq!(order, vec![1, 2, 3, 4, 5, 6]);
        assert_eq!(r.sched.pending_commands(), 0);
        assert_eq!(r.sched.stats().commands_sent, 6);
    }

    #[test]
    fn hot_ring_events_drive_go_panic_and_live_faders() {
        let mut r = rig(vec![audio_cue(1, 0.0, 0.0, DoNotContinue), audio_cue(2, 0.0, 0.0, DoNotContinue)]);
        let ring = Box::new(HotControlRing::new());
        ring.push(HotControlEvent::Go).unwrap();
        ring.push(HotControlEvent::LiveCrosspointDrag { cue_num_id: 1, row: 1, col: 2, db: -9.0 }).unwrap();
        ring.push(HotControlEvent::Go).unwrap();
        ring.push(HotControlEvent::Panic).unwrap();
        assert_eq!(r.sched.poll_hot_ring(&ring, SEC), 4);
        let cmds = drain(&mut r.cmd_rx);
        assert!(matches!(cmds[0], AudioCommand::Play { voice_id: 1, .. }));
        assert_eq!(cmds[1], AudioCommand::SetCrosspointDb { voice_id: 1, row: 1, col: 2, db: -9.0 });
        assert!(matches!(cmds[2], AudioCommand::Play { voice_id: 2, .. }));
        assert_eq!(cmds.iter().filter(|c| matches!(c, AudioCommand::Stop { .. })).count(), 2);
        assert_eq!(r.sched.poll_hot_ring(&ring, SEC), 0);
        ring.push(HotControlEvent::StopAll).unwrap();
        assert_eq!(r.sched.poll_hot_ring(&ring, SEC), 1);
    }

    #[test]
    fn show_status_reports_playhead_and_progress() {
        let mut r = rig(vec![audio_cue(1, 0.5, 0.0, DoNotContinue), audio_cue(2, 0.0, 2.0, AutoContinue), audio_cue(3, 0.0, 0.0, DoNotContinue)]);
        r.sched.go(0);
        r.sched.tick(250_000);
        let s = r.sched.show_status(250_000);
        assert_eq!(s.active_playhead_cue_id, 2);
        assert!(s.is_playing);
        assert_eq!(s.running_cues[0].cue_id, 1);
        assert_eq!(s.running_cues[0].phase, CuePhase::PreWait);
        assert!((s.running_cues[0].pre_wait_elapsed_sec - 0.25).abs() < 1e-6);
        assert_eq!(s.running_cues[1].phase, CuePhase::Idle);

        r.sched.tick(500_000);
        r.sched.go(600_000);
        let s = r.sched.show_status(1_100_000);
        let (a, b) = (s.running_cues[0], s.running_cues[1]);
        assert_eq!((a.cue_id, a.phase), (1, CuePhase::Action));
        assert!((a.action_elapsed_sec - 0.6).abs() < 1e-6);
        assert_eq!((b.cue_id, b.phase), (2, CuePhase::Action));
        assert!((b.post_wait_elapsed_sec - 0.5).abs() < 1e-6);

        let shm = SeqlockShowStatus::new();
        r.sched.publish_status(&shm, 1_100_000);
        assert_eq!(shm.snapshot().unwrap().running_cues[1].cue_id, 2);
    }

    #[test]
    fn groups_dispatch_all_children_at_once() {
        let mut list = CueList::new(1, Timebase::new(48_000));
        list.add_cue(group_cue(10, GroupMode::Timeline)).unwrap();
        list.add_child(CueId(10), audio_cue(11, 0.0, 0.0, DoNotContinue)).unwrap();
        list.add_child(CueId(10), audio_cue(12, 0.0, 0.0, DoNotContinue)).unwrap();
        let (cmd_tx, mut cmd_rx) = HeapRb::<AudioCommand>::new(16).split();
        let (_st_tx, st_rx) = HeapRb::<AudioStatus>::new(16).split();
        let mut s = Scheduler::new(SchedulerConfig::default(), list, cmd_tx, st_rx);
        s.go(SEC);
        let ids: Vec<_> = drain(&mut cmd_rx).into_iter().map(|c| match c {
            AudioCommand::Play { voice_id, start_host_time_ns } => (voice_id, start_host_time_ns),
            o => panic!("{o:?}"),
        }).collect();
        assert_eq!(ids, vec![(11, 1_000_000_000), (12, 1_000_000_000)]);
    }

    #[test]
    fn run_loop_serves_go_pressed_from_another_thread() {
        let mut r = rig(vec![audio_cue(1, 0.0, 0.0, DoNotContinue), audio_cue(2, 0.2, 0.0, DoNotContinue)]);
        let ring = HotControlRing::new();
        let stop = AtomicBool::new(false);
        let clock = SystemClock::new();
        let t0 = std::time::Instant::now();
        let mut seen = Vec::new();
        let sched = std::thread::scope(|s| {
            let worker = s.spawn(|| {
                r.sched.run(&clock, Some(&ring), &stop);
                r.sched
            });
            // La UI pulsa GO dos veces; el segundo tiene 200 ms de pre-wait.
            ring.push(HotControlEvent::Go).unwrap();
            ring.push(HotControlEvent::Go).unwrap();
            while seen.len() < 2 && t0.elapsed() < Duration::from_secs(3) {
                if let Some(c) = r.cmd_rx.try_pop() {
                    seen.push((c, t0.elapsed()));
                }
                std::thread::sleep(Duration::from_micros(200));
            }
            stop.store(true, Ordering::Release);
            worker.join().unwrap()
        });
        assert_eq!(seen.len(), 2, "llegaron {seen:?}");
        assert!(matches!(seen[0].0, AudioCommand::Play { voice_id: 1, .. }));
        assert!(seen[0].1 < Duration::from_millis(50), "GO tardó {:?}", seen[0].1);
        assert!(matches!(seen[1].0, AudioCommand::Play { voice_id: 2, .. }));
        let gap = seen[1].1 - seen[0].1;
        assert!((Duration::from_millis(190)..Duration::from_millis(260)).contains(&gap), "pre-wait de {gap:?}");
        assert_eq!(sched.list().playback_position(), None);
    }

    #[test]
    fn manual_clock_driven_loop_steps_deterministically() {
        let mut r = rig(vec![audio_cue(1, 1.0, 0.0, DoNotContinue)]);
        let clock = ManualClock::new(0);
        r.sched.go(clock.now_micros());
        for _ in 0..1_000 {
            clock.advance(1_000);
            r.sched.tick(clock.now_micros());
        }
        assert_eq!(drain(&mut r.cmd_rx), vec![AudioCommand::Play { voice_id: 1, start_host_time_ns: 1_000_000_000 }]);
        assert_eq!(r.sched.stats().ticks, 1_000);
    }

    // -- Extremo a extremo con el motor real ----------------------------------------------------

    #[test]
    fn end_to_end_with_the_real_engine_auto_follow() {
        use qlab_engine::disk_streamer::StreamingVoiceReader;
        use qlab_engine::mixer_simd::process_rt_callback;
        use qlab_engine::voice::{AudioLevelMatrixRt, Voice, VoiceInit, VoiceRtState};
        use qlab_engine::voice_pool::VoicePool;
        use std::sync::Arc;

        let mut list = CueList::new(1, Timebase::new(48_000));
        list.add_cue(audio_cue(1, 0.0, 0.0, AutoFollow)).unwrap();
        list.add_cue(audio_cue(2, 0.0, 0.0, DoNotContinue)).unwrap();
        let (cmd_tx, mut cmd_rx) = HeapRb::<AudioCommand>::new(64).split();
        let (mut st_tx, st_rx) = HeapRb::<AudioStatus>::new(256).split();
        let mut sched = Scheduler::new(SchedulerConfig::new(48_000), list, cmd_tx, st_rx);

        let make_voice = |id: u64, level: f32, frames: usize| {
            let reader = StreamingVoiceReader::from_interleaved(2, 0, vec![level; frames * 2].into_boxed_slice());
            let rt = VoiceRtState::new(0, &[], AudioLevelMatrixRt::new(2, 2)).unwrap();
            Arc::new(Voice::with_reader(
                VoiceInit {
                    id,
                    cue_unique_id: id as u128,
                    patch_id: 0,
                    file_sample_rate: 48_000,
                    start_frame: 0,
                    end_frame: frames as u64,
                    infinite_loop: false,
                    rate: 1.0,
                },
                rt,
                reader,
            ))
        };
        let mut pool = VoicePool::new(4);
        let (v1, v2) = (make_voice(1, 0.25, 1_000), make_voice(2, 0.5, 1_000));
        pool.insert_voice(Arc::clone(&v1));
        pool.insert_voice(Arc::clone(&v2));
        let handle = pool.rt_handle();
        drop(handle.load());

        let mut tail = [0.0f32; 64];
        let mut out = vec![0.0f32; 256 * 2];
        let mut all = Vec::new();
        sched.go(0);
        for block in 0..16u64 {
            process_rt_callback(&mut out, 2, 48_000, &handle, &mut cmd_rx, &mut st_tx, &mut tail);
            all.extend(out.chunks(2).map(|f| f[0]));
            // El control corre entre bloques, con el reloj en el instante del bloque siguiente.
            sched.tick((block + 1) * 256 * 1_000_000 / 48_000);
        }
        assert!(all[..1_000].iter().all(|&s| (s - 0.25).abs() < 1e-6), "primera pista");
        let second_start = all.iter().position(|&s| (s - 0.5).abs() < 1e-6).expect("la segunda pista debe sonar");
        assert!((1_000..=1_024).contains(&second_start), "AutoFollow arranca en el bloque siguiente: {second_start}");
        assert!(all[second_start..second_start + 1_000].iter().all(|&s| (s - 0.5).abs() < 1e-6));
        assert!(all[second_start + 1_000..].iter().all(|&s| s == 0.0));
        assert_eq!(sched.list().state(CueId(1)), Some(CueState::Completed));
        assert_eq!(sched.list().state(CueId(2)), Some(CueState::Completed));
        assert_eq!(sched.list().playback_position(), None);
    }
}
