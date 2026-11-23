//! Botonera de Carts: una cuadrícula de 4×4 pads para disparar efectos al instante, sin tocar la lista de cues.
//!
//! La vista solo dibuja y devuelve [`CartEvent`]s; la aplicación habla con el daemon (cada pad es un reproductor
//! independiente) y guarda los pads dentro del show.

use std::sync::mpsc::{channel, Receiver};
use std::time::Instant;

use eframe::egui::{self, Color32, RichText, Sense, Vec2};
use qlab_protocol::cue_dto::{CartBank, CartMode, CueColor, CART_MAX_GAIN_DB, CART_MIN_GAIN_DB, CART_PADS};
use qlab_protocol::matrix::{OutputRoute, RouteSide};

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
    /// Asignar una ruta de salida ya configurada a todos los pads de Carts.
    SetAllRoutes(OutputRoute),
    /// Elegir directamente otra placa de sonido del sistema para la salida de Carts.
    UseDeviceForAll(String),
    /// Abrir la ventana de Configuración de audio.
    OpenAudioSettings,
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
    /// Cuándo empezó a sonar cada pad (para la barra de progreso en vivo).
    pub started_at: [Option<Instant>; CART_PADS],
    /// Duración conocida de cada archivo, en segundos.
    pub duration: [Option<f64>; CART_PADS],
    /// Pad mantenido con el mouse (modo Hold).
    pub held_by_mouse: Option<usize>,
    /// Pads que el teclado mantiene pulsados.
    pub held_by_key: [bool; CART_PADS],
    /// Salidas que se ofrecen en «Sale por».
    pub outputs: super::Outputs,
    /// Estado real de apertura de cada salida adicional (del `Hello` del daemon: `Some(placa)` = activa, `None` = caída).
    pub extra_outputs_live: Vec<Option<String>>,
    /// Placa principal abierta o configurada (para no duplicarla al ofrecer otras placas del sistema).
    pub main_device: Option<String>,
    /// Salida por defecto para nuevos pads y cuando todos los pads están vacíos.
    pub default_route: OutputRoute,
    /// `(host, dispositivo)` de placas detectadas en la PC; `None` mientras se buscan.
    pub devices: Option<Vec<(String, String)>>,
    scan: Option<Receiver<Vec<(String, String)>>>,
    scanned_while_open: bool,
}

impl CartsState {
    pub fn set_bank(&mut self, bank: CartBank) {
        self.bank = bank.clamped();
        if let Some(r) = self.common_route() {
            self.default_route = r;
        }
    }

    /// Si todos los pads con archivo comparten la misma salida, la devuelve; si están vacíos, devuelve `default_route`.
    pub fn common_route(&self) -> Option<OutputRoute> {
        let mut iter = self.bank.pads.iter().filter(|p| !p.is_empty()).map(|p| p.route);
        let Some(first) = iter.next() else {
            return Some(self.default_route);
        };
        iter.all(|r| r == first).then_some(first)
    }

    /// Busca las placas de sonido disponibles en segundo plano para no congelar la interfaz.
    pub fn start_device_scan(&mut self) {
        self.devices = None;
        let (tx, rx) = channel();
        self.scan = Some(rx);
        let _ = std::thread::Builder::new().name("qlab-carts-device-scan".into()).spawn(move || {
            let _ = tx.send(qlab_daemon::backend_audio::list_output_devices());
        });
    }

    fn poll_devices(&mut self) {
        if let Some(rx) = &self.scan {
            if let Ok(list) = rx.try_recv() {
                self.devices = Some(list);
                self.scan = None;
            }
        }
    }

    pub fn set_devices_for_test(&mut self, list: Vec<(String, String)>) {
        self.devices = Some(list);
        self.scan = None;
        self.scanned_while_open = true;
    }

    pub fn playing(&self, pad: usize, now: Instant) -> bool {
        self.until.get(pad).copied().flatten().is_some_and(|t| now < t)
    }

    pub fn any_playing(&self, now: Instant) -> bool {
        (0..CART_PADS).any(|i| self.playing(i, now))
    }

