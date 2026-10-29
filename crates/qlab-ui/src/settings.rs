//! Ajustes de audio del operador (dispositivo, canales, buffer), guardados en un JSON por usuario.
//!
//! El motor embebido y el daemon como proceso aparte los usan al arrancar; así la PC de cabina recuerda su
//! interfaz de sonido multicanal sin tocar la línea de órdenes.

use std::path::PathBuf;

use qlab_daemon::DaemonConfig;
use serde::{Deserialize, Serialize};

pub const BUFFER_CHOICES: [u32; 6] = [64, 128, 256, 512, 1024, 2048];
pub const RATE_CHOICES: [u32; 3] = [44_100, 48_000, 96_000];
pub const MAX_CHANNELS: u16 = 64;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioSettings {
    /// Host de audio (`wasapi`, `asio`, `alsa`, `pipewire`…); `None` = el predeterminado.
    pub host: Option<String>,
    /// Dispositivo de salida; `None` = el predeterminado del sistema.
    pub device: Option<String>,
    pub channels: u16,
    pub buffer_frames: u32,
    pub sample_rate: u32,
    /// Puerto UDP del control remoto OSC; `None` = apagado.
    pub osc_port: Option<u16>,
    /// Auriculares (PFL / audición): primer canal (desde 0) del par de salida; `None` = sin auriculares.
    pub monitor_first: Option<u16>,
    /// Auriculares en **otra placa** de sonido (el jack de la notebook, una placa USB aparte): nombre del dispositivo.
    /// Tiene prioridad sobre `monitor_first`.
    pub monitor_device: Option<String>,
    /// Pre-roll de las memorias (segundos antes del marcador): 0 = apagado, 2 o 4.
    pub preroll_secs: u8,
}

impl Default for AudioSettings {
    fn default() -> Self {
        Self { host: None, device: None, channels: 2, buffer_frames: 128, sample_rate: 48_000, osc_port: None, monitor_first: None, monitor_device: None, preroll_secs: 0 }
    }
}

impl AudioSettings {
    /// Corrige valores fuera de rango (un JSON editado a mano no debe impedir que arranque el audio).
    pub fn sanitized(mut self) -> Self {
        self.channels = self.channels.clamp(1, MAX_CHANNELS);
        self.buffer_frames = self.buffer_frames.clamp(16, 16_384);
        self.sample_rate = self.sample_rate.clamp(8_000, 384_000);
        self.preroll_secs = if matches!(self.preroll_secs, 2 | 4) { self.preroll_secs } else { 0 };
        // El par de auriculares tiene que caber en los canales de salida y no puede ser el de la sala (canales 1-2).
        self.monitor_device = self.monitor_device.take().map(|d| d.trim().to_string()).filter(|d| !d.is_empty());
        // Los auriculares aparte no usan canales de la placa principal.
        if self.monitor_device.is_some() {
            self.monitor_first = None;
        }
        self.monitor_first = self.monitor_first.filter(|&m| m >= 2 && usize::from(m) + 2 <= usize::from(self.channels));
        self.osc_port = self.osc_port.filter(|p| *p != 0);
        self.host = self.host.filter(|s| !s.trim().is_empty());
        self.device = self.device.filter(|s| !s.trim().is_empty());
        self
    }

    pub fn apply_to(&self, cfg: &mut DaemonConfig) {
        cfg.backend.host = self.host.clone();
        cfg.backend.device = self.device.clone();
        cfg.backend.channels = self.channels;
        cfg.backend.buffer_frames = self.buffer_frames;
        cfg.backend.sample_rate = self.sample_rate;
        cfg.osc_port = self.osc_port;
        cfg.monitor_first = self.monitor_first;
        cfg.backend.monitor_device = self.monitor_device.clone();
    }

