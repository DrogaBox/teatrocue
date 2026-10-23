//! Silenciar los **sonidos del sistema** de Windows (notificaciones, avisos, el «ding» de un error) mientras TeatroCue está
//! abierto: es la sesión «Sonidos del sistema» del Mezclador de volumen de cada salida. El audio del show no pasa por esa
//! sesión, así que no se toca; las demás aplicaciones tampoco.
//!
//! - Solo se silencian las sesiones que **estaban sonando**; las que el operador ya tenía silenciadas quedan como estaban.
//! - Las que silenció TeatroCue se anotan en `sonidos-sistema.txt` (junto a los ajustes): al quitar la opción o al cerrar se
//!   devuelven, y si el programa se cerrara de golpe, el próximo arranque sabe cuáles eran suyas.
//! - Un hilo aparte lo vuelve a aplicar cada pocos segundos: una placa que se conecta o una sesión que Windows crea tarde
//!   también quedan en silencio.
//!
//! En Linux no hace nada (el botón no se muestra).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::time::Duration;

/// ¿Este sistema permite silenciar sus sonidos desde aquí?
pub const SUPPORTED: bool = cfg!(windows);

/// Cada cuánto se vuelve a aplicar el silencio (placas nuevas, sesiones que aparecen tarde).
const REAPPLY: Duration = Duration::from_secs(3);

/// Archivo donde se anotan las salidas cuyo sonido de sistema silenció TeatroCue.
pub fn marker_path(settings_path: &Path) -> PathBuf {
    settings_path.with_file_name("sonidos-sistema.txt")
}

fn load_marker(path: Option<&Path>) -> BTreeSet<String> {
    path.and_then(|p| std::fs::read_to_string(p).ok()).map(|t| t.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect()).unwrap_or_default()
}

fn save_marker(path: Option<&Path>, ours: &BTreeSet<String>) {
    let Some(p) = path else { return };
    if ours.is_empty() {
        let _ = std::fs::remove_file(p);
    } else {
        let _ = std::fs::write(p, ours.iter().map(|s| format!("{s}\n")).collect::<String>());
    }
}

/// Una sesión «Sonidos del sistema» de una salida: su identificador y si está silenciada.
pub trait Backend: Send + 'static {
    /// Salidas activas con su sesión de sonidos del sistema: (id de la salida, silenciada).
    fn sessions(&mut self) -> Result<Vec<(String, bool)>, String>;
    fn set_muted(&mut self, device: &str, muted: bool) -> Result<(), String>;
}

/// Lo que hay que hacer en una pasada: silenciar las que suenan (y anotarlas como propias) o devolver solo las propias.
/// Devuelve cuántas sesiones quedaron silenciadas por TeatroCue.
pub fn apply(backend: &mut dyn Backend, want_muted: bool, ours: &mut BTreeSet<String>) -> Result<usize, String> {
    let sessions = backend.sessions()?;
    let mut first_err = None;
    for (dev, muted) in &sessions {
        if want_muted && !muted {
            match backend.set_muted(dev, true) {
                Ok(()) => {
                    ours.insert(dev.clone());
                }
                Err(e) => first_err = first_err.or(Some(e)),
            }
        } else if !want_muted && ours.contains(dev) {
            if *muted {
                if let Err(e) = backend.set_muted(dev, false) {
                    first_err = first_err.or(Some(e));
                    continue;
                }
            }
            ours.remove(dev);
        }
    }
    if !want_muted {
        // Salidas que ya no están (placa desconectada): no se pueden devolver ahora; se olvidan para no arrastrarlas.
        let present: BTreeSet<&String> = sessions.iter().map(|(d, _)| d).collect();
        ours.retain(|d| present.contains(d));
    }
    match first_err {
        Some(e) if ours.is_empty() => Err(e),
        _ => Ok(ours.len()),
    }
}

/// Estado que ve la interfaz.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Status {
    pub muted_sessions: usize,
    pub error: Option<String>,
}

