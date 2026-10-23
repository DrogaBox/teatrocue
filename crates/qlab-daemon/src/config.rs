//! Configuración del daemon y análisis de la línea de órdenes.

use std::path::PathBuf;
use std::time::Duration;

use qlab_protocol::ipc_msg::default_socket_name;

use crate::backend_audio::{BackendConfig, BackendKind};
use crate::ipc_server::RingProducer;
use crate::sequencer_thread::SequencerConfig;
use crate::shm::default_shm_name;
use crate::watchdog::WatchdogConfig;

/// Qué hacer cuando el callback de audio asigna o libera memoria.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AllocCheck {
    /// No vigilar.
    Off,
    /// Contar las violaciones (aparecen en `get_status`). Valor por defecto.
    Count,
    /// Abortar con un backtrace (CI y desarrollo).
    Abort,
}

#[derive(Clone, Debug)]
pub struct DaemonConfig {
    pub socket: String,
    pub shm_name: String,
    pub backend: BackendConfig,
    pub sequencer: SequencerConfig,
    pub watchdog: WatchdogConfig,
    pub ring_producer: RingProducer,
    pub lock_memory: bool,
    pub disk_threads: usize,
    pub show: Option<PathBuf>,
    pub alloc_check: AllocCheck,
    /// Entorno de licencia para la reactivación de emergencia en caliente (`None` = no disponible).
    pub license: Option<qlab_license::activation::Setup>,
    /// Puerto UDP del control remoto OSC (`None` = apagado).
    pub osc_port: Option<u16>,
    /// Primer canal (desde 0) del par de auriculares (PFL / audición); `None` = sin salida de auriculares.
    pub monitor_first: Option<u16>,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            socket: default_socket_name().to_string(),
            shm_name: default_shm_name().to_string(),
            backend: BackendConfig::default(),
            sequencer: SequencerConfig::default(),
            watchdog: WatchdogConfig::default(),
            ring_producer: RingProducer::IpcServer,
            lock_memory: true,
            disk_threads: qlab_engine::disk_streamer::DiskStreamerPool::default_thread_count(),
            show: None,
            alloc_check: AllocCheck::Count,
            license: None,
            osc_port: None,
            monitor_first: None,
        }
    }
}

#[derive(Debug)]
pub enum CliAction {
    Run(Box<DaemonConfig>),
    ListDevices,
    /// Imprime el código de solicitud de reactivación de emergencia de esta PC.
    LicenseRequest,
    /// Canjea una clave de reactivación de emergencia.
    LicenseApply(String),
    Help,
    Version,
}

pub const USAGE: &str = "\
teatrocue-daemon — servidor de audio de TeatroCue

USO: teatrocue-daemon [opciones]

  --socket RUTA          socket Unix o tubería (por defecto /tmp/teatrocue.sock o \\\\.\\pipe\\teatrocue_ipc)
  --shm NOMBRE           segmento de memoria compartida
  --show ARCHIVO         show nativo (qlab-rs/v1) que se carga al arrancar
  --backend cpal|null    dispositivo real (cpal) o virtual sin hardware
  --host NOMBRE          host de audio de cpal (pipewire, alsa, wasapi, asio, jack…)
  --device NOMBRE        dispositivo de salida (en WASAPI exclusivo, su ID de endpoint)
  --rate HZ              frecuencia de muestreo (48000)
  --channels N           canales de salida (2)
  --buffer FRAMES        tamaño del buffer de audio (128)
  --exclusive            Windows: WASAPI en modo exclusivo
  --rt-priority N        prioridad SCHED_FIFO del hilo de audio en Linux (90)
  --seq-priority N       ídem del secuenciador (80)
  --ui-ring              una UI escribe directamente en el anillo caliente (el IPC no lo usa)
  --no-mlock             no fijar la memoria del proceso (Linux)
  --watchdog-ms N        umbral de congelamiento del watchdog (250)
  --no-safe-mute         no silenciar si el secuenciador se congela
  --alloc-check MODO     off | count | abort — vigilancia de asignaciones en el callback
  --monitor-device NOM   auriculares (PFL) en OTRA placa de sonido (p. ej. el jack de la notebook), por nombre
  --monitor-first N      auriculares (PFL): primer canal (desde 1) del par de salida, p. ej. 3 = canales 3-4
  --osc-port N           activa el control remoto OSC por UDP en el puerto N (apagado por defecto)
  --list-devices         lista las salidas de audio y termina
  --license-request      imprime el código de solicitud de reactivación de emergencia y termina
  --license-apply CLAVE  canjea una clave de reactivación (RESP-…) y termina
  --version, --help
