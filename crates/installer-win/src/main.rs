//! `Instalar_TeatroCue.exe`: se ejecuta desde el pendrive en la PC del teatro (versión libre, sin licencia).
//!
//! ```text
//! Instalar_TeatroCue.exe [--target CARPETA] [--no-shortcut] [--no-pause]
//! ```

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use installer_win::{install_free, Console, LicenseTerm, DEFAULT_TARGET};
use zeroize::Zeroizing;

struct Term;

impl Console for Term {
    fn say(&mut self, text: &str) {
        println!("{text}");
    }

    fn ask_password(&mut self, _attempt: usize) -> Result<Zeroizing<String>, String> {
        Ok(Zeroizing::new(String::new()))
    }

    fn choose_term(&mut self, _current: Option<&str>) -> Result<LicenseTerm, String> {
        Ok(LicenseTerm::Perpetual)
    }

    fn confirm(&mut self, question: &str) -> bool {
        read_line(&format!("{question} [s/N]: "))
            .map(|a| matches!(a.trim().to_lowercase().as_str(), "s" | "si" | "sí" | "y"))
            .unwrap_or(false)
    }
}

fn read_line(prompt: &str) -> Result<String, String> {
    print!("{prompt}");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    let n = std::io::stdin().lock().read_line(&mut line).map_err(|e| e.to_string())?;
    if n == 0 {
        return Err("no hay entrada por consola".into());
    }
    Ok(line)
}

/// Menú de inicio (instalador abierto con doble clic). `Ok(None)` = salir sin hacer nada.
fn choose_free_mode() -> Result<Option<(PathBuf, bool)>, String> {
    println!("¿Qué desea hacer?");
    println!("  1) Instalar o actualizar TeatroCue en {DEFAULT_TARGET} (con acceso directo)");
    println!("  2) Instalar o actualizar TeatroCue en otra carpeta");
    println!("  3) Instalar o actualizar sin acceso directo en el escritorio");
    println!("  0) Salir sin hacer nada");
    loop {
        match read_line("Elija una opción [0-3]: ")?.trim() {
            "0" => return Ok(None),
            "1" | "" => {
                println!();
                return Ok(Some((PathBuf::from(DEFAULT_TARGET), true)));
            }
            "2" => {
                let target = match read_line(&format!("Carpeta de destino [{DEFAULT_TARGET}]: "))?.trim() {
                    "" => PathBuf::from(DEFAULT_TARGET),
                    t => PathBuf::from(t.trim_matches('"')),
                };
                println!();
                return Ok(Some((target, true)));
            }
            "3" => {
                println!();
                return Ok(Some((PathBuf::from(DEFAULT_TARGET), false)));
            }
            other => println!("opción inválida: `{other}`"),
        }
    }
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
    let mut target = args
        .iter()
        .position(|a| a == "--target")
        .and_then(|i| args.get(i + 1))
        .map_or_else(|| PathBuf::from(DEFAULT_TARGET), PathBuf::from);
    let mut create_shortcut = !args.iter().any(|a| a == "--no-shortcut");

    println!("=== TeatroCue — instalación libre (sin licencia) — Ballet Oficial de Córdoba ===\n");

    if args.iter().all(|a| a == "--no-pause") {
        match choose_free_mode() {
            Ok(Some((t, sc))) => {
                target = t;
                create_shortcut = sc;
            }
            Ok(None) => return ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("\nNO SE PUDO INSTALAR: {e}");
                pause(no_pause);
                return ExitCode::from(1);
            }
        }
    }

    let result = (|| {
        let media_root = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
            .ok_or("no se pudo ubicar el instalador")?;
        install_free(&media_root, &target, create_shortcut, &mut Term)
    })();

    match result {
        Ok(report) => {
            for w in &report.warnings {
                println!("AVISO: {w}");
            }
            if let Some(s) = &report.shortcut {
                println!("Acceso directo: {}", s.display());
            }
            println!("\nTeatroCue instalado exitosamente (edición libre, sin licencia).");
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
