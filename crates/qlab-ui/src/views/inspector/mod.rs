//! Panel inferior del Inspector: las 4 pestañas de audio de QLab (Basics, Triggers, Time & Loops,
//! Audio Levels) sobre un borrador editable de la cue seleccionada.
//!
//! El borrador (`Draft`) es una copia del `CueNodeDto` del daemon. Las pestañas lo modifican; la
//! aplicación lo envía al daemon (`UpdateCue`) cuando hay cambios y el usuario no está tecleando ni
//! arrastrando. Los arrastres de fader van además en vivo (`FaderDrag`) para oírlos al momento.

pub mod tab_basics;
pub mod tab_control;
pub mod tab_osc;
pub mod tab_eq;
pub mod tab_fade;
pub mod tab_levels;
pub mod tab_time_loops;
pub mod tab_triggers;
pub mod tab_video;

use eframe::egui::{self, RichText, Ui};
use qlab_protocol::show_format::{CueBody, CueNodeDto};

use crate::model::{Telemetry, UiModel};
use crate::theme;
use crate::waveform::WaveformCache;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Tab {
    #[default]
    Basics,
    /// Solo en las cues de vídeo: vista previa, ajuste e información del archivo.
    Video,
    /// Solo en las cues de fade: destino, duración, curva y niveles.
    Fade,
    /// Solo en las cues de control: destino y acción.
    Control,
    /// Solo en las cues de red: destino y mensaje OSC.
    Osc,
    Triggers,
    TimeLoops,
    Levels,
    /// Solo en las cues de audio y vídeo: ecualizador paramétrico de 4 bandas.
    Eq,
}

impl Tab {
    pub const ALL: [Tab; 9] = [Tab::Basics, Tab::Video, Tab::Fade, Tab::Control, Tab::Osc, Tab::Triggers, Tab::TimeLoops, Tab::Levels, Tab::Eq];

    pub fn title(self) -> &'static str {
        match self {
            Tab::Basics => "Basics",
            Tab::Video => "Video",
            Tab::Fade => "Fade",
            Tab::Control => "Control",
            Tab::Osc => "OSC",
            Tab::Triggers => "Triggers",
            Tab::TimeLoops => "Time & Loops",
            Tab::Levels => "Audio Levels",
            Tab::Eq => "EQ",
        }
    }

    /// Qué pestañas tienen sentido para el tipo de cue.
    pub fn available_for(self, node: &CueNodeDto) -> bool {
        match (&node.body, self) {
            (_, Tab::Basics | Tab::Triggers) => true,
            (CueBody::Audio(_) | CueBody::Video(_), Tab::TimeLoops | Tab::Levels | Tab::Eq) => true,
            (CueBody::Video(_), Tab::Video) => true,
            (CueBody::Fade(_), Tab::Fade) => true,
            (CueBody::Control(_), Tab::Control) => true,
            (CueBody::Osc(_), Tab::Osc) => true,
            _ => false,
        }
    }
}

/// Tiempos y bucles de una cue con archivo (audio o vídeo).
pub fn time_loops_ref(node: &CueNodeDto) -> Option<&qlab_protocol::cue_dto::AudioTimeAndLoopsData> {
    node.body.audio().map(|a| &a.time_loops)
}

pub fn time_loops_mut(node: &mut CueNodeDto) -> Option<&mut qlab_protocol::cue_dto::AudioTimeAndLoopsData> {
    node.body.audio_mut().map(|a| &mut a.time_loops)
}

/// Copia editable de una cue y la última versión que confirmó el daemon.
#[derive(Clone, Debug)]
pub struct Draft {
    pub node: CueNodeDto,
    pub baseline: CueNodeDto,
    pub state: String,
}

impl Draft {
    pub fn new(node: CueNodeDto, state: String) -> Self {
        Self { baseline: node.clone(), node, state }
    }

    pub fn dirty(&self) -> bool {
        self.node != self.baseline
    }

    pub fn id(&self) -> u64 {
        self.node.basics.unique_id.0
    }
}

