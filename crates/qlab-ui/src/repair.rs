//! «Crear copia reparada»: cuando el audio de un archivo está dañado desde cierto punto (la forma de onda lo detecta), se
//! escribe al lado una copia que llega solo hasta ahí, sin recodificar (`ffmpeg -c copy`): mismo audio y misma imagen, pero
//! un archivo coherente que cualquier programa reproduce sin errores. El original no se toca.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Estado de la reparación de un archivo.
#[derive(Clone, Debug, PartialEq)]
pub enum RepairState {
    Running,
    /// La copia quedó escrita en esta ruta.
    Done(PathBuf),
    Failed(String),
}

/// Margen antes del punto dañado: el daño se detecta en el audio, pero en un MP4 la imagen va intercalada y su parte rota puede
/// empezar un poco antes (en el archivo real de 2 h 24 min, la imagen se rompía 0,7 s antes que el sonido).
pub const SAFETY_MARGIN_SECS: f64 = 2.0;

/// Hasta dónde llega la copia reparada si el audio está dañado desde `damaged_from`.
pub fn safe_cut(damaged_from: f64) -> f64 {
    (damaged_from - SAFETY_MARGIN_SECS).max(0.0)
}

/// `1h32m48s`, `12m05s`, `45s`: para el nombre de la copia.
pub fn short_time(secs: f64) -> String {
    let s = secs.max(0.0).floor() as u64;
    let (h, m, s) = (s / 3600, (s / 60) % 60, s % 60);
    match (h, m) {
        (0, 0) => format!("{s}s"),
        (0, _) => format!("{m}m{s:02}s"),
        _ => format!("{h}h{m:02}m{s:02}s"),
    }
}

/// Ruta de la copia, junto al original: `nombre (reparado hasta 1h32m48s).mp4`. Si ya existe, se numera.
pub fn repaired_path(original: &Path, until_secs: f64) -> PathBuf {
    let dir = original.parent().unwrap_or(Path::new("."));
    let stem = original.file_stem().map_or_else(|| "audio".to_string(), |s| s.to_string_lossy().into_owned());
    let ext = original.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    let base = format!("{stem} (reparado hasta {})", short_time(until_secs));
    let mut p = dir.join(format!("{base}{ext}"));
    let mut n = 2;
    while p.exists() {
        p = dir.join(format!("{base} {n}{ext}"));
        n += 1;
    }
    p
}

fn ffmpeg() -> Command {
    let mut c = Command::new("ffmpeg");
    c.stdin(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    c
}

/// Escribe en `out` los primeros `until_secs` segundos de `original` copiando los datos tal cual (sin recodificar).
/// Bloquea hasta que termina: llamar desde un hilo. Si falla no deja un archivo a medias.
pub fn write_repaired_copy(original: &Path, out: &Path, until_secs: f64) -> Result<(), String> {
    if !qlab_engine::ffmpeg_pcm::available() {
        return Err("no se encontró ffmpeg (viene en el paquete de instalación)".into());
    }
    if !(until_secs.is_finite() && until_secs > 0.5) {
        return Err("no queda una parte sana que copiar".into());
    }
    // Se escribe con otro nombre y se renombra al final: si algo falla no queda una copia incompleta con el nombre bueno.
    let tmp = out.with_file_name(format!(".{}.parcial", out.file_name().map_or_else(|| "copia".into(), |n| n.to_string_lossy().into_owned())));
    let ext = out.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    let mut cmd = ffmpeg();
    cmd.args(["-v", "error", "-nostdin", "-y", "-i"]).arg(original).arg("-t").arg(format!("{until_secs:.3}")).args(["-map", "0:v?", "-map", "0:a?", "-c", "copy"]);
    if matches!(ext.as_str(), "mp4" | "m4a" | "mov" | "m4v") {
        cmd.args(["-movflags", "+faststart"]);
    }
    // El temporal no lleva la extensión real: se indica el formato de salida explícitamente.
    let format = match ext.as_str() {
        "mp4" | "m4v" => "mp4",
        "m4a" => "ipod",
        "mov" => "mov",
        "mkv" => "matroska",
        "ogg" | "oga" => "ogg",
        "flac" => "flac",
        "mp3" => "mp3",
        "wav" => "wav",
        _ => "mp4",
    };
    cmd.args(["-f", format]).arg(&tmp).stdout(Stdio::null()).stderr(Stdio::piped());
    let result = cmd.output().map_err(|e| format!("no se pudo ejecutar ffmpeg: {e}"))?;
    if !result.status.success() || !tmp.is_file() {
        let _ = std::fs::remove_file(&tmp);
        let msg = String::from_utf8_lossy(&result.stderr);
        return Err(format!("ffmpeg no pudo crear la copia: {}", msg.lines().last().unwrap_or("error desconocido")));
    }
    std::fs::rename(&tmp, out).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("{}: {e}", out.display())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_copy_is_named_after_the_original_with_the_cut_point_and_never_overwrites() {
        assert_eq!(short_time(5568.4), "1h32m48s");
        assert_eq!(short_time(725.0), "12m05s");
        assert_eq!(short_time(45.9), "45s");
        let dir = std::env::temp_dir().join(format!("qlab-repair-name-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let orig = dir.join("file 720x576 02h24m_008542.mp4");
        let p1 = repaired_path(&orig, 5568.4);
        assert_eq!(p1, dir.join("file 720x576 02h24m_008542 (reparado hasta 1h32m48s).mp4"));
        std::fs::write(&p1, b"x").unwrap();
        assert_eq!(repaired_path(&orig, 5568.4), dir.join("file 720x576 02h24m_008542 (reparado hasta 1h32m48s) 2.mp4"), "no pisa una copia anterior");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_damaged_file_gets_a_clean_copy_up_to_the_damage_and_the_original_is_untouched() {
        let Some(orig) = crate::waveform::tests_support::damaged_m4a("repair") else { return eprintln!("ffmpeg no está instalado: se omite") };
        let before = std::fs::read(&orig).unwrap();
        let pool = qlab_engine::disk_streamer::DiskStreamerPool::new(1);
        let damaged = crate::waveform::compute_peaks(&pool, &orig, 200).unwrap();
        let from = damaged.damaged_from.expect("el original está dañado");
        let cut = safe_cut(from);
        let out = repaired_path(&orig, cut);
        write_repaired_copy(&orig, &out, cut).unwrap();
        assert!(out.is_file());
        assert_eq!(std::fs::read(&orig).unwrap(), before, "el original no se modifica");
        // La copia se lee de punta a punta sin daño y dura lo que había sano.
        let fixed = crate::waveform::compute_peaks(&pool, &out, 200).unwrap();
        assert_eq!(fixed.damaged_from, None, "la copia está sana");
        assert!((fixed.duration_secs() - cut).abs() < 0.2, "dura hasta poco antes del daño: {} vs {cut}", fixed.duration_secs());
        assert!(fixed.max.iter().all(|&m| m > 0.05), "y tiene sonido en toda su duración");
        // No quedan temporales.
        let leftovers = std::fs::read_dir(orig.parent().unwrap()).unwrap().filter_map(|e| e.ok()).filter(|e| e.file_name().to_string_lossy().ends_with(".parcial")).count();
        assert_eq!(leftovers, 0);
        assert!(write_repaired_copy(&orig, &out.with_extension("x.mp4"), 0.0).is_err(), "sin parte sana no hay copia");
        std::fs::remove_file(out).ok();
        std::fs::remove_file(orig).ok();
    }
}
