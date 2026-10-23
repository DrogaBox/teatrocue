//! Orquestación del ciclo de vida del daemon (spec §4.3, fase 4).
//!
//! Arranque: memoria compartida → carga de voces → backend de audio → secuenciador → watchdog → IPC →
//! show inicial. Apagado en orden inverso, parando antes el audio con una rampa de 2 ms.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use qlab_core::clock::Timebase;
use qlab_core::cue_list::CueList;
use qlab_core::scheduler::{Scheduler, SchedulerConfig};
use qlab_engine::rt_command::{COMMAND_RING_CAPACITY, STATUS_RING_CAPACITY};
use qlab_protocol::ipc_msg::ServerEvent;
use ringbuf::traits::Split;
use ringbuf::HeapRb;

use crate::backend_audio::{AudioBackend, BackendError, BackendInfo, RtAudioContext};
use crate::config::{AllocCheck, DaemonConfig};
use crate::events::EventHub;
use crate::health::{Epoch, Health};
use crate::ipc_server::{IpcContext, IpcServer, ServerInfo};
use crate::rt_priority::{lock_process_memory, raise_process_priority};
use crate::sequencer_thread::{Control, SeqCommand, SequencerHandle};
use crate::shm::SharedMemory;
use crate::voice_loader::VoiceLoader;
use crate::watchdog::WatchdogHandle;

#[derive(Debug)]
pub enum DaemonError {
    SharedMemory(std::io::Error),
    Audio(BackendError),
    Sequencer(std::io::Error),
    Watchdog(std::io::Error),
    Ipc(std::io::Error),
    Show(String),
}

impl std::fmt::Display for DaemonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SharedMemory(e) => write!(f, "memoria compartida: {e}"),
            Self::Audio(e) => write!(f, "audio: {e}"),
            Self::Sequencer(e) => write!(f, "secuenciador: {e}"),
            Self::Watchdog(e) => write!(f, "watchdog: {e}"),
            Self::Ipc(e) => write!(f, "IPC: {e}"),
            Self::Show(e) => write!(f, "show inicial: {e}"),
        }
    }
}

impl std::error::Error for DaemonError {}

fn log(msg: impl AsRef<str>) {
    eprintln!("[TeatroCue] {}", msg.as_ref());
}

pub struct Daemon {
    cfg: DaemonConfig,
    health: Arc<Health>,
    events: Arc<EventHub>,
    shm: Arc<SharedMemory>,
    loader: Arc<Mutex<VoiceLoader>>,
    ipc_ctx: Arc<IpcContext>,
    backend: AudioBackend,
    sequencer: SequencerHandle,
    watchdog: WatchdogHandle,
    ipc: IpcServer,
    osc: Option<crate::osc::OscServer>,
    shutdown: Arc<AtomicBool>,
    stopped: bool,
}

impl Daemon {
    pub fn start(cfg: DaemonConfig) -> Result<Self, DaemonError> {
        if cfg.lock_memory {
            match lock_process_memory() {
                Ok(m) => log(format!("memoria fijada: {m}")),
                Err(e) => log(format!("aviso: {e}")),
            }
        }
        if let Ok(m) = raise_process_priority() {
            log(format!("clase de proceso: {m}"));
        }
        qlab_engine::alloc_guard::set_violation_policy(match cfg.alloc_check {
            AllocCheck::Abort => qlab_engine::alloc_guard::ViolationPolicy::Abort,
            _ => qlab_engine::alloc_guard::ViolationPolicy::CountOnly,
        });

        let epoch = Epoch::now();
        let health = Arc::new(Health::default());
        // Auriculares: en otra placa son un par de canales internos extra al final de la mezcla; en la misma placa, el par
        // `monitor_first` elegido.
        let monitor_slot = match cfg.backend.separate_monitor_device() {
            Some(_) => Some(cfg.backend.channels),
            None => cfg.monitor_first,
        };
        health.monitor_first.store(monitor_slot.map_or(0, |m| u32::from(m) + 1), std::sync::atomic::Ordering::Relaxed);
        let events = Arc::new(EventHub::new());
        let shm = Arc::new(SharedMemory::create(&cfg.shm_name).map_err(DaemonError::SharedMemory)?);
        log(format!("memoria compartida `{}`", shm.name()));

        let channels = cfg.backend.mix_channels();
        let loader = Arc::new(Mutex::new(VoiceLoader::new(cfg.disk_threads, channels)));
        let (cmd_tx, cmd_rx) = HeapRb::new(COMMAND_RING_CAPACITY).split();
        let (status_tx, status_rx) = HeapRb::new(STATUS_RING_CAPACITY).split();

        let ctx = RtAudioContext::new(loader.lock().expect("envenenado").rt_handle(), cmd_rx, status_tx);
        let mut backend_cfg = cfg.backend.clone();
        backend_cfg.rt_alloc_check = cfg.alloc_check != AllocCheck::Off;
        let backend = AudioBackend::start(backend_cfg, ctx, Arc::clone(&health), Arc::clone(&shm), epoch).map_err(DaemonError::Audio)?;
        let info = backend.info().clone();
        log(format!(
            "audio: {} · {} · {} Hz · {} canales · buffer {} frames ({:.2} ms){}",
            info.backend,
            info.device,
            info.sample_rate,
            info.channels,
            info.buffer_frames,
            info.latency_ms(),
            if info.exclusive_active { " · exclusivo" } else { "" }
        ));
        if let Some(m) = &info.monitor_device {
            log(format!("auriculares (PFL): salida independiente `{m}`"));
        }
        for w in &info.warnings {
            log(format!("aviso: {w}"));
        }

        let timebase = Timebase::new(info.sample_rate);
        let scheduler = Scheduler::new(SchedulerConfig::new(info.sample_rate), CueList::new(0, timebase), cmd_tx, status_rx);
        let sequencer =
            SequencerHandle::spawn_with_voices(scheduler, Arc::clone(&shm), Arc::clone(&health), Arc::clone(&events), epoch, cfg.sequencer.clone(), Some(loader.lock().expect("envenenado").rt_handle()))
                .map_err(DaemonError::Sequencer)?;
        let watchdog = WatchdogHandle::spawn(cfg.watchdog, epoch, Arc::clone(&health), Arc::clone(&events)).map_err(DaemonError::Watchdog)?;

        let shutdown = Arc::new(AtomicBool::new(false));
        let ipc_ctx = Arc::new(IpcContext::new(
            sequencer.sender(),
            Arc::clone(&shm),
            Arc::clone(&events),
            Arc::clone(&health),
            Arc::clone(&loader),
            timebase,
            ServerInfo { shm_name: shm.name().to_string(), sample_rate: info.sample_rate, channels: info.channels, backend: info.backend.clone() },
            cfg.ring_producer,
            Arc::clone(&shutdown),
        ).with_license(cfg.license.clone()));
        let ipc = IpcServer::start(&cfg.socket, Arc::clone(&ipc_ctx)).map_err(DaemonError::Ipc)?;
        log(format!("IPC en `{}`", ipc.path()));
        let osc = match cfg.osc_port {
            Some(port) => match crate::osc::OscServer::start(port, Arc::clone(&ipc_ctx)) {
                Ok(o) => {
                    log(format!("control remoto OSC en UDP {}", o.port()));
                    Some(o)
                }
                Err(e) => {
                    log(format!("aviso: no se pudo abrir el puerto OSC {port}: {e}"));
                    None
                }
            },
            None => None,
        };

        let daemon = Self {
            cfg: cfg.clone(),
            health,
            events,
            shm,
            loader,
            ipc_ctx,
            backend,
            sequencer,
            watchdog,
            ipc,
            osc,
            shutdown,
            stopped: false,
        };
        if let Some(path) = &cfg.show {
            daemon.load_show(path).map_err(DaemonError::Show)?;
        }
        Ok(daemon)
    }

