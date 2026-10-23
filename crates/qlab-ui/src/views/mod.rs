//! Vistas de la ventana principal. Cada una dibuja a partir de datos de solo lectura y devuelve
//! [`Action`]s; la aplicación las ejecuta después, así que ninguna vista toca el enlace con el daemon.

pub mod audio_settings;
pub mod audio_tools;
pub mod carts;
pub mod clock;
pub mod cue_table;
pub mod help_modal;
pub mod inspector;
pub mod license_lock;
pub mod memories;
pub mod preferences;
pub mod projection_viewport;
pub mod reactivation;
pub mod reconnect;
pub mod top_bar;

use crate::link::HelloInfo;
use crate::model::{Telemetry, UiModel};

/// Lo que el usuario pide con un gesto en la interfaz.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    Go,
    /// Se cambió una preferencia desde el menú o la barra: guardarla y aplicarla.
    PrefsChanged,
    /// Parada suave; una segunda pulsación seguida es una parada dura.
    Panic,
    HardStopAll,
    StopAll,
    PauseAll,
    ResumeAll,
    SelectCue(u64),
    SetPlayhead(u64),
    /// Doble clic: coloca el playhead y dispara.
    FireCue(u64),
    ToggleGroup(u64),
    /// Abrir show (diálogo nativo o selector propio).
    OpenShowPicker,
    PickAudioFile,
    NewShow,
    SaveShow,
    SaveShowAs,
    OpenRecent(std::path::PathBuf),
    Undo,
    Duplicate,
    /// Volumen maestro en dB (−60 o menos = silencio).
    SetMasterDb(f32),
    Redo,
    /// Archivo → Empaquetar show…: copia los medios y guarda el show en una carpeta autocontenida.
    BundleShow,
    /// Archivo > Importar audio (convertir / copiar de CD).
    OpenAudioTools,
    /// Archivo → Exportar Guion Técnico…: planilla HTML imprimible con toda la obra.
    ExportCueSheet,
    /// Bloqueo de función (Ctrl+L).
    ToggleLock,
    /// Abre o cierra la ventana secundaria de proyección.
    OpenProjector,
    /// Cue nueva después de la seleccionada: `audio`, `fade` o `group`.
    AddCue(&'static str),
    /// Celda «Sig.»: pasa al siguiente modo de continuación.
    CycleContinue(u64),
    DeleteSelected,
    /// Sube (−1) o baja (+1) la cue seleccionada.
    MoveSelected(i32),
    RetryConnection,
    StartEmbedded { with_audio: bool },
    LaunchDaemonProcess,
    Quit,
}

/// Datos de solo lectura que reciben las vistas.
pub struct ViewCtx<'a> {
    pub model: &'a UiModel,
    pub telemetry: &'a Telemetry,
    pub hello: Option<&'a HelloInfo>,
    pub safe_mute: bool,
    /// Volumen maestro actual en dB.
    pub master_db: f32,
    /// Se debe desplazar la tabla hasta la cue seleccionada.
    pub scroll_to_selected: bool,
}