#[derive(Default)]
pub struct InspectorState {
    pub tab: Tab,
    pub draft: Option<Draft>,
    pub levels_ui: tab_levels::LevelsUi,
    /// Última versión enviada al daemon y aún sin confirmar (evita reenviarla en cada fotograma).
    pub in_flight: Option<CueNodeDto>,
    /// Cue cuyo Inspector se dibujó por última vez (para elegir la pestaña inicial de un fade).
    pub shown: Option<u64>,
    /// La cue abierta suena ahora en la audición (PFL): el botón se ve pulsado.
    pub pfl_active: bool,
    /// Bloqueo de función: el contenido de las pestañas no se puede editar.
    pub locked: bool,
}

/// Lo que las pestañas necesitan además del borrador.
pub struct TabCtx<'a> {
    pub model: &'a UiModel,
    pub telemetry: &'a Telemetry,
    pub waveforms: &'a mut WaveformCache,
    pub device_channels: usize,
    /// Pre-roll vigente para los saltos a memoria (0, 2 o 4 s).
    pub preroll: u8,
    pub video: &'a mut crate::video::VideoHub,
    /// Estado de la copia reparada del archivo de la cue abierta (si se pidió una).
    pub repair: Option<crate::repair::RepairState>,
    /// Tecla de «Marcar memoria» para el rótulo del botón (el operador la puede cambiar).
    pub memory_key: String,
}

/// Peticiones de las pestañas a la aplicación.
#[derive(Default, Debug, PartialEq)]
pub struct InspectorOutput {
    /// Arrastres de fader en vivo: `(cue, fila, columna, dB)`.
    pub drags: Vec<(u64, u8, u8, f32)>,
    pub pick_audio: bool,
    /// Devamp de una cue de audio atrapada en un slice infinito.
    pub devamp: Option<u64>,
    /// Abrir la ventana de proyección (botón de la pestaña Video).
    pub open_projector: bool,
    /// Botón PFL: escuchar la cue en los auriculares (o detener la audición).
    pub pfl: bool,
    /// Se eligió otro pre-roll en la pestaña Time & Loops.
    pub preroll: Option<u8>,
    /// «Crear copia reparada»: archivo original y segundo hasta el que está sano.
    pub repair: Option<(std::path::PathBuf, f64)>,
    /// «Usar la copia en esta cue».
    pub use_repaired: Option<std::path::PathBuf>,
}