    /// Argumentos equivalentes para lanzar `teatrocue-daemon` como proceso aparte.
    pub fn daemon_args(&self) -> Vec<String> {
        let mut a = vec!["--rate".into(), self.sample_rate.to_string(), "--channels".into(), self.channels.to_string(), "--buffer".into(), self.buffer_frames.to_string()];
        if let Some(d) = &self.monitor_device {
            a.extend(["--monitor-device".into(), d.clone()]);
        }
        if let Some(m) = self.monitor_first {
            a.extend(["--monitor-first".into(), (m + 1).to_string()]);
        }
        if let Some(p) = self.osc_port {
            a.extend(["--osc-port".into(), p.to_string()]);
        }
        if let Some(h) = &self.host {
            a.extend(["--host".into(), h.clone()]);
        }
        if let Some(d) = &self.device {
            a.extend(["--device".into(), d.clone()]);
        }
        a
    }

    pub fn describe(&self) -> String {
        let phones = match (&self.monitor_device, self.monitor_first) {
            (Some(d), _) => format!(" · auriculares en {d}"),
            (None, Some(m)) => format!(" · auriculares {}-{}", m + 1, m + 2),
            _ => String::new(),
        };
        format!("{} · {} canales · buffer {} · {} Hz{phones}", self.device.as_deref().unwrap_or("dispositivo predeterminado"), self.channels, self.buffer_frames, self.sample_rate)
    }
}

/// `%APPDATA%\TeatroCue\audio.json` (Windows) o `~/.config/teatrocue/audio.json`.
pub fn default_path() -> Option<PathBuf> {
    let base = if cfg!(windows) { std::env::var_os("APPDATA").map(PathBuf::from) } else { std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config"))) }?;
    Some(base.join(if cfg!(windows) { "TeatroCue" } else { "teatrocue" }).join("audio.json"))
}

/// Lee los ajustes; si falta o está dañado devuelve los de fábrica (nunca falla).
pub fn load(path: &std::path::Path) -> AudioSettings {
    std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str::<AudioSettings>(&t).ok()).unwrap_or_default().sanitized()
}

/// Guarda de forma atómica (temporal + renombrado).
pub fn save(path: &std::path::Path, s: &AudioSettings) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(s).map_err(|e| e.to_string())?).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
}

pub const MAX_RECENT: usize = 8;

/// `recent.json` junto al archivo de ajustes de audio.
pub fn recent_path(settings_path: &std::path::Path) -> PathBuf {
    settings_path.with_file_name("recent.json")
}

/// Show más reciente primero, sin repetidos y como mucho `MAX_RECENT`.
pub fn push_recent(list: &mut Vec<PathBuf>, path: PathBuf) {
    list.retain(|p| p != &path);
    list.insert(0, path);
    list.truncate(MAX_RECENT);
}

pub fn load_recent(path: &std::path::Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    v.truncate(MAX_RECENT);
    v
}

