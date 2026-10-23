//! Estado de salud compartido entre el backend de audio, el secuenciador, el watchdog y el IPC.
//!
//! Todo son atómicos: ningún hilo bloquea a otro, y el callback de audio puede escribirlos.

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64};
use std::time::Instant;

use qlab_core::clock::{Micros, MonotonicClock};

/// Origen común de los relojes del daemon (`Instant` monotónico). Es el reloj del `Scheduler`, de
/// los latidos y del watchdog.
#[derive(Clone, Copy, Debug)]
pub struct Epoch(Instant);

impl Epoch {
    pub fn now() -> Self {
        Self(Instant::now())
    }

    #[inline]
    pub fn now_ns(&self) -> u64 {
        self.0.elapsed().as_nanos() as u64
    }

    /// Nanosegundos del instante `t` (para el callback, que ya midió su propio `Instant`).
    #[inline]
    pub fn ns_of(&self, t: Instant) -> u64 {
        t.saturating_duration_since(self.0).as_nanos() as u64
    }

    pub fn instant(&self) -> Instant {
        self.0
    }
}

impl MonotonicClock for Epoch {
    fn now_micros(&self) -> Micros {
        self.0.elapsed().as_micros() as Micros
    }
}

/// Décimas de dB por debajo de las cuales el maestro silencia por completo (−60,0 dB).
pub const MASTER_MUTE_X10: i32 = -600;
/// Máximo del maestro: +6,0 dB.
pub const MASTER_MAX_X10: i32 = 60;

#[derive(Debug, Default)]
pub struct Health {
    // -- Audio (lo escribe el callback) --
    pub audio_heartbeat_ns: AtomicU64,
    pub audio_running: AtomicBool,
    pub audio_callbacks: AtomicU64,
    /// Huecos entre callbacks y errores del stream.
    pub xruns: AtomicU64,
    pub cpu_permille: AtomicU32,
    pub peak_cpu_permille: AtomicU32,
    /// Duración del callback (ns): máximo, suma y nº de callbacks que superaron su periodo.
    pub callback_max_ns: AtomicU64,
    pub callback_total_ns: AtomicU64,
    pub callback_over_budget: AtomicU64,
    /// Jitter de llegada: |intervalo real - periodo esperado| (ns): máximo, suma y nº de muestras.
    pub jitter_max_ns: AtomicU64,
    pub jitter_total_ns: AtomicU64,
    pub jitter_samples: AtomicU64,
    /// Asignaciones o liberaciones detectadas dentro del callback (modo `CountOnly`).
    pub rt_alloc_violations: AtomicU64,
    /// El hilo de audio consiguió la prioridad de tiempo real.
    pub audio_priority_ok: AtomicBool,

    // -- Secuenciador --
    pub seq_heartbeat_ns: AtomicU64,
    pub seq_running: AtomicBool,
    pub seq_ticks: AtomicU64,
    /// Ticks que llegaron con más de un periodo de retraso.
    pub seq_overruns: AtomicU64,
    pub seq_max_late_ns: AtomicU64,
    pub commands_sent: AtomicU64,

    // -- Decisiones del watchdog --
    pub safe_mute: AtomicBool,
    /// Volumen maestro en décimas de dB (0 = sin cambio). `MASTER_MUTE_X10` o menos = silencio total.
    pub master_db_x10: AtomicI32,
    /// Salida de auriculares (PFL): índice del primer canal del par **más uno** (0 = sin salida de auriculares).
    /// El volumen maestro no la toca.
    pub monitor_first: std::sync::atomic::AtomicU32,
    pub audio_stalled: AtomicBool,
    pub sequencer_stalled: AtomicBool,
    /// El watchdog pide al hilo principal reiniciar el stream de audio.
    pub restart_audio: AtomicBool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_is_monotonic_and_converts() {
        let e = Epoch::now();
        let a = e.now_ns();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = e.now_ns();
        assert!(b >= a + 1_000_000);
        assert!(e.now_micros() >= b / 1_000);
        assert!(e.ns_of(Instant::now()) >= b);
        assert_eq!(e.ns_of(e.instant()), 0);
    }
}
