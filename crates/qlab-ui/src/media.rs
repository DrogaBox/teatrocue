//! Tipos de archivo multimedia que acepta la aplicación (arrastrar y soltar, diálogos, importación).

use std::path::Path;

pub const AUDIO_EXTENSIONS: [&str; 5] = ["wav", "flac", "mp3", "ogg", "oga"];
pub const VIDEO_EXTENSIONS: [&str; 3] = ["mp4", "mov", "mkv"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaKind {
    Audio,
    Video,
}

impl MediaKind {
    /// Tipo de cue que se crea para este archivo (`audio` o `video`).
    pub fn cue_kind(self) -> &'static str {
        match self {
            Self::Audio => "audio",
            Self::Video => "video",
        }
    }
}

pub fn media_kind(path: &Path) -> Option<MediaKind> {
    let ext = path.extension()?.to_str()?;
    let is = |list: &[&str]| list.iter().any(|e| e.eq_ignore_ascii_case(ext));
    if is(&AUDIO_EXTENSIONS) {
        Some(MediaKind::Audio)
    } else if is(&VIDEO_EXTENSIONS) {
        Some(MediaKind::Video)
    } else {
        None
    }
}

/// Nombre de cue a partir del archivo: su nombre sin extensión.
/// Nombre para una cue nueva a partir de su archivo: el **título** de las etiquetas (con el artista, si lo hay) o, si el
/// archivo no trae etiquetas, su nombre limpio ([`name_from_file_name`]).
pub fn cue_name_of(path: &Path) -> String {
    let tags = qlab_engine::disk_streamer::media_tags(path);
    name_from_tags(&tags).unwrap_or_else(|| name_from_file_name(path))
}

/// «Título — Artista», o solo el título. `None` sin título.
pub fn name_from_tags(tags: &qlab_engine::disk_streamer::MediaTags) -> Option<String> {
    let title = tags.title.as_deref().map(str::trim).filter(|t| !t.is_empty())?;
    Some(match tags.artist.as_deref().map(str::trim).filter(|a| !a.is_empty()) {
        Some(a) => format!("{title} — {a}"),
        None => title.to_string(),
    })
}

/// Nombre legible desde el nombre del archivo: sin extensión, con `_` como espacios, sin espacios repetidos y sin el número de
/// pista del principio («03 - Giselle_acto_1.wav» → «Giselle acto 1»).
pub fn name_from_file_name(path: &Path) -> String {
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let spaced: String = stem.replace('_', " ").split_whitespace().collect::<Vec<_>>().join(" ");
    // Número de pista inicial («01 », «01. », «01 - »): se quita solo si queda algo después.
    let digits = spaced.chars().take_while(char::is_ascii_digit).count();
    if (1..=3).contains(&digits) {
        let rest = spaced[digits..].trim_start_matches([' ', '.', '-', ')']).trim();
        let had_separator = spaced[digits..].starts_with([' ', '.', '-', ')']);
        if had_separator && !rest.is_empty() {
            return rest.to_string();
        }
    }
    spaced
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_kind_recognises_audio_and_video_by_extension_case_insensitively() {
        for (name, kind) in [("a.wav", Some(MediaKind::Audio)), ("a.FLAC", Some(MediaKind::Audio)), ("a.Mp3", Some(MediaKind::Audio)), ("a.ogg", Some(MediaKind::Audio)),
            ("v.mp4", Some(MediaKind::Video)), ("v.MOV", Some(MediaKind::Video)), ("v.mkv", Some(MediaKind::Video)),
            ("notas.txt", None), ("sin_extension", None), ("a.json", None)] {
            assert_eq!(media_kind(Path::new(name)), kind, "{name}");
        }
        assert_eq!(MediaKind::Video.cue_kind(), "video");
    }

    #[test]
    fn cue_names_drop_the_extension_only() {
        assert_eq!(cue_name_of(Path::new("/x/Entrada coro.v2.wav")), "Entrada coro.v2");
        assert_eq!(cue_name_of(Path::new("/x/a.mp4")), "a");
    }

    #[test]
    fn file_names_become_readable_cue_names() {
        assert_eq!(name_from_file_name(Path::new("/m/03 - Giselle_acto_1.wav")), "Giselle acto 1");
        assert_eq!(name_from_file_name(Path::new("01. Lago de los cisnes.flac")), "Lago de los cisnes");
        assert_eq!(name_from_file_name(Path::new("Don_Quijote__pas_de_deux.mp3")), "Don Quijote pas de deux");
        assert_eq!(name_from_file_name(Path::new("file 720x576 02h24m_008542.mp4")), "file 720x576 02h24m 008542", "«02h…» no es un número de pista");
        assert_eq!(name_from_file_name(Path::new("1812.wav")), "1812", "un número solo se conserva");
        assert_eq!(name_from_file_name(Path::new("2001 una odisea.mp3")), "2001 una odisea", "cuatro cifras no son pista");
    }

    #[test]
    fn tags_win_over_the_file_name_and_include_the_artist() {
        use qlab_engine::disk_streamer::MediaTags;
        let t = |title: Option<&str>, artist: Option<&str>| MediaTags { title: title.map(String::from), artist: artist.map(String::from) };
        assert_eq!(name_from_tags(&t(Some("Giselle, Acto I"), Some("Adolphe Adam"))).as_deref(), Some("Giselle, Acto I — Adolphe Adam"));
        assert_eq!(name_from_tags(&t(Some("Kitri"), None)).as_deref(), Some("Kitri"));
        assert_eq!(name_from_tags(&t(Some("  "), Some("X"))), None, "sin título se usa el nombre del archivo");
        assert_eq!(name_from_tags(&t(None, Some("X"))), None);
    }

    #[test]
    fn real_tags_are_read_from_mp3_m4a_and_flac_files() {
        let dir = std::env::temp_dir().join(format!("qlab-tags-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut any = false;
        for ext in ["mp3", "m4a", "flac"] {
            let path = dir.join(format!("01_pista_sin_nombre.{ext}"));
            let ok = std::process::Command::new("ffmpeg")
                .args(["-v", "error", "-y", "-f", "lavfi", "-i", "sine=duration=1", "-metadata", "title=Entrada de Basilio", "-metadata", "artist=Ludwig Minkus"])
                .arg(&path)
                .status()
                .is_ok_and(|s| s.success());
            if !ok {
                continue;
            }
            any = true;
            assert_eq!(cue_name_of(&path), "Entrada de Basilio — Ludwig Minkus", "etiquetas de {ext}");
        }
        // Sin etiquetas: el nombre del archivo, limpio.
        let plain = dir.join("02 - Variacion_de_Kitri.wav");
        let src = crate::waveform::write_test_wav("plain", 1, 100, 48_000, |_, _| 0.0);
        std::fs::rename(&src, &plain).unwrap();
        assert_eq!(cue_name_of(&plain), "Variacion de Kitri");
        if !any {
            eprintln!("ffmpeg no está instalado: solo se probó el nombre de archivo");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