pub fn save_recent(path: &std::path::Path, list: &[PathBuf]) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    // Escritura atómica (temporal + renombrar): un corte de luz a mitad de escritura no daña la lista.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(list).map_err(|e| e.to_string())?).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_and_survives_missing_or_broken_files() {
        let dir = std::env::temp_dir().join(format!("teatrocue-settings-{}", std::process::id()));
        let path = dir.join("sub/audio.json");
        assert_eq!(load(&path), AudioSettings::default(), "sin archivo: valores de fábrica");
        let s = AudioSettings { host: Some("wasapi".into()), device: Some("Focusrite USB".into()), channels: 8, buffer_frames: 256, sample_rate: 96_000, osc_port: Some(53000), monitor_first: Some(2), monitor_device: None, preroll_secs: 4 };
        save(&path, &s).unwrap();
        assert_eq!(load(&path), s);
        std::fs::write(&path, "{ esto no es json").unwrap();
        assert_eq!(load(&path), AudioSettings::default(), "archivo dañado: valores de fábrica");
        std::fs::write(&path, r#"{"channels": 9999, "buffer_frames": 1, "device": "  "}"#).unwrap();
        let l = load(&path);
        assert_eq!((l.channels, l.buffer_frames, l.device), (MAX_CHANNELS, 16, None), "fuera de rango se corrige y los campos que faltan quedan por defecto");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recent_shows_are_most_recent_first_unique_bounded_and_persisted() {
        let mut v = Vec::new();
        for i in 0..12 {
            push_recent(&mut v, PathBuf::from(format!("/s/{i}.json")));
        }
        push_recent(&mut v, PathBuf::from("/s/5.json"));
        assert_eq!(v.len(), MAX_RECENT);
        assert_eq!(v[0], PathBuf::from("/s/5.json"));
        assert_eq!(v.iter().filter(|p| p.as_path() == std::path::Path::new("/s/5.json")).count(), 1);
        let dir = std::env::temp_dir().join(format!("teatrocue-recent-{}", std::process::id()));
        let path = recent_path(&dir.join("audio.json"));
        assert!(load_recent(&path).is_empty());
        save_recent(&path, &v).unwrap();
        assert_eq!(load_recent(&path), v);
        std::fs::write(&path, "no es json").unwrap();
        assert!(load_recent(&path).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn settings_reach_the_daemon_config_and_the_command_line() {
        let s = AudioSettings { host: Some("asio".into()), device: Some("MOTU".into()), channels: 16, buffer_frames: 64, sample_rate: 48_000, osc_port: None, monitor_first: None, monitor_device: None, preroll_secs: 0 };
        let mut cfg = DaemonConfig::default();
        s.apply_to(&mut cfg);
        assert_eq!((cfg.backend.host.as_deref(), cfg.backend.device.as_deref(), cfg.backend.channels, cfg.backend.buffer_frames), (Some("asio"), Some("MOTU"), 16, 64));
        assert_eq!(s.daemon_args(), ["--rate", "48000", "--channels", "16", "--buffer", "64", "--host", "asio", "--device", "MOTU"]);
        assert!(AudioSettings::default().daemon_args().len() == 6);
        assert!(s.describe().contains("MOTU") && AudioSettings::default().describe().contains("predeterminado"));
    }

    #[test]
    fn the_headphone_pair_must_fit_and_cannot_be_the_room_pair() {
        let mut s = AudioSettings { channels: 4, monitor_first: Some(2), ..Default::default() };
        assert_eq!(s.clone().sanitized().monitor_first, Some(2));
        assert!(s.describe().contains("auriculares 3-4"));
        assert!(s.daemon_args().windows(2).any(|w| w == ["--monitor-first", "3"]));
        s.channels = 3;
        assert_eq!(s.clone().sanitized().monitor_first, None, "el par 3-4 no cabe en 3 canales");
        s.channels = 8;
        s.monitor_first = Some(0);
        assert_eq!(s.clone().sanitized().monitor_first, None, "los canales 1-2 son la sala");
        let mut cfg = DaemonConfig::default();
        AudioSettings { channels: 6, monitor_first: Some(4), ..Default::default() }.apply_to(&mut cfg);
        assert_eq!(cfg.monitor_first, Some(4));
    }

    #[test]
    fn headphones_on_another_card_replace_the_channel_pair_and_reach_the_daemon_config() {
        let s = AudioSettings { channels: 2, monitor_first: Some(2), monitor_device: Some("  Auriculares USB ".into()), ..Default::default() }.sanitized();
        assert_eq!((s.monitor_device.as_deref(), s.monitor_first), (Some("Auriculares USB"), None), "el dispositivo aparte manda y no exige 4 canales");
        assert!(s.describe().contains("auriculares en Auriculares USB"));
        assert!(s.daemon_args().windows(2).any(|w| w == ["--monitor-device", "Auriculares USB"]));
        let mut cfg = DaemonConfig::default();
        s.apply_to(&mut cfg);
        assert_eq!(cfg.backend.monitor_device.as_deref(), Some("Auriculares USB"));
        assert_eq!(AudioSettings { monitor_device: Some("   ".into()), ..Default::default() }.sanitized().monitor_device, None);
    }
}
