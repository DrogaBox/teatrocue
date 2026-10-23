//! `teatrocue-daemon`: arranque, señales de apagado y línea de órdenes.

use std::process::ExitCode;
use std::sync::atomic::Ordering;

use qlab_daemon::backend_audio::list_output_devices;
use qlab_daemon::config::{parse_alloc_check, parse_args, CliAction, USAGE};
use qlab_daemon::{AllocCheck, Daemon};

// Cualquier asignación dentro del callback de audio se detecta (ver `--alloc-check`).
#[global_allocator]
static ALLOCATOR: qlab_engine::alloc_guard::RtGuardAllocator = qlab_engine::alloc_guard::RtGuardAllocator::system();

/// Con la feature `enforce-license`, sin un `license.key` válido para esta PC el proceso termina con código 1
/// **antes** de abrir ningún dispositivo de audio. Sin la feature (desarrollo) no se exige.
fn license_gate() -> Option<ExitCode> {
    #[cfg(feature = "enforce-license")]
    {
        let status = qlab_license::check_installed();
        if let Some(msg) = qlab_license::lock_message(&status) {
            eprintln!("[TeatroCue] {msg}");
            return Some(ExitCode::from(1));
        }
        if let Ok(l) = &status.result {
            let term = match l.days_left(status.now) {
                None => "sin vencimiento".to_string(),
                Some(_) => format!("vence el {} (quedan {} días)", qlab_license::fmt_date(status.effective_expires_unix), (status.effective_expires_unix as i64 - status.now as i64).div_euclid(86_400)),
            };
            eprintln!("[TeatroCue] licencia válida: {} · {} · {term}", l.licensee, l.machine);
        }
    }
    None
}

fn main() -> ExitCode {
    qlab_protocol::crashlog::install("teatrocue-daemon", env!("CARGO_PKG_VERSION"));
    let mut cfg = match parse_args(std::env::args().skip(1)) {
        Ok(CliAction::Run(cfg)) => {
            if let Some(code) = license_gate() {
                return code;
            }
            cfg
        }
        Ok(CliAction::Help) => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Ok(CliAction::Version) => {
            println!("teatrocue-daemon {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Ok(CliAction::LicenseRequest) => {
            return match qlab_license::activation::Setup::installed().challenge() {
                Ok(c) => {
                    println!("{c}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    ExitCode::FAILURE
                }
            };
        }
        Ok(CliAction::LicenseApply(key)) => {
            return match qlab_license::activation::Setup::installed().apply(&key) {
                Ok(a) => {
                    println!("Licencia extendida hasta: {}", qlab_license::fmt_date(a.valid_until));
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    ExitCode::FAILURE
                }
            };
        }
        Ok(CliAction::ListDevices) => {
            if let Some(code) = license_gate() {
                return code;
            }
            for (host, device) in list_output_devices() {
                println!("{host}\t{device}");
            }
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("error: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    #[cfg(feature = "enforce-license")]
    {
        cfg.license = Some(qlab_license::activation::Setup::installed());
    }
    // La variable de entorno completa a la línea de órdenes (útil en servicios y CI).
    if let Ok(v) = std::env::var("TEATROCUE_RT_ALLOC_CHECK") {
        match parse_alloc_check(&v) {
            Ok(mode) => cfg.alloc_check = mode,
            Err(e) => eprintln!("aviso: TEATROCUE_RT_ALLOC_CHECK ignorada: {e}"),
        }
    }
    if cfg.alloc_check == AllocCheck::Abort {
        eprintln!("[daemon] --alloc-check abort: una asignación en el callback de audio abortará el proceso");
    }

    let mut daemon = match Daemon::start(*cfg) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("error al arrancar: {e}");
            return ExitCode::FAILURE;
        }
    };

    // SIGINT/SIGTERM/SIGHUP en Linux; Ctrl+C, Ctrl+Break y cierre de consola en Windows.
    let flag = daemon.shutdown_flag();
    if let Err(e) = ctrlc::set_handler(move || flag.store(true, Ordering::Release)) {
        eprintln!("aviso: no se pudo instalar el manejador de señales: {e}");
    }

    daemon.run();
    daemon.stop();
    ExitCode::SUCCESS
}
