//! Utilidades de las pruebas: archivos WAV sintéticos y rutas temporales únicas.

use std::path::PathBuf;

/// Ruta única en el directorio temporal.
pub fn temp_path(tag: &str, ext: &str) -> PathBuf {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!("qlab-daemon-test-{}-{n}-{tag}.{ext}", std::process::id()))
}

/// WAV `f32` IEEE sin pérdidas con la muestra `sample(frame, canal)`.
pub fn write_wav(tag: &str, channels: usize, frames: u64, rate: u32, sample: impl Fn(u64, usize) -> f32) -> PathBuf {
    let mut data = Vec::with_capacity(frames as usize * channels * 4);
    for f in 0..frames {
        for c in 0..channels {
            data.extend_from_slice(&sample(f, c).to_le_bytes());
        }
    }
    let mut wav = Vec::new();
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&3u16.to_le_bytes());
    wav.extend_from_slice(&(channels as u16).to_le_bytes());
    wav.extend_from_slice(&rate.to_le_bytes());
    wav.extend_from_slice(&(rate * channels as u32 * 4).to_le_bytes());
    wav.extend_from_slice(&(channels as u16 * 4).to_le_bytes());
    wav.extend_from_slice(&32u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
    wav.extend_from_slice(&data);
    let path = temp_path(tag, "wav");
    std::fs::write(&path, wav).expect("no se pudo escribir el WAV de prueba");
    path
}

/// Borra el archivo al salir del ámbito.
pub struct Cleanup(pub Vec<PathBuf>);

impl Drop for Cleanup {
    fn drop(&mut self) {
        for p in &self.0 {
            let _ = std::fs::remove_file(p);
        }
    }
}
