//! Una sola copia de TeatroCue por PC: dos copias con su propio motor se pelean por la placa de sonido, la memoria
//! compartida y la pantalla de proyección (el video deja de verse). El bloqueo es un archivo (`instancia.lock`, junto a los
//! ajustes) que el sistema libera solo al cerrar el programa, también si se cierra de golpe.

use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};

/// Mientras exista, esta es la única copia abierta.
#[derive(Debug)]
pub struct Guard {
    _file: File,
}

pub fn lock_path(settings_path: &Path) -> PathBuf {
    settings_path.with_file_name("instancia.lock")
}

/// `Ok(Some)`: somos la única copia. `Ok(None)`: ya hay otra abierta. Si el archivo no se puede abrir (disco de solo
/// lectura), no se impide el arranque: es mejor una copia sin bloqueo que ninguna.
pub fn acquire(path: &Path) -> Option<Guard> {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let Ok(file) = OpenOptions::new().create(true).truncate(false).write(true).open(path) else {
        return Some(Guard { _file: File::open(std::env::current_exe().unwrap_or_default()).ok()? });
    };
    match file.try_lock() {
        Ok(()) => Some(Guard { _file: file }),
        Err(TryLockError::WouldBlock) => None,
        Err(TryLockError::Error(_)) => Some(Guard { _file: file }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_copy_is_refused_while_the_first_is_open_and_allowed_after_it_closes() {
        let dir = std::env::temp_dir().join(format!("qlab-instance-{}", std::process::id()));
        let p = lock_path(&dir.join("audio.json"));
        let first = acquire(&p).expect("la primera copia arranca");
        assert!(acquire(&p).is_none(), "la segunda se rechaza");
        drop(first);
        assert!(acquire(&p).is_some(), "al cerrar la primera (o si se cae) se puede abrir otra");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
