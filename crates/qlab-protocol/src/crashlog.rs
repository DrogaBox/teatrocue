//! Registro de fallos: si un programa entra en pánico, queda escrito en `crash.log` (con fecha, programa, versión y
//! lugar del error) en vez de desaparecer sin rastro. Vive en la carpeta de datos del usuario, nunca falla al escribir.

use std::io::Write;
use std::path::PathBuf;

/// `%PROGRAMDATA%\TeatroCue\crash.log` en Windows; `~/.local/state/teatrocue/crash.log` en Linux.
pub fn log_path() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("PROGRAMDATA").map(PathBuf::from)?.join("TeatroCue")
    } else {
        let state = std::env::var_os("XDG_STATE_HOME").map(PathBuf::from).or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
        state.join("teatrocue")
    };
    Some(base.join("crash.log"))
}

pub fn format_report(app: &str, version: &str, unix: u64, info: &std::panic::PanicHookInfo<'_>) -> String {
    let place = info.location().map_or("?".to_string(), |l| format!("{}:{}", l.file(), l.line()));
    let msg = info.payload().downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| info.payload().downcast_ref::<String>().cloned()).unwrap_or_else(|| "(sin mensaje)".into());
    let thread = std::thread::current().name().unwrap_or("?").to_string();
    format!("[unix {unix}] {app} {version} · hilo `{thread}` · {place}\n  {msg}\n")
}

/// Instala el gancho de pánicos para `app`. Conserva el gancho anterior (el mensaje en consola sigue saliendo).
pub fn install(app: &'static str, version: &'static str) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let unix = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        write_report(&format_report(app, version, unix, info));
        previous(info);
    }));
}

fn write_report(text: &str) {
    let Some(path) = log_path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // Si el registro creció demasiado, se empieza otro: nunca debe llenar el disco.
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > 512 * 1024) {
        let _ = std::fs::rename(&path, path.with_extension("old"));
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = f.write_all(text.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_panic_leaves_a_readable_report_in_the_crash_log() {
        let dir = std::env::temp_dir().join(format!("teatrocue-crash-{}", std::process::id()));
        std::env::set_var(if cfg!(windows) { "PROGRAMDATA" } else { "XDG_STATE_HOME" }, &dir);
        install("prueba", "9.9.9");
        let _ = std::thread::Builder::new().name("hilo-de-prueba".into()).spawn(|| panic!("falla de prueba")).unwrap().join();
        let text = std::fs::read_to_string(log_path().unwrap()).unwrap();
        assert!(text.contains("prueba 9.9.9") && text.contains("hilo-de-prueba") && text.contains("falla de prueba") && text.contains("crashlog.rs"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
