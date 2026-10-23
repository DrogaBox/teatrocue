//! Barra superior: gran botón GO, cue en espera, transporte y estado del sistema.

use eframe::egui::{self, vec2, Color32, RichText, Ui};

use super::{Action, ViewCtx};
use crate::theme;
use crate::widgets;

const GO_SIZE: egui::Vec2 = egui::vec2(150.0, 60.0);

/// Ancho fijo de la lectura numérica del maestro: no cambia de tamaño al mover el deslizador.
const MASTER_READOUT_WIDTH: f32 = 72.0;
const MASTER_SLIDER_WIDTH: f32 = 170.0;

/// Anchos de los bloques de la barra según el ancho de la ventana: en una pantalla chica (1366 px o una ventana sin
/// maximizar) se achican para que el estado del motor nunca quede cortado ni empuje los controles fuera de la pantalla.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Metrics {
    pub standby_w: f32,
    pub clock_w: f32,
    pub clock_font: f32,
    pub slider_w: f32,
    pub meter_w: f32,
}

/// Ancho que hace falta para el estado del motor (LED + nombre del dispositivo, XR y CPU) y los márgenes entre bloques.
const STATUS_AND_GAPS_W: f32 = 210.0;

impl Metrics {
    pub fn for_width(avail: f32) -> Self {
        if avail >= 1420.0 {
            Self { standby_w: 260.0, clock_w: 188.0, clock_font: 34.0, slider_w: MASTER_SLIDER_WIDTH, meter_w: 96.0 }
        } else if avail >= 1180.0 {
            Self { standby_w: 210.0, clock_w: 160.0, clock_font: 30.0, slider_w: 120.0, meter_w: 64.0 }
        } else {
            Self { standby_w: 150.0, clock_w: 128.0, clock_font: 24.0, slider_w: 70.0, meter_w: 36.0 }
        }
    }

    /// Ancho aproximado de todo lo que va en la barra con estos valores (para las pruebas y para decidir el nivel).
    pub fn total_width(&self, channels: usize) -> f32 {
        let meters = if channels <= 2 { channels as f32 * (self.meter_w + 24.0) } else { 4.0 * (self.bar_w() + 4.0) };
        GO_SIZE.x + self.standby_w + self.clock_w + 60.0 + self.slider_w + MASTER_READOUT_WIDTH + meters + STATUS_AND_GAPS_W
    }

    fn bar_w(&self) -> f32 {
        (self.meter_w * 2.0 / 3.0).max(30.0)
    }
}

pub fn show(ui: &mut Ui, v: &ViewCtx, actions: &mut Vec<Action>) {
    let m = Metrics::for_width(ui.available_width());
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        go_button(ui, v, actions);
        standby(ui, v, m.standby_w);
        super::clock::show(ui, v, m.clock_w, m.clock_font);
        control_block(ui, v, actions, &m);
        // El estado del sistema (LED, XR, CPU) queda a la derecha con su propio ancho: no roba espacio al bloque de control.
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            system_status(ui, v);
        });
    });
}

/// Esquina superior derecha, a la derecha de STANDBY, en dos filas claras:
/// 1) transporte (Pausar, Reanudar, Parar y PANIC en rojo); 2) MASTER, deslizador, lectura en dB y vúmetros.
fn control_block(ui: &mut Ui, v: &ViewCtx, actions: &mut Vec<Action>, m: &Metrics) {
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 8.0;
        transport_row(ui, actions);
        master_row(ui, v, actions, m);
    });
}

fn transport_row(ui: &mut Ui, actions: &mut Vec<Action>) {
    ui.horizontal(|ui| {
        if ui.button("Pausar todo").clicked() {
            actions.push(Action::PauseAll);
        }
        if ui.button("Reanudar todo").clicked() {
            actions.push(Action::ResumeAll);
        }
        if ui.button("Parar todo").clicked() {
            actions.push(Action::StopAll);
        }
        let panic = egui::Button::new(RichText::new("PANIC").strong().color(Color32::WHITE)).fill(theme::RED).min_size(vec2(84.0, 0.0));
        if ui.add(panic).on_hover_text("Parada suave. Pulsa dos veces seguidas (o Esc, Esc) para una parada dura").clicked() {
            actions.push(Action::Panic);
        }
    });
}

