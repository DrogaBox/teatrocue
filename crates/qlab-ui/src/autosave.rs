//! Autoguardado continuo y recuperación tras una caída.
//!
//! Cada minuto, si hay cambios sin guardar, la interfaz le pide al daemon que escriba el show en
//! `~<nombre>.autosave` (junto al show; un show sin nombre usa la carpeta de ajustes). El daemon ya escribe de forma
//! atómica (archivo temporal + renombrado). Al cerrar bien el archivo se borra; si sigue ahí al arrancar y es más
//! nuevo que el último guardado manual, la sesión anterior se cerró de golpe.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub const EXTENSION: &str = "autosave";
/// Nombre base de un show que nunca se guardó.
pub const UNTITLED: &str = "sin_titulo";

/// Un autoguardado más nuevo que el show al que pertenece.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recoverable {
    pub autosave: PathBuf,
    /// Show al que pertenece (`None` = nunca se guardó con nombre).
    pub original: Option<PathBuf>,
    pub saved_at: SystemTime,
}

/// Dónde se autoguarda `show` (o, sin nombre, el show nuevo): `~<nombre>.autosave` en su carpeta.
pub fn path_for(show: Option<&Path>, fallback_dir: &Path) -> PathBuf {
    match show {
        Some(p) => {
            let stem = p.file_stem().map_or_else(|| UNTITLED.into(), |s| s.to_string_lossy().into_owned());
            p.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(fallback_dir).join(format!("~{stem}.{EXTENSION}"))
        }
        None => fallback_dir.join(format!("~{UNTITLED}.{EXTENSION}")),
    }
}

fn mtime(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

/// Busca el autoguardado más reciente que quedó pendiente de recuperar entre los shows conocidos y el show sin nombre.
pub fn find_recoverable(known_shows: &[PathBuf], fallback_dir: &Path) -> Option<Recoverable> {
    let mut found: Vec<Recoverable> = Vec::new();
    let mut check = |original: Option<&Path>| {
        let auto = path_for(original, fallback_dir);
        let Some(saved_at) = mtime(&auto) else { return };
        // Un autoguardado vacío o más viejo que el guardado manual no aporta nada.
        let newer = original.and_then(mtime).is_none_or(|saved| saved_at > saved);
        if newer && std::fs::metadata(&auto).is_ok_and(|m| m.len() > 0) && !found.iter().any(|f| f.autosave == auto) {
            found.push(Recoverable { autosave: auto, original: original.map(Path::to_path_buf), saved_at });
        }
    };
    for s in known_shows {
        check(Some(s));
    }
    check(None);
    found.into_iter().max_by_key(|f| f.saved_at)
}

/// «3 min», «2 h»… desde `t` hasta ahora.
pub fn fmt_ago(t: SystemTime) -> String {
    let s = SystemTime::now().duration_since(t).map_or(0, |d| d.as_secs());
    match s {
        0..=59 => "menos de 1 min".into(),
        60..=3599 => format!("{} min", s / 60),
        3600..=86_399 => format!("{} h", s / 3600),
        _ => format!("{} días", s / 86_400),
    }
}

/// Borra el autoguardado de `show`; no es un error que no exista.
pub fn remove(show: Option<&Path>, fallback_dir: &Path) {
    let _ = std::fs::remove_file(path_for(show, fallback_dir));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("teatrocue-autosave-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn set_age(p: &Path, secs_ago: u64) {
        let t = SystemTime::now() - Duration::from_secs(secs_ago);
        std::fs::File::options().write(true).open(p).unwrap().set_modified(t).unwrap();
    }

    #[test]
    fn the_autosave_lives_next_to_the_show_and_unnamed_ones_in_the_fallback_dir() {
        let fb = Path::new("/cfg");
        assert_eq!(path_for(Some(Path::new("/shows/Giselle.teatroshow")), fb), Path::new("/shows/~Giselle.autosave"));
        assert_eq!(path_for(None, fb), Path::new("/cfg/~sin_titulo.autosave"));
        assert_eq!(path_for(Some(Path::new("suelto.json")), fb), Path::new("/cfg/~suelto.autosave"));
    }

    #[test]
    fn only_an_autosave_newer_than_the_manual_save_is_offered() {
        let d = tmp("newer");
        let show = d.join("Obra.teatroshow");
        std::fs::write(&show, "{}").unwrap();
        let auto = path_for(Some(&show), &d);
        assert_eq!(find_recoverable(&[show.clone()], &d), None, "sin autoguardado no hay nada que ofrecer");
        std::fs::write(&auto, "{\"x\":1}").unwrap();
        set_age(&show, 100);
        set_age(&auto, 10);
        let r = find_recoverable(&[show.clone()], &d).expect("autoguardado más nuevo");
        assert_eq!((r.autosave.clone(), r.original.clone()), (auto.clone(), Some(show.clone())));
        // Se guardó a mano después: el autoguardado quedó viejo.
        set_age(&show, 1);
        set_age(&auto, 50);
        assert_eq!(find_recoverable(&[show.clone()], &d), None);
        // Un archivo vacío (escritura interrumpida) tampoco se ofrece.
        std::fs::write(&auto, "").unwrap();
        set_age(&show, 100);
        assert_eq!(find_recoverable(&[show], &d), None);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_unnamed_session_is_recovered_and_the_newest_wins() {
        let d = tmp("unnamed");
        let named = d.join("A.teatroshow");
        std::fs::write(&named, "{}").unwrap();
        set_age(&named, 500);
        std::fs::write(path_for(Some(&named), &d), "{\"a\":1}").unwrap();
        set_age(&path_for(Some(&named), &d), 200);
        std::fs::write(path_for(None, &d), "{\"b\":1}").unwrap();
        set_age(&path_for(None, &d), 20);
        let r = find_recoverable(&[named], &d).unwrap();
        assert_eq!(r.original, None, "gana el más reciente: el show sin nombre");
        remove(None, &d);
        assert!(find_recoverable(&[], &d).is_none());
        remove(None, &d); // no es un error borrar lo que no está
        let _ = std::fs::remove_dir_all(&d);
    }
}
