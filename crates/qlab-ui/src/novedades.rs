//! «Novedades de esta versión»: el `CAMBIOS.txt` de la raíz del repositorio va dentro del programa y se muestra solo la
//! primera vez que se abre una versión nueva (o después de cada actualización). Qué versión ya se vio queda en
//! `novedades-vistas.txt`, junto a los ajustes de audio; es la huella del propio texto, así que vuelve a salir exactamente
//! cuando el texto de los cambios es otro.

use std::path::{Path, PathBuf};

const RAW: &str = include_str!("../../../CAMBIOS.txt");

/// El texto de los cambios, sin la marca BOM ni los retornos de carro del archivo para el Bloc de notas.
pub fn text() -> String {
    RAW.trim_start_matches('\u{feff}').replace("\r\n", "\n").trim_end().to_string()
}

/// Huella estable del texto (FNV-1a de 64 bits): no cambia entre compilaciones ni versiones de Rust.
pub fn fingerprint() -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in text().bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// Dónde se anota la última versión de las novedades que se vio (junto a `audio.json`).
pub fn seen_path(settings_path: &Path) -> PathBuf {
    settings_path.with_file_name("novedades-vistas.txt")
}

/// ¿Hay que mostrar las novedades al abrir? Sí si nunca se vieron o si las que se vieron eran otras.
pub fn should_show(seen: &Path) -> bool {
    std::fs::read_to_string(seen).map_or(true, |s| s.trim() != fingerprint())
}

/// Anota que estas novedades ya se vieron. Un fallo (disco de solo lectura) solo hace que vuelvan a salir la próxima vez.
pub fn mark_seen(seen: &Path) -> Result<(), String> {
    if let Some(dir) = seen.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    std::fs::write(seen, format!("{}\n", fingerprint())).map_err(|e| e.to_string())
}

/// Cómo se dibuja cada renglón del texto.
#[derive(Debug, PartialEq, Eq)]
pub enum Line<'a> {
    /// Título de sección (el renglón siguiente es «-----» o «=====»).
    Heading(&'a str),
    /// Comienzo de un punto numerado («3. Obras completas…»).
    Item(&'a str),
    Text(&'a str),
    Blank,
}

/// Divide el texto en renglones con su estilo; los subrayados de guiones o iguales no se dibujan.
pub fn lines(text: &str) -> Vec<Line<'_>> {
    let raw: Vec<&str> = text.lines().collect();
    let underline = |s: &str| s.len() >= 3 && s.trim().chars().all(|c| c == '-' || c == '=');
    let mut out = Vec::new();
    for (i, l) in raw.iter().enumerate() {
        if underline(l) {
            continue;
        }
        let t = l.trim_end();
        if t.trim().is_empty() {
            out.push(Line::Blank);
        } else if raw.get(i + 1).is_some_and(|n| underline(n)) {
            out.push(Line::Heading(t.trim()));
        } else if t.split_once(". ").is_some_and(|(n, _)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit())) {
            out.push(Line::Item(t));
        } else {
            out.push(Line::Text(t));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_embedded_changelog_is_clean_and_has_its_sections() {
        let t = text();
        assert!(!t.contains('\r') && !t.starts_with('\u{feff}'));
        let ls = lines(&t);
        assert_eq!(ls[0], Line::Heading("TeatroCue - CAMBIOS"));
        for h in ["NUEVO", "CORREGIDO"] {
            assert!(ls.contains(&Line::Heading(h)), "falta la sección {h}");
        }
        assert!(ls.iter().any(|l| matches!(l, Line::Item(s) if s.starts_with("1. "))));
        assert!(!ls.iter().any(|l| matches!(l, Line::Text(s) if s.starts_with("---"))), "los subrayados no se dibujan");
        // La tipografía de la interfaz no tiene estos signos (se verían como cuadraditos).
        for c in ['→', '✓', '▶', '●'] {
            assert!(!t.contains(c), "el texto usa «{c}»");
        }
    }

    #[test]
    fn the_news_shows_once_per_version_and_again_after_an_update() {
        let dir = std::env::temp_dir().join(format!("qlab-novedades-{}", std::process::id()));
        let seen = seen_path(&dir.join("audio.json"));
        let _ = std::fs::remove_file(&seen);
        assert!(should_show(&seen), "primera vez");
        mark_seen(&seen).unwrap();
        assert!(!should_show(&seen), "ya vistas");
        std::fs::write(&seen, "0123456789abcdef\n").unwrap(); // lo que dejó la versión anterior
        assert!(should_show(&seen), "otra versión: vuelven a salir");
        assert_eq!(fingerprint(), fingerprint());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