/// Hilo que mantiene el silencio. Al soltarlo devuelve los sonidos y espera a que termine (al cerrar el programa).
pub struct Keeper {
    tx: Option<Sender<bool>>,
    status: std::sync::Arc<std::sync::Mutex<Status>>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl Keeper {
    /// `marker`: dónde anotar las sesiones propias (`None`: pruebas, sin archivo).
    pub fn start(backend: Box<dyn Backend>, marker: Option<PathBuf>, want_muted: bool) -> Self {
        let (tx, rx) = mpsc::channel::<bool>();
        let status = std::sync::Arc::new(std::sync::Mutex::new(Status::default()));
        let st = status.clone();
        let join = std::thread::Builder::new()
            .name("sonidos-sistema".into())
            .spawn(move || {
                let mut backend = backend;
                let mut ours = load_marker(marker.as_deref());
                let mut want = want_muted;
                loop {
                    let r = apply(backend.as_mut(), want, &mut ours);
                    save_marker(marker.as_deref(), &ours);
                    if let Ok(mut s) = st.lock() {
                        *s = match r {
                            Ok(n) => Status { muted_sessions: n, error: None },
                            Err(e) => Status { muted_sessions: ours.len(), error: Some(e) },
                        };
                    }
                    match rx.recv_timeout(if want { REAPPLY } else { Duration::from_secs(3600) }) {
                        Ok(w) => want = w,
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => {
                            // Al cerrar: devolver lo que se silenció.
                            let _ = apply(backend.as_mut(), false, &mut ours);
                            save_marker(marker.as_deref(), &ours);
                            return;
                        }
                    }
                }
            })
            .ok();
        Self { tx: Some(tx), status, join }
    }

    /// Para el programa real: el mezclador de Windows (en otros sistemas, una salida vacía que no hace nada).
    pub fn system(settings_path: Option<&Path>, want_muted: bool) -> Self {
        Self::start(Box::new(SystemBackend), settings_path.map(marker_path), want_muted)
    }

    pub fn set(&self, muted: bool) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(muted);
        }
    }

    pub fn status(&self) -> Status {
        self.status.lock().map(|s| s.clone()).unwrap_or_default()
    }
}

