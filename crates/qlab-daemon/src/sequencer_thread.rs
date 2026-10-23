//! Hilo secuenciador `qlab-sequencer`: el *tick loop* de 1 kHz del `Scheduler` (spec §2.1).
//!
//! Cada tick (1 ms, con plazos absolutos y temporizador de alta resolución):
//! 1. aplica las órdenes estructurales del IPC,
//! 2. atiende el anillo caliente de memoria compartida (GO/Panic/fader de la UI),
//! 3. `Scheduler::tick`: drena el estado del motor y empuja `AudioCommand` al anillo SPSC,
//! 4. difunde los eventos a los clientes, anota su latido y, a ~120 Hz, publica la telemetría.
//!
//! [`SequencerCore`] contiene esa lógica sin hilos ni relojes, para probarla paso a paso.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use qlab_core::clock::Micros;
use qlab_core::cue::CueKind;
use qlab_protocol::matrix::AudioLevelMatrixData;
use qlab_protocol::show_format::{validate_node, CueNodeDto};
use qlab_core::cue_list::{CueList, Effect};
use qlab_core::scheduler::Scheduler;
use qlab_engine::rt_command::{AudioCommand, AudioStatus};
use qlab_protocol::cue_dto::CueId;
use qlab_protocol::ipc_msg::{CueSummary, RunningCueInfo, ServerEvent, StatusSnapshot};
use qlab_protocol::ipc_shm::HotControlEvent;
use ringbuf::{HeapCons, HeapProd};

use crate::cue_edit::{cue_to_node, level_commands, node_to_kind};
use crate::events::EventHub;
use crate::health::{Epoch, Health};
use crate::rt_priority::{elevate_current_thread, ThreadRole};
use crate::shm::SharedMemory;

pub type DaemonScheduler = Scheduler<HeapProd<AudioCommand>, HeapCons<AudioStatus>>;

