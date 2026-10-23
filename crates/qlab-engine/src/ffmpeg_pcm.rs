//! Respaldo de decodificación con `ffmpeg` para archivos que el decodificador propio no lee bien.
//!
//! El motor decodifica con Symphonia, que es estricto: un archivo largo de captura (por ejemplo una función entera de 2 h 24 min
//! en MP4 con AAC con cuadros dañados) puede llenarse de errores y quedar mudo a partir de cierto punto. `ffmpeg`, como los
//! programas de audio profesionales, descarta lo dañado y sigue. Cuando Symphonia empieza a fallar sostenidamente, el streamer
//! pasa a este lector: un subproceso `ffmpeg` que entrega PCM `f32` entrelazado por una tubería, a la frecuencia y los canales
//! del archivo, con relleno de silencio en los huecos para que el tiempo no se corra (`aresample=async`).
//!
//! No toca el hilo de audio: se usa solo desde los hilos de disco y de la forma de onda.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};

/// Lector secuencial de PCM de `ffmpeg`, posicionado en un frame del archivo.
pub struct FfmpegPcm {
    child: Child,
    out: ChildStdout,
    path: PathBuf,
    channels: usize,
    sample_rate: u32,
    /// Próximo frame que se entregará.
    pos: u64,
}

/// ¿Hay un `ffmpeg` ejecutable? (en Windows se busca primero junto al programa, como hace la proyección de vídeo)
pub fn available() -> bool {
    static OK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *OK.get_or_init(|| command().arg("-version").stdout(Stdio::null()).stderr(Stdio::null()).status().map(|s| s.success()).unwrap_or(false))
}

fn command() -> Command {
    let mut c = Command::new("ffmpeg");
    c.stdin(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    c
}

impl FfmpegPcm {
    /// Lanza `ffmpeg` desde el frame `start` de la primera pista de audio de `path`.
    pub fn spawn(path: &Path, start: u64, channels: usize, sample_rate: u32) -> std::io::Result<Self> {
        let secs = start as f64 / f64::from(sample_rate.max(1));
        let mut child = command()
            .args(["-v", "error", "-nostdin", "-ss"])
            .arg(format!("{secs:.6}"))
            .arg("-i")
            .arg(path)
            .args(["-vn", "-map", "0:a:0", "-af", "aresample=async=1000:min_hard_comp=0.1", "-f", "f32le", "-acodec", "pcm_f32le", "-ac"])
            .arg(channels.to_string())
            .arg("-ar")
            .arg(sample_rate.to_string())
            .arg("pipe:1")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let out = child.stdout.take().ok_or_else(|| std::io::Error::other("ffmpeg no abrió su salida"))?;
        Ok(Self { child, out, path: path.to_path_buf(), channels, sample_rate, pos: start })
    }

    pub fn pos(&self) -> u64 {
        self.pos
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Vuelve a empezar en `start` (un salto: se relanza el subproceso).
    pub fn seek(&mut self, start: u64) -> std::io::Result<()> {
        *self = Self::spawn(&self.path.clone(), start, self.channels, self.sample_rate)?;
        Ok(())
    }

    /// Lee hasta `frames` frames entrelazados y los **añade** a `out`. Devuelve los leídos (menos solo al final del archivo).
    pub fn read(&mut self, frames: usize, out: &mut Vec<f32>) -> std::io::Result<usize> {
        let want = frames * self.channels * 4;
        let mut bytes = vec![0u8; want];
        let mut got = 0;
        while got < want {
            match self.out.read(&mut bytes[got..]) {
                Ok(0) => break,
                Ok(n) => got += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        let frames_got = got / (self.channels * 4);
        out.extend(bytes[..frames_got * self.channels * 4].chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])));
        self.pos += frames_got as u64;
        Ok(frames_got)
    }
}

impl Drop for FfmpegPcm {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_reads_pcm_from_a_given_frame_and_seeks() {
        if !available() {
            return eprintln!("ffmpeg no está instalado: se omite");
        }
        // Un tono de 440 Hz de 4 s a 48 kHz estéreo, generado por el propio ffmpeg.
        let path = std::env::temp_dir().join(format!("qlab-ffpcm-{}.wav", std::process::id()));
        let st = command().args(["-v", "error", "-y", "-f", "lavfi", "-i", "sine=frequency=440:duration=4:sample_rate=48000", "-ac", "2"]).arg(&path).status().unwrap();
        assert!(st.success());
        let mut r = FfmpegPcm::spawn(&path, 48_000, 2, 48_000).unwrap();
        let mut v = Vec::new();
        assert_eq!(r.read(4_800, &mut v).unwrap(), 4_800);
        assert_eq!((v.len(), r.pos()), (9_600, 52_800));
        assert!(v.iter().any(|&x| x.abs() > 0.05), "hay señal");
        // Un salto atrás: la lectura continúa desde el frame pedido.
        r.seek(1_000).unwrap();
        let mut w = Vec::new();
        assert_eq!(r.read(100, &mut w).unwrap(), 100);
        assert_eq!(r.pos(), 1_100);
        // Hasta el final: devuelve lo que queda y luego 0.
        r.seek(190_000).unwrap();
        let mut tail = Vec::new();
        let n = r.read(10_000, &mut tail).unwrap();
        assert!((1_900..=2_100).contains(&n), "quedaban ≈2000 frames y se leyeron {n}");
        assert_eq!(r.read(10, &mut Vec::new()).unwrap(), 0);
        std::fs::remove_file(path).ok();
    }
}
