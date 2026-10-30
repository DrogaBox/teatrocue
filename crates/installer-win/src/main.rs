//! `Instalar_TeatroCue.exe`: se ejecuta desde el pendrive en la PC del teatro.
//!
//! ```text
//! Instalar_TeatroCue.exe [--days N | --perpetual] [--puesto "Cabina Principal"] [--solo-licencia] [--reset-reloj] [--target CARPETA] [--no-shortcut] [--no-pause]
//! ```
//! Sin `--days`/`--perpetual` se pregunta cuánto debe durar la licencia. Si ya hay TeatroCue instalado, volver a ejecutarlo
//! **renueva** la licencia (`--solo-licencia` no vuelve a copiar los programas).
//! Abierto sin opciones (doble clic desde el pendrive) muestra un menú con los mismos modos que las opciones.
//! La contraseña maestra se pide por consola sin eco (o de `TEATROCUE_MASTER_PASSWORD`, solo para automatizar).

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use installer_win::{install, parse_days, parse_menu_choice, parse_mode_choice, Console, LicenseTerm, Mode, Options, Registry, DEFAULT_TARGET, MAX_DAYS, MODE_MENU, SUCCESS_MESSAGE, TERM_MENU};
use qlab_license::{clock, embedded_public_key, fmt_date, now_unix, MachineId};
use zeroize::Zeroizing;

struct Term;

impl Console for Term {
    fn say(&mut self, text: &str) {
        println!("{text}");
    }

    fn ask_password(&mut self, attempt: usize) -> Result<Zeroizing<String>, String> {
        if let Ok(p) = std::env::var("TEATROCUE_MASTER_PASSWORD") {
            // Automatización: un solo valor; si no sirve, no se reintenta en bucle.
            if attempt > 1 {
                return Err("la contraseña de TEATROCUE_MASTER_PASSWORD es incorrecta".into());
            }
            return Ok(Zeroizing::new(p));
        }
        print!("Contraseña maestra del desarrollador: ");
        let _ = std::io::stdout().flush();
        rpassword::read_password().map(Zeroizing::new).map_err(|e| format!("no se pudo leer la contraseña: {e}"))
    }

    fn choose_term(&mut self, current: Option<&str>) -> Result<LicenseTerm, String> {
        if let Some(c) = current {
            println!("({c})");
        }
        println!("\n¿Por cuánto tiempo debe durar la licencia de esta PC?");
        for (key, text, _) in TERM_MENU {
            println!("  {key}) {text}");
        }
        loop {
            match parse_menu_choice(&read_line("Elija una opción [1-6]: ")?) {
                Ok(Some(t)) => return Ok(t),
                Ok(None) => loop {
                    match parse_days(&read_line(&format!("Cantidad de días (1 a {MAX_DAYS}): "))?) {
                        Ok(t) => return Ok(t),
                        Err(e) => println!("{e}"),
                    }
                },
                Err(e) => println!("{e}"),
            }
        }
    }

    fn ask_puesto(&mut self, default: &str) -> String {
        // Sin consola interactiva (instalación automatizada) se usa la propuesta.
        match read_line(&format!("Puesto / ubicación de esta PC [{default}]: ")) {
            Ok(a) if !a.trim().is_empty() => a.trim().to_string(),
            _ => default.to_string(),
        }
    }

    fn confirm(&mut self, question: &str) -> bool {
        read_line(&format!("{question} [s/N]: ")).map(|a| matches!(a.trim().to_lowercase().as_str(), "s" | "si" | "sí" | "y")).unwrap_or(false)
    }
}

fn read_line(prompt: &str) -> Result<String, String> {
    print!("{prompt}");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    let n = std::io::stdin().lock().read_line(&mut line).map_err(|e| e.to_string())?;
    if n == 0 {
        return Err("no hay entrada por consola (use --days o --perpetual para instalar sin preguntar)".into());
    }
    Ok(line)
}

/// Menú de inicio (instalador abierto sin opciones). `Ok(None)` = salir sin hacer nada.
fn choose_mode() -> Result<Option<(Mode, PathBuf)>, String> {
    println!("¿Qué desea hacer?");
    for (key, text, _) in MODE_MENU {
        println!("  {key}) {text}");
    }
    let mode = loop {
        match parse_mode_choice(&read_line("Elija una opción [0-6]: ")?) {
            Ok(Some(m)) => break m,
            Ok(None) => return Ok(None),
            Err(e) => println!("{e}"),
        }
    };
    let target = if mode.ask_target {
        match read_line(&format!("Carpeta de destino [{DEFAULT_TARGET}]: "))?.trim() {
            "" => PathBuf::from(DEFAULT_TARGET),
            t => PathBuf::from(t.trim_matches('"')),
        }
    } else {
        PathBuf::from(DEFAULT_TARGET)
    };
    println!();
    Ok(Some((mode, target)))
}

/// La consola de Windows usa por defecto una página de códigos que rompe los acentos.
#[cfg(windows)]
fn utf8_console() {
    use windows::Win32::System::Console::SetConsoleOutputCP;
    // SAFETY: llamada simple sin punteros.
    unsafe {
        let _ = SetConsoleOutputCP(65001);
    }
}

