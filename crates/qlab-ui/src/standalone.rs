//! Modo independiente: arrancar el motor dentro de la ventana, o lanzar el daemon como proceso aparte.
//!
//! El motor embebido es el mismo `qlab-daemon` en un hilo, y la UI lo usa por el mismo socket y la
//! misma memoria compartida que usaría con un daemon externo, así que no hay un segundo camino de
//! código. El proceso aparte es lo recomendable en escena: el audio sobrevive si la ventana se cierra.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::channel;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use qlab_daemon::backend_audio::BackendKind;
use qlab_daemon::{Daemon, DaemonConfig};

/// Nombre de la memoria compartida para un socket: el predeterminado usa el nombre estándar; cualquier
/// otro socket (pruebas, varias instancias) tiene el suyo para no pisar el de otro daemon.
pub fn shm_name_for_socket(socket: &str) -> String {
    if socket == qlab_protocol::ipc_msg::default_socket_name() {
        return qlab_daemon::shm::default_shm_name().to_string();
    }
    let hash = socket.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3));
    format!("teatrocue_rt_{hash:016x}")
}

pub fn embedded_config(socket: &str, with_audio: bool, audio: &crate::settings::AudioSettings) -> DaemonConfig {
    let mut cfg = DaemonConfig::default();
    if with_audio {
        audio.apply_to(&mut cfg);
    }
    cfg.socket = socket.to_string();
    cfg.shm_name = shm_name_for_socket(socket);
    cfg.backend.kind = if with_audio { BackendKind::Cpal } else { BackendKind::Null };
    cfg.lock_memory = false;
    cfg
}

pub struct EmbeddedEngine {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl EmbeddedEngine {
    /// Arranca el motor y espera (hasta 15 s) a que esté listo o falle.
    pub fn start(cfg: DaemonConfig) -> Result<Self, String> {
        let (ready_tx, ready_rx) = channel::<Result<(), String>>();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::clone(&stop);
        let join = std::thread::Builder::new()
            .name("qlab-embedded-engine".into())
            .spawn(move || {
                // El motor se crea y vive en este hilo (el stream de audio no es `Send`).
                let mut daemon = match Daemon::start(cfg) {
                    Ok(d) => {
                        let _ = ready_tx.send(Ok(()));
                        d
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e.to_string()));
                        return;
                    }
                };
                while !stop_flag.load(Ordering::Acquire) && !daemon.shutdown_requested() {
                    daemon.poll();
                    std::thread::sleep(Duration::from_millis(20));
                }
                daemon.stop();
            })
            .map_err(|e| e.to_string())?;
        match ready_rx.recv_timeout(Duration::from_secs(15)) {
            Ok(Ok(())) => Ok(Self { stop, join: Some(join) }),
            Ok(Err(e)) => {
                let _ = join.join();
                Err(e)
            }
            Err(_) => Err("el motor embebido no arrancó en 15 s".into()),
        }
    }

    pub fn is_running(&self) -> bool {
        self.join.as_ref().is_some_and(|j| !j.is_finished())
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

impl Drop for EmbeddedEngine {
    fn drop(&mut self) {
        self.stop();
    }
}

/// `teatrocue-daemon` junto al ejecutable de la UI (o en el `PATH`).
pub fn find_daemon_binary() -> Option<PathBuf> {
    let name = if cfg!(windows) { "teatrocue-daemon.exe" } else { "teatrocue-daemon" };
    let beside = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join(name))).filter(|p| p.is_file());
    beside.or_else(|| {
        std::env::var_os("PATH").and_then(|paths| std::env::split_paths(&paths).map(|d| d.join(name)).find(|p| p.is_file()))
    })
}

/// Lanza el daemon como proceso independiente (sobrevive a la ventana).
pub fn launch_daemon_process(socket: &str, audio: &crate::settings::AudioSettings) -> Result<(), String> {
    let bin = find_daemon_binary().ok_or("no se encontró teatrocue-daemon junto a esta aplicación ni en el PATH")?;
    let mut child = Command::new(&bin)
        .args(["--socket", socket])
        .args(audio.daemon_args())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("{}: {e}", bin.display()))?;
    // Recoge su estado de salida sin bloquear a nadie para no dejar un proceso zombi.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shm_names_are_isolated_per_socket() {
        let default = qlab_protocol::ipc_msg::default_socket_name();
        assert_eq!(shm_name_for_socket(default), qlab_daemon::shm::default_shm_name());
        let (a, b) = (shm_name_for_socket("/tmp/a.sock"), shm_name_for_socket("/tmp/b.sock"));
        assert_ne!(a, b);
        assert_eq!(a, shm_name_for_socket("/tmp/a.sock"), "es determinista");
        assert_ne!(a, qlab_daemon::shm::default_shm_name(), "no pisa al daemon estándar");
        assert!(a.starts_with("teatrocue_rt_") && !a.contains('/'));
    }

    #[test]
    fn embedded_config_picks_the_backend() {
        assert_eq!(embedded_config("/tmp/x.sock", true, &Default::default()).backend.kind, BackendKind::Cpal);
        assert_eq!(embedded_config("/tmp/x.sock", false, &Default::default()).backend.kind, BackendKind::Null);
        assert_eq!(embedded_config("/tmp/x.sock", false, &Default::default()).socket, "/tmp/x.sock");
    }

    #[test]
    fn the_embedded_engine_starts_serves_and_stops() {
        let socket = format!("/tmp/qlab-ui-embedded-{}.sock", std::process::id());
        let cfg = embedded_config(&socket, false, &Default::default());
        let mut engine = EmbeddedEngine::start(cfg).expect("arranca");
        assert!(engine.is_running());
        let c = qlab_protocol::ipc_client::IpcClient::connect(&socket).unwrap();
        let r = c.request(qlab_protocol::ipc_msg::ClientRequest::Ping { nonce: 5 }, Duration::from_secs(2)).unwrap();
        assert_eq!(r, qlab_protocol::ipc_msg::ServerResponse::Pong { nonce: 5 });
        drop(c);
        engine.stop();
        assert!(!engine.is_running());
        assert!(qlab_protocol::ipc_client::IpcClient::connect(&socket).is_err(), "el socket desaparece al parar");
    }

    #[test]
    fn a_failing_engine_reports_why_instead_of_hanging() {
        let mut cfg = embedded_config("/tmp/qlab-ui-embedded-bad.sock", true, &Default::default());
        cfg.backend.host = Some("no-existe".into());
        cfg.shm_name = format!("qlab_ui_embedded_bad_{}", std::process::id());
        let err = EmbeddedEngine::start(cfg).err().expect("debe fallar");
        assert!(err.contains("no-existe"), "{err}");
    }

    #[test]
    fn launching_without_a_binary_is_an_error_not_a_panic() {
        // En las pruebas no hay `teatrocue-daemon` junto al ejecutable de pruebas.
        if find_daemon_binary().is_none() {
            assert!(launch_daemon_process("/tmp/x.sock", &Default::default()).is_err());
        }
    }
}
