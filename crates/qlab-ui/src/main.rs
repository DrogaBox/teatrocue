#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! `teatrocue-ui`: la interfaz gráfica. Se conecta al daemon por el socket local; si no está
//! activo, muestra la pantalla de reconexión desde la que se puede arrancar el motor embebido.

use std::time::Duration;

use eframe::egui;
use qlab_ui::app::{AppConfig, QlabApp};

// El motor embebido vigila las asignaciones del callback de audio con este asignador.
#[global_allocator]
static ALLOCATOR: qlab_engine::alloc_guard::RtGuardAllocator = qlab_engine::alloc_guard::RtGuardAllocator::system();

const USAGE: &str = "\
teatrocue-ui — interfaz de TeatroCue

USO: teatrocue-ui [opciones]

  (sin opciones)       arranca el motor embebido con el audio predeterminado
  --remote RUTA        se conecta a un daemon externo (socket o pipe) en vez de usar el motor embebido
  --socket RUTA        igual que --remote
  --standalone         motor embebido con el audio predeterminado (es lo normal)
  --standalone-null    arranca el motor embebido sin tarjeta de sonido
  --exit-after-ms N    cierra la ventana pasados N ms (pruebas de humo)
  --help
";

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Option<AppConfig>, String> {
    let mut cfg = AppConfig::new(qlab_protocol::ipc_msg::default_socket_name());
    let mut args = args.into_iter();
    let mut remote = false;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--help" | "-h" => return Ok(None),
            "--socket" | "--remote" => {
                cfg.socket = args.next().ok_or("--remote necesita un valor")?;
                remote = true;
            }
            "--standalone" => cfg.embedded_on_start = Some(true),
            "--standalone-null" => cfg.embedded_on_start = Some(false),
            "--exit-after-ms" => {
                let v = args.next().ok_or("--exit-after-ms necesita un valor")?;
                cfg.exit_after = Some(Duration::from_millis(v.parse().map_err(|_| format!("`{v}` no es un número"))?));
            }
            other => return Err(format!("opción desconocida `{other}`")),
        }
    }
    // Sin argumentos el motor va embebido (con audio); solo `--remote` busca un daemon externo.
    if !remote && cfg.embedded_on_start.is_none() {
        cfg.embedded_on_start = Some(true);
    }
    Ok(Some(cfg))
}

/// En Windows sube la prioridad del proceso para que el audio no se corte si otra aplicación carga la PC.
/// Se usa HIGH y no REALTIME: sin permisos de administrador Windows degrada REALTIME a HIGH, y con ellos
/// REALTIME puede dejar sin CPU al mouse y al teclado.
#[cfg(windows)]
fn raise_priority() {
    extern "system" {
        fn GetCurrentProcess() -> *mut core::ffi::c_void;
        fn SetPriorityClass(h: *mut core::ffi::c_void, class: u32) -> i32;
    }
    const HIGH_PRIORITY_CLASS: u32 = 0x80;
    // SAFETY: llamadas simples a kernel32 con el pseudo-handle del proceso actual.
    unsafe {
        SetPriorityClass(GetCurrentProcess(), HIGH_PRIORITY_CLASS);
    }
}
#[cfg(not(windows))]
fn raise_priority() {}

/// Comprueba la licencia una vez al arrancar (solo si esta compilación la exige).
fn teatrocue_license() -> qlab_ui::license_gate::LicenseInfo {
    qlab_ui::license_gate::LicenseInfo::detect()
}

fn main() -> eframe::Result {
    raise_priority();
    qlab_protocol::crashlog::install("teatrocue-ui", env!("CARGO_PKG_VERSION"));
    let cfg = match parse_args(std::env::args().skip(1)) {
        Ok(Some(mut cfg)) => {
            cfg.settings_path = qlab_ui::settings::default_path();
            if let Some(p) = &cfg.settings_path {
                cfg.audio = qlab_ui::settings::load(p);
            }
            cfg.autosave_secs = Some(60);
            cfg.license = teatrocue_license();
            if cfg!(feature = "enforce-license") {
                cfg.license_setup = Some(qlab_license::activation::Setup::installed());
            }
            cfg
        }
        Ok(None) => {
            print!("{USAGE}");
            return Ok(());
        }
        Err(e) => {
            eprintln!("error: {e}\n\n{USAGE}");
            std::process::exit(2);
        }
    };
    // Una sola copia con motor propio: la segunda se negaría la placa de sonido y la proyección.
    let _instance = if cfg.embedded_on_start.is_some() && cfg.exit_after.is_none() {
        match cfg.settings_path.as_deref().map(|p| qlab_ui::single_instance::acquire(&qlab_ui::single_instance::lock_path(p))) {
            Some(None) => {
                rfd::MessageDialog::new()
                    .set_level(rfd::MessageLevel::Warning)
                    .set_title("TeatroCue ya está abierto")
                    .set_description("Ya hay un TeatroCue abierto en esta PC. Dos copias a la vez se pelean por la placa de sonido y por la pantalla de proyección (el video no se ve). Use la ventana que ya está abierta; si no la encuentra, ciérrela desde la barra de tareas y vuelva a abrir.")
                    .set_buttons(rfd::MessageButtons::Ok)
                    .show();
                return Ok(());
            }
            other => other.flatten(),
        }
    } else {
        None
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_title("TeatroCue").with_inner_size([1280.0, 800.0]).with_min_inner_size([900.0, 560.0]),
        ..Default::default()
    };
    eframe::run_native("TeatroCue", options, Box::new(move |cc| Ok(Box::new(QlabApp::new(&cc.egui_ctx, cfg)))))
}