    /// Puerto UDP del control remoto OSC si está activo.
    pub fn osc_port(&self) -> Option<u16> {
        self.osc.as_ref().map(crate::osc::OscServer::port)
    }

    /// Carga un show nativo y lo instala (lo mismo que el comando IPC `load_show`).
    pub fn load_show(&self, path: &Path) -> Result<(), String> {
        match self.ipc_ctx.load_show_file(path)? {
            qlab_protocol::ipc_msg::ServerResponse::ShowLoaded { name, cues, warnings } => {
                log(format!("show `{name}` cargado: {cues} cues"));
                for w in warnings {
                    log(format!("aviso: {w}"));
                }
                Ok(())
            }
            other => Err(format!("respuesta inesperada: {other:?}")),
        }
    }

    pub fn config(&self) -> &DaemonConfig {
        &self.cfg
    }

    pub fn health(&self) -> &Arc<Health> {
        &self.health
    }

    pub fn events(&self) -> &Arc<EventHub> {
        &self.events
    }

    pub fn shm(&self) -> &Arc<SharedMemory> {
        &self.shm
    }

    pub fn loader(&self) -> &Arc<Mutex<VoiceLoader>> {
        &self.loader
    }

    pub fn backend_info(&self) -> &BackendInfo {
        self.backend.info()
    }

    pub fn shutdown_requested(&self) -> bool {
        self.shutdown.load(Ordering::Acquire)
    }

    /// Bandera que un manejador de señales puede activar para apagar el daemon.
    pub fn shutdown_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.shutdown)
    }

    /// Una vuelta del hilo principal: reinicia el stream si el watchdog lo pidió.
    pub fn poll(&mut self) {
        if self.health.restart_audio.swap(false, Ordering::AcqRel) {
            match self.backend.restart() {
                Ok(()) => log("stream de audio reiniciado"),
                Err(e) => {
                    log(format!("no se pudo reiniciar el stream de audio: {e}"));
                    self.events.publish(ServerEvent::Warning { source: "audio".into(), message: format!("reinicio fallido: {e}") });
                }
            }
        }
    }

    /// Bucle del hilo principal hasta que se pida el apagado.
    pub fn run(&mut self) {
        while !self.shutdown.load(Ordering::Acquire) {
            self.poll();
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Solo para pruebas: detiene el hilo del secuenciador sin tocar el resto, como si se hubiera
    /// congelado (su latido deja de avanzar pero el estado sigue marcado como "en marcha").
    #[cfg(test)]
    pub(crate) fn pause_sequencer_for_test(&mut self) {
        self.sequencer.stop();
        self.health.seq_running.store(true, Ordering::Release);
    }

    /// Apagado ordenado. Es idempotente.
    pub fn stop(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        log("apagando…");
        self.events.publish(ServerEvent::ShuttingDown);
        self.shutdown.store(true, Ordering::Release);
        if let Some(o) = self.osc.as_mut() {
            o.stop();
        }
        self.ipc.stop();
        // Hard stop con rampa de 2 ms y tiempo para que salga por el audio antes de cortar el stream.
        if self.sequencer.send(SeqCommand::Control(Control::HardStopAll)) {
            std::thread::sleep(Duration::from_millis(40));
        }
        self.sequencer.stop();
        self.watchdog.stop();
        self.backend.stop();
        if let Ok(mut loader) = self.loader.lock() {
            loader.unload_all();
        }
        // Las conexiones IPC abiertas pueden retener el segmento un rato: se le quita el nombre ya.
        self.shm.unlink();
        log("apagado");
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.stop();
    }
}