    /// Marca el pad como sonando (hasta el fin del archivo o, si no se sabe, hasta que se detenga).
    pub fn mark_started(&mut self, pad: usize, now: Instant) {
        if pad < CART_PADS {
            let secs = self.duration[pad].unwrap_or(3600.0);
            self.started_at[pad] = Some(now);
            self.until[pad] = Some(now + std::time::Duration::from_secs_f64(secs + 0.1));
        }
    }

    pub fn mark_stopped(&mut self, pad: usize) {
        if pad < CART_PADS {
            self.started_at[pad] = None;
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

/// Etiqueta corta de salida para la esquina del pad cuando no es la predeterminada («Principal · las dos fichas»).
pub fn compact_route_badge(r: OutputRoute) -> Option<String> {
    if r == OutputRoute::default() {
        return None;
    }
    Some(match (r.output, r.side) {
        (0, RouteSide::First) => "P·1".to_string(),
        (0, RouteSide::Second) => "P·2".to_string(),
        (0, RouteSide::Both) => return None,
        (n, RouteSide::Both) => format!("S{}", n + 1),
        (n, RouteSide::First) => format!("S{}·1", n + 1),
        (n, RouteSide::Second) => format!("S{}·2", n + 1),
    })
}

fn fmt_mm_ss(secs: f64) -> String {
    let total = secs.max(0.0).ceil() as u64;
    format!("{:02}:{:02}", total / 60, total % 60)
}

/// Estado visual de la salida elegida para Carts: `(color, texto, tooltip)`.
fn route_status(st: &CartsState, current: Option<OutputRoute>) -> (Color32, &'static str, String) {
    match current {
        None => (theme::AMBER, "Varias", "Los pads usan distintas salidas".to_string()),
        Some(r) if r.output == 0 => (theme::GREEN, "Principal", "Los pads salen por la placa principal".to_string()),
        Some(r) => {
            let idx = (r.output - 1) as usize;
            let place = format!("Salida {}", r.output + 1);
            match st.extra_outputs_live.get(idx).and_then(|o| o.as_deref()) {
                Some(opened) => (theme::GREEN, "Activa", format!("{place} conectada y activa: {opened}")),
                None => {
                    if let Some(cfg_name) = st.outputs.extras.get(idx).map(|s| s.trim()).filter(|s| !s.is_empty()) {
                        (theme::RED, "Desconectada", format!("{place} ({cfg_name}) no está conectada: sale por Principal"))
                    } else {
                        (theme::AMBER, "Sin placa", format!("{place} sin placa asignada: sale por Principal"))
                    }
                }
            }
        }
    }
}

/// Dibuja la ventana flotante.
pub fn show(ctx: &egui::Context, st: &mut CartsState, locked: bool) -> Vec<CartEvent> {
    let mut events = Vec::new();
    if !st.open {
        st.scanned_while_open = false;
        return events;
    }
    if !st.scanned_while_open {
        st.scanned_while_open = true;
        st.start_device_scan();
    }
    st.poll_devices();
    if st.bank.pads.len() != CART_PADS {
        st.bank = std::mem::take(&mut st.bank).clamped();
    }
    let mut open = true;
    let now = Instant::now();
    egui::Window::new(TITLE).open(&mut open).resizable(false).default_pos([40.0, 80.0]).show(ctx, |ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new("Salida de Carts:").strong());
            let current = st.common_route();
            let text = current.map_or_else(|| "Varias salidas…".to_string(), |r| st.outputs.label(r));
            if locked {
                ui.label(RichText::new(text).color(theme::TEXT_DIM));
            } else {
                egui::ComboBox::from_id_salt("carts_global_route").selected_text(text).width(270.0).show_ui(ui, |ui| {
                    for r in OutputRoute::all(st.outputs.extras.len()) {
                        if ui.selectable_label(current == Some(r), st.outputs.label(r)).clicked() && current != Some(r) {
                            st.default_route = r;
                            events.push(CartEvent::SetAllRoutes(r));
                        }
                    }
                    ui.separator();
                    ui.label(RichText::new("Otra placa de sonido (para anuncios)").small().color(theme::TEXT_DIM));
                    match &st.devices {
                        None => {
                            ui.label(RichText::new("Buscando placas de sonido…").small().color(theme::TEXT_DIM));
                        }
                        Some(list) => {
                            let mut seen: Vec<&str> = Vec::new();
                            let mut any = false;
                            for (_, name) in list.iter().filter(|(_, n)| !n.is_empty()) {
                                if seen.contains(&name.as_str()) {
                                    continue;
                                }
                                seen.push(name.as_str());
                                if st.main_device.as_deref().is_some_and(|m| m.eq_ignore_ascii_case(name)) {
                                    continue;
                                }
                                if st.outputs.extras.iter().any(|e| e.eq_ignore_ascii_case(name)) {
                                    continue;
                                }
                                any = true;
                                if ui.selectable_label(false, format!("Usar placa: {name}")).clicked() {
                                    events.push(CartEvent::UseDeviceForAll(name.clone()));
                                }
                            }
                            if !any {
                                ui.label(RichText::new("No hay otras placas conectadas.").small().color(theme::TEXT_DIM));
                            }
                        }
                    }
                });
                if ui.button("Buscar").on_hover_text("Volver a buscar placas de sonido conectadas").clicked() {
                    st.start_device_scan();
                }
                if ui.button("Audio…").on_hover_text("Abrir Configuración de audio").clicked() {
                    events.push(CartEvent::OpenAudioSettings);
                }
            }
            let (dot_color, status_text, status_tip) = route_status(st, current);
            let (dot_rect, dot_resp) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
            ui.painter().circle_filled(dot_rect.center(), 4.0, dot_color);
            dot_resp.on_hover_text(&status_tip);
            ui.label(RichText::new(status_text).small().strong().color(dot_color)).on_hover_text(status_tip);
        });
        ui.add_space(6.0);
        // Distribución horizontal (768p-safe): cuadrícula 4×4 a la izquierda y editor del pad a la derecha.
        ui.horizontal_top(|ui| {
            ui.vertical(|ui| {
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
                        painter.text(rect.center() - Vec2::new(0.0, 6.0), egui::Align2::CENTER_CENTER, title, egui::FontId::proportional(13.5), if pad.is_empty() { theme::TEXT_DIM } else { Color32::WHITE });
                        let sub = match (pad.key.is_empty(), pad.is_empty()) {
                            (_, true) => String::new(),
                            (true, _) => format!("{}", i + 1),
                            (false, _) => format!("[{}]", pad.key),
                        };
                        painter.text(rect.center() + Vec2::new(0.0, 14.0), egui::Align2::CENTER_CENTER, sub, egui::FontId::monospace(11.0), Color32::from_white_alpha(190));
                        if !pad.is_empty() {
                            if let Some(badge) = compact_route_badge(pad.route) {
                                let badge_w = (badge.chars().count() as f32 * 6.5 + 8.0).max(22.0);
                                let badge_rect = egui::Rect::from_min_size(egui::pos2(rect.right() - badge_w - 4.0, rect.top() + 3.0), Vec2::new(badge_w, 14.0));
                                painter.rect_filled(badge_rect, 3.0, Color32::from_black_alpha(165));
                                painter.text(badge_rect.center(), egui::Align2::CENTER_CENTER, badge, egui::FontId::monospace(10.0), theme::AMBER);
                            }
                        }
                        if playing {
                            let rem = st.until[i].map(|t| t.saturating_duration_since(now).as_secs_f64());
                            let tag = match (st.duration[i], rem) {
                                (Some(_), Some(r)) => format!("SUENA {}", fmt_mm_ss(r)),
                                _ => "SUENA".to_string(),
                            };
                            painter.text(rect.left_top() + Vec2::new(5.0, 4.0), egui::Align2::LEFT_TOP, tag, egui::FontId::monospace(10.0), Color32::WHITE);
                            if let (Some(dur), Some(started)) = (st.duration[i].filter(|&d| d > 0.0), st.started_at[i]) {
                                let frac = (now.saturating_duration_since(started).as_secs_f64() / dur).clamp(0.0, 1.0) as f32;
                                let bar = egui::Rect::from_min_max(egui::pos2(rect.left() + 4.0, rect.bottom() - 7.0), egui::pos2(rect.right() - 4.0, rect.bottom() - 3.0));
                                painter.rect_filled(bar, 2.0, Color32::from_black_alpha(140));
                                let fill_bar = egui::Rect::from_min_size(bar.min, Vec2::new(bar.width() * frac, bar.height()));
                                painter.rect_filled(fill_bar, 2.0, theme::AMBER);
                            }
                        }
                        resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, format!("Pad {}", i + 1)));
                        let resp = resp.on_hover_text(if pad.is_empty() { "Clic derecho: asignar un archivo".to_string() } else { format!("{} · {}\nSale por: {}\nClic derecho: editar", pad.mode.label(), pad.path, st.outputs.label(pad.route)) });
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
            });
            if let Some(i) = st.editing.filter(|&i| i < CART_PADS && !locked) {
                ui.separator();
                ui.vertical(|ui| {
                    editor(ui, st, i, &mut events);
                });
            }
        });
    });
    if !open {
        st.open = false;
    }
    events
}