";

fn value(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, String> {
    args.next().ok_or_else(|| format!("{flag} necesita un valor"))
}

fn number<T: std::str::FromStr>(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<T, String> {
    let v = value(args, flag)?;
    v.parse().map_err(|_| format!("{flag}: `{v}` no es un número válido"))
}

pub fn parse_args(args: impl IntoIterator<Item = String>) -> Result<CliAction, String> {
    let mut cfg = DaemonConfig::default();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => return Ok(CliAction::Help),
            "--version" | "-V" => return Ok(CliAction::Version),
            "--list-devices" => return Ok(CliAction::ListDevices),
            "--license-request" => return Ok(CliAction::LicenseRequest),
            "--license-apply" => return Ok(CliAction::LicenseApply(value(&mut args, &arg)?)),
            "--socket" => cfg.socket = value(&mut args, &arg)?,
            "--shm" => cfg.shm_name = value(&mut args, &arg)?,
            "--show" => cfg.show = Some(PathBuf::from(value(&mut args, &arg)?)),
            "--backend" => {
                cfg.backend.kind = match value(&mut args, &arg)?.as_str() {
                    "cpal" => BackendKind::Cpal,
                    "null" => BackendKind::Null,
                    other => return Err(format!("--backend: `{other}` no existe (cpal | null)")),
                }
            }
            "--host" => cfg.backend.host = Some(value(&mut args, &arg)?),
            "--device" => cfg.backend.device = Some(value(&mut args, &arg)?),
            "--rate" => cfg.backend.sample_rate = number(&mut args, &arg)?,
            "--channels" => cfg.backend.channels = number(&mut args, &arg)?,
            "--buffer" => cfg.backend.buffer_frames = number(&mut args, &arg)?,
            "--exclusive" => cfg.backend.exclusive = true,
            "--rt-priority" => cfg.backend.rt_priority = number(&mut args, &arg)?,
            "--seq-priority" => cfg.sequencer.rt_priority = number(&mut args, &arg)?,
            "--ui-ring" => cfg.ring_producer = RingProducer::ExternalUi,
            "--no-mlock" => cfg.lock_memory = false,
            "--monitor-device" => cfg.backend.monitor_device = Some(value(&mut args, &arg)?),
            "--monitor-first" => {
                let n: u16 = number(&mut args, &arg)?;
                if n == 0 {
                    return Err("--monitor-first cuenta desde 1".into());
                }
                cfg.monitor_first = Some(n - 1);
            }
            "--osc-port" => cfg.osc_port = Some(number(&mut args, &arg)?),
            "--watchdog-ms" => cfg.watchdog.timeout = Duration::from_millis(number(&mut args, &arg)?),
            "--no-safe-mute" => cfg.watchdog.mute_on_sequencer_stall = false,
            "--alloc-check" => cfg.alloc_check = parse_alloc_check(&value(&mut args, &arg)?)?,
            other => return Err(format!("opción desconocida `{other}` (usa --help)")),
        }
    }
    if cfg.backend.sample_rate < 8_000 || cfg.backend.sample_rate > 384_000 {
        return Err(format!("--rate {} fuera de 8000..=384000", cfg.backend.sample_rate));
    }
    if let Some(m) = cfg.monitor_first {
        if usize::from(m) + 2 > usize::from(cfg.backend.channels) {
            return Err(format!("--monitor-first {} necesita al menos {} canales de salida (hay {})", m + 1, m + 2, cfg.backend.channels));
        }
    }
    if !(1..=64).contains(&cfg.backend.channels) {
        return Err(format!("--channels {} fuera de 1..=64", cfg.backend.channels));
    }
    if cfg.backend.buffer_frames < 16 || cfg.backend.buffer_frames > 16_384 {
        return Err(format!("--buffer {} fuera de 16..=16384", cfg.backend.buffer_frames));
    }
    Ok(CliAction::Run(Box::new(cfg)))
}

