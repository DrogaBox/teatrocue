//! Pestaña Fade: todo lo de un `FadeCue` en un solo sitio — cue destino, duración, curva y las celdas
//! de la matriz de niveles de la cue destino con su nivel final en dB.

use eframe::egui::{self, RichText, Ui};
use qlab_protocol::cue_dto::{CueBasicsData, CueId};
use qlab_protocol::show_format::CueBody;

use super::{labeled, tab_levels, time_drag, Draft, TabCtx};
use crate::theme;
use crate::widgets;

/// Desplegable con las cues de audio que un fade puede modular.
pub fn target_picker(ui: &mut Ui, basics: &mut CueBasicsData, cx: &TabCtx) {
    let name_of = |id: Option<CueId>| id.and_then(|i| cx.model.summary(i.0)).map_or("— ninguno —".to_string(), |c| format!("{}  {}", c.number, crate::format::ellipsize(&c.name, 36)));
    egui::ComboBox::from_id_salt("fade_target").selected_text(name_of(basics.cue_target_id)).width(320.0).show_ui(ui, |ui| {
        ui.selectable_value(&mut basics.cue_target_id, None, "— ninguno —");
        for c in cx.model.summaries.iter().filter(|c| c.kind == "audio") {
            ui.selectable_value(&mut basics.cue_target_id, Some(CueId(c.id)), format!("{}  {}", c.number, crate::format::ellipsize(&c.name, 36)));
        }
    });
}

pub fn show(ui: &mut Ui, d: &mut Draft, cx: &mut TabCtx) {
    widgets::section(ui, "Destino y duración", |ui| {
        ui.horizontal_wrapped(|ui| {
            labeled(ui, "Cue destino", |ui| target_picker(ui, &mut d.node.basics, cx));
            labeled(ui, "Duración", |ui| time_drag(ui, &mut d.node.basics.action_duration, 0.0..=86_400.0));
        });
        if d.node.basics.cue_target_id.is_none() {
            ui.label(RichText::new("Elige la cue de audio que se va a modular").small().color(theme::AMBER));
        }
    });
    if let CueBody::Fade(f) = &mut d.node.body {
        tab_levels::fade_editor(ui, f, cx);
    }
}
