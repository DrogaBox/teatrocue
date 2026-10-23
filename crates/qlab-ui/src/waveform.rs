//! Picos de forma de onda calculados en un hilo aparte (la UI nunca decodifica audio).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;

use qlab_engine::disk_streamer::{scan_peaks, scan_peaks_range, DiskStreamerPool};
use std::sync::atomic::AtomicBool;

/// Resolución de los picos: columnas mínimo/máximo que luego se remuestrean al ancho del dibujo.
pub const BUCKETS: usize = 4096;

#[derive(Debug, PartialEq)]
pub struct Peaks {
    pub sample_rate: u32,
    pub total_frames: u64,
    pub min: Vec<f32>,
    pub max: Vec<f32>,
    /// Segundo desde el que el audio del archivo está dañado: el resto se dibuja plano y suena en silencio.
    pub damaged_from: Option<f64>,
}

impl Peaks {
    pub fn duration_secs(&self) -> f64 {
        self.total_frames as f64 / self.sample_rate.max(1) as f64
    }

    /// Mínimo y máximo de la fracción `[from, to]` (0..=1) del archivo.
    pub fn range(&self, from: f32, to: f32) -> (f32, f32) {
        let n = self.min.len();
        if n == 0 {
            return (0.0, 0.0);
        }
        let a = ((from.clamp(0.0, 1.0) * n as f32) as usize).min(n - 1);
        let b = ((to.clamp(0.0, 1.0) * n as f32).ceil() as usize).clamp(a + 1, n);
        let lo = self.min[a..b].iter().copied().fold(f32::MAX, f32::min);
        let hi = self.max[a..b].iter().copied().fold(f32::MIN, f32::max);
        (lo, hi)
    }
}

/// Calcula los picos de `path` en streaming (sin cargarlo en RAM; sirve para obras de horas) y los reduce a `buckets`
/// columnas mínimo/máximo. Es síncrono: llamar desde un hilo. `pool` ya no se usa (se conserva por compatibilidad).
pub fn compute_peaks(_pool: &DiskStreamerPool, path: &Path, buckets: usize) -> Result<Peaks, String> {
    compute_peaks_cancelable(path, buckets, &AtomicBool::new(false))?.ok_or_else(|| "cancelado".to_string())
}

/// Como [`compute_peaks`], con una señal para abandonar el barrido (`Ok(None)`).
pub fn compute_peaks_cancelable(path: &Path, buckets: usize, cancel: &AtomicBool) -> Result<Option<Peaks>, String> {
    let scan = scan_peaks(path, buckets, cancel).map_err(|e| e.to_string())?;
    Ok(scan.map(|s| Peaks { sample_rate: s.sample_rate, total_frames: s.total_frames, min: s.min, max: s.max, damaged_from: s.damaged_from_frame.map(|f| f as f64 / f64::from(s.sample_rate.max(1))) }))
}

#[derive(Clone)]
pub enum WaveState {
    Pending,
    Ready(Arc<Peaks>),
    Failed(String),
}

/// Picos de un tramo ampliado (el «zoom»): `BUCKETS` columnas que cubren `[start_secs, start_secs + len_secs]`.
#[derive(Debug, PartialEq)]
pub struct Detail {
    pub start_secs: f64,
    pub len_secs: f64,
    pub min: Vec<f32>,
    pub max: Vec<f32>,
}

impl Detail {
    /// Mínimo y máximo entre los segundos `from` y `to` (que deben caer dentro del tramo).
    pub fn range(&self, from: f64, to: f64) -> (f32, f32) {
        let n = self.min.len();
        if n == 0 || self.len_secs <= 0.0 {
            return (0.0, 0.0);
        }
        let col = |t: f64| ((t - self.start_secs) / self.len_secs * n as f64).clamp(0.0, n as f64);
        let a = (col(from) as usize).min(n - 1);
        let b = (col(to).ceil() as usize).clamp(a + 1, n);
        (self.min[a..b].iter().copied().fold(f32::MAX, f32::min), self.max[a..b].iter().copied().fold(f32::MIN, f32::max))
    }