/// MASTER: etiqueta, deslizador horizontal (−60 a +6 dB, doble clic = 0 dB), lectura fija en dB y vúmetros 1 y 2.
fn master_row(ui: &mut Ui, v: &ViewCtx, actions: &mut Vec<Action>, m: &Metrics) {
    ui.horizontal(|ui| {
        let tuned = v.master_db.abs() >= 0.05;
        ui.add(egui::Label::new(RichText::new("MASTER").strong().color(if tuned { theme::AMBER } else { theme::TEXT_DIM })).extend());
        ui.spacing_mut().slider_width = m.slider_w;
        let mut db = v.master_db;
        let r = ui.add(egui::Slider::new(&mut db, -60.0..=6.0).show_value(false).step_by(0.5)).on_hover_text("Volumen maestro de toda la salida (no cambia los niveles de las cues). Doble clic: 0 dB");
        if r.double_clicked() {
            actions.push(Action::SetMasterDb(0.0));
        } else if r.changed() {
            actions.push(Action::SetMasterDb(db));
        }
        let reading = if v.master_db <= -60.0 { "-inf".to_string() } else { format!("{:.1} dB", v.master_db) };
        ui.add_sized([MASTER_READOUT_WIDTH, 18.0], egui::Label::new(RichText::new(reading).monospace().color(if tuned { theme::AMBER } else { theme::TEXT })).extend());
        ui.add_space(6.0);
        master_meters(ui, v, m);
    });
}

fn go_button(ui: &mut Ui, v: &ViewCtx, actions: &mut Vec<Action>) {
    let can_go = v.model.playhead.is_some();
    let label = RichText::new(if can_go { "GO" } else { "FIN" }).size(38.0).strong().color(Color32::BLACK);
    let button = egui::Button::new(label).fill(if can_go { theme::GREEN } else { theme::GRID }).min_size(GO_SIZE);
    let r = ui.add_enabled(can_go, button).on_hover_text("Espacio");
    if r.clicked() {
        actions.push(Action::Go);
    }
}

fn standby(ui: &mut Ui, v: &ViewCtx, width: f32) {
    egui::Frame::new().fill(theme::BG).stroke(egui::Stroke::new(1.0, theme::AMBER_DIM)).inner_margin(egui::Margin::symmetric(10, 6)).show(ui, |ui| {
        ui.set_min_size(vec2(width, GO_SIZE.y - 14.0));
        ui.set_max_width(width);
        ui.vertical(|ui| {
            ui.label(RichText::new("STANDBY").small().color(theme::TEXT_DIM));
            match v.model.playhead.and_then(|id| v.model.summary(id)) {
                Some(c) => {
                    // Una sola línea: si el nombre no entra, se desplaza de lado a lado en vez de partirse o salirse del recuadro.
                    widgets::marquee(ui, &format!("{}  {}", c.number, c.name), egui::FontId::proportional(20.0), theme::AMBER, width, true);
                }
                None => {
                    ui.label(RichText::new("— fin de la lista —").size(18.0).color(theme::TEXT_DIM));
                }
            }
        });
    });
}

/// Vúmetros de la salida. Con 1 o 2 canales van uno al lado del otro en la misma fila; con más, en una rejilla compacta.
fn master_meters(ui: &mut Ui, v: &ViewCtx, m: &Metrics) {
    let channels = v.hello.map_or(2, |h| h.channels as usize).clamp(1, 8);
    if channels <= 2 {
        ui.spacing_mut().item_spacing.x = 4.0;
        for ch in 0..channels {
            ui.add(egui::Label::new(RichText::new(format!("{}", ch + 1)).small().color(theme::TEXT_DIM)).extend());
            widgets::meter_bar(ui, vec2(m.meter_w, 12.0), &v.telemetry.meters[ch]);
            ui.add_space(4.0);
        }
        return;
    }
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing = vec2(4.0, 3.0);
        for row in 0..channels.div_ceil(4) {
            ui.horizontal(|ui| {
                for ch in (row * 4)..((row + 1) * 4).min(channels) {
                    widgets::meter_bar(ui, vec2(m.bar_w(), 10.0), &v.telemetry.meters[ch]);
                }
            });
        }
    });
}

fn system_status(ui: &mut Ui, v: &ViewCtx) {
    ui.vertical(|ui| {
        let t = v.telemetry;
        ui.horizontal(|ui| {
            widgets::led(ui, theme::GREEN, true, "conectado al daemon");
            let backend = v.hello.map_or("—", |h| h.backend.as_str());
            ui.label(RichText::new(crate::format::ellipsize(backend, 18)).small().color(theme::TEXT_DIM));
        });
        ui.horizontal(|ui| {
            let xr = t.xruns();
            ui.label(RichText::new(format!("XR {xr}")).monospace().small().color(if xr > 0 { theme::RED } else { theme::TEXT_DIM }));
            let cpu = t.cpu_permille() as f32 / 10.0;
            ui.label(RichText::new(format!("CPU {cpu:>4.1}%")).monospace().small().color(if cpu > 70.0 { theme::AMBER } else { theme::TEXT_DIM }));
            widgets::led(ui, theme::RED, t.frozen(), "audio congelado");
            widgets::led(ui, theme::AMBER, v.safe_mute, "safe mute activo");
        });
    });
}
