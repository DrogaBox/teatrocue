//! Diálogos nativos del sistema (`rfd`) en un hilo aparte: la interfaz nunca se congela esperando al
//! usuario. Un diálogo a la vez; el resultado vuelve por un canal y la UI lo recoge en `poll`.
//!
//! Con `TEATROCUE_BUILTIN_PICKER=1` (o en las pruebas) se desactivan y la aplicación usa su selector
//! propio, útil en máquinas sin portal de escritorio.

use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DialogKind {
    OpenShow,
    SaveShowAs,
    PickAudio,
    /// Carpeta de destino para empaquetar un show.
    PickBundleFolder,
    /// Archivo HTML donde exportar el guion técnico.
    ExportCueSheet,
    /// Archivo de audio de un pad de la botonera.
    PickCartAudio,
    /// Varios archivos de audio o vídeo para convertir.
    PickConvertFiles,
    /// Carpeta de destino del convertidor o de la copia de CD.
    PickToolFolder,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DialogOutcome {
    pub kind: DialogKind,
    /// `None` si el usuario canceló.
    pub path: Option<PathBuf>,
    /// Los demás archivos cuando se eligieron varios (el primero va en `path`).
    pub more: Vec<PathBuf>,
}

/// Extensión propia de los shows y las que también se aceptan (JSON plano y el nombre anterior).
pub const SHOW_EXTENSION: &str = "teatroshow";
pub const SHOW_EXTENSIONS: [&str; 3] = ["teatroshow", "json", "qlabshow"];
pub use crate::media::AUDIO_EXTENSIONS;

pub struct NativeDialogs {
    tx: Sender<DialogOutcome>,
    rx: Receiver<DialogOutcome>,
    busy: Option<DialogKind>,
    wake: Arc<dyn Fn() + Send + Sync>,
    enabled: bool,
}

impl NativeDialogs {
    pub fn new(wake: impl Fn() + Send + Sync + 'static) -> Self {
        let (tx, rx) = channel();
        let enabled = !cfg!(test) && std::env::var_os("TEATROCUE_BUILTIN_PICKER").is_none();
        Self { tx, rx, busy: None, wake: Arc::new(wake), enabled }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn is_busy(&self) -> bool {
        self.busy.is_some()
    }

    /// Abre un diálogo en un hilo. `false` si ya hay uno abierto o no hay hilos disponibles.
    pub fn open(&mut self, kind: DialogKind, start_dir: Option<PathBuf>, file_name: Option<String>) -> bool {
        if self.busy.is_some() {
            return false;
        }
        let (tx, wake) = (self.tx.clone(), Arc::clone(&self.wake));
        let spawned = std::thread::Builder::new().name("qlab-native-dialog".into()).spawn(move || {
            let mut files = run_dialog(kind, start_dir, file_name).into_iter();
            let path = files.next();
            let _ = tx.send(DialogOutcome { kind, path, more: files.collect() });
            wake();
        });
        if spawned.is_ok() {
            self.busy = Some(kind);
        }
        spawned.is_ok()
    }

    /// Resultado de un diálogo ya cerrado, si lo hay.
    pub fn poll(&mut self) -> Option<DialogOutcome> {
        let out = self.rx.try_recv().ok()?;
        self.busy = None;
        Some(out)
    }

    /// Entrega un resultado como si el usuario hubiera elegido (pruebas).
    #[cfg(test)]
    pub fn deliver(&mut self, outcome: DialogOutcome) {
        self.busy = Some(outcome.kind);
        let _ = self.tx.send(outcome);
    }
}

fn run_dialog(kind: DialogKind, dir: Option<PathBuf>, file_name: Option<String>) -> Vec<PathBuf> {
    let mut d = rfd::FileDialog::new();
    if let Some(dir) = dir.filter(|p| p.is_dir()) {
        d = d.set_directory(dir);
    }
    match kind {
        DialogKind::PickConvertFiles => {
            let all: Vec<&str> = AUDIO_EXTENSIONS.iter().chain(crate::media::VIDEO_EXTENSIONS.iter()).copied().chain(["m4a", "aac", "wma", "aiff", "aif", "opus"]).collect();
            d.set_title("Elegir los archivos a convertir").add_filter("Audio y vídeo", &all).pick_files().unwrap_or_default()
        }
        DialogKind::PickToolFolder => d.set_title("Carpeta de destino").pick_folder().into_iter().collect(),
        other => run_dialog_one(other, d, file_name).into_iter().collect(),
    }
}

fn run_dialog_one(kind: DialogKind, d: rfd::FileDialog, file_name: Option<String>) -> Option<PathBuf> {
    match kind {
        DialogKind::PickConvertFiles | DialogKind::PickToolFolder => None,
        DialogKind::OpenShow => d.set_title("Abrir show").add_filter("Show de TeatroCue (.teatroshow, .json)", &SHOW_EXTENSIONS).pick_file(),
        DialogKind::SaveShowAs => {
            let mut d = d.set_title("Guardar show como").add_filter("Show de TeatroCue (.teatroshow, .json)", &SHOW_EXTENSIONS);
            if let Some(n) = file_name {
                d = d.set_file_name(n);
            }
            d.save_file().map(with_show_extension)
        }
        DialogKind::ExportCueSheet => {
            let mut d = d.set_title("Exportar guion técnico").add_filter("Página web (.html)", &["html"]);
            if let Some(n) = file_name {
                d = d.set_file_name(n);
            }
            d.save_file().map(|mut p| {
                if p.extension().is_none() {
                    p.set_extension("html");
                }
                p
            })
        }
        DialogKind::PickBundleFolder => d.set_title("Carpeta donde empaquetar el show").pick_folder(),
        DialogKind::PickCartAudio => d.set_title("Elegir el efecto del pad").add_filter("Audio (.wav .flac .mp3 .ogg)", &AUDIO_EXTENSIONS).pick_file(),
        DialogKind::PickAudio => {
            let all: Vec<&str> = AUDIO_EXTENSIONS.iter().chain(crate::media::VIDEO_EXTENSIONS.iter()).copied().collect();
            d.set_title("Elegir archivo de audio o vídeo")
                .add_filter("Audio y vídeo (.wav .flac .mp3 .ogg .mp4 .mov .mkv)", &all)
                .add_filter("Audio (.wav .flac .mp3 .ogg)", &AUDIO_EXTENSIONS)
                .add_filter("Vídeo (.mp4 .mov .mkv)", &crate::media::VIDEO_EXTENSIONS)
                .pick_file()
        }
    }
}

/// Añade `.teatroshow` si el usuario no escribió ninguna extensión de show.
pub fn with_show_extension(mut p: PathBuf) -> PathBuf {
    let ok = p.extension().and_then(|e| e.to_str()).is_some_and(|e| SHOW_EXTENSIONS.iter().any(|x| x.eq_ignore_ascii_case(e)));
    if !ok {
        let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "show".into());
        p.set_file_name(format!("{name}.{SHOW_EXTENSION}"));
    }
    p
}

/// Nombre de show a partir de la ruta: el nombre del archivo sin extensión.
pub fn show_name_of(path: &std::path::Path) -> String {
    path.file_stem().map(|s| s.to_string_lossy().into_owned()).filter(|s| !s.is_empty()).unwrap_or_else(|| "Sin título".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn show_extension_is_added_only_when_missing() {
        assert_eq!(with_show_extension("/tmp/a".into()), PathBuf::from("/tmp/a.teatroshow"));
        assert_eq!(with_show_extension("/tmp/a.txt".into()), PathBuf::from("/tmp/a.txt.teatroshow"));
        assert_eq!(with_show_extension("/tmp/a.json".into()), PathBuf::from("/tmp/a.json"), "el JSON plano sigue siendo válido");
        assert_eq!(with_show_extension("/tmp/a.teatroshow".into()), PathBuf::from("/tmp/a.teatroshow"));
        assert_eq!(with_show_extension("/tmp/a.QLABSHOW".into()), PathBuf::from("/tmp/a.QLABSHOW"));
    }

    #[test]
    fn show_name_comes_from_the_file_stem() {
        assert_eq!(show_name_of(std::path::Path::new("/x/Acto 1.qlabshow")), "Acto 1");
        assert_eq!(show_name_of(std::path::Path::new("/")), "Sin título");
    }

    #[test]
    fn delivered_outcomes_are_polled_once_and_free_the_slot() {
        let mut d = NativeDialogs::new(|| {});
        assert!(!d.enabled(), "en pruebas no se abren diálogos reales");
        d.deliver(DialogOutcome { kind: DialogKind::PickAudio, path: Some("/a.wav".into()), more: Vec::new() });
        assert!(d.is_busy());
        assert_eq!(d.poll().unwrap().path, Some(PathBuf::from("/a.wav")));
        assert!(d.poll().is_none() && !d.is_busy());
    }
}
