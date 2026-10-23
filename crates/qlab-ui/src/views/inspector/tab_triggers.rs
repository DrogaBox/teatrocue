//! Pestaña Triggers: fade & stop de otras cues, ducking, segundo disparo, hotkey, MIDI y reloj.
//!
//! Estos ajustes se guardan con la cue; el motor aún no ejecuta hotkeys, MIDI, reloj de pared,
//! fade/stop ni ducking de otras cues, y la pestaña lo indica.

use eframe::egui::{self, RichText, Ui};
use qlab_protocol::cue_dto::{FadeOthersScope, KeyChord, MidiCommand, MidiTriggerSpec, SecondTriggerAction};

use super::{labeled, time_drag, Draft};
use crate::theme;

pub fn scope_label(s: FadeOthersScope) -> &'static str {
    match s {
        FadeOthersScope::Disabled => "Desactivado",
        FadeOthersScope::Peers => "Hermanas (mismo grupo)",
        FadeOthersScope::CueList => "Toda la lista de cues",
        FadeOthersScope::All => "Todo el workspace",
    }
}

pub fn second_label(a: SecondTriggerAction) -> &'static str {
    match a {
        SecondTriggerAction::Nothing => "Nada",
        SecondTriggerAction::Panic => "Panic (fade suave)",
        SecondTriggerAction::Stop => "Stop (5 ms)",
        SecondTriggerAction::HardStop => "Hard stop",
        SecondTriggerAction::HardStopAndRestart => "Hard stop y reiniciar",
        SecondTriggerAction::Devamp => "Devamp",
        SecondTriggerAction::PlaylistPlaysNext => "Playlist: siguiente",
        SecondTriggerAction::PlaylistPlaysPrevious => "Playlist: anterior",
    }
}

fn midi_label(c: MidiCommand) -> &'static str {
    match c {
        MidiCommand::NoteOn => "Note On",
        MidiCommand::NoteOff => "Note Off",
        MidiCommand::ProgramChange => "Program Change",
        MidiCommand::ControlChange => "Control Change",
    }
}

const WEEKDAYS: [&str; 7] = ["Dom", "Lun", "Mar", "Mié", "Jue", "Vie", "Sáb"];

/// Tarjeta con título que ocupa todo el ancho de su columna (nunca desborda por el margen derecho).
fn card(ui: &mut Ui, title: &str, add: impl FnOnce(&mut Ui)) {
    ui.group(|ui| {
        ui.set_width(ui.available_width());
        ui.label(RichText::new(title).strong().color(theme::AMBER));
        ui.add_space(2.0);
        add(ui);
    });
    ui.add_space(4.0);
}

/// Ancho de un desplegable dentro de una columna, dejando sitio a su etiqueta.
fn combo_width(ui: &Ui) -> f32 {
    (ui.available_width() - 150.0).clamp(110.0, 240.0)
}