    pub fn covers(&self, from: f64, to: f64) -> bool {
        from >= self.start_secs - 1e-9 && to <= self.start_secs + self.len_secs + 1e-9
    }
}

/// Qué tramo se pide para mostrar `[start, start + len]`: el doble de ancho, con el comienzo alineado a medio ancho (y el
/// ancho redondeado a pasos de 2^(1/4)), así el detalle sirve mientras se desplaza o se acerca un poco sin recalcular.
pub fn detail_window(start: f64, len: f64, duration: f64) -> (f64, f64) {
    let q = 2f64.powf((len.max(1e-3).log2() * 4.0).round() / 4.0);
    let w = (q * 2.0).min(duration.max(1e-3));
    let step = q / 2.0;
    let s = ((start / step).floor() * step).clamp(0.0, (duration - w).max(0.0));
    (s, w)
}

type DetailKey = (PathBuf, i64, i64);

fn detail_key(path: &Path, start: f64, len: f64) -> DetailKey {
    (path.to_path_buf(), (start * 1000.0).round() as i64, (len * 1000.0).round() as i64)
}

pub struct WaveformCache {
    detail_requests: Sender<(DetailKey, f64, f64, u32)>,
    detail_results: Receiver<(DetailKey, Option<Detail>)>,
    /// `None`: se está calculando. Se guardan los últimos 12.
    details: HashMap<DetailKey, Option<Arc<Detail>>>,
    detail_order: std::collections::VecDeque<DetailKey>,
    requests: Sender<PathBuf>,
    results: Receiver<(PathBuf, Result<Peaks, String>)>,
    entries: HashMap<PathBuf, WaveState>,
    /// Archivos cuyo audio resultó dañado y aún no se avisó al operador: (ruta, segundo desde el que está dañado).
    damaged: Vec<(PathBuf, f64)>,
}

impl WaveformCache {
    pub fn new(wake: impl Fn() + Send + Sync + 'static) -> Self {
        let (requests, req_rx) = channel::<PathBuf>();
        let (res_tx, results) = channel();
        // Un despachador reparte cada archivo a su propio hilo: una obra de una hora no hace esperar a una pista corta, y como el
        // barrido es en streaming (memoria constante) no hay riesgo en tener varios a la vez.
        let wake = Arc::new(wake);
        let dwake = Arc::clone(&wake);
        let _ = std::thread::Builder::new().name("qlab-ui-waveform".into()).spawn(move || {
            while let Ok(path) = req_rx.recv() {
                let (res_tx, wake) = (res_tx.clone(), Arc::clone(&wake));
                let _ = std::thread::Builder::new().name("qlab-ui-peaks".into()).spawn(move || {
                    let r = compute_peaks_cancelable(&path, BUCKETS, &AtomicBool::new(false)).and_then(|o| o.ok_or_else(|| "cancelado".to_string()));
                    if res_tx.send((path, r)).is_ok() {
                        wake();
                    }
                });
            }
        });
        // Hilo del detalle: un tramo a la vez (el último pedido manda: los viejos se descartan al llegar).
        let (detail_requests, dreq_rx) = channel::<(DetailKey, f64, f64, u32)>();
        let (dres_tx, detail_results) = channel();
        let _ = std::thread::Builder::new().name("qlab-ui-detail".into()).spawn(move || {
            while let Ok(mut job) = dreq_rx.recv() {
                while let Ok(newer) = dreq_rx.try_recv() {
                    // Los pedidos atrasados (el operador siguió moviendo la vista) no se calculan.
                    let _ = dres_tx.send((job.0.clone(), None));
                    job = newer;
                }
                let (key, start, len, sr) = job;
                let r = scan_peaks_range(&key.0, (start * f64::from(sr)).round() as u64, ((len * f64::from(sr)).round() as u64).max(1), BUCKETS, &AtomicBool::new(false)).ok().flatten().map(|(min, max)| Detail { start_secs: start, len_secs: len, min, max });
                if dres_tx.send((key, r)).is_ok() {
                    dwake();
                }
            }
        });
        Self { requests, results, entries: HashMap::new(), damaged: Vec::new(), detail_requests, detail_results, details: HashMap::new(), detail_order: Default::default() }
    }

