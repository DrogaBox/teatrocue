//! Pestaña Control: qué hace la cue sobre su cue destino (Stop, Pause, Resume o Devamp).

use eframe::egui::{self, RichText, Ui};
use qlab_protocol::cue_dto::CueId;
use qlab_protocol::show_format::{ControlActionDto, CueBody};

use super::{labeled, time_drag, Draft, TabCtx};
use crate::theme;
use crate::widgets;

pub fn show(ui: &mut Ui, d: &mut Draft, cx: &mut TabCtx) {
    widgets::section(ui, "Destino y acción", |ui| {
        labeled(ui, "Cue destino", |ui| {
            let basics = &mut d.node.basics;
            let name_of = |id: Option<CueId>| id.and_then(|i| cx.model.summary(i.0)).map_or("— ninguno —".to_string(), |c| format!("{}  {}", c.number, crate::format::ellipsize(&c.name, 36)));
            egui::ComboBox::from_id_salt("control_target").selected_text(name_of(basics.cue_target_id)).width(320.0).show_ui(ui, |ui| {
                ui.selectable_value(&mut basics.cue_target_id, None, "— ninguno —");
                for c in cx.model.summaries.iter().filter(|c| c.id != basics.unique_id.0) {
                    ui.selectable_value(&mut basics.cue_target_id, Some(CueId(c.id)), format!("{}  {}", c.number, crate::format::ellipsize(&c.name, 36)));
                }
            });
        });
        let mut action = None;
        if let CueBody::Control(c) = &mut d.node.body {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Acción").color(theme::TEXT_DIM));
                for a in ControlActionDto::ALL {
                    ui.selectable_value(&mut c.action, a, a.label());
                }
            });
            action = Some(c.action);
        }
        if action == Some(ControlActionDto::Stop) {
            labeled(ui, "Fundido de salida", |ui| time_drag(ui, &mut d.node.basics.action_duration, 0.0..=86_400.0));
            ui.label(RichText::new("0 = corte inmediato. La cue de control dura lo mismo que el fundido.").small().color(theme::TEXT_DIM));
        }
        if d.node.basics.cue_target_id.is_none() {
            ui.label(RichText::new("Elige la cue sobre la que actúa; sin destino no hace nada").small().color(theme::AMBER));
        }
    });
}
