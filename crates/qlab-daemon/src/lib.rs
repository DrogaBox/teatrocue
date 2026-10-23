//! `teatrocue-daemon`: el servidor de audio desacoplado de la UI (spec §2.1).
//!
//! * [`backend_audio`]: stream de tiempo real (`cpal`: PipeWire/ALSA, WASAPI, ASIO) y callback.
//! * [`sequencer_thread`]: el `Scheduler` de `qlab-core` a 1 kHz.
//! * [`ipc_server`] y [`shm`]: socket local y memoria compartida con la UI.
//! * [`watchdog`]: vigilancia de 250 ms con safe mute y reinicio del stream.
//! * [`daemon`]: orquesta el ciclo de vida. `src/main.rs` es solo la línea de órdenes y las señales.

pub mod adhoc;
pub mod backend_audio;
pub mod config;
pub mod daemon;
pub mod cue_edit;
pub mod events;
pub mod health;
pub mod ipc_server;
pub mod osc;
pub mod rt_priority;
pub mod sequencer_thread;
pub mod shm;
pub mod show_loader;
pub mod voice_loader;
pub mod watchdog;

pub use config::{AllocCheck, CliAction, DaemonConfig};
pub use daemon::{Daemon, DaemonError};

#[cfg(test)]
pub(crate) mod test_support;
#[cfg(test)]
mod integration_tests;

#[cfg(test)]
#[global_allocator]
static TEST_ALLOCATOR: qlab_engine::alloc_guard::RtGuardAllocator = qlab_engine::alloc_guard::RtGuardAllocator::system();