#[cfg(not(windows))]
fn utf8_console() {}

fn pause(no_pause: bool) {
    if no_pause {
        return;
    }
    print!("\nPresione Enter para cerrar…");
    let _ = std::io::stdout().flush();
    let _ = std::io::stdin().lock().read_line(&mut String::new());
}

fn launch_gui() -> ExitCode {
    let current_exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("."));
    let exe_dir = current_exe.parent().unwrap_or_else(|| std::path::Path::new("."));

    let candidates = [
        exe_dir.join("Instalador_Unificado.ps1"),
        exe_dir.join("..").join("Instalador_Unificado.ps1"),
        PathBuf::from("Instalador_Unificado.ps1"),
        exe_dir.join("Instalador_Grafico_TeatroCue.ps1"),
        exe_dir.join("Instalador_Grafico.ps1"),
        exe_dir.join("..").join("Instalador_Grafico_TeatroCue.ps1"),
        PathBuf::from("Instalador_Grafico_TeatroCue.ps1"),
    ];

    if let Some(script) = candidates.iter().find(|p| p.exists()) {
        println!("[*] Iniciando Interfaz Gráfica de TeatroCue...");
        let status = std::process::Command::new("powershell")
            .args([
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-STA",
                "-File",
                &script.to_string_lossy(),
            ])
            .status();

        match status {
            Ok(s) if s.success() => ExitCode::SUCCESS,
            _ => ExitCode::from(1),
        }
    } else {
        eprintln!("[!] No se encontró 'Instalador_Grafico_TeatroCue.ps1'.");
        eprintln!("    Iniciando instalador en consola...");
        ExitCode::from(2)
    }
}

fn main() -> ExitCode {
    qlab_protocol::crashlog::install("Instalar_TeatroCue", env!("CARGO_PKG_VERSION"));
    utf8_console();
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|a| a == "--gui" || a == "-g") {
        let code = launch_gui();
        if code != ExitCode::from(2) {
            return code;
        }
    }

    let no_pause = args.iter().any(|a| a == "--no-pause");
    let mut target = args.iter().position(|a| a == "--target").and_then(|i| args.get(i + 1)).map_or_else(|| PathBuf::from(DEFAULT_TARGET), PathBuf::from);

    println!("=== TeatroCue — instalación para el Ballet Oficial de Córdoba ===\n");
    // Sin opciones (doble clic): el menú elige lo mismo que las opciones de consola.
    let menu = if args.iter().all(|a| a == "--no-pause") {
        match choose_mode() {
            Ok(Some((mode, t))) => {
                target = t;
                Some(mode)
            }
            Ok(None) => return ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("\nNO SE PUDO INSTALAR: {e}");
                pause(no_pause);
                return ExitCode::from(1);
            }
        }
    } else {
        None
    };
    let result = (|| {
        let media_root = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf())).ok_or("no se pudo ubicar el instalador")?;
        let machine = MachineId::local()?;
        let term = match (args.iter().position(|a| a == "--days").and_then(|i| args.get(i + 1)), args.iter().any(|a| a == "--perpetual")) {
            (Some(d), false) => Some(parse_days(d)?),
            (None, true) => Some(LicenseTerm::Perpetual),
            (None, false) => None,
            (Some(_), true) => return Err("use --days N o --perpetual, no ambos".into()),
        };
        let stores = clock::default_stores(&target);
        let opts = Options {
            media_root,
            target,
            create_shortcut: menu.map_or_else(|| !args.iter().any(|a| a == "--no-shortcut"), |m| m.shortcut),
            machine,
            expected_public: embedded_public_key(),
            term,
            license_only: menu.map_or_else(|| args.iter().any(|a| a == "--solo-licencia").then_some(true), |m| Some(m.license_only)),
            clock_override: menu.map_or_else(|| args.iter().any(|a| a == "--reset-reloj").then_some(true), |m| m.reset_clock.then_some(true)),
            now: now_unix(),
            anchors: clock::system_anchors(),
            stores,
            puesto: args.iter().position(|a| a == "--puesto").and_then(|i| args.get(i + 1)).cloned(),
            registry: Registry::Auto,
        };
        install(&opts, &mut Term)
    })();
    match result {
        Ok(report) => {
            for w in &report.warnings {
                println!("AVISO: {w}");
            }
            if report.registry.is_some() {
                println!("[OK] Registro guardado en licencias.db del pendrive");
            }
            if let Some(s) = &report.shortcut {
                println!("Acceso directo: {}", s.display());
            }
            match report.license.days_left(now_unix()) {
                None => println!("\nLa licencia NO vence."),
                Some(d) => println!("\nLa licencia vence el {} (dentro de {d} días). Para renovarla, vuelva a ejecutar este instalador con el pendrive.", fmt_date(report.license.expires_unix)),
            }
            if report.renewed {
                println!("Licencia renovada{}.", if report.license_only { " (solo la licencia; los programas no se tocaron)" } else { "" });
            }
            println!("\n{SUCCESS_MESSAGE}");
            pause(no_pause);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("\nNO SE PUDO INSTALAR: {e}");
            pause(no_pause);
            ExitCode::from(1)
        }
    }
}
