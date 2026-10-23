//! Botonera de Carts: una cuadrícula de 4×4 pads para disparar efectos al instante, sin tocar la lista de cues.
//!
//! La vista solo dibuja y devuelve [`CartEvent`]s; la aplicación habla con el daemon (cada pad es un reproductor
//! independiente) y guarda los pads dentro del show.

use std::time::Instant;

use eframe::egui::{self, Color32, RichText, Sense, Vec2};
use qlab_protocol::cue_dto::{CartBank, CartMode, CueColor, CART_MAX_GAIN_DB, CART_MIN_GAIN_DB, CART_PADS};

use crate::theme;

pub const TITLE: &str = "Botonera de Carts";
pub const COLUMNS: usize = 4;
/// Atajos que se pueden asignar a un pad. Se evitan F1 (manual), F2 y F3 (esta ventana) y las letras, que usan otras funciones.
pub const KEYS: [&str; 18] = ["1", "2", "3", "4", "5", "6", "7", "8", "9", "0", "F5", "F6", "F7", "F8", "F9", "F10", "F11", "F12"];

pub fn key_of(name: &str) -> Option<egui::Key> {
    use egui::Key::*;
    Some(match name {
        "1" => Num1,
        "2" => Num2,
        "3" => Num3,
        "4" => Num4,
        "5" => Num5,
        "6" => Num6,
        "7" => Num7,
        "8" => Num8,
        "9" => Num9,
        "0" => Num0,
        "F5" => F5,
        "F6" => F6,
        "F7" => F7,
        "F8" => F8,
        "F9" => F9,
        "F10" => F10,
        "F11" => F11,
        "F12" => F12,
        _ => return None,
    })
}

#[derive(Debug, PartialEq)]
pub enum CartEvent {
    /// Pulsación del pad (clic o tecla).
    Press(usize),
    /// Se soltó el pad (importa en el modo «Pulsar para reproducir»).
    Release(usize),
    /// Cambió la configuración de un pad (nombre, archivo, color, volumen, atajo o modo).
    Edited(usize),
    /// «Examinar…» en el editor del pad.
    Browse(usize),
    /// Detener todos los pads.
    StopAll,
}

/// Estado de la ventana (se conserva entre fotogramas).
#[derive(Default)]
pub struct CartsState {
    pub open: bool,
    pub bank: CartBank,
    /// Pad abierto en el editor.
    pub editing: Option<usize>,
    /// Hasta cuándo suena cada pad (según la duración del archivo); `None` = parado.
    pub until: [Option<Instant>; CART_PADS],
    /// Duración conocida de cada archivo, en segundos.
    pub duration: [Option<f64>; CART_PADS],
    /// Pad mantenido con el mouse (modo Hold).
    pub held_by_mouse: Option<usize>,
    /// Pads que el teclado mantiene pulsados.
    pub held_by_key: [bool; CART_PADS],
}

impl CartsState {
    pub fn set_bank(&mut self, bank: CartBank) {
        self.bank = bank.clamped();
    }

    pub fn playing(&self, pad: usize, now: Instant) -> bool {
        self.until.get(pad).copied().flatten().is_some_and(|t| now < t)
    }

    /// Marca el pad como sonando (hasta el fin del archivo o, si no se sabe, hasta que se detenga).
    pub fn mark_started(&mut self, pad: usize, now: Instant) {
        if pad < CART_PADS {
            let secs = self.duration[pad].unwrap_or(3600.0);
            self.until[pad] = Some(now + std::time::Duration::from_secs_f64(secs + 0.1));
        }
    }

    pub fn mark_stopped(&mut self, pad: usize) {
        if pad < CART_PADS {
            self.until[pad] = None;
        }
    }

    /// Pad que responde a la tecla `name`.
    pub fn pad_for_key(&self, name: &str) -> Option<usize> {
        self.bank.pads.iter().position(|p| !p.is_empty() && p.key.eq_ignore_ascii_case(name))
    }
}

fn pad_color(c: CueColor) -> Color32 {
    theme::cue_color(c).unwrap_or(Color32::from_rgb(0x4a, 0x4d, 0x55))
}