    /// Recoge los resultados terminados.
    pub fn poll(&mut self) {
        for (path, r) in self.results.try_iter() {
            self.entries.insert(path.clone(), match r {
                Ok(p) => {
                    if let Some(from) = p.damaged_from {
                        self.damaged.push((path, from));
                    }
                    WaveState::Ready(Arc::new(p))
                }
                Err(e) => WaveState::Failed(e),
            });
        }
    }

    /// Estado de `path`; si no se conocía, lo pide y devuelve `Pending`.
    pub fn get(&mut self, path: &Path) -> WaveState {
        self.poll();
        if let Some(s) = self.entries.get(path) {
            return s.clone();
        }
        self.entries.insert(path.to_path_buf(), WaveState::Pending);
        let _ = self.requests.send(path.to_path_buf());
        WaveState::Pending
    }

    /// El detalle (zoom) del tramo `[start, start + len]` de `path`, si ya está calculado; si no, lo pide y devuelve `None`
    /// (mientras tanto se dibuja la vista general). `sample_rate` es el del archivo (de sus picos generales).
    pub fn detail(&mut self, path: &Path, start: f64, len: f64, duration: f64, sample_rate: u32) -> Option<Arc<Detail>> {
        for (key, r) in self.detail_results.try_iter() {
            match r {
                Some(d) => {
                    self.details.insert(key, Some(Arc::new(d)));
                }
                None => {
                    self.details.remove(&key);
                    self.detail_order.retain(|k| k != &key);
                }
            }
        }
        // ¿Algún tramo ya calculado cubre lo que se ve ahora?
        let hit = self.details.iter().find_map(|(k, d)| d.as_ref().filter(|d| k.0 == path && d.covers(start, start + len) && d.len_secs <= len * 4.5).cloned());
        if hit.is_some() {
            return hit;
        }
        let (ws, wl) = detail_window(start, len, duration);
        let key = detail_key(path, ws, wl);
        if !self.details.contains_key(&key) {
            self.details.insert(key.clone(), None);
            self.detail_order.push_back(key.clone());
            while self.detail_order.len() > 12 {
                if let Some(old) = self.detail_order.pop_front() {
                    self.details.remove(&old);
                }
            }
            let _ = self.detail_requests.send((key, ws, wl, sample_rate));
        }
        None
    }

    /// Archivos con audio dañado descubiertos desde la última llamada (para avisar una sola vez).
    pub fn take_damaged(&mut self) -> Vec<(PathBuf, f64)> {
        std::mem::take(&mut self.damaged)
    }

    /// Estado conocido de `path` (sin pedir el cálculo ni recoger resultados).
    pub fn peek(&self, path: &Path) -> Option<&WaveState> {
        self.entries.get(path)
    }

    /// Olvida el resultado para recalcularlo (el archivo cambió).
    pub fn forget(&mut self, path: &Path) {
        self.entries.remove(path);
        self.details.retain(|k, _| k.0 != path);
        self.detail_order.retain(|k| k.0 != path);
    }
}

#[cfg(test)]
mod detail_tests {
    use super::*;

