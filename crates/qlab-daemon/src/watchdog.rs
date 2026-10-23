//! Watchdog de 250 ms (spec §4.3, fase 4): vigila los latidos del callback de audio y del
//! secuenciador.
//!
//! * **Audio congelado** (sin latido > 250 ms con el stream en marcha): se avisa, se marca
//!   `audio_freeze_detected` en la telemetría y se pide al hilo principal reiniciar el stream, con
//!   un número limitado de intentos espaciados.
//! * **Secuenciador congelado**: el motor seguiría sonando sin nadie que controle los cues, así que
//!   (si está activado) se silencia con una rampa (*safe mute*) y se libera tras varias
//!   comprobaciones sanas seguidas.
//!
//! [`Watchdog`] es una máquina de estados pura (testeable con tiempos inventados);
//! [`WatchdogHandle`] la ejecuta en su propio hilo.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use qlab_protocol::ipc_msg::ServerEvent;

use crate::events::EventHub;
use crate::health::{Epoch, Health};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Subsystem {
    Audio,
    Sequencer,
}

impl Subsystem {
    pub fn name(self) -> &'static str {
        match self {
            Self::Audio => "audio",
            Self::Sequencer => "sequencer",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WatchdogConfig {
    /// Tiempo sin latido que se considera congelamiento.
    pub timeout: Duration,
    pub check_period: Duration,
    pub mute_on_sequencer_stall: bool,
    /// Comprobaciones sanas consecutivas para soltar el safe mute.
    pub healthy_checks_to_unmute: u32,
    pub max_restart_attempts: u32,
    /// Espera mínima entre reinicios del stream.
    pub restart_backoff: Duration,
}

impl Default for WatchdogConfig {
    fn default() -> Self {
        Self {
            timeout: Duration::from_millis(250),
            check_period: Duration::from_millis(50),
            mute_on_sequencer_stall: true,
            healthy_checks_to_unmute: 3,
            max_restart_attempts: 3,
            restart_backoff: Duration::from_secs(1),
        }
    }
}

/// Latidos observados en un instante. `None` = ese subsistema no está en marcha (no se vigila).
#[derive(Clone, Copy, Debug)]
pub struct Beats {
    pub now_ns: u64,
    pub audio_ns: Option<u64>,
    pub sequencer_ns: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WatchdogEvent {
    Stalled { who: Subsystem, age_ms: u64 },
    Recovered { who: Subsystem },
    SafeMuteEngaged,
    SafeMuteReleased,
    RestartAudio { attempt: u32 },
    GaveUp,
}

#[derive(Debug)]
pub struct Watchdog {
    cfg: WatchdogConfig,
    audio_stalled: bool,
    seq_stalled: bool,
    muted: bool,
    healthy_streak: u32,
    restarts: u32,
    last_restart_ns: Option<u64>,
    gave_up: bool,
}

impl Watchdog {
    pub fn new(cfg: WatchdogConfig) -> Self {
        Self {
            cfg,
            audio_stalled: false,
            seq_stalled: false,
            muted: false,
            healthy_streak: 0,
            restarts: 0,
            last_restart_ns: None,
            gave_up: false,
        }
    }

    pub fn audio_stalled(&self) -> bool {
        self.audio_stalled
    }

    pub fn sequencer_stalled(&self) -> bool {
        self.seq_stalled
    }

    pub fn safe_mute(&self) -> bool {
        self.muted
    }

    pub fn restart_attempts(&self) -> u32 {
        self.restarts
    }

    fn judge(&self, now_ns: u64, beat: Option<u64>) -> Option<(bool, u64)> {
        let age = now_ns.saturating_sub(beat?);
        Some((age > self.cfg.timeout.as_nanos() as u64, age))
    }

    /// Una comprobación. Devuelve los cambios de estado y las acciones que hay que ejecutar.
    pub fn check(&mut self, beats: Beats) -> Vec<WatchdogEvent> {
        let mut out = Vec::new();

        // Audio
        match self.judge(beats.now_ns, beats.audio_ns) {
            Some((true, age)) => {
                if !self.audio_stalled {
                    self.audio_stalled = true;
                    out.push(WatchdogEvent::Stalled { who: Subsystem::Audio, age_ms: age / 1_000_000 });
                }
                self.maybe_restart(beats.now_ns, &mut out);
            }
            Some((false, _)) => {
                if self.audio_stalled {
                    self.audio_stalled = false;
                    self.restarts = 0;
                    self.gave_up = false;
                    out.push(WatchdogEvent::Recovered { who: Subsystem::Audio });
                }
            }
            None => self.audio_stalled = false,
        }

        // Secuenciador
        match self.judge(beats.now_ns, beats.sequencer_ns) {
            Some((true, age)) => {
                if !self.seq_stalled {
                    self.seq_stalled = true;
                    out.push(WatchdogEvent::Stalled { who: Subsystem::Sequencer, age_ms: age / 1_000_000 });
                }
                self.healthy_streak = 0;
                if self.cfg.mute_on_sequencer_stall && !self.muted {
                    self.muted = true;
                    out.push(WatchdogEvent::SafeMuteEngaged);
                }
            }
            Some((false, _)) => {
                if self.seq_stalled {
                    self.seq_stalled = false;
                    out.push(WatchdogEvent::Recovered { who: Subsystem::Sequencer });
                }
                if self.muted {
                    self.healthy_streak += 1;
                    if self.healthy_streak >= self.cfg.healthy_checks_to_unmute {
                        self.muted = false;
                        self.healthy_streak = 0;
                        out.push(WatchdogEvent::SafeMuteReleased);
                    }
                }
            }
            None => self.seq_stalled = false,
        }
        out
    }

    fn maybe_restart(&mut self, now_ns: u64, out: &mut Vec<WatchdogEvent>) {
        let backoff = self.cfg.restart_backoff.as_nanos() as u64;
        let due = self.last_restart_ns.map_or(true, |t| now_ns.saturating_sub(t) >= backoff);
        if !due {
            return;
        }
        if self.restarts < self.cfg.max_restart_attempts {
            self.restarts += 1;
            self.last_restart_ns = Some(now_ns);
            out.push(WatchdogEvent::RestartAudio { attempt: self.restarts });
        } else if !self.gave_up {
            self.gave_up = true;
            out.push(WatchdogEvent::GaveUp);
        }
    }
}

/// Hilo del watchdog: lee los latidos de [`Health`], aplica las decisiones y avisa por el bus de eventos.
pub struct WatchdogHandle {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl WatchdogHandle {
    pub fn spawn(cfg: WatchdogConfig, epoch: Epoch, health: Arc<Health>, events: Arc<EventHub>) -> std::io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::clone(&stop);
        let join = std::thread::Builder::new().name("qlab-watchdog".into()).spawn(move || {
            let mut dog = Watchdog::new(cfg);
            while !stop_flag.load(Ordering::Acquire) {
                let beats = Beats {
                    now_ns: epoch.now_ns(),
                    audio_ns: health.audio_running.load(Ordering::Acquire).then(|| health.audio_heartbeat_ns.load(Ordering::Acquire)),
                    sequencer_ns: health.seq_running.load(Ordering::Acquire).then(|| health.seq_heartbeat_ns.load(Ordering::Acquire)),
                };
                for event in dog.check(beats) {
                    apply(&event, &health, &events);
                }
                health.audio_stalled.store(dog.audio_stalled(), Ordering::Release);
                health.sequencer_stalled.store(dog.sequencer_stalled(), Ordering::Release);
                std::thread::sleep(cfg.check_period);
            }
        })?;
        Ok(Self { stop, join: Some(join) })
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

impl Drop for WatchdogHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

fn apply(event: &WatchdogEvent, health: &Health, events: &EventHub) {
    let warn = |source: &str, message: String| {
        eprintln!("[watchdog] {message}");
        events.publish(ServerEvent::Warning { source: source.into(), message });
    };
    match event {
        WatchdogEvent::Stalled { who, age_ms } => {
            warn(who.name(), format!("{} sin latido desde hace {age_ms} ms", who.name()));
        }
        WatchdogEvent::Recovered { who } => warn(who.name(), format!("{} recuperado", who.name())),
        WatchdogEvent::SafeMuteEngaged => {
            health.safe_mute.store(true, Ordering::Release);
            events.publish(ServerEvent::SafeMute { engaged: true });
            eprintln!("[watchdog] safe mute activado");
        }
        WatchdogEvent::SafeMuteReleased => {
            health.safe_mute.store(false, Ordering::Release);
            events.publish(ServerEvent::SafeMute { engaged: false });
            eprintln!("[watchdog] safe mute liberado");
        }
        WatchdogEvent::RestartAudio { attempt } => {
            health.restart_audio.store(true, Ordering::Release);
            warn("audio", format!("reiniciando el stream de audio (intento {attempt})"));
        }
        WatchdogEvent::GaveUp => warn("audio", "el stream de audio no se recupera tras varios reinicios".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1_000_000;

    fn dog() -> Watchdog {
        Watchdog::new(WatchdogConfig::default())
    }

    fn beats(now_ms: u64, audio_ms: Option<u64>, seq_ms: Option<u64>) -> Beats {
        Beats { now_ns: now_ms * MS, audio_ns: audio_ms.map(|m| m * MS), sequencer_ns: seq_ms.map(|m| m * MS) }
    }

    #[test]
    fn healthy_systems_produce_no_events() {
        let mut d = dog();
        for t in (0..2_000).step_by(50) {
            assert!(d.check(beats(t, Some(t - t % 5), Some(t))).is_empty());
        }
        assert!(!d.audio_stalled() && !d.sequencer_stalled() && !d.safe_mute());
    }

    #[test]
    fn the_threshold_is_250_ms() {
        let mut d = dog();
        assert!(d.check(beats(250, Some(0), None)).is_empty(), "250 ms exactos aún no es congelamiento");
        let ev = d.check(beats(251, Some(0), None));
        assert_eq!(ev[0], WatchdogEvent::Stalled { who: Subsystem::Audio, age_ms: 251 });
        assert!(d.audio_stalled());
    }

    #[test]
    fn audio_stall_requests_spaced_and_bounded_restarts() {
        let mut d = dog();
        let mut restarts = Vec::new();
        let mut gave_up = 0;
        let mut stalled = 0;
        for t in (300..=10_000).step_by(50) {
            for e in d.check(beats(t, Some(0), Some(t))) {
                match e {
                    WatchdogEvent::RestartAudio { attempt } => restarts.push((t, attempt)),
                    WatchdogEvent::GaveUp => gave_up += 1,
                    WatchdogEvent::Stalled { .. } => stalled += 1,
                    other => panic!("{other:?}"),
                }
            }
        }
        assert_eq!(restarts, vec![(300, 1), (1_300, 2), (2_300, 3)], "reintentos espaciados por 1 s");
        assert_eq!((gave_up, stalled), (1, 1), "se rinde una sola vez y el aviso no se repite");
        assert!(!d.safe_mute(), "un audio congelado no silencia (no hay nada que silenciar)");
    }

    #[test]
    fn audio_recovery_resets_the_restart_budget() {
        let mut d = dog();
        d.check(beats(300, Some(0), None));
        assert_eq!(d.restart_attempts(), 1);
        let ev = d.check(beats(350, Some(340), None));
        assert_eq!(ev, vec![WatchdogEvent::Recovered { who: Subsystem::Audio }]);
        assert_eq!(d.restart_attempts(), 0);
        assert!(!d.audio_stalled());
    }

    #[test]
    fn sequencer_stall_engages_safe_mute_and_releases_after_healthy_checks() {
        let mut d = dog();
        let ev = d.check(beats(400, Some(400), Some(100)));
        assert_eq!(ev, vec![WatchdogEvent::Stalled { who: Subsystem::Sequencer, age_ms: 300 }, WatchdogEvent::SafeMuteEngaged]);
        assert!(d.safe_mute() && d.sequencer_stalled());
        assert!(d.check(beats(450, Some(450), Some(100))).is_empty(), "sigue congelado: no se repite el aviso");

        assert_eq!(d.check(beats(500, Some(500), Some(500))), vec![WatchdogEvent::Recovered { who: Subsystem::Sequencer }]);
        assert!(d.safe_mute(), "no se suelta de inmediato");
        assert!(d.check(beats(550, Some(550), Some(550))).is_empty());
        assert_eq!(d.check(beats(600, Some(600), Some(600))), vec![WatchdogEvent::SafeMuteReleased]);
        assert!(!d.safe_mute());
    }

    #[test]
    fn a_relapse_during_the_unmute_countdown_restarts_it() {
        let mut d = dog();
        d.check(beats(400, None, Some(0)));
        d.check(beats(450, None, Some(450)));
        d.check(beats(500, None, Some(500))); // 2 sanas
        d.check(beats(900, None, Some(500))); // vuelve a congelarse
        assert!(d.safe_mute());
        d.check(beats(950, None, Some(950)));
        d.check(beats(1_000, None, Some(1_000)));
        assert!(d.safe_mute(), "la cuenta empezó de nuevo");
        assert!(d.check(beats(1_050, None, Some(1_050))).contains(&WatchdogEvent::SafeMuteReleased));
    }

    #[test]
    fn safe_mute_can_be_disabled() {
        let mut d = Watchdog::new(WatchdogConfig { mute_on_sequencer_stall: false, ..Default::default() });
        let ev = d.check(beats(1_000, None, Some(0)));
        assert_eq!(ev.len(), 1);
        assert!(!d.safe_mute());
    }

    #[test]
    fn unmonitored_subsystems_are_ignored() {
        let mut d = dog();
        assert!(d.check(beats(10_000, None, None)).is_empty());
        d.check(beats(10_000, Some(0), None));
        assert!(d.audio_stalled());
        d.check(beats(10_050, None, None)); // el stream se detuvo a propósito
        assert!(!d.audio_stalled());
    }

    #[test]
    fn handle_applies_decisions_to_shared_health() {
        let epoch = Epoch::now();
        let health = Arc::new(Health::default());
        let events = Arc::new(EventHub::new());
        let rx = events.subscribe();
        let cfg = WatchdogConfig {
            timeout: Duration::from_millis(40),
            check_period: Duration::from_millis(5),
            healthy_checks_to_unmute: 2,
            restart_backoff: Duration::from_millis(30),
            ..Default::default()
        };
        // Ambos "en marcha" pero el secuenciador nunca late y el audio late un rato y se congela.
        health.seq_running.store(true, Ordering::Release);
        health.seq_heartbeat_ns.store(epoch.now_ns(), Ordering::Release);
        health.audio_running.store(true, Ordering::Release);
        health.audio_heartbeat_ns.store(epoch.now_ns(), Ordering::Release);
        let mut handle = WatchdogHandle::spawn(cfg, epoch, Arc::clone(&health), Arc::clone(&events)).unwrap();

        let wait = |cond: &dyn Fn() -> bool| {
            let t0 = std::time::Instant::now();
            while !cond() {
                assert!(t0.elapsed() < Duration::from_secs(3), "el watchdog no reaccionó");
                std::thread::sleep(Duration::from_millis(2));
            }
        };
        wait(&|| health.safe_mute.load(Ordering::Acquire));
        wait(&|| health.restart_audio.load(Ordering::Acquire));
        wait(&|| health.audio_stalled.load(Ordering::Acquire) && health.sequencer_stalled.load(Ordering::Acquire));

        // El secuenciador "vuelve": latidos frescos hasta que se libere el mute.
        let beat = || health.seq_heartbeat_ns.store(epoch.now_ns(), Ordering::Release);
        wait(&|| {
            beat();
            !health.safe_mute.load(Ordering::Acquire)
        });
        handle.stop();
        let got: Vec<_> = rx.try_iter().collect();
        assert!(got.contains(&ServerEvent::SafeMute { engaged: true }));
        assert!(got.contains(&ServerEvent::SafeMute { engaged: false }));
        assert!(got.iter().any(|e| matches!(e, ServerEvent::Warning { source, .. } if source == "audio")));
    }
}