/// Dibuja la ventana flotante.
pub fn show(ctx: &egui::Context, st: &mut CartsState, locked: bool) -> Vec<CartEvent> {
    let mut events = Vec::new();
    if !st.open {
        return events;
    }
    if st.bank.pads.len() != CART_PADS {
        st.bank = std::mem::take(&mut st.bank).clamped();
    }
    let mut open = true;
    let now = Instant::now();
    egui::Window::new(TITLE).open(&mut open).resizable(false).default_pos([60.0, 120.0]).show(ctx, |ui| {
        let size = Vec2::new(118.0, 74.0);
        egui::Grid::new("carts_grid").spacing([8.0, 8.0]).show(ui, |ui| {
            for i in 0..CART_PADS {
                let pad = &st.bank.pads[i];
                let playing = st.playing(i, now);
                let (rect, resp) = ui.allocate_exact_size(size, Sense::click_and_drag());
                let base = if pad.is_empty() { Color32::from_rgb(0x2a, 0x2c, 0x32) } else { pad_color(pad.color) };
                let fill = if playing { base.gamma_multiply(1.35) } else if resp.hovered() { base.gamma_multiply(1.12) } else { base };
                let painter = ui.painter();
                painter.rect_filled(rect, 6.0, fill);
                let selected = st.editing == Some(i);
                painter.rect_stroke(rect, 6.0, egui::Stroke::new(if selected { 2.5 } else { 1.0 }, if playing { theme::AMBER } else { Color32::from_black_alpha(120) }), egui::StrokeKind::Inside);
                let title = if pad.is_empty() { "(vacío)".to_string() } else { crate::format::ellipsize(&pad.title(), 16) };
                painter.text(rect.center() - Vec2::new(0.0, 8.0), egui::Align2::CENTER_CENTER, title, egui::FontId::proportional(14.0), if pad.is_empty() { theme::TEXT_DIM } else { Color32::WHITE });
                let sub = match (pad.key.is_empty(), pad.is_empty()) {
                    (_, true) => String::new(),
                    (true, _) => format!("{}", i + 1),
                    (false, _) => format!("[{}]", pad.key),
                };
                painter.text(rect.center() + Vec2::new(0.0, 14.0), egui::Align2::CENTER_CENTER, sub, egui::FontId::monospace(11.0), Color32::from_white_alpha(190));
                if playing {
                    painter.text(rect.left_top() + Vec2::new(6.0, 4.0), egui::Align2::LEFT_TOP, "SUENA", egui::FontId::proportional(11.0), Color32::WHITE);
                }
                resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, format!("Pad {}", i + 1)));
                let resp = resp.on_hover_text(if pad.is_empty() { "Clic derecho: asignar un archivo".to_string() } else { format!("{} · {}\nClic derecho: editar", pad.mode.label(), pad.path) });
                if resp.secondary_clicked() && !locked {
                    st.editing = Some(i);
                }
                if !st.bank.pads[i].is_empty() {
                    if resp.hovered() && st.held_by_mouse.is_none() && ui.input(|inp| inp.pointer.primary_pressed()) {
                        st.held_by_mouse = Some(i);
                        events.push(CartEvent::Press(i));
                    }
                } else if resp.clicked() && !locked {
                    st.editing = Some(i);
                }
                if (i + 1) % COLUMNS == 0 {
                    ui.end_row();
                }
            }
        });
        if let Some(i) = st.held_by_mouse {
            if !ui.input(|inp| inp.pointer.primary_down()) {
                st.held_by_mouse = None;
                events.push(CartEvent::Release(i));
            }
        }
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if ui.button("Detener todos").clicked() {
                events.push(CartEvent::StopAll);
            }
            ui.label(RichText::new("Clic derecho en un pad para editarlo").small().color(theme::TEXT_DIM));
        });
        if let Some(i) = st.editing.filter(|&i| i < CART_PADS && !locked) {
            ui.separator();
            editor(ui, st, i, &mut events);
        }
    });
    if !open {
        st.open = false;
    }
    events
}

fn editor(ui: &mut egui::Ui, st: &mut CartsState, i: usize, events: &mut Vec<CartEvent>) {
    let others: Vec<String> = st.bank.pads.iter().enumerate().filter(|&(j, p)| j != i && !p.is_empty()).map(|(_, p)| p.key.clone()).collect();
    let pad = &mut st.bank.pads[i];
    let before = pad.clone();
    ui.label(RichText::new(format!("Pad {}", i + 1)).strong().color(theme::AMBER));
    egui::Grid::new("cart_editor").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
        ui.label("Nombre");
        ui.add(egui::TextEdit::singleline(&mut pad.name).hint_text("Nombre del efecto").desired_width(260.0));
        ui.end_row();
        ui.label("Archivo");
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut pad.path).hint_text("wav, mp3, ogg…").desired_width(210.0));
            if ui.button("Examinar…").clicked() {
                events.push(CartEvent::Browse(i));
            }
        });
        ui.end_row();
        ui.label("Color");
        ui.horizontal(|ui| {
            for (c, name) in [(CueColor::None, "Gris"), (CueColor::Red, "Rojo"), (CueColor::Orange, "Naranja"), (CueColor::Green, "Verde"), (CueColor::Blue, "Azul"), (CueColor::Purple, "Violeta")] {
                let on = pad.color == c;
                let (r, resp) = ui.allocate_exact_size(Vec2::splat(22.0), Sense::click());
                ui.painter().rect_filled(r, 4.0, pad_color(c));
                if on {
                    ui.painter().rect_stroke(r, 4.0, egui::Stroke::new(2.0, Color32::WHITE), egui::StrokeKind::Outside);
                }
                resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, format!("Color {name}")));
                if resp.on_hover_text(name).clicked() {
                    pad.color = c;
                }
            }
        });
        ui.end_row();
        ui.label("Volumen");
        ui.add(egui::Slider::new(&mut pad.gain_db, CART_MIN_GAIN_DB..=CART_MAX_GAIN_DB).suffix(" dB").fixed_decimals(1));
        ui.end_row();
        ui.label("Atajo");
        egui::ComboBox::from_id_salt("cart_key").selected_text(if pad.key.is_empty() { "Sin atajo".to_string() } else { pad.key.clone() }).show_ui(ui, |ui| {
            ui.selectable_value(&mut pad.key, String::new(), "Sin atajo");
            for k in KEYS {
                let label = if others.iter().any(|o| o == k) { format!("{k} (ya usado)") } else { k.to_string() };
                ui.selectable_value(&mut pad.key, k.to_string(), label);
            }
        });
        ui.end_row();
        ui.label("Modo");
        egui::ComboBox::from_id_salt("cart_mode").selected_text(pad.mode.label()).width(200.0).show_ui(ui, |ui| {
            for m in CartMode::ALL {
                ui.selectable_value(&mut pad.mode, m, m.label());
            }
        });
        ui.end_row();
    });
    if ui.button("Vaciar pad").clicked() {
        *pad = Default::default();
    }
    if *pad != before {
        events.push(CartEvent::Edited(i));
    }
}
