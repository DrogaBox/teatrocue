//! Pantalla de reconexión: el daemon no está activo. Reintenta sola y ofrece arrancar un motor.

use eframe::egui::{self, vec2, RichText, Ui};

use super::Action;
use crate::theme;

pub struct ReconnectInfo<'a> {
    pub socket: &'a str,
    pub reason: &'a str,
    pub connecting: bool,
    pub seconds_down: f32,
    /// Resultado del último intento de arrancar el motor embebido o un daemon externo.
    pub engine_message: Option<&'a str>,
    pub embedded_running: bool,
    pub daemon_binary_found: bool,
}

pub fn show(ui: &mut Ui, info: &ReconnectInfo, actions: &mut Vec<Action>) {
    ui.vertical_centered(|ui| {
        ui.add_space((ui.available_height() * 0.18).max(20.0));
        ui.label(RichText::new("TeatroCue").size(42.0).strong().color(theme::AMBER));
        ui.add_space(6.0);
        if info.connecting {
            // Sin spinner animado: la pantalla de espera no debe mantener la ventana repintando a 60 fps.
            ui.label(RichText::new("Conectando con el daemon…").size(18.0));
        } else {
            ui.label(RichText::new("El daemon de audio no está activo").size(20.0).color(theme::RED));
            ui.label(RichText::new(format!("Buscando en {}", info.socket)).color(theme::TEXT_DIM));
            if !info.reason.is_empty() {
                ui.label(RichText::new(info.reason).small().color(theme::TEXT_DIM));
            }
            ui.label(RichText::new(format!("Reintentando automáticamente… ({:.0} s)", info.seconds_down)).small().color(theme::TEXT_DIM));
        }
        ui.add_space(18.0);
        let size = vec2(300.0, 32.0);
        if ui.add_sized(size, egui::Button::new("Reintentar ahora")).clicked() {
            actions.push(Action::RetryConnection);
        }
        ui.add_space(4.0);
        ui.add_enabled_ui(!info.embedded_running, |ui| {
            if ui.add_sized(size, egui::Button::new("Iniciar motor embebido (modo independiente)")).on_hover_text("Lanza el motor dentro de esta ventana, con el audio predeterminado").clicked() {
                actions.push(Action::StartEmbedded { with_audio: true });
            }
            if ui.add_sized(size, egui::Button::new("Iniciar motor embebido sin audio")).on_hover_text("Dispositivo virtual: para preparar el show sin tarjeta de sonido").clicked() {
                actions.push(Action::StartEmbedded { with_audio: false });
            }
        });
        ui.add_enabled_ui(info.daemon_binary_found, |ui| {
            if ui.add_sized(size, egui::Button::new("Lanzar teatrocue-daemon (proceso aparte)")).on_hover_text("Recomendado en escena: el audio sobrevive si esta ventana se cierra").clicked() {
                actions.push(Action::LaunchDaemonProcess);
            }
        });
        if let Some(msg) = info.engine_message {
            ui.add_space(10.0);
            ui.label(RichText::new(msg).color(theme::AMBER));
        }
    });
}