impl Drop for Keeper {
    fn drop(&mut self) {
        self.tx = None;
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// Mezclador de volumen de Windows.
pub struct SystemBackend;

#[cfg(not(windows))]
impl Backend for SystemBackend {
    fn sessions(&mut self) -> Result<Vec<(String, bool)>, String> {
        Ok(Vec::new())
    }
    fn set_muted(&mut self, _device: &str, _muted: bool) -> Result<(), String> {
        Ok(())
    }
}

#[cfg(windows)]
mod win {
    use windows::core::Interface;
    use windows::Win32::Media::Audio::{eRender, IAudioSessionControl2, IAudioSessionManager2, IMMDevice, IMMDeviceEnumerator, ISimpleAudioVolume, MMDeviceEnumerator, DEVICE_STATE_ACTIVE};
    use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED};

    fn com() {
        // El hilo es nuestro: COM en modo multihilo. Si ya estaba iniciado, sigue igual.
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
    }

    fn devices() -> windows::core::Result<Vec<(String, IMMDevice)>> {
        com();
        unsafe {
            let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let coll = en.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
            let mut out = Vec::new();
            for i in 0..coll.GetCount()? {
                let dev = coll.Item(i)?;
                let id = dev.GetId()?;
                let name = id.to_string().unwrap_or_default();
                CoTaskMemFree(Some(id.0 as *const _));
                out.push((name, dev));
            }
            Ok(out)
        }
    }

    /// La sesión «Sonidos del sistema» de una salida, si Windows ya la creó.
    fn system_session(dev: &IMMDevice) -> windows::core::Result<Option<ISimpleAudioVolume>> {
        unsafe {
            let mgr: IAudioSessionManager2 = dev.Activate(CLSCTX_ALL, None)?;
            let list = mgr.GetSessionEnumerator()?;
            for j in 0..list.GetCount()? {
                let ctl = list.GetSession(j)?;
                let ctl2: IAudioSessionControl2 = ctl.cast()?;
                if ctl2.IsSystemSoundsSession().0 == 0 {
                    return Ok(Some(ctl.cast()?));
                }
            }
            Ok(None)
        }
    }

    impl super::Backend for super::SystemBackend {
        fn sessions(&mut self) -> Result<Vec<(String, bool)>, String> {
            let mut out = Vec::new();
            for (id, dev) in devices().map_err(|e| format!("no se pudo leer el mezclador de Windows: {e}"))? {
                if let Ok(Some(v)) = system_session(&dev) {
                    let muted = unsafe { v.GetMute() }.map(|b| b.as_bool()).unwrap_or(false);
                    out.push((id, muted));
                }
            }
            Ok(out)
        }

        fn set_muted(&mut self, device: &str, muted: bool) -> Result<(), String> {
            let devs = devices().map_err(|e| e.to_string())?;
            let Some((_, dev)) = devs.iter().find(|(id, _)| id == device) else { return Ok(()) };
            match system_session(dev).map_err(|e| e.to_string())? {
                Some(v) => unsafe { v.SetMute(muted, std::ptr::null()) }.map_err(|e| format!("no se pudo silenciar: {e}")),
                None => Ok(()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// Mezclador falso: salidas con su estado de silencio, compartido para mirarlo desde la prueba.
    #[derive(Clone, Default)]
    struct Fake(Arc<Mutex<Vec<(String, bool)>>>);

    impl Backend for Fake {
        fn sessions(&mut self) -> Result<Vec<(String, bool)>, String> {
            Ok(self.0.lock().unwrap().clone())
        }
        fn set_muted(&mut self, device: &str, muted: bool) -> Result<(), String> {
            for s in self.0.lock().unwrap().iter_mut() {
                if s.0 == device {
                    s.1 = muted;
                }
            }
            Ok(())
        }
    }

    fn fake(v: &[(&str, bool)]) -> Fake {
        Fake(Arc::new(Mutex::new(v.iter().map(|(d, m)| (d.to_string(), *m)).collect())))
    }

    #[test]
    fn mutes_what_was_sounding_and_gives_back_only_that_leaving_what_the_operator_had_muted() {
        let mut f = fake(&[("parlantes", false), ("hdmi", true)]);
        let mut ours = BTreeSet::new();
        assert_eq!(apply(&mut f, true, &mut ours), Ok(1));
        assert_eq!(f.sessions().unwrap(), vec![("parlantes".into(), true), ("hdmi".into(), true)]);
        assert_eq!(apply(&mut f, false, &mut ours), Ok(0));
        assert_eq!(f.sessions().unwrap(), vec![("parlantes".into(), false), ("hdmi".into(), true)], "el HDMI ya estaba silenciado: queda así");
    }

    #[test]
    fn a_card_plugged_in_later_is_muted_on_the_next_pass_and_a_vanished_one_is_forgotten() {
        let mut f = fake(&[("parlantes", false)]);
        let mut ours = BTreeSet::new();
        apply(&mut f, true, &mut ours).unwrap();
        f.0.lock().unwrap().push(("placa usb".into(), false));
        assert_eq!(apply(&mut f, true, &mut ours), Ok(2));
        f.0.lock().unwrap().retain(|s| s.0 != "placa usb");
        assert_eq!(apply(&mut f, false, &mut ours), Ok(0));
        assert!(ours.is_empty());
    }

    #[test]
    fn the_keeper_restores_on_close_and_a_crash_is_repaired_on_the_next_start() {
        let dir = std::env::temp_dir().join(format!("qlab-syssnd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let marker = marker_path(&dir.join("audio.json"));
        let mut f = fake(&[("parlantes", false)]);
        let k = Keeper::start(Box::new(f.clone()), Some(marker.clone()), true);
        let t0 = std::time::Instant::now();
        while k.status().muted_sessions != 1 {
            assert!(t0.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(std::fs::read_to_string(&marker).unwrap().trim(), "parlantes", "anotado por si el programa se cierra de golpe");
        drop(k); // cerrar TeatroCue
        assert_eq!(f.sessions().unwrap(), vec![("parlantes".into(), false)], "al cerrar vuelve a sonar");
        assert!(!marker.exists());

        // Cierre de golpe: quedó silenciado y anotado. Al volver a abrir con la opción apagada, se devuelve.
        f.0.lock().unwrap()[0].1 = true;
        std::fs::write(&marker, "parlantes\n").unwrap();
        let k = Keeper::start(Box::new(f.clone()), Some(marker.clone()), false);
        let t0 = std::time::Instant::now();
        while f.sessions().unwrap()[0].1 {
            assert!(t0.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(5));
        }
        drop(k);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
