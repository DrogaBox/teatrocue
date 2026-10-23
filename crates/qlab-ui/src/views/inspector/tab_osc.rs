//! Pestaña OSC: destino y mensaje de una cue de red.

use eframe::egui::{self, RichText, Ui};
use qlab_protocol::show_format::CueBody;

use super::{labeled, time_drag, Draft};
use crate::theme;
use crate::widgets;

pub fn show(ui: &mut Ui, d: &mut Draft) {
    widgets::section(ui, "Mensaje OSC", |ui| {
        if let CueBody::Osc(o) = &mut d.node.body {
            labeled(ui, "Destino", |ui| ui.add(egui::TextEdit::singleline(&mut o.destination).desired_width(280.0).hint_text("192.168.0.20:8000").id_salt("osc_destination")));
            labeled(ui, "Mensaje", |ui| ui.add(egui::TextEdit::singleline(&mut o.message).desired_width(420.0).hint_text("/luces/escena 3 0.5 \"acto uno\"").id_salt("osc_message")));
            if o.destination.trim().is_empty() || !o.message.trim_start().starts_with('/') {
                ui.label(RichText::new("Indique el destino (equipo:puerto) y un mensaje que empiece con /").small().color(theme::AMBER));
            }
            ui.label(RichText::new("Los números enteros y decimales se envían como números; lo demás como texto (entre comillas si lleva espacios).").small().color(theme::TEXT_DIM));
        }
        labeled(ui, "Duración", |ui| time_drag(ui, &mut d.node.basics.action_duration, 0.0..=86_400.0));
    });
}
