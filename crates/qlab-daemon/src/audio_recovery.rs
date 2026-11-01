//! Recuperación de la salida de audio cuando se pierde la placa (se desenchufa la interfaz o Windows la cambia).
//!
//! El watchdog reinicia el stream cuando el callback deja de latir, con pocos intentos. Si la placa ya no está, esos
//! reinicios fallan (o abren un stream que nunca late) y el motor quedaba mudo sin reintentar más. [`AudioRecovery`]
//! decide en cada vuelta del hilo principal cuándo volver a abrir la salida y cuándo está caída («SIN AUDIO»):
//! mientras no hay audio reintenta cada [`RETRY_EVERY`], y la da por recuperada recién cuando el callback vuelve a
//! correr, no cuando el stream solo se abrió.
//!
//! Es una máquina de estados pura, como [`crate::watchdog::Watchdog`]: se prueba con tiempos inventados.

use std::time::Duration;

/// Espera entre reintentos mientras la salida está caída.
pub const RETRY_EVERY: Duration = Duration::from_secs(2);
/// Callbacks que tienen que llegar después de un reinicio para dar la salida por recuperada.
const CALLBACKS_TO_RECOVER: u64 = 3;

/// Lo que se observa en una vuelta del hilo principal.
#[derive(Clone, Copy, Debug)]
pub struct Observed {
    pub now_ns: u64,
    /// Alguien pidió reiniciar ya: el watchdog, el callback que vio desaparecer la placa o «Reintentar» en la interfaz.
    pub restart_requested: bool,
    /// Hay un stream abierto.
    pub running: bool,
    /// El watchdog agotó sus reinicios sin que el callback volviera a latir.
    pub watchdog_gave_up: bool,
    /// Callbacks de audio desde el arranque.
    pub callbacks: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    /// La salida se cayó.
    Lost,
    /// La salida volvió a sonar.
    Recovered,
}

#[derive(Debug, Default)]
pub struct AudioRecovery {
    down: bool,
    next_retry_ns: Option<u64>,
    /// Callbacks contados al abrir el último stream mientras la salida estaba caída.
    callbacks_at_open: Option<u64>,
}

impl AudioRecovery {
    pub fn new() -> Self {
        Self::default()
    }

    /// La salida está caída: el motor no tiene por dónde sonar.
    pub fn is_down(&self) -> bool {
        self.down
    }

    /// Una vuelta: devuelve si hay que reiniciar el stream ahora y el cambio de estado, si lo hubo.
    pub fn observe(&mut self, o: Observed) -> (bool, Option<Change>) {
        let mut change = None;
        if self.down {
            let flowing = self.callbacks_at_open.is_some_and(|at| o.callbacks >= at + CALLBACKS_TO_RECOVER);
            if o.running && !o.watchdog_gave_up && flowing {
                *self = Self::default();
                return (o.restart_requested, Some(Change::Recovered));
            }
        } else if o.watchdog_gave_up {
            self.down = true;
            self.next_retry_ns = None;
            change = Some(Change::Lost);
        } else if !o.running {
            // Sin stream: se intenta abrir enseguida y, si falla, [`Self::restarted`] da la salida por caída con el
            // motivo real.
            return (true, None);
        }
        let due = self.down && self.next_retry_ns.map_or(true, |t| o.now_ns >= t);
        (o.restart_requested || due, change)
    }

    /// Resultado de un reinicio. Si falla con la salida en marcha, la salida se cae.
    pub fn restarted(&mut self, ok: bool, now_ns: u64, callbacks: u64) -> Option<Change> {
        self.next_retry_ns = Some(now_ns + RETRY_EVERY.as_nanos() as u64);
        self.callbacks_at_open = ok.then_some(callbacks);
        if !ok && !self.down {
            self.down = true;
            return Some(Change::Lost);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: u64 = 1_000_000_000;

    fn seen(now_s: f64, running: bool, callbacks: u64) -> Observed {
        Observed { now_ns: (now_s * S as f64) as u64, restart_requested: false, running, watchdog_gave_up: false, callbacks }
    }

    #[test]
    fn a_healthy_output_never_restarts() {
        let mut r = AudioRecovery::new();
        for t in 0..100 {
            assert_eq!(r.observe(seen(t as f64 * 0.02, true, t * 8)), (false, None));
        }
        assert!(!r.is_down());
    }

    #[test]
    fn a_requested_restart_that_works_keeps_the_output_up() {
        let mut r = AudioRecovery::new();
        let o = Observed { restart_requested: true, ..seen(1.0, true, 100) };
        assert_eq!(r.observe(o), (true, None));
        assert_eq!(r.restarted(true, S, 100), None);
        assert!(!r.is_down());
        assert_eq!(r.observe(seen(1.02, true, 108)), (false, None));
    }

    #[test]
    fn a_missing_card_is_retried_every_two_seconds_until_it_comes_back() {
        let mut r = AudioRecovery::new();
        // El watchdog pide reiniciar y la placa ya no está: el reinicio falla y la salida se cae.
        let o = Observed { restart_requested: true, ..seen(0.0, true, 500) };
        assert_eq!(r.observe(o), (true, None));
        assert_eq!(r.restarted(false, 0, 500), Some(Change::Lost));
        assert!(r.is_down());
        let mut retries = Vec::new();
        for ms in (20u32..7_000).step_by(20) {
            let (restart, change) = r.observe(seen(f64::from(ms) / 1_000.0, false, 500));
            assert_eq!(change, None, "el aviso de caída sale una sola vez");
            if restart {
                retries.push(ms);
                assert_eq!(r.restarted(false, u64::from(ms) * 1_000_000, 500), None);
            }
        }
        assert_eq!(retries, vec![2_000, 4_000, 6_000], "reintentos cada 2 s, sin ráfagas");
        // Vuelve la placa: el stream abre, pero recién cuando corre el callback se da por recuperada.
        let (restart, _) = r.observe(seen(8.0, false, 500));
        assert!(restart);
        assert_eq!(r.restarted(true, 8 * S, 500), None);
        assert_eq!(r.observe(seen(8.02, true, 501)), (false, None), "abrir no alcanza: falta que suene");
        assert!(r.is_down());
        assert_eq!(r.observe(seen(8.04, true, 504)), (false, Some(Change::Recovered)));
        assert!(!r.is_down());
    }

    #[test]
    fn a_stream_that_opens_but_never_runs_stays_down_and_keeps_retrying() {
        let mut r = AudioRecovery::new();
        // El watchdog agotó sus reinicios: la salida está caída aunque haya un stream abierto.
        let gave_up = |t: f64| Observed { watchdog_gave_up: true, ..seen(t, true, 40) };
        assert_eq!(r.observe(gave_up(3.0)), (true, Some(Change::Lost)));
        r.restarted(true, 3 * S, 40);
        assert_eq!(r.observe(seen(3.5, true, 40)), (false, None), "sin callbacks no hay recuperación");
        assert_eq!(r.observe(seen(5.0, true, 40)), (true, None), "a los 2 s se vuelve a intentar");
    }

    #[test]
    fn retry_from_the_interface_does_not_wait_for_the_next_slot() {
        let mut r = AudioRecovery::new();
        assert_eq!(r.observe(seen(0.0, false, 0)), (true, None), "sin stream se intenta abrir antes de avisar");
        assert_eq!(r.restarted(false, 0, 0), Some(Change::Lost));
        assert_eq!(r.observe(seen(0.5, false, 0)), (false, None));
        assert_eq!(r.observe(Observed { restart_requested: true, ..seen(0.6, false, 0) }), (true, None), "«Reintentar» va enseguida");
    }
}