pub fn show(ui: &mut Ui, st: &mut InspectorState, cx: &mut TabCtx, out: &mut InspectorOutput) {
    let Some(draft) = st.draft.as_mut() else {
        ui.vertical_centered(|ui| {
            ui.add_space(24.0);
            ui.label(RichText::new("Selecciona una cue para editarla").color(theme::TEXT_DIM));
        });
        return;
    };
    if st.shown != Some(draft.id()) {
        st.shown = Some(draft.id());
        // Al llegar a un fade se muestra su pestaña propia; al salir de uno se vuelve a Basics.
        match draft.node.body {
            CueBody::Fade(_) => st.tab = Tab::Fade,
            CueBody::Control(_) => st.tab = Tab::Control,
            CueBody::Osc(_) => st.tab = Tab::Osc,
            CueBody::Video(_) => st.tab = Tab::Video,
            _ if matches!(st.tab, Tab::Fade | Tab::Video | Tab::Control | Tab::Osc) => st.tab = Tab::Basics,
            _ => {}
        }
    }
    if !st.tab.available_for(&draft.node) {
        st.tab = Tab::Basics;
    }
    ui.horizontal(|ui| {
        for tab in Tab::ALL {
            if !tab.available_for(&draft.node) {
                continue;
            }
            let selected = st.tab == tab;
            let label = RichText::new(tab.title()).strong().color(if selected { theme::AMBER } else { theme::TEXT });
            if ui.selectable_label(selected, label).clicked() {
                st.tab = tab;
            }
        }
        ui.separator();
        let kind = match &draft.node.body {
            CueBody::Audio(_) => "audio",
            CueBody::Video(_) => "vídeo",
            CueBody::Fade(_) => "fade",
            CueBody::Control(_) => "control",
            CueBody::Osc(_) => "osc",
            CueBody::Memo => "memo",
            CueBody::Group(_) => "grupo",
        };
        let name = if draft.node.basics.name.is_empty() { "(sin nombre)" } else { draft.node.basics.name.as_str() };
        let show_devamp = st.locked && matches!(draft.node.body, CueBody::Audio(_) | CueBody::Video(_)) && cx.telemetry.running.get(&draft.id()).is_some_and(|i| i.phase.is_some());
        // Lo que va a la derecha del nombre necesita su sitio; el nombre ocupa el resto y, si no entra, se desplaza.
        let reserve = if draft.dirty() { 90.0 } else { 0.0 } + if show_devamp { 85.0 } else { 0.0 } + if matches!(draft.node.body, CueBody::Audio(_) | CueBody::Video(_)) { 55.0 } else { 0.0 };
        let width = (ui.available_width() - reserve).max(80.0);
        let font = egui::TextStyle::Body.resolve(ui.style());
        crate::widgets::marquee(ui, &format!("{} · {} · {}", draft.node.basics.number, name, kind), font, theme::TEXT_DIM, width, true);
        if draft.dirty() {
            ui.label(RichText::new("* sin guardar").small().color(theme::AMBER));
        }
        if show_devamp {
            // Con el bloqueo puesto el Devamp sigue a mano: es parte de la función.
            ui.separator();
            if ui.button("Devamp").on_hover_text("Salir del bucle al final de la pasada en curso").clicked() {
                out.devamp = Some(draft.id());
            }
        }
        if matches!(draft.node.body, CueBody::Audio(_) | CueBody::Video(_)) {
            ui.separator();
            let label = RichText::new("PFL").strong().color(if st.pfl_active { theme::AMBER } else { theme::TEXT });
            if ui.selectable_label(st.pfl_active, label).on_hover_text("Audición: escuchar esta cue solo en los auriculares, sin que salga por la sala").clicked() {
                out.pfl = true;
            }
        }
    });
    ui.separator();
    let locked = st.locked;
    egui::ScrollArea::vertical().id_salt("inspector_scroll").auto_shrink([false, false]).show(ui, |ui| ui.add_enabled_ui(!locked, |ui| match st.tab {
        Tab::Basics => tab_basics::show(ui, draft, cx, out),
        Tab::Video => tab_video::show(ui, draft, cx, out),
        Tab::Fade => tab_fade::show(ui, draft, cx),
        Tab::Control => tab_control::show(ui, draft, cx),
        Tab::Osc => tab_osc::show(ui, draft),
        Tab::Eq => tab_eq::show(ui, draft, cx.telemetry.running.get(&draft.id()).is_some_and(|i| i.phase.is_some())),
        Tab::Triggers => tab_triggers::show(ui, draft),
        Tab::TimeLoops => tab_time_loops::show(ui, draft, cx, out),
        // La matriz de niveles es ancha: scroll horizontal propio dentro del vertical.
        Tab::Levels => {
            egui::ScrollArea::horizontal().id_salt("levels_h_scroll").auto_shrink([false, true]).show(ui, |ui| tab_levels::show(ui, draft, &mut st.levels_ui, cx, out));
        }
    }));
}

// ---------------------------------------------------------------------------------------------
// Ayudas comunes de las pestañas
// ---------------------------------------------------------------------------------------------

use std::ops::RangeInclusive;

use crate::format::{fmt_time, parse_time};

/// Segundos con el formato de QLab: arrastrar o teclear (`1:30.5`).
pub fn time_drag(ui: &mut Ui, v: &mut f64, range: RangeInclusive<f64>) -> egui::Response {
    ui.add(
        egui::DragValue::new(v)
            .range(range)
            .speed(0.01)
            .custom_formatter(|n, _| fmt_time(n))
            .custom_parser(parse_time),
    )
}

pub fn labeled<R>(ui: &mut Ui, label: &str, add: impl FnOnce(&mut Ui) -> R) -> R {
    ui.horizontal(|ui| {
        ui.label(RichText::new(label).color(theme::TEXT_DIM));
        add(ui)
    })
    .inner
}

pub fn text_row(ui: &mut Ui, label: &str, text: &mut String, width: f32, hint: &str) -> egui::Response {
    labeled(ui, label, |ui| ui.add(egui::TextEdit::singleline(text).desired_width(width).hint_text(hint)))
}