pub fn parse_alloc_check(s: &str) -> Result<AllocCheck, String> {
    match s {
        "off" => Ok(AllocCheck::Off),
        "count" => Ok(AllocCheck::Count),
        "abort" => Ok(AllocCheck::Abort),
        other => Err(format!("--alloc-check: `{other}` no existe (off | count | abort)")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<CliAction, String> {
        parse_args(args.iter().map(|s| s.to_string()))
    }

    fn cfg(args: &[&str]) -> DaemonConfig {
        match parse(args).unwrap() {
            CliAction::Run(c) => *c,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn defaults_match_the_spec() {
        let c = cfg(&[]);
        assert_eq!((c.backend.sample_rate, c.backend.channels, c.backend.buffer_frames), (48_000, 2, 128));
        assert_eq!((c.backend.rt_priority, c.sequencer.rt_priority), (90, 80));
        assert_eq!(c.sequencer.period, Duration::from_millis(1));
        assert_eq!(c.watchdog.timeout, Duration::from_millis(250));
        assert_eq!(c.ring_producer, RingProducer::IpcServer);
        assert_eq!(c.alloc_check, AllocCheck::Count);
        assert!(c.lock_memory && c.disk_threads >= 2 && c.show.is_none());
        assert!(c.socket.contains("teatrocue") && c.shm_name.contains("teatrocue"));
    }

    #[test]
    fn every_option() {
        let c = cfg(&[
            "--socket", "/tmp/x.sock", "--shm", "seg", "--show", "a.json", "--backend", "null", "--host", "alsa", "--device", "Mi tarjeta",
            "--rate", "96000", "--channels", "8", "--buffer", "256", "--exclusive", "--rt-priority", "70", "--seq-priority", "60", "--ui-ring",
            "--no-mlock", "--watchdog-ms", "500", "--no-safe-mute", "--alloc-check", "abort",
        ]);
        assert_eq!((c.socket.as_str(), c.shm_name.as_str()), ("/tmp/x.sock", "seg"));
        assert_eq!(c.show, Some(PathBuf::from("a.json")));
        assert_eq!(c.backend.kind, BackendKind::Null);
        assert_eq!((c.backend.host.as_deref(), c.backend.device.as_deref()), (Some("alsa"), Some("Mi tarjeta")));
        assert_eq!((c.backend.sample_rate, c.backend.channels, c.backend.buffer_frames), (96_000, 8, 256));
        assert!(c.backend.exclusive && !c.lock_memory && !c.watchdog.mute_on_sequencer_stall);
        assert_eq!((c.backend.rt_priority, c.sequencer.rt_priority), (70, 60));
        assert_eq!(c.ring_producer, RingProducer::ExternalUi);
        assert_eq!(c.watchdog.timeout, Duration::from_millis(500));
        assert_eq!(c.alloc_check, AllocCheck::Abort);
    }

    #[test]
    fn actions_and_errors() {
        assert!(matches!(parse(&["--help"]), Ok(CliAction::Help)));
        assert!(matches!(parse(&["--version"]), Ok(CliAction::Version)));
        assert!(matches!(parse(&["--list-devices"]), Ok(CliAction::ListDevices)));
        for bad in [
            &["--nope"][..],
            &["--rate"],
            &["--rate", "abc"],
            &["--rate", "100"],
            &["--channels", "0"],
            &["--channels", "65"],
            &["--buffer", "4"],
            &["--backend", "oss"],
            &["--alloc-check", "maybe"],
        ] {
            assert!(parse(bad).is_err(), "{bad:?} debería rechazarse");
        }
        assert!(USAGE.contains("--exclusive"));
    }

    #[test]
    fn the_monitor_device_flag_selects_another_card_for_the_headphones() {
        let c = cfg(&["--channels", "2", "--monitor-device", "Auriculares USB"]);
        assert_eq!(c.backend.monitor_device.as_deref(), Some("Auriculares USB"));
        assert_eq!(c.backend.separate_monitor_device(), Some("Auriculares USB"));
        assert!(parse(&["--monitor-device"]).is_err(), "falta el nombre");
    }
}
