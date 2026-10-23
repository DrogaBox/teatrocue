//! Pestaña Basics: número, nombre, archivo/destino, tiempos, continuación, flags y notas.

use eframe::egui::{self, RichText, Ui};
use qlab_protocol::cue_dto::{ContinueMode, CueColor, MediaTarget};
use qlab_protocol::show_format::{CueBody, GroupModeDto};

use super::{labeled, text_row, time_drag, Draft, InspectorOutput, TabCtx};
use crate::theme;
use crate::widgets;

pub fn continue_label(m: ContinueMode) -> &'static str {
    match m {
        ContinueMode::DoNotContinue => "No continuar",
        ContinueMode::AutoContinue => "Auto-continue",
        ContinueMode::AutoFollow => "Auto-follow",
    }
}

pub fn color_label(c: CueColor) -> &'static str {
    match c {
        CueColor::None => "Sin color",
        CueColor::Red => "Rojo",
        CueColor::Orange => "Naranja",
        CueColor::Green => "Verde",
        CueColor::Blue => "Azul",
        CueColor::Purple => "Morado",
    }
}

pub fn group_mode_label(m: GroupModeDto) -> &'static str {
    match m {
        GroupModeDto::Timeline => "Timeline (todos, con sus pre-waits)",
        GroupModeDto::FireFirstGoToNext => "Primero y pasar a la siguiente",
        GroupModeDto::FireAllSimultaneously => "Todos a la vez",
    }
}

pub fn show(ui: &mut Ui, d: &mut Draft, cx: &mut TabCtx, out: &mut InspectorOutput) {
    let is_audio = d.node.body.audio().is_some(); // audio, o el archivo de un vídeo
    let is_fade = matches!(d.node.body, CueBody::Fade(_));
    let has_duration = is_fade || matches!(d.node.body, CueBody::Memo);

    widgets::section(ui, "Identificación", |ui| {
        ui.horizontal(|ui| {
            text_row(ui, "Número", &mut d.node.basics.number, 70.0, "1");
            let hint = if d.node.basics.default_name.is_empty() { "nombre de la cue" } else { d.node.basics.default_name.as_str() };
            let hint = hint.to_string();
            text_row(ui, "Nombre", &mut d.node.basics.name, 360.0, &hint);
        });
        if is_audio {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Archivo").color(theme::TEXT_DIM));
                let mut path = d.node.basics.file_target.as_ref().map(|t| t.absolute_path.clone()).unwrap_or_default();
                let missing = path.is_empty() || !std::path::Path::new(&path).is_file();
                let r = ui.add(egui::TextEdit::singleline(&mut path).desired_width(420.0).hint_text("ruta del archivo de audio").text_color(if missing { theme::RED } else { theme::TEXT }));
                if r.changed() {
                    d.node.basics.file_target = if path.is_empty() { None } else { Some(MediaTarget { absolute_path: path.clone(), ..d.node.basics.file_target.clone().unwrap_or_default() }) };
                }
                if ui.button("Examinar…").on_hover_text("Elegir un archivo de audio (.wav, .flac, .mp3, .ogg)").clicked() {
                    out.pick_audio = true;
                }
                if missing && !path.is_empty() {
                    ui.label(RichText::new("no existe").small().color(theme::RED));
                }
            });
        }
        if is_fade {
            labeled(ui, "Destino", |ui| super::tab_fade::target_picker(ui, &mut d.node.basics, cx));
        }
        if let CueBody::Group(g) = &mut d.node.body {
            labeled(ui, "Modo", |ui| {
                egui::ComboBox::from_id_salt("group_mode").selected_text(group_mode_label(g.mode)).width(320.0).show_ui(ui, |ui| {
                    for m in [GroupModeDto::Timeline, GroupModeDto::FireFirstGoToNext, GroupModeDto::FireAllSimultaneously] {
                        ui.selectable_value(&mut g.mode, m, group_mode_label(m));
                    }
                });
            });
        }
    });

    widgets::section(ui, "Tiempos", |ui| {
        ui.horizontal(|ui| {
            labeled(ui, "Pre-Wait", |ui| time_drag(ui, &mut d.node.basics.pre_wait, 0.0..=86_400.0));
            if has_duration {
                labeled(ui, "Duración", |ui| time_drag(ui, &mut d.node.basics.action_duration, 0.0..=86_400.0));
            }
            let post_enabled = d.node.basics.continue_mode == ContinueMode::AutoContinue;
            ui.add_enabled_ui(post_enabled, |ui| {
                labeled(ui, "Post-Wait", |ui| time_drag(ui, &mut d.node.basics.post_wait, 0.0..=86_400.0)).on_disabled_hover_text("Solo cuenta con Auto-continue");
            });
            labeled(ui, "Continuar", |ui| {
                egui::ComboBox::from_id_salt("continue_mode").selected_text(continue_label(d.node.basics.continue_mode)).width(130.0).show_ui(ui, |ui| {
                    for m in [ContinueMode::DoNotContinue, ContinueMode::AutoContinue, ContinueMode::AutoFollow] {
                        ui.selectable_value(&mut d.node.basics.continue_mode, m, continue_label(m));
                    }
                });
            });
        });
    });

    widgets::section(ui, "Estado", |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.checkbox(&mut d.node.basics.armed, "Armada");
            ui.checkbox(&mut d.node.basics.flagged, "Flag");
            ui.add_enabled(!d.node.basics.armed, egui::Checkbox::new(&mut d.node.basics.skip_if_disarmed, "Saltar si está desarmada"));
            ui.checkbox(&mut d.node.basics.auto_load, "Cargar automáticamente");
            ui.checkbox(&mut d.node.dead, "Omitir (muerta)");
            labeled(ui, "Color", |ui| {
                egui::ComboBox::from_id_salt("cue_color").selected_text(color_label(d.node.basics.color)).width(100.0).show_ui(ui, |ui| {
                    for c in [CueColor::None, CueColor::Red, CueColor::Orange, CueColor::Green, CueColor::Blue, CueColor::Purple] {
                        ui.selectable_value(&mut d.node.basics.color, c, color_label(c));
                    }
                });
            });
        });
    });

    widgets::section(ui, "Notas", |ui| {
        ui.add(egui::TextEdit::multiline(&mut d.node.basics.notes).desired_rows(3).desired_width(f32::INFINITY).hint_text("notas de la cue"));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_cover_every_variant() {
        for m in [ContinueMode::DoNotContinue, ContinueMode::AutoContinue, ContinueMode::AutoFollow] {
            assert!(!continue_label(m).is_empty());
        }
        for c in [CueColor::None, CueColor::Red, CueColor::Orange, CueColor::Green, CueColor::Blue, CueColor::Purple] {
            assert!(!color_label(c).is_empty());
        }
        for m in [GroupModeDto::Timeline, GroupModeDto::FireFirstGoToNext, GroupModeDto::FireAllSimultaneously] {
            assert!(!group_mode_label(m).is_empty());
        }
    }
}