fn editor(ui: &mut egui::Ui, st: &mut CartsState, i: usize, events: &mut Vec<CartEvent>) {
    let others: Vec<String> = st.bank.pads.iter().enumerate().filter(|&(j, p)| j != i && !p.is_empty()).map(|(_, p)| p.key.clone()).collect();
    let default_route = st.default_route;
    let pad = &mut st.bank.pads[i];
    let before = pad.clone();
    let was_empty = before.is_empty();
    ui.label(RichText::new(format!("Pad {}", i + 1)).strong().color(theme::AMBER));
    egui::Grid::new("cart_editor").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
        ui.label("Nombre");
        ui.add(egui::TextEdit::singleline(&mut pad.name).hint_text("Nombre del efecto").desired_width(220.0));
        ui.end_row();
        ui.label("Archivo");
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut pad.path).hint_text("wav, mp3, ogg…").desired_width(165.0));
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
        ui.label(super::ROUTE_LABEL);
        let current = pad.route;
        egui::ComboBox::from_id_salt("cart_route").selected_text(st.outputs.label(current)).width(235.0).show_ui(ui, |ui| {
            for r in qlab_protocol::matrix::OutputRoute::all(st.outputs.extras.len()) {
                ui.selectable_value(&mut pad.route, r, st.outputs.label(r));
            }
        });
        ui.end_row();
        ui.label("Modo");
        egui::ComboBox::from_id_salt("cart_mode").selected_text(pad.mode.label()).width(190.0).show_ui(ui, |ui| {
            for m in CartMode::ALL {
                ui.selectable_value(&mut pad.mode, m, m.label());
            }
        });
        ui.end_row();
    });
    if was_empty && !pad.is_empty() && pad.route == before.route {
        pad.route = default_route;
    }
    if ui.button("Vaciar pad").clicked() {
        *pad = Default::default();
        pad.route = default_route;
    }
    if *pad != before {
        events.push(CartEvent::Edited(i));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_route_badges_match_non_default_routes() {
        assert_eq!(compact_route_badge(OutputRoute::default()), None);
        assert_eq!(compact_route_badge(OutputRoute { output: 0, side: RouteSide::First }).as_deref(), Some("P·1"));
        assert_eq!(compact_route_badge(OutputRoute { output: 0, side: RouteSide::Second }).as_deref(), Some("P·2"));
        assert_eq!(compact_route_badge(OutputRoute { output: 1, side: RouteSide::Both }).as_deref(), Some("S2"));
        assert_eq!(compact_route_badge(OutputRoute { output: 2, side: RouteSide::Both }).as_deref(), Some("S3"));
        assert_eq!(compact_route_badge(OutputRoute { output: 3, side: RouteSide::Both }).as_deref(), Some("S4"));
        assert_eq!(compact_route_badge(OutputRoute { output: 1, side: RouteSide::First }).as_deref(), Some("S2·1"));
    }
}
