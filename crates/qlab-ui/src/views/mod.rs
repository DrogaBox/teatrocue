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

use qlab_protocol::matrix::{AudioLevelMatrixData, OutputRoute};

/// Rótulo del selector de salida de cues y pads.
pub const ROUTE_LABEL: &str = "Sale por";
/// Lo que muestra el selector cuando la matriz de la cue no es ninguna de sus rutas.
pub const CUSTOM_ROUTE: &str = "Personalizada (ver la matriz)";

/// Las salidas a las que una cue o un pad pueden mandar sonido: la principal y las placas adicionales configuradas.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Outputs {
    /// Canales de la placa principal.
    pub main: usize,
    /// Placa de cada salida adicional («Salida 2»…); vacía = sin placa.
    pub extras: Vec<String>,
}

impl Outputs {
    /// «Principal · solo ficha 1», «Salida 2 (Realtek) · las dos fichas»…
    pub fn label(&self, r: OutputRoute) -> String {
        let label = r.label();
        if r.output == 0 {
            return label;
        }
        let place = format!("Salida {}", r.output + 1);
        let card = match self.extras.get(r.output as usize - 1).map(|n| n.trim()).filter(|n| !n.is_empty()) {
            Some(n) => format!("{place} ({n})"),
            None => format!("{place} (sin placa: sale por la principal)"),
        };
        label.replacen(&place, &card, 1)
    }

    /// Cabecera de la columna `j` (desde 1) de una matriz: «Out 2», o «S2 · 1» en una salida adicional.
    pub fn column(&self, j: usize) -> String {
        match j.checked_sub(self.main.max(1) + 1) {
            Some(k) if k / 2 < self.extras.len() => format!("S{} · {}", k / 2 + 2, k % 2 + 1),
            _ => format!("Out {j}"),
        }
    }

    /// Selector «Sale por»: muestra la ruta de la matriz y, al elegir otra, la reescribe. `true` si cambió.
    pub fn picker(&self, ui: &mut eframe::egui::Ui, id: eframe::egui::Id, current: Option<OutputRoute>, mut pick: impl FnMut(OutputRoute)) -> bool {
        let mut changed = false;
        ui.horizontal(|ui| {
            ui.label(eframe::egui::RichText::new(ROUTE_LABEL).strong());
            let text = current.map_or_else(|| CUSTOM_ROUTE.to_string(), |r| self.label(r));
            eframe::egui::ComboBox::from_id_salt(id).selected_text(text).width(340.0).show_ui(ui, |ui| {
                for r in OutputRoute::all(self.extras.len()) {
                    if ui.selectable_label(current == Some(r), self.label(r)).clicked() && current != Some(r) {
                        pick(r);
                        changed = true;
                    }
                }
            });
        });
        changed
    }

    /// La ruta de la matriz de una cue (`None`: tocada a mano).
    pub fn route_of(&self, m: &AudioLevelMatrixData) -> Option<OutputRoute> {
        m.route(self.main, self.extras.len())
    }
}

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
    SetMasterMono(bool),
    Redo,
    /// Archivo → Empaquetar show…: copia los medios y guarda el show en una carpeta autocontenida.
    BundleShow,
    /// Archivo → Buscar archivos faltantes…: revincular los medios que no se encuentran desde una carpeta.
    RelinkMedia,
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
    /// Cartel SIN AUDIO: volver a abrir la placa ya.
    RetryAudio,
    /// Cartel SIN AUDIO: abrir la configuración de audio para elegir otra salida.
    ChooseAudioOutput,
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
    /// Maestro en mono.
    pub master_mono: bool,
    /// Se debe desplazar la tabla hasta la cue seleccionada.
    pub scroll_to_selected: bool,
}
