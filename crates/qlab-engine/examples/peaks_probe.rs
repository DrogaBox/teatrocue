//! Prueba de diagnóstico: barre un archivo como lo hace la forma de onda e informa tiempo y resultado.
use std::sync::atomic::AtomicBool;
fn main() {
    let path = std::env::args().nth(1).expect("uso: peaks_probe <archivo>");
    let t0 = std::time::Instant::now();
    match qlab_engine::disk_streamer::scan_peaks(std::path::Path::new(&path), 4096, &AtomicBool::new(false)) {
        Ok(Some(p)) => println!("OK {} frames ({:.1} min) en {:?}; max global {:.3}, paquetes sin decodificar: {}, dañado desde frame {:?}", p.total_frames, p.total_frames as f64 / p.sample_rate as f64 / 60.0, t0.elapsed(), p.max.iter().cloned().fold(0.0, f32::max), p.skipped_packets, p.damaged_from_frame),
        Ok(None) => println!("cancelado"),
        Err(e) => println!("ERROR tras {:?}: {e}", t0.elapsed()),
    }
}