pub fn show(ui: &mut Ui, d: &mut Draft) {
    let t = &mut d.node.triggers;
    ui.label(RichText::new("Se guardan con la cue; el motor aún no ejecuta estos disparadores.").small().color(theme::TEXT_DIM));
    ui.add_space(2.0);

    ui.columns(2, |cols| {
        // Columna 1: qué dispara la cue.
        let ui = &mut cols[0];
        card(ui, "Hotkey", |ui| {
            ui.checkbox(&mut t.use_hotkey, "Disparar con una tecla");
            ui.add_enabled_ui(t.use_hotkey, |ui| {
                let chord = t.hotkey.get_or_insert_with(|| KeyChord { key: String::new(), modifiers: 0 });
                labeled(ui, "Tecla", |ui| ui.add(egui::TextEdit::singleline(&mut chord.key).desired_width(70.0).hint_text("A")));
                ui.horizontal_wrapped(|ui| {
                    for (bit, name) in [(1u8, "Mayús"), (2, "Ctrl"), (4, "Alt"), (8, "Cmd")] {
                        let mut on = chord.modifiers & bit != 0;
                        if ui.checkbox(&mut on, name).changed() {
                            chord.modifiers = if on { chord.modifiers | bit } else { chord.modifiers & !bit };
                        }
                    }
                });
            });
        });
        card(ui, "MIDI", |ui| {
            ui.checkbox(&mut t.use_midi, "Disparar con un mensaje MIDI");
            ui.add_enabled_ui(t.use_midi, |ui| {
                let spec = t.midi_trigger.get_or_insert(MidiTriggerSpec { command: MidiCommand::NoteOn, channel: -1, byte1: 60, byte2_min: 0, byte2_max: 127 });
                let w = combo_width(ui);
                labeled(ui, "Mensaje", |ui| {
                    egui::ComboBox::from_id_salt("midi_cmd").selected_text(midi_label(spec.command)).width(w).show_ui(ui, |ui| {
                        for c in [MidiCommand::NoteOn, MidiCommand::NoteOff, MidiCommand::ProgramChange, MidiCommand::ControlChange] {
                            ui.selectable_value(&mut spec.command, c, midi_label(c));
                        }
                    });
                });
                ui.horizontal_wrapped(|ui| {
                    ui.label("Canal");
                    let mut ch = spec.channel as i32;
                    if ui.add(egui::DragValue::new(&mut ch).range(-1..=15).custom_formatter(|v, _| if v < 0.0 { "todos".into() } else { format!("{}", v as i32 + 1) })).changed() {
                        spec.channel = ch as i8;
                    }
                    ui.label("Byte 1");
                    ui.add(egui::DragValue::new(&mut spec.byte1).range(0..=127));
                    ui.label("Byte 2");
                    ui.add(egui::DragValue::new(&mut spec.byte2_min).range(0..=127));
                    ui.label("–");
                    ui.add(egui::DragValue::new(&mut spec.byte2_max).range(0..=127));
                });
                if spec.byte2_min > spec.byte2_max {
                    spec.byte2_max = spec.byte2_min;
                }
            });
        });
        card(ui, "Reloj de pared", |ui| {
            ui.checkbox(&mut t.use_wall_clock, "Disparar a una hora");
            ui.add_enabled_ui(t.use_wall_clock, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label("Hora");
                    ui.add(egui::DragValue::new(&mut t.wall_hms.0).range(0..=23).custom_formatter(|v, _| format!("{:02}", v as u8)));
                    ui.label(":");
                    ui.add(egui::DragValue::new(&mut t.wall_hms.1).range(0..=59).custom_formatter(|v, _| format!("{:02}", v as u8)));
                    ui.label(":");
                    ui.add(egui::DragValue::new(&mut t.wall_hms.2).range(0..=59).custom_formatter(|v, _| format!("{:02}", v as u8)));
                });
                ui.horizontal_wrapped(|ui| {
                    for (i, name) in WEEKDAYS.iter().enumerate() {
                        let mut on = t.weekdays_mask & (1 << i) != 0;
                        if ui.toggle_value(&mut on, *name).changed() {
                            t.weekdays_mask = if on { t.weekdays_mask | (1 << i) } else { t.weekdays_mask & !(1 << i) };
                        }
                    }
                });
            });
        });

        // Columna 2: qué hace la cue al dispararse.
        let ui = &mut cols[1];
        card(ui, "Fade & Stop Others (al disparar esta cue)", |ui| {
            let w = combo_width(ui);
            labeled(ui, "Alcance", |ui| {
                egui::ComboBox::from_id_salt("fade_others_scope").selected_text(scope_label(t.fade_and_stop_others_mode)).width(w).show_ui(ui, |ui| {
                    for s in [FadeOthersScope::Disabled, FadeOthersScope::Peers, FadeOthersScope::CueList, FadeOthersScope::All] {
                        ui.selectable_value(&mut t.fade_and_stop_others_mode, s, scope_label(s));
                    }
                });
            });
            ui.add_enabled_ui(t.fade_and_stop_others_mode != FadeOthersScope::Disabled, |ui| {
                labeled(ui, "Fade-out de las demás", |ui| time_drag(ui, &mut t.fade_and_stop_others_time, 0.0..=600.0));
            });
        });
        card(ui, "Duck / Boost Others", |ui| {
            ui.checkbox(&mut t.duck_others_enabled, "Atenuar (o realzar) a las demás mientras suene");
            ui.add_enabled_ui(t.duck_others_enabled, |ui| {
                ui.horizontal_wrapped(|ui| {
                    labeled(ui, "Nivel", |ui| {
                        ui.add(egui::DragValue::new(&mut t.duck_level_db).range(-60.0..=12.0).speed(0.1).suffix(" dB").min_decimals(1));
                    });
                    labeled(ui, "Tiempo de rampa", |ui| time_drag(ui, &mut t.duck_time_sec, 0.0..=60.0));
                });
            });
        });
        card(ui, "Segundo disparo", |ui| {
            let w = combo_width(ui);
            labeled(ui, "Acción", |ui| {
                egui::ComboBox::from_id_salt("second_trigger").selected_text(second_label(t.second_trigger_action)).width(w).show_ui(ui, |ui| {
                    for a in [
                        SecondTriggerAction::Nothing,
                        SecondTriggerAction::Panic,
                        SecondTriggerAction::Stop,
                        SecondTriggerAction::HardStop,
                        SecondTriggerAction::HardStopAndRestart,
                        SecondTriggerAction::Devamp,
                        SecondTriggerAction::PlaylistPlaysNext,
                        SecondTriggerAction::PlaylistPlaysPrevious,
                    ] {
                        ui.selectable_value(&mut t.second_trigger_action, a, second_label(a));
                    }
                });
            });
            ui.checkbox(&mut t.second_trigger_on_release, "Al soltar la tecla / nota");
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_are_distinct() {
        let scopes = [FadeOthersScope::Disabled, FadeOthersScope::Peers, FadeOthersScope::CueList, FadeOthersScope::All];
        let set: std::collections::HashSet<_> = scopes.iter().map(|s| scope_label(*s)).collect();
        assert_eq!(set.len(), scopes.len());
        assert!(second_label(SecondTriggerAction::Devamp).contains("Devamp"));
        assert_eq!(WEEKDAYS.len(), 7);
    }
}