    #[test]
    fn the_detail_window_is_twice_the_view_aligned_so_small_moves_reuse_it() {
        let (s, w) = detail_window(100.3, 10.0, 3_600.0);
        assert!(s <= 100.3 && s + w >= 100.3 + 10.0 * 1.5, "cubre lo visible con margen: {s} +{w}");
        // Moverse un poco queda dentro del mismo tramo.
        assert_eq!(detail_window(101.0, 10.0, 3_600.0), (s, w));
        // Nunca sale del archivo.
        let (s, w) = detail_window(3_595.0, 10.0, 3_600.0);
        assert!(s + w <= 3_600.0 + 1e-9);
        let (s, w) = detail_window(0.0, 10.0, 4.0);
        assert_eq!((s, w), (0.0, 4.0));
    }

    #[test]
    fn a_detail_reads_its_columns_by_seconds() {
        let d = Detail { start_secs: 10.0, len_secs: 4.0, min: (0..4).map(|i| -(i as f32)).collect(), max: (0..4).map(|i| i as f32).collect() };
        assert_eq!(d.range(10.0, 11.0), (0.0, 0.0));
        assert_eq!(d.range(12.0, 13.9), (-3.0, 3.0));
        assert!(d.covers(10.5, 13.5) && !d.covers(9.0, 11.0));
    }

    #[test]
    fn the_cache_calculates_the_zoomed_detail_in_the_background_and_then_serves_it() {
        let wav = write_test_wav("detail", 1, 48_000 * 6, 48_000, |i, _| (i as f32) / (48_000.0 * 6.0));
        let mut cache = WaveformCache::new(|| {});
        let t0 = std::time::Instant::now();
        let d = loop {
            if let Some(d) = cache.detail(&wav, 4.0, 1.0, 6.0, 48_000) {
                break d;
            }
            assert!(t0.elapsed() < std::time::Duration::from_secs(10), "el detalle no llegó");
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        assert!(d.covers(4.0, 5.0));
        // En el segundo 4 la rampa vale ~0,667 y en el 5, ~0,833.
        let (_, hi) = d.range(4.0, 4.05);
        assert!((hi - 4.05 / 6.0).abs() < 0.02, "{hi}");
        assert!(cache.detail(&wav, 4.2, 1.0, 6.0, 48_000).is_some(), "otro desplazamiento chico sale de la caché");
        let _ = std::fs::remove_file(wav);
    }
}

#[cfg(test)]
pub(crate) fn write_test_wav(tag: &str, channels: usize, frames: u64, rate: u32, sample: impl Fn(u64, usize) -> f32) -> PathBuf {
    let mut data = Vec::new();
    for f in 0..frames {
        for c in 0..channels {
            data.extend_from_slice(&sample(f, c).to_le_bytes());
        }
    }
    let mut w = Vec::new();
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&3u16.to_le_bytes());
    w.extend_from_slice(&(channels as u16).to_le_bytes());
    w.extend_from_slice(&rate.to_le_bytes());
    w.extend_from_slice(&(rate * channels as u32 * 4).to_le_bytes());
    w.extend_from_slice(&(channels as u16 * 4).to_le_bytes());
    w.extend_from_slice(&32u16.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&(data.len() as u32).to_le_bytes());
    w.extend_from_slice(&data);
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!("qlab-ui-test-{}-{}-{tag}.wav", std::process::id(), N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
    std::fs::write(&path, w).unwrap();
    path
}

#[cfg(test)]
pub(crate) mod tests_support {
    use std::path::PathBuf;