/// Órdenes que el IPC (otros hilos) manda al secuenciador, que es el único dueño de la lista.
pub enum SeqCommand {
    /// Sustituye el show. Para antes todo lo que suena; responde al terminar.
    InstallShow { list: CueList, reply: Sender<()> },
    Hot(HotControlEvent),
    /// Comando directo al motor de audio (reproductores independientes: Carts y audición).
    Audio(qlab_engine::rt_command::AudioCommand),
    Control(Control),
    SetPlayhead(Option<CueId>),
    Devamp(CueId),
    /// Acción de control sobre una cue (control remoto).
    CueAction(CueId, qlab_core::cue::ControlAction),
    /// Inserta una cue nueva (ver `CueList::insert_cue`).
    AddCue { cue: Box<qlab_core::cue::Cue>, parent: Option<CueId>, after: Option<CueId>, reply: Sender<Result<(), String>> },
    /// Elimina una cue y sus hijas; responde con los ids eliminados.
    RemoveCue { id: CueId, reply: Sender<Result<Vec<CueId>, String>> },
    MoveCue { id: CueId, delta: i32, reply: Sender<Result<bool, String>> },
    /// La lista completa como árbol del formato nativo, para guardar.
    ExportShow(Sender<Vec<CueNodeDto>>),
    GetStatus(Sender<StatusSnapshot>),
    GetCues(Sender<Vec<CueSummary>>),
    /// Datos completos de una cue: `(nodo, padre, estado)`.
    GetCueDetail(CueId, Sender<Option<(CueNodeDto, Option<u64>, String)>>),
    /// Aplica una edición del Inspector (la voz ya se recargó si hacía falta).
    UpdateCue { node: Box<CueNodeDto>, reply: Sender<Result<(), String>> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Control {
    PauseAll,
    ResumeAll,
    HardStopAll,
}

#[derive(Clone, Debug)]
pub struct SequencerConfig {
    /// Cadencia del tick (1 ms = 1000 Hz).
    pub period: Duration,
    /// Cada cuánto se publica la telemetría (8 ms ≈ 120 Hz).
    pub status_period: Duration,
    /// Prioridad `SCHED_FIFO` en Linux.
    pub rt_priority: u8,
    /// Leer el anillo caliente de la memoria compartida (UI que escribe directo).
    pub poll_hot_ring: bool,
}

impl Default for SequencerConfig {
    fn default() -> Self {
        Self { period: Duration::from_millis(1), status_period: Duration::from_millis(8), rt_priority: 80, poll_hot_ring: true }
    }
}

// ---------------------------------------------------------------------------------------------
// Lógica de un tick
// ---------------------------------------------------------------------------------------------

pub struct SequencerCore {
    scheduler: DaemonScheduler,
    rx: Receiver<SeqCommand>,
    shm: Arc<SharedMemory>,
    health: Arc<Health>,
    events: Arc<EventHub>,
    cfg: SequencerConfig,
    last_status_ns: Option<u64>,
    effects: Vec<Effect>,
    /// Voces del motor, de donde se lee la posición real de lectura (`published_frame_pos`).
    voices: Option<VoiceHandle>,
}

pub type VoiceHandle = Arc<arc_swap::ArcSwap<Vec<Arc<qlab_engine::voice::Voice>>>>;

/// Cota de órdenes del IPC por tick, para que una ráfaga no robe tiempo al reloj.
const MAX_COMMANDS_PER_TICK: usize = 256;

impl SequencerCore {
    pub fn new(
        scheduler: DaemonScheduler,
        rx: Receiver<SeqCommand>,
        shm: Arc<SharedMemory>,
        health: Arc<Health>,
        events: Arc<EventHub>,
        cfg: SequencerConfig,
    ) -> Self {
        Self { scheduler, rx, shm, health, events, cfg, last_status_ns: None, effects: Vec::new(), voices: None }
    }

    /// Conecta las voces del motor para publicar la posición real dentro del archivo.
    pub fn with_voices(mut self, voices: Option<VoiceHandle>) -> Self {
        self.voices = voices;
        self
    }

    pub fn scheduler(&self) -> &DaemonScheduler {
        &self.scheduler
    }

    /// Un tick en el instante `now_ns` del reloj del daemon (el mismo del `Scheduler`).
    pub fn tick(&mut self, now_ns: u64) {
        let now = now_ns / 1_000;
        for _ in 0..MAX_COMMANDS_PER_TICK {
            match self.rx.try_recv() {
                Ok(cmd) => self.handle(cmd, now),
                Err(_) => break,
            }
        }
        if self.cfg.poll_hot_ring {
            self.scheduler.poll_hot_ring(&self.shm.telemetry().hot_ring, now);
        }
        self.scheduler.tick(now);
        self.forward_events();

        self.health.seq_heartbeat_ns.store(now_ns, Ordering::Release);
        self.health.seq_ticks.fetch_add(1, Ordering::Relaxed);
        self.health.commands_sent.store(self.scheduler.stats().commands_sent, Ordering::Relaxed);
        self.shm.telemetry().beat(now_ns);

        let due = self.last_status_ns.map_or(true, |t| now_ns.saturating_sub(t) >= self.cfg.status_period.as_nanos() as u64);
        if due {
            self.last_status_ns = Some(now_ns);
            self.publish_status(now);
        }
    }

    fn handle(&mut self, cmd: SeqCommand, now: Micros) {
        match cmd {
            SeqCommand::InstallShow { list, reply } => {
                self.scheduler.hard_stop_all(now);
                let _old = self.scheduler.replace_list(list);
                self.forward_events();
                let _ = reply.send(());
            }
            SeqCommand::Hot(event) => self.scheduler.apply_hot_event(event, now),
            SeqCommand::Control(Control::PauseAll) => self.scheduler.pause_all(now),
            SeqCommand::Control(Control::ResumeAll) => self.scheduler.resume_all(now),
            SeqCommand::Control(Control::HardStopAll) => self.scheduler.hard_stop_all(now),
            SeqCommand::AddCue { cue, parent, after, reply } => {
                let r = self.scheduler.list_mut().insert_cue(*cue, parent, after).map(|_| ()).map_err(|e| e.to_string());
                if r.is_ok() {
                    self.events.publish(ServerEvent::StructureChanged);
                }
                let _ = reply.send(r);
            }
            SeqCommand::RemoveCue { id, reply } => {
                let r = self.scheduler.list_mut().remove_cue(id).map_err(|e| e.to_string());
                if r.is_ok() {
                    self.events.publish(ServerEvent::StructureChanged);
                }
                let _ = reply.send(r);
            }
            SeqCommand::MoveCue { id, delta, reply } => {
                let r = self.scheduler.list_mut().move_cue(id, delta).map_err(|e| e.to_string());
                if matches!(r, Ok(true)) {
                    self.events.publish(ServerEvent::StructureChanged);
                }
                let _ = reply.send(r);
            }
            SeqCommand::ExportShow(reply) => {
                let _ = reply.send(crate::cue_edit::list_to_nodes(self.scheduler.list()));
            }
            SeqCommand::Devamp(cue) => {
                self.scheduler.devamp(cue, now);
            }
            SeqCommand::Audio(cmd) => self.scheduler.send_command(cmd),
            SeqCommand::CueAction(cue, action) => self.scheduler.cue_action(cue, action, now),
            SeqCommand::SetPlayhead(cue) => {
                if let Err(e) = self.scheduler.set_playhead(cue) {
                    self.events.publish(ServerEvent::Warning { source: "sequencer".into(), message: e.to_string() });
                }
            }
            SeqCommand::GetStatus(reply) => {
                let _ = reply.send(self.status_snapshot(now));
            }
            SeqCommand::GetCues(reply) => {
                let _ = reply.send(cue_summaries(self.scheduler.list()));
            }
            SeqCommand::GetCueDetail(id, reply) => {
                let list = self.scheduler.list();
                let detail = list.cue(id).map(|c| (cue_to_node(c), list.parent_of(id).map(|p| p.0), format!("{:?}", c.state())));
                let _ = reply.send(detail);
            }
            SeqCommand::UpdateCue { node, reply } => {
                let _ = reply.send(self.update_cue(*node));
            }
        }
    }

    /// Sustituye los datos de una cue conservando su estado de ejecución y avisa al motor de los
    /// niveles, el mute/solo y la velocidad que cambiaron.
    fn update_cue(&mut self, node: CueNodeDto) -> Result<(), String> {
        validate_node(&node).map_err(|e| e.to_string())?;
        let id = node.basics.unique_id;
        let old_audio = match self.scheduler.list().cue(id) {
            Some(c) => c.audio_data().cloned(),
            None => return Err(format!("no existe la cue {}", id.0)),
        };
        let kind = node_to_kind(&node, old_audio.as_ref().map(|a| &a.levels), AudioLevelMatrixData::new_one_to_one(2, 2).expect("2x2 es válido"));
        let dead = node.dead;
        let list = self.scheduler.list_mut();
        list.update_cue(id, node.basics, node.triggers, kind).map_err(|e| e.to_string())?;
        if let Some(c) = list.cue_mut(id) {
            c.dead = dead;
        }
        let new_audio = list.cue(id).and_then(|c| c.audio_data()).cloned();
        if let (Some(old), Some(new)) = (old_audio, new_audio) {
            for cmd in level_commands(id, &old, &new) {
                self.scheduler.send_command(cmd);
            }
        }
        Ok(())
    }

    fn forward_events(&mut self) {
        self.scheduler.drain_events(&mut self.effects);
        for effect in self.effects.drain(..) {
            if let Effect::SendOsc { destination, message, .. } = &effect {
                crate::osc::send_async(destination.clone(), message.clone(), Arc::clone(&self.events));
            }
            if let Some(event) = to_event(&effect) {
                self.events.publish(event);
            }
        }
    }

    fn publish_status(&mut self, now: Micros) {
        let mut status = self.scheduler.show_status(now);
        let h = &self.health;
        status.total_xruns = h.xruns.load(Ordering::Relaxed);
        status.callback_cpu_load_permille = h.cpu_permille.load(Ordering::Relaxed);
        status.audio_freeze_detected = h.audio_stalled.load(Ordering::Relaxed);
        if let Some(voices) = &self.voices {
            // Posición real del motor: el frame del archivo que se leerá a continuación (retrocede en cada
            // vuelta de un bucle o slice), no un tiempo acumulado.
            let list = voices.load();
            for c in status.running_cues.iter_mut().filter(|c| c.phase == qlab_protocol::ipc_shm::CuePhase::Action) {
                if let Some(v) = list.iter().find(|v| v.id == c.cue_id) {
                    c.source_frame = v.published_frame_pos.load(Ordering::Relaxed);
                    c.peak = v.take_peak();
                }
            }
        }
        self.shm.telemetry().status.publish(&status);
    }

    pub fn status_snapshot(&mut self, now: Micros) -> StatusSnapshot {
        let status = self.scheduler.show_status(now);
        let now_frames = self.scheduler.frames_at(now);
        let h = &self.health;
        StatusSnapshot {
            playhead: self.scheduler.list().playback_position().map(|c| c.0),
            running: status
                .running_cues
                .iter()
                .filter(|c| c.phase != qlab_protocol::ipc_shm::CuePhase::Idle)
                .map(|c| {
                    let slices = self.scheduler.list().cue(CueId(c.cue_id)).and_then(|cue| cue.audio_data()).is_some_and(|a| !a.time_loops.slices.is_empty() || a.time_loops.infinite_loop);
                    let st = if slices { self.scheduler.list_mut().slice_status(CueId(c.cue_id), now_frames) } else { None };
                    RunningCueInfo {
                        id: c.cue_id,
                        phase: format!("{:?}", c.phase),
                        pre_wait_elapsed_sec: c.pre_wait_elapsed_sec,
                        action_elapsed_sec: c.action_elapsed_sec,
                        post_wait_elapsed_sec: c.post_wait_elapsed_sec,
                        slice: st.map(|s| s.slice as u16),
                        vamping: st.is_some_and(|s| s.vamping),
                    }
                })
                .collect(),
            xruns: h.xruns.load(Ordering::Relaxed),
            cpu_permille: h.cpu_permille.load(Ordering::Relaxed),
            audio_stalled: h.audio_stalled.load(Ordering::Relaxed),
            sequencer_stalled: h.sequencer_stalled.load(Ordering::Relaxed),
            safe_mute: h.safe_mute.load(Ordering::Relaxed),
            sequencer_ticks: h.seq_ticks.load(Ordering::Relaxed),
            commands_sent: self.scheduler.stats().commands_sent,
            rt_alloc_violations: h.rt_alloc_violations.load(Ordering::Relaxed),
        }
    }
}

fn to_event(effect: &Effect) -> Option<ServerEvent> {
    Some(match effect {
        Effect::PlayheadMoved { position, .. } => ServerEvent::PlayheadMoved { cue: position.map(|c| c.0) },
        Effect::ArmChanged { cue } => ServerEvent::CueUpdated { cue: cue.0 },
        Effect::StateChanged { cue, to, .. } => ServerEvent::CueStateChanged { cue: cue.0, state: format!("{to:?}") },
        Effect::Engine(AudioStatus::DiskBufferUnderrun { voice_id, at_frame }) => ServerEvent::Warning {
            source: "engine".into(),
            message: format!("underrun de disco en la voz {voice_id} (frame {at_frame})"),
        },
        Effect::Engine(AudioStatus::SeamDetected { output_channel, jump_delta }) => ServerEvent::Warning {
            source: "engine".into(),
            message: format!("costura en la salida {output_channel} (salto de {jump_delta:.3})"),
        },
        _ => return None,
    })
}

/// Todas las cues (grupos y sus hijos incluidos), en orden de lista.
pub fn cue_summaries(list: &CueList) -> Vec<CueSummary> {
    fn walk(list: &CueList, id: CueId, out: &mut Vec<CueSummary>) {
        let Some(cue) = list.cue(id) else { return };
        let kind = match &cue.kind {
            CueKind::Audio(_) => "audio",
            CueKind::Video(_) => "video",
            CueKind::Fade(_) => "fade",
            CueKind::Control(_) => "control",
            CueKind::Memo => "memo",
            CueKind::Osc(_) => "osc",
            CueKind::Group(_) => "group",
        };
        let tb = list.timebase();
        let timing = cue.timing(tb);
        let duration = match &cue.kind {
            CueKind::Group(_) => list.group_timeline_frames(id),
            _ => timing.action,
        }
        .map(|f| tb.frames_to_secs(f));
        let target = match &cue.kind {
            CueKind::Audio(_) | CueKind::Video(_) => cue
                .basics
                .file_target
                .as_ref()
                .and_then(|t| std::path::Path::new(&t.absolute_path).file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            CueKind::Fade(f) => f.target.and_then(|t| list.cue(t)).map(|t| format!("→ {}", t.basics.number)).unwrap_or_default(),
            CueKind::Control(c) => {
                let verb = match c.action {
                    qlab_core::cue::ControlAction::Stop => "Stop",
                    qlab_core::cue::ControlAction::Pause => "Pause",
                    qlab_core::cue::ControlAction::Resume => "Resume",
                    qlab_core::cue::ControlAction::Devamp => "Devamp",
                    qlab_core::cue::ControlAction::GoTo => "Go to",
                    qlab_core::cue::ControlAction::Load => "Load",
                    qlab_core::cue::ControlAction::Arm => "Arm",
                    qlab_core::cue::ControlAction::Disarm => "Disarm",
                };
                c.target.and_then(|t| list.cue(t)).map(|t| format!("{verb} -> {}", t.basics.number)).unwrap_or_else(|| verb.to_string())
            }
            CueKind::Memo => String::new(),
            CueKind::Osc(o) => o.destination.clone(),
            CueKind::Group(_) => String::new(),
        };
        out.push(CueSummary {
            id: id.0,
            number: cue.basics.number.clone(),
            name: cue.basics.display_name().to_string(),
            kind: kind.into(),
            state: format!("{:?}", cue.state()),
            armed: cue.basics.armed,
            parent: list.parent_of(id).map(|p| p.0),
            target,
            pre_wait: cue.basics.pre_wait,
            post_wait: cue.basics.post_wait,
            duration,
            continue_mode: cue.basics.continue_mode,
            flagged: cue.basics.flagged,
            color: cue.basics.color,
            dead: cue.dead,
            memories: cue.audio_data().map(|a| a.time_loops.memories.clone()).unwrap_or_default(),
            file_sample_rate: cue.audio_data().map_or(0, |a| a.time_loops.file_sample_rate),
            start_override_secs: cue.audio_data().and_then(|a| a.time_loops.start_override_secs),
        });
        if let CueKind::Group(g) = &cue.kind {
            for &child in &g.children {
                walk(list, child, out);
            }
        }
    }
    let mut out = Vec::new();
    for &id in list.order() {
        walk(list, id, &mut out);
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Temporizador periódico de alta resolución
// ---------------------------------------------------------------------------------------------

/// Resultado de una espera.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Wake {
    /// Retraso respecto al plazo ideal.
    pub late_ns: u64,
    /// Plazos enteros que se perdieron (el temporizador se resincroniza en vez de recuperar a ráfagas).
    pub missed: u64,
}

#[cfg(target_os = "linux")]
mod timer_imp {
    use super::*;

    /// `clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME)`: plazos absolutos, sin deriva acumulada.
    pub struct PeriodicTimer {
        next_ns: i128,
        period_ns: i128,
    }

    fn now_ns() -> i128 {
        let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        // SAFETY: `ts` es válido para escritura.
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
        ts.tv_sec as i128 * 1_000_000_000 + ts.tv_nsec as i128
    }

    impl PeriodicTimer {
        pub fn new(period: Duration) -> std::io::Result<Self> {
            Ok(Self { next_ns: now_ns(), period_ns: period.as_nanos() as i128 })
        }

        pub fn wait_next(&mut self) -> Wake {
            self.next_ns += self.period_ns;
            let ts = libc::timespec {
                tv_sec: (self.next_ns / 1_000_000_000) as libc::time_t,
                tv_nsec: (self.next_ns % 1_000_000_000) as libc::c_long,
            };
            loop {
                // SAFETY: `ts` es válido; con TIMER_ABSTIME no hay `remain`.
                let rc = unsafe { libc::clock_nanosleep(libc::CLOCK_MONOTONIC, libc::TIMER_ABSTIME, &ts, std::ptr::null_mut()) };
                if rc != libc::EINTR {
                    break;
                }
            }
            let late = (now_ns() - self.next_ns).max(0);
            let mut wake = Wake { late_ns: late as u64, missed: 0 };
            if late >= self.period_ns {
                wake.missed = (late / self.period_ns) as u64;
                self.next_ns = now_ns();
            }
            wake
        }
    }
}

#[cfg(windows)]
mod timer_imp {
    use super::*;
    use std::time::Instant;
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::Threading::{
        CreateWaitableTimerExW, SetWaitableTimer, WaitForSingleObject, CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, INFINITE,
        TIMER_ALL_ACCESS,
    };

    /// Temporizador de espera de alta resolución (Windows 10 1803+) más un tramo final de espera activa.
    pub struct PeriodicTimer {
        handle: HANDLE,
        next: Instant,
        period: Duration,
    }

    // SAFETY: el HANDLE del temporizador solo lo usa el hilo que lo posee.
    unsafe impl Send for PeriodicTimer {}

    /// Margen que se deja a la espera activa: la resolución real del temporizador ronda 0,5 ms.
    const SPIN_MARGIN: Duration = Duration::from_micros(300);

    impl PeriodicTimer {
        pub fn new(period: Duration) -> std::io::Result<Self> {
            // SAFETY: sin atributos ni nombre.
            let handle = unsafe { CreateWaitableTimerExW(None, None, CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_ALL_ACCESS.0) }
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            Ok(Self { handle, next: Instant::now(), period })
        }

        pub fn wait_next(&mut self) -> Wake {
            self.next += self.period;
            let now = Instant::now();
            if self.next > now + SPIN_MARGIN {
                let wait = self.next - now - SPIN_MARGIN;
                let due = -((wait.as_nanos() / 100) as i64); // negativo = relativo, en unidades de 100 ns
                // SAFETY: `due` vive durante la llamada; `handle` es válido.
                unsafe {
                    if SetWaitableTimer(self.handle, &due, 0, None, None, false).is_ok() {
                        WaitForSingleObject(self.handle, INFINITE);
                    }
                }
            }
            while Instant::now() < self.next {
                std::hint::spin_loop();
            }
            let late = Instant::now().saturating_duration_since(self.next);
            let mut wake = Wake { late_ns: late.as_nanos() as u64, missed: 0 };
            if late >= self.period {
                wake.missed = (late.as_nanos() / self.period.as_nanos().max(1)) as u64;
                self.next = Instant::now();
            }
            wake
        }
    }

    impl Drop for PeriodicTimer {
        fn drop(&mut self) {
            // SAFETY: se cierra una sola vez.
            unsafe {
                let _ = CloseHandle(self.handle);
            }
        }
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
mod timer_imp {
    use super::*;
    use std::time::Instant;

    pub struct PeriodicTimer {
        next: Instant,
        period: Duration,
    }

    impl PeriodicTimer {
        pub fn new(period: Duration) -> std::io::Result<Self> {
            Ok(Self { next: Instant::now(), period })
        }

        pub fn wait_next(&mut self) -> Wake {
            self.next += self.period;
            let now = Instant::now();
            if self.next > now + Duration::from_micros(200) {
                std::thread::sleep(self.next - now - Duration::from_micros(200));
            }
            while Instant::now() < self.next {
                std::hint::spin_loop();
            }
            let late = Instant::now().saturating_duration_since(self.next);
            let mut wake = Wake { late_ns: late.as_nanos() as u64, missed: 0 };
            if late >= self.period {
                wake.missed = (late.as_nanos() / self.period.as_nanos().max(1)) as u64;
                self.next = Instant::now();
            }
            wake
        }
    }
}

pub use timer_imp::PeriodicTimer;

// ---------------------------------------------------------------------------------------------
// Hilo
// ---------------------------------------------------------------------------------------------

pub struct SequencerHandle {
    tx: Sender<SeqCommand>,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl SequencerHandle {
    /// Crea el hilo. Devuelve también el emisor para las órdenes estructurales.
    pub fn spawn(scheduler: DaemonScheduler, shm: Arc<SharedMemory>, health: Arc<Health>, events: Arc<EventHub>, epoch: Epoch, cfg: SequencerConfig) -> std::io::Result<Self> {
        Self::spawn_with_voices(scheduler, shm, health, events, epoch, cfg, None)
    }

    /// Como `spawn`, con acceso a las voces del motor para publicar la posición real de lectura.
    pub fn spawn_with_voices(
        scheduler: DaemonScheduler,
        shm: Arc<SharedMemory>,
        health: Arc<Health>,
        events: Arc<EventHub>,
        epoch: Epoch,
        cfg: SequencerConfig,
        voices: Option<VoiceHandle>,
    ) -> std::io::Result<Self> {
        let (tx, rx) = channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::clone(&stop);
        let join = std::thread::Builder::new().name("qlab-sequencer".into()).spawn(move || {
            match elevate_current_thread(ThreadRole::Sequencer, cfg.rt_priority) {
                Ok(msg) => eprintln!("[sequencer] prioridad: {msg}"),
                Err(e) => eprintln!("[sequencer] aviso: {e}"),
            }
            let period = cfg.period;
            let mut timer = match PeriodicTimer::new(period) {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("[sequencer] sin temporizador de alta resolución: {e}");
                    return;
                }
            };
            let health_loop = Arc::clone(&health);
            let mut core = SequencerCore::new(scheduler, rx, shm, health, events, cfg).with_voices(voices);
            health_loop.seq_heartbeat_ns.store(epoch.now_ns(), Ordering::Release);
            health_loop.seq_running.store(true, Ordering::Release);
            while !stop_flag.load(Ordering::Acquire) {
                let wake = timer.wait_next();
                core.tick(epoch.now_ns());
                health_loop.seq_max_late_ns.fetch_max(wake.late_ns, Ordering::Relaxed);
                if wake.missed > 0 {
                    health_loop.seq_overruns.fetch_add(wake.missed, Ordering::Relaxed);
                }
            }
            health_loop.seq_running.store(false, Ordering::Release);
        })?;
        Ok(Self { tx, stop, join: Some(join) })
    }

    pub fn sender(&self) -> Sender<SeqCommand> {
        self.tx.clone()
    }

    pub fn send(&self, cmd: SeqCommand) -> bool {
        self.tx.send(cmd).is_ok()
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

impl Drop for SequencerHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use qlab_core::clock::Timebase;
    use qlab_core::cue::{AudioCueData, Cue};
    use qlab_core::scheduler::SchedulerConfig;
    use qlab_engine::rt_command::{COMMAND_RING_CAPACITY, STATUS_RING_CAPACITY};
    use qlab_protocol::cue_dto::{AudioTimeAndLoopsData, CueBasicsData, ContinueMode};
    use qlab_protocol::matrix::AudioLevelMatrixData;
    use ringbuf::traits::{Consumer, Producer, Split};
    use ringbuf::HeapRb;
    use std::sync::mpsc::channel;

    const MS: u64 = 1_000_000;

    pub(crate) fn audio_cue(id: u64, post: f64, mode: ContinueMode) -> Cue {
        let basics = CueBasicsData { unique_id: CueId(id), number: id.to_string(), post_wait: post, continue_mode: mode, ..Default::default() };
        Cue::audio(
            basics,
            AudioCueData::new(
                AudioTimeAndLoopsData { end_time_sec: 10.0, ..Default::default() },
                AudioLevelMatrixData::new_one_to_one(2, 2).unwrap(),
            ),
        )
    }

    pub(crate) fn list_of(cues: Vec<Cue>) -> CueList {
        let mut l = CueList::new(1, Timebase::new(48_000));
        for c in cues {
            l.add_cue(c).unwrap();
        }
        l
    }

    struct Rig {
        core: SequencerCore,
        tx: Sender<SeqCommand>,
        cmd_rx: HeapCons<AudioCommand>,
        st_tx: HeapProd<AudioStatus>,
        shm: Arc<SharedMemory>,
        health: Arc<Health>,
        events_rx: Receiver<ServerEvent>,
    }

    fn rig(tag: &str, cues: Vec<Cue>) -> Rig {
        let (cmd_tx, cmd_rx) = HeapRb::<AudioCommand>::new(COMMAND_RING_CAPACITY).split();
        let (st_tx, st_rx) = HeapRb::<AudioStatus>::new(STATUS_RING_CAPACITY).split();
        let scheduler = Scheduler::new(SchedulerConfig::new(48_000), list_of(cues), cmd_tx, st_rx);
        let (tx, rx) = channel();
        let shm = Arc::new(SharedMemory::create(&format!("qlab_test_{}_{tag}", std::process::id())).unwrap());
        let health = Arc::new(Health::default());
        let events = Arc::new(EventHub::new());
        let events_rx = events.subscribe();
        let core = SequencerCore::new(scheduler, rx, Arc::clone(&shm), Arc::clone(&health), events, SequencerConfig::default());
        Rig { core, tx, cmd_rx, st_tx, shm, health, events_rx }
    }

    fn drain(rx: &mut HeapCons<AudioCommand>) -> Vec<AudioCommand> {
        std::iter::from_fn(|| rx.try_pop()).collect()
    }

    #[test]
    fn go_from_ipc_reaches_the_audio_ring_and_moves_the_playhead() {
        let mut r = rig("seq_go", vec![audio_cue(1, 0.0, ContinueMode::DoNotContinue), audio_cue(2, 0.0, ContinueMode::DoNotContinue)]);
        r.tx.send(SeqCommand::Hot(HotControlEvent::Go)).unwrap();
        r.core.tick(5 * MS);
        assert_eq!(drain(&mut r.cmd_rx), vec![AudioCommand::Play { voice_id: 1, start_host_time_ns: 5_000_000 }]);
        let evs: Vec<_> = r.events_rx.try_iter().collect();
        assert!(evs.contains(&ServerEvent::PlayheadMoved { cue: Some(2) }));
        assert!(evs.contains(&ServerEvent::CueStateChanged { cue: 1, state: "Running".into() }));
        assert_eq!(r.health.seq_ticks.load(Ordering::Relaxed), 1);
        assert_eq!(r.health.seq_heartbeat_ns.load(Ordering::Relaxed), 5 * MS);
        assert_eq!(r.shm.telemetry().last_heartbeat_ns(), 5 * MS);
        assert_eq!(r.health.commands_sent.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn hot_ring_from_the_ui_is_polled_each_tick() {
        let mut r = rig("seq_ring", vec![audio_cue(1, 0.0, ContinueMode::DoNotContinue)]);
        r.shm.telemetry().hot_ring.push(HotControlEvent::Go).unwrap();
        r.shm.telemetry().hot_ring.push(HotControlEvent::LiveCrosspointDrag { cue_num_id: 1, row: 1, col: 1, db: -9.0 }).unwrap();
        r.core.tick(MS);
        let cmds = drain(&mut r.cmd_rx);
        assert!(matches!(cmds[0], AudioCommand::Play { voice_id: 1, .. }));
        assert_eq!(cmds[1], AudioCommand::SetCrosspointDb { voice_id: 1, row: 1, col: 1, db: -9.0 });
        assert!(r.shm.telemetry().hot_ring.is_empty());

        // Con el sondeo desactivado el anillo se deja intacto (otro lo consume).
        let mut cfg_off = rig("seq_ring_off", vec![audio_cue(1, 0.0, ContinueMode::DoNotContinue)]);
        cfg_off.core.cfg.poll_hot_ring = false;
        cfg_off.shm.telemetry().hot_ring.push(HotControlEvent::Go).unwrap();
        cfg_off.core.tick(MS);
        assert!(drain(&mut cfg_off.cmd_rx).is_empty());
        assert_eq!(cfg_off.shm.telemetry().hot_ring.len(), 1);
    }

    #[test]
    fn the_one_millisecond_tick_drives_waits_and_chains() {
        let mut r = rig("seq_chain", vec![audio_cue(1, 0.5, ContinueMode::AutoContinue), audio_cue(2, 0.0, ContinueMode::DoNotContinue)]);
        r.tx.send(SeqCommand::Hot(HotControlEvent::Go)).unwrap();
        let mut plays = Vec::new();
        for ms in 0..=700u64 {
            r.core.tick(ms * MS);
            for c in drain(&mut r.cmd_rx) {
                if let AudioCommand::Play { voice_id, start_host_time_ns } = c {
                    plays.push((voice_id, start_host_time_ns));
                }
            }
        }
        assert_eq!(plays, vec![(1, 0), (2, 500 * MS)]);
        assert_eq!(r.health.seq_ticks.load(Ordering::Relaxed), 701);
    }

    #[test]
    fn engine_status_flows_back_and_drives_auto_follow() {
        let mut r = rig("seq_follow", vec![audio_cue(1, 0.0, ContinueMode::AutoFollow), audio_cue(2, 0.0, ContinueMode::DoNotContinue)]);
        r.core.tick(0);
        r.tx.send(SeqCommand::Hot(HotControlEvent::Go)).unwrap();
        r.core.tick(MS);
        drain(&mut r.cmd_rx);
        assert!(r.st_tx.try_push(AudioStatus::VoiceCompleted { voice_id: 1, natural_eof: true }).is_ok());
        r.core.tick(2 * MS);
        assert!(matches!(drain(&mut r.cmd_rx)[..], [AudioCommand::Play { voice_id: 2, .. }]));
        assert!(r.st_tx.try_push(AudioStatus::DiskBufferUnderrun { voice_id: 2, at_frame: 9 }).is_ok());
        r.core.tick(3 * MS);
        assert!(r.events_rx.try_iter().any(|e| matches!(e, ServerEvent::Warning { source, .. } if source == "engine")));
    }

    #[test]
    fn telemetry_is_published_at_about_120_hz_with_health_overlaid() {
        let mut r = rig("seq_status", vec![audio_cue(1, 0.0, ContinueMode::DoNotContinue)]);
        r.health.xruns.store(7, Ordering::Relaxed);
        r.health.cpu_permille.store(123, Ordering::Relaxed);
        r.health.audio_stalled.store(true, Ordering::Relaxed);
        r.core.tick(0);
        let s = r.shm.telemetry().status.snapshot().unwrap();
        assert_eq!((s.active_playhead_cue_id, s.total_xruns, s.callback_cpu_load_permille, s.audio_freeze_detected), (1, 7, 123, true));
        let v0 = r.shm.telemetry().status.version();
        for ms in 1..=80u64 {
            r.core.tick(ms * MS);
        }
        let published = r.shm.telemetry().status.version() - v0;
        assert!((9..=11).contains(&published), "80 ms a 8 ms por publicación, hubo {published}");
        r.tx.send(SeqCommand::Hot(HotControlEvent::Go)).unwrap();
        r.core.tick(90 * MS);
        r.core.tick(99 * MS);
        let s = r.shm.telemetry().status.snapshot().unwrap();
        assert!(s.is_playing);
        assert_eq!(s.running_cues[0].cue_id, 1);
    }

    #[test]
    fn structural_queries_and_show_replacement() {
        let mut r = rig("seq_query", vec![audio_cue(1, 0.0, ContinueMode::DoNotContinue), audio_cue(2, 0.0, ContinueMode::DoNotContinue)]);
        r.tx.send(SeqCommand::Hot(HotControlEvent::Go)).unwrap();
        let (cues_tx, cues_rx) = channel();
        let (st_tx, st_rx) = channel();
        r.tx.send(SeqCommand::GetCues(cues_tx)).unwrap();
        r.tx.send(SeqCommand::GetStatus(st_tx)).unwrap();
        r.core.tick(MS);
        let cues = cues_rx.try_recv().unwrap();
        assert_eq!(cues.iter().map(|c| (c.id, c.state.as_str())).collect::<Vec<_>>(), vec![(1, "Running"), (2, "Idle")]);
        assert_eq!(cues[0].kind, "audio");
        assert_eq!(cues[0].duration, Some(10.0), "región 0..10 s");
        let status = st_rx.try_recv().unwrap();
        assert_eq!(status.playhead, Some(2));
        assert_eq!(status.running.len(), 1);

        // Reemplazar el show: para lo que suena y deja la lista nueva con el playhead al inicio.
        drain(&mut r.cmd_rx);
        let (done_tx, done_rx) = channel();
        r.tx.send(SeqCommand::InstallShow { list: list_of(vec![audio_cue(10, 0.0, ContinueMode::DoNotContinue)]), reply: done_tx }).unwrap();
        r.core.tick(2 * MS);
        done_rx.try_recv().unwrap();
        assert!(drain(&mut r.cmd_rx).iter().any(|c| matches!(c, AudioCommand::Stop { voice_id: 1, fade_frames: 96, .. })));
        r.tx.send(SeqCommand::Hot(HotControlEvent::Go)).unwrap();
        r.core.tick(3 * MS);
        assert!(drain(&mut r.cmd_rx).iter().any(|c| matches!(c, AudioCommand::Play { voice_id: 10, .. })));

        // Playhead inexistente: aviso, sin caída.
        r.tx.send(SeqCommand::SetPlayhead(Some(CueId(999)))).unwrap();
        r.core.tick(4 * MS);
        assert!(r.events_rx.try_iter().any(|e| matches!(e, ServerEvent::Warning { source, .. } if source == "sequencer")));
    }

    #[test]
    fn cue_detail_and_live_edits_reach_the_engine() {
        use qlab_protocol::show_format::CueBody;
        let mut r = rig("seq_edit", vec![audio_cue(1, 0.0, ContinueMode::DoNotContinue)]);
        r.tx.send(SeqCommand::Hot(HotControlEvent::Go)).unwrap();
        r.core.tick(MS);
        drain(&mut r.cmd_rx);
        let (tx, rx) = channel();
        r.tx.send(SeqCommand::GetCueDetail(CueId(1), tx)).unwrap();
        let (tx2, rx2) = channel();
        r.tx.send(SeqCommand::GetCueDetail(CueId(99), tx2)).unwrap();
        r.core.tick(2 * MS);
        let (mut node, parent, state) = rx.try_recv().unwrap().expect("existe");
        assert_eq!((parent, state.as_str()), (None, "Running"));
        assert!(rx2.try_recv().unwrap().is_none());

        // Editar con la cue sonando: metadatos, niveles y velocidad se aplican sin parar nada.
        node.basics.name = "editada".into();
        node.basics.post_wait = 2.0;
        let CueBody::Audio(a) = &mut node.body else { panic!() };
        a.time_loops.rate = 1.25;
        let levels = a.levels.as_mut().unwrap();
        levels.set_db(1, 1, -12.0).unwrap();
        levels.set_mute(2, true).unwrap();
        let (done_tx, done_rx) = channel();
        r.tx.send(SeqCommand::UpdateCue { node: Box::new(node.clone()), reply: done_tx }).unwrap();
        r.core.tick(3 * MS);
        done_rx.try_recv().unwrap().expect("edición aceptada");
        let cmds = drain(&mut r.cmd_rx);
        assert!(cmds.contains(&AudioCommand::SetCrosspointDb { voice_id: 1, row: 1, col: 1, db: -12.0 }));
        assert!(cmds.contains(&AudioCommand::SetMuteSoloMask { voice_id: 1, mute_mask: 0b10, solo_mask: 0 }));
        assert!(cmds.contains(&AudioCommand::SetRate { voice_id: 1, rate: 1.25 }));
        let cue = r.core.scheduler().list().cue(CueId(1)).unwrap();
        assert_eq!(cue.basics.name, "editada");
        assert_eq!(cue.state(), qlab_core::cue_state::CueState::Running, "sigue sonando");

        // Rechazos: cue inexistente, id cambiado de tipo y datos inválidos.
        for bad in [
            { let mut n = node.clone(); n.basics.unique_id = CueId(42); n },
            { let mut n = node.clone(); n.basics.pre_wait = -1.0; n },
            { let mut n = node.clone(); if let CueBody::Audio(a) = &mut n.body { a.time_loops.rate = 99.0; } n },
        ] {
            let (t, rx) = channel();
            r.tx.send(SeqCommand::UpdateCue { node: Box::new(bad), reply: t }).unwrap();
            r.core.tick(4 * MS);
            assert!(rx.try_recv().unwrap().is_err());
        }
    }

    #[test]
    fn pause_resume_and_hard_stop_controls() {
        let mut r = rig("seq_ctl", vec![audio_cue(1, 0.0, ContinueMode::DoNotContinue)]);
        r.tx.send(SeqCommand::Hot(HotControlEvent::Go)).unwrap();
        r.core.tick(MS);
        drain(&mut r.cmd_rx);
        for (c, ms) in [(Control::PauseAll, 2), (Control::ResumeAll, 3), (Control::HardStopAll, 4)] {
            r.tx.send(SeqCommand::Control(c)).unwrap();
            r.core.tick(ms * MS);
        }
        let cmds = drain(&mut r.cmd_rx);
        assert!(matches!(cmds[0], AudioCommand::Pause { voice_id: 1, .. }));
        assert!(matches!(cmds[1], AudioCommand::Resume { voice_id: 1, .. }));
        assert!(matches!(cmds[2], AudioCommand::Stop { voice_id: 1, fade_frames: 96, .. }));
    }

    #[test]
    fn periodic_timer_keeps_a_one_millisecond_cadence() {
        let mut t = PeriodicTimer::new(Duration::from_millis(1)).unwrap();
        t.wait_next(); // alinear
        let t0 = std::time::Instant::now();
        let mut worst = 0u64;
        let mut missed = 0;
        for _ in 0..300 {
            let w = t.wait_next();
            worst = worst.max(w.late_ns);
            missed += w.missed;
        }
        let elapsed = t0.elapsed();
        // Plazos absolutos: 300 ticks son 300 ms sin deriva, aunque cada espera tenga jitter.
        assert!((Duration::from_millis(295)..Duration::from_millis(320)).contains(&elapsed), "{elapsed:?}");
        assert!(worst < 20_000_000, "el peor retraso fue de {} µs", worst / 1_000);
        assert!(missed <= 2, "plazos perdidos: {missed}");
        println!("temporizador: peor retraso {} µs en 300 ticks", worst / 1_000);
    }

    #[test]
    fn the_sequencer_thread_ticks_at_1khz_and_stops_cleanly() {
        let (cmd_tx, mut cmd_rx) = HeapRb::<AudioCommand>::new(COMMAND_RING_CAPACITY).split();
        let (_st_tx, st_rx) = HeapRb::<AudioStatus>::new(STATUS_RING_CAPACITY).split();
        let epoch = Epoch::now();
        let scheduler = Scheduler::new(SchedulerConfig::new(48_000), list_of(vec![audio_cue(1, 0.0, ContinueMode::DoNotContinue)]), cmd_tx, st_rx);
        let shm = Arc::new(SharedMemory::create(&format!("qlab_test_{}_seq_thread", std::process::id())).unwrap());
        let health = Arc::new(Health::default());
        let mut handle =
            SequencerHandle::spawn(scheduler, Arc::clone(&shm), Arc::clone(&health), Arc::new(EventHub::new()), epoch, SequencerConfig::default()).unwrap();

        let wait = |cond: &dyn Fn() -> bool| {
            let t0 = std::time::Instant::now();
            while !cond() {
                assert!(t0.elapsed() < Duration::from_secs(3), "timeout");
                std::thread::sleep(Duration::from_millis(1));
            }
        };
        wait(&|| health.seq_running.load(Ordering::Acquire));
        let (t_a, t0) = (health.seq_ticks.load(Ordering::Relaxed), std::time::Instant::now());
        // GO desde la UI por el anillo compartido mientras corre el hilo.
        shm.telemetry().hot_ring.push(HotControlEvent::Go).unwrap();
        let first = loop {
            if let Some(c) = cmd_rx.try_pop() {
                break c;
            }
            assert!(t0.elapsed() < Duration::from_secs(3), "el GO no llegó");
            std::thread::sleep(Duration::from_micros(100));
        };
        let play_latency = t0.elapsed();
        assert!(matches!(first, AudioCommand::Play { voice_id: 1, .. }));
        assert!(play_latency < Duration::from_millis(50), "GO tardó {play_latency:?}");

        std::thread::sleep(Duration::from_millis(500));
        let ticks = health.seq_ticks.load(Ordering::Relaxed) - t_a;
        let rate = ticks as f64 / t0.elapsed().as_secs_f64();
        assert!((850.0..=1_050.0).contains(&rate), "cadencia de {rate:.0} Hz");
        let hb = health.seq_heartbeat_ns.load(Ordering::Acquire);
        assert!(hb > 0 && epoch.now_ns() - hb < 50_000_000, "latido fresco");
        handle.stop();
        assert!(!health.seq_running.load(Ordering::Acquire));
        assert!(!handle.send(SeqCommand::Control(Control::PauseAll)), "el hilo ya no escucha");
        println!("secuenciador: {rate:.0} Hz, GO en {play_latency:?}, peor retraso {} µs", health.seq_max_late_ns.load(Ordering::Relaxed) / 1_000);
    }
}
