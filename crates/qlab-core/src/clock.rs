//! Base de tiempo: microsegundos del reloj de pared ↔ frames a la frecuencia nominal.
//!
//! Toda la lógica temporal del núcleo se expresa en **frames** a la frecuencia nominal del show
//! (enteros, sin deriva). Los microsegundos solo aparecen en la frontera con el reloj monotónico:
//! cada conversión se hace desde un instante absoluto, así que el redondeo (≤ 0,5 frame) no se
//! acumula entre ticks.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

pub type Micros = u64;
pub type Frames = u64;

pub const DEFAULT_SAMPLE_RATE: u32 = 48_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timebase {
    sample_rate: u32,
}

impl Default for Timebase {
    fn default() -> Self {
        Self::new(DEFAULT_SAMPLE_RATE)
    }
}

impl Timebase {
    pub const fn new(sample_rate: u32) -> Self {
        Self { sample_rate: if sample_rate == 0 { 1 } else { sample_rate } }
    }

    #[inline]
    pub const fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// `round(µs · fs / 10⁶)`.
    #[inline]
    pub fn micros_to_frames(&self, micros: Micros) -> Frames {
        ((micros as u128 * self.sample_rate as u128 + 500_000) / 1_000_000) as Frames
    }

    /// `round(frames · 10⁶ / fs)`. Con `fs < 10⁶`, `micros_to_frames(frames_to_micros(f)) == f`.
    #[inline]
    pub fn frames_to_micros(&self, frames: Frames) -> Micros {
        ((frames as u128 * 1_000_000 + self.sample_rate as u128 / 2) / self.sample_rate as u128) as Micros
    }

    #[inline]
    pub fn frames_to_nanos(&self, frames: Frames) -> u64 {
        ((frames as u128 * 1_000_000_000 + self.sample_rate as u128 / 2) / self.sample_rate as u128) as u64
    }

    /// Segundos → frames, redondeando al más cercano. Negativos y NaN dan 0; saturan en `u64::MAX`.
    #[inline]
    pub fn secs_to_frames(&self, secs: f64) -> Frames {
        if !(secs > 0.0) {
            return 0;
        }
        (secs * self.sample_rate as f64).round() as Frames
    }

    #[inline]
    pub fn frames_to_secs(&self, frames: Frames) -> f64 {
        frames as f64 / self.sample_rate as f64
    }
}

/// Reloj monotónico en microsegundos.
pub trait MonotonicClock: Send + Sync {
    fn now_micros(&self) -> Micros;
}

/// Reloj del sistema (`Instant`).
pub struct SystemClock {
    origin: Instant,
}

impl SystemClock {
    pub fn new() -> Self {
        Self { origin: Instant::now() }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl MonotonicClock for SystemClock {
    fn now_micros(&self) -> Micros {
        self.origin.elapsed().as_micros() as Micros
    }
}

/// Reloj manual para pruebas deterministas.
#[derive(Default)]
pub struct ManualClock {
    micros: AtomicU64,
}

impl ManualClock {
    pub fn new(start: Micros) -> Self {
        Self { micros: AtomicU64::new(start) }
    }

    pub fn set(&self, micros: Micros) {
        self.micros.store(micros, Ordering::Release);
    }

    pub fn advance(&self, micros: Micros) {
        self.micros.fetch_add(micros, Ordering::AcqRel);
    }
}

impl MonotonicClock for ManualClock {
    fn now_micros(&self) -> Micros {
        self.micros.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_micros_roundtrip_is_exact_below_one_megahertz() {
        for sr in [8_000, 44_100, 48_000, 88_200, 96_000, 192_000] {
            let tb = Timebase::new(sr);
            for f in (0..200_000u64).chain([1 << 32, 1 << 40, 123_456_789_012]) {
                assert_eq!(tb.micros_to_frames(tb.frames_to_micros(f)), f, "sr {sr} frame {f}");
            }
        }
    }

    #[test]
    fn known_conversions() {
        let tb = Timebase::new(48_000);
        assert_eq!(tb.micros_to_frames(1_000_000), 48_000);
        assert_eq!(tb.micros_to_frames(1_500_000), 72_000);
        assert_eq!(tb.frames_to_micros(48), 1_000);
        assert_eq!(tb.secs_to_frames(1.5), 72_000);
        assert_eq!(tb.secs_to_frames(0.000_010_4), 0); // medio frame es 10,4 µs
        assert_eq!(tb.secs_to_frames(0.000_010_5), 1);
        assert_eq!(tb.frames_to_nanos(48_000), 1_000_000_000);
        assert_eq!(tb.secs_to_frames(-3.0), 0);
        assert_eq!(tb.secs_to_frames(f64::NAN), 0);
        assert_eq!(tb.secs_to_frames(f64::INFINITY), u64::MAX);
        assert!((tb.frames_to_secs(24_000) - 0.5).abs() < 1e-12);
    }

    #[test]
    fn rounding_does_not_accumulate_across_ticks() {
        // 1 ms de tick a 44,1 kHz son 44,1 frames: tras 10 minutos no hay deriva.
        let tb = Timebase::new(44_100);
        let ticks = 600_000u64;
        assert_eq!(tb.micros_to_frames(ticks * 1_000), 26_460_000);
    }

    #[test]
    fn manual_clock() {
        let c = ManualClock::new(10);
        c.advance(5);
        assert_eq!(c.now_micros(), 15);
        c.set(2);
        assert_eq!(c.now_micros(), 2);
        assert!(SystemClock::new().now_micros() < 1_000_000);
    }
}