    /// `.m4a` de 20 s con el último tercio destruido; `None` si no hay ffmpeg.
    pub(crate) fn damaged_m4a(tag: &str) -> Option<PathBuf> {
        let path = std::env::temp_dir().join(format!("qlab-ui-damaged-{}-{tag}.m4a", std::process::id()));
        let made = std::process::Command::new("ffmpeg").args(["-v", "error", "-y", "-f", "lavfi", "-i", "sine=frequency=330:duration=20:sample_rate=48000", "-ac", "2", "-c:a", "aac"]).arg(&path).status().is_ok_and(|s| s.success());
        if !made {
            return None;
        }
        let mut data = std::fs::read(&path).unwrap();
        let (mut at, mut range) = (0usize, None);
        while at + 8 <= data.len() {
            let size = u32::from_be_bytes(data[at..at + 4].try_into().unwrap()) as usize;
            if &data[at + 4..at + 8] == b"mdat" {
                range = Some((at + 8, at + size));
                break;
            }
            at += size.max(8);
        }
        let (a, b) = range.unwrap();
        let mut x = 7u32;
        for byte in &mut data[a + (b - a) * 2 / 3..b] {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            *byte = (x >> 24) as u8;
        }
        std::fs::write(&path, data).unwrap();
        Some(path)
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn peaks_capture_the_envelope() {
        // Silencio durante 1 s y luego un tono de amplitud 0,8 durante 1 s.
        let wav = write_test_wav("env", 2, 96_000, 48_000, |f, c| if f < 48_000 { 0.0 } else { 0.8 * ((f as f32 * 0.05).sin()) * if c == 0 { 1.0 } else { 0.5 } });
        let pool = DiskStreamerPool::new(1);
        let p = compute_peaks(&pool, &wav, 100).unwrap();
        assert_eq!((p.sample_rate, p.total_frames, p.min.len()), (48_000, 96_000, 100));
        assert!((p.duration_secs() - 2.0).abs() < 1e-9);
        let (lo, hi) = p.range(0.0, 0.4);
        assert_eq!((lo, hi), (0.0, 0.0), "la primera mitad está en silencio");
        let (lo, hi) = p.range(0.55, 1.0);
        assert!(hi > 0.7 && hi <= 0.8 && lo < -0.7 && lo >= -0.8, "({lo}, {hi})");
        assert_eq!(p.range(0.0, 1.0).1, p.max.iter().copied().fold(f32::MIN, f32::max));
        std::fs::remove_file(wav).ok();
    }

    #[test]
    fn short_files_get_fewer_buckets_and_ranges_are_safe() {
        let wav = write_test_wav("short", 1, 10, 48_000, |f, _| f as f32 / 10.0);
        let pool = DiskStreamerPool::new(1);
        let p = compute_peaks(&pool, &wav, BUCKETS).unwrap();
        assert_eq!(p.min.len(), 10);
        assert_eq!(p.range(0.0, 1.0), (0.0, 0.9));
        assert_eq!(p.range(2.0, 3.0).1, 0.9, "fuera de rango se recorta");
        assert_eq!(p.range(0.5, 0.5).0, 0.5, "un rango vacío toma una columna");
        let empty = Peaks { sample_rate: 1, total_frames: 0, min: vec![], max: vec![], damaged_from: None };
        assert_eq!(empty.range(0.0, 1.0), (0.0, 0.0));
        std::fs::remove_file(wav).ok();
    }

    #[test]
    fn a_three_hour_file_is_reduced_to_a_fixed_number_of_columns_without_losing_a_transient() {
        // 3 horas a 1 kHz (10,8 millones de frames: el mismo recorrido que 3 h reales, con un archivo de 43 MB).
        // Un único chasquido de amplitud 1,0 al 90 % de la pista y un murmullo de 0,05 en el resto.
        let (rate, secs) = (1_000u32, 3 * 3_600u64);
        let total = u64::from(rate) * secs;
        let spike = total * 9 / 10;
        let wav = write_test_wav("long3h", 1, total, rate, |f, _| if f == spike { 1.0 } else { 0.05 * ((f % 7) as f32 / 7.0) });
        let t0 = Instant::now();
        let p = compute_peaks(&DiskStreamerPool::new(1), &wav, BUCKETS).expect("un archivo de 3 horas ya no se rechaza");
        let took = t0.elapsed();
        assert_eq!((p.sample_rate, p.total_frames, p.min.len(), p.max.len()), (rate, total, BUCKETS, BUCKETS), "siempre {BUCKETS} columnas, dure lo que dure");
        assert!((p.duration_secs() - 10_800.0).abs() < 1e-6);
        // La decimación por picos conserva el transitorio: la columna del chasquido llega a 1,0 y las demás no.
        let col = (spike * BUCKETS as u64 / total) as usize;
        assert_eq!(p.max[col], 1.0, "el chasquido está en su columna");
        assert_eq!(p.max.iter().filter(|&&m| m > 0.5).count(), 1, "y solo en una");
        assert!(p.max.iter().enumerate().all(|(i, &m)| i == col || m <= 0.05 + 1e-6));
        assert!(took < Duration::from_secs(60), "el barrido de 3 h tardó {took:?}");
        // Se cancela a mitad de camino sin devolver nada.
        let flag = AtomicBool::new(true);
        assert_eq!(compute_peaks_cancelable(&wav, BUCKETS, &flag).unwrap(), None);
        std::fs::remove_file(wav).ok();
    }

    #[test]
    fn a_damaged_aac_reports_the_second_where_it_breaks_so_the_waveform_can_mark_it() {
        let Some(path) = tests_support::damaged_m4a("peaks") else { return eprintln!("ffmpeg no está instalado: se omite") };
        let p = compute_peaks(&DiskStreamerPool::new(1), &path, 200).expect("un archivo dañado igual se dibuja");
        let from = p.damaged_from.expect("se avisa dónde se rompe");
        assert!((10.0..=15.0).contains(&from), "el daño empieza hacia los 13,3 s: {from}");
        assert!((p.duration_secs() - 20.0).abs() < 0.1, "se dibuja toda la duración");
        assert!(p.range(0.9, 1.0) == (0.0, 0.0), "lo dañado se dibuja plano");
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn the_cache_hands_out_each_damaged_file_once_so_the_app_can_warn() {
        let Some(path) = tests_support::damaged_m4a("cache") else { return eprintln!("ffmpeg no está instalado: se omite") };
        let mut c = WaveformCache::new(|| {});
        let t0 = Instant::now();
        while !matches!(c.get(&path), WaveState::Ready(_)) {
            assert!(t0.elapsed() < Duration::from_secs(20), "tiempo agotado");
            std::thread::sleep(Duration::from_millis(10));
        }
        let d = c.take_damaged();
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].0, path);
        assert!(c.take_damaged().is_empty(), "se avisa una sola vez");
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn errors_are_reported() {
        let pool = DiskStreamerPool::new(1);
        assert!(compute_peaks(&pool, Path::new("/no/existe.wav"), 100).is_err());
    }

    #[test]
    fn the_cache_computes_in_the_background_and_wakes_the_ui() {
        let wav = write_test_wav("cache", 1, 48_000, 48_000, |f, _| (f as f32 * 0.01).sin());
        let woke = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let w = Arc::clone(&woke);
        let mut cache = WaveformCache::new(move || {
            w.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        });
        assert!(matches!(cache.get(&wav), WaveState::Pending));
        let t0 = Instant::now();
        let ready = loop {
            if let WaveState::Ready(p) = cache.get(&wav) {
                break p;
            }
            assert!(t0.elapsed() < Duration::from_secs(5), "la forma de onda no llegó");
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(ready.total_frames, 48_000);
        assert!(woke.load(std::sync::atomic::Ordering::Relaxed) >= 1);
        // Un archivo inexistente falla sin colgar y queda recordado.
        let missing = Path::new("/no/hay/nada.wav");
        let t0 = Instant::now();
        loop {
            if let WaveState::Failed(e) = cache.get(missing) {
                assert!(!e.is_empty());
                break;
            }
            assert!(t0.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(5));
        }
        cache.forget(&wav);
        assert!(matches!(cache.get(&wav), WaveState::Pending), "tras olvidar se recalcula");
        std::fs::remove_file(wav).ok();
    }
}
