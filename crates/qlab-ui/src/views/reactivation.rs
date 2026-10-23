//! Bloque «Reactivación de Emergencia»: código de solicitud, campo para pegar la clave de activación y botón Activar.
//! Se dibuja en la pantalla de bloqueo por licencia vencida y en Ayuda > Licencia.

use eframe::egui::{self, RichText, Ui};

use crate::theme;

pub const TITLE: &str = "Reactivación de Emergencia";
pub const HINT: &str = "Pegar clave de activación";

/// Resultado del último intento (se muestra bajo el botón).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Ok(String),
    Err(String),
}

#[derive(Default)]
pub struct ReactivationState {
    /// Texto pegado por el operador.
    pub input: String,
    pub outcome: Option<Outcome>,
    /// Código de solicitud (o por qué no hay): se calcula una vez y se renueva tras cada intento.
    pub challenge: Option<Result<String, String>>,
}

impl ReactivationState {
    pub fn invalidate(&mut self) {
        self.challenge = None;
    }
}

/// Dibuja el bloque. Devuelve la clave pegada cuando el operador pulsa «Activar».
pub fn show(ui: &mut Ui, st: &mut ReactivationState, id_salt: &str) -> Option<String> {
    let mut apply = None;
    egui::Frame::new().fill(theme::PANEL).stroke(egui::Stroke::new(1.0, theme::AMBER)).corner_radius(4).inner_margin(egui::Margin::same(12)).show(ui, |ui| {
        ui.set_max_width(640.0);
        ui.label(RichText::new(TITLE).strong().size(17.0).color(theme::AMBER));
        ui.add_space(4.0);
        ui.label(RichText::new("¿Sin el pendrive? Envíe este código de solicitud al desarrollador (por WhatsApp, por ejemplo) y pegue abajo la clave de activación que le devuelva.").color(theme::TEXT_DIM));
        ui.add_space(8.0);
        match st.challenge.clone() {
            Some(Ok(code)) => {
                ui.label(RichText::new(&code).monospace().size(20.0).strong().color(theme::GREEN));
                ui.add_space(4.0);
                if ui.button("Copiar código").clicked() {
                    ui.ctx().copy_text(code);
                }
            }
            Some(Err(why)) => {
                ui.label(RichText::new(format!("No se pudo generar el código de solicitud: {why}")).color(theme::RED));
            }
            None => {}
        }
        ui.add_space(10.0);
        ui.add(egui::TextEdit::multiline(&mut st.input).id_salt((id_salt, "key")).hint_text(HINT).desired_rows(3).desired_width(f32::INFINITY).font(egui::TextStyle::Monospace));
        ui.add_space(6.0);
        if ui.add_enabled(!st.input.trim().is_empty(), egui::Button::new(RichText::new("Activar").strong())).clicked() {
            apply = Some(st.input.clone());
        }
        match &st.outcome {
            Some(Outcome::Ok(m)) => {
                ui.add_space(6.0);
                ui.label(RichText::new(m).color(theme::GREEN).strong());
            }
            Some(Outcome::Err(m)) => {
                ui.add_space(6.0);
                ui.label(RichText::new(m).color(theme::RED));
            }
            None => {}
        }
    });
    apply
}
