//! Pantalla de bloqueo: la licencia de esta PC no es válida. No hay nada más que hacer que enviar el Machine ID.

use eframe::egui::{self, RichText, Ui};

use crate::license_gate::{LicenseInfo, LicenseState};
use crate::theme;
use crate::views::reactivation::{self, ReactivationState};

/// Devuelve la clave de activación pegada cuando el operador pulsa «Activar».
pub fn show(ui: &mut Ui, info: &LicenseInfo, can_reactivate: bool, react: &mut ReactivationState) -> Option<String> {
    let mut key = None;
    let reason = match &info.state {
        LicenseState::Invalid { reason } => reason.as_str(),
        _ => "",
    };
    egui::CentralPanel::default().show(ui, |ui| {
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        ui.vertical_centered(|ui| {
            ui.add_space((ui.available_height() * 0.18).max(20.0));
            ui.label(RichText::new("TeatroCue").size(44.0).strong().color(theme::AMBER));
            ui.add_space(6.0);
            ui.label(RichText::new("Licencia no válida en esta PC").size(22.0).color(theme::RED));
            ui.add_space(14.0);
            ui.label(RichText::new(reason).color(theme::TEXT));
            ui.add_space(22.0);
            ui.label(RichText::new("Machine ID de esta PC").small().color(theme::TEXT_DIM));
            let id = info.machine_id.clone().unwrap_or_else(|| "(no disponible)".into());
            ui.label(RichText::new(&id).monospace().size(34.0).strong().color(theme::GREEN));
            ui.add_space(8.0);
            if info.machine_id.is_some() && ui.button("Copiar Machine ID").clicked() {
                ui.ctx().copy_text(id);
            }
            ui.add_space(22.0);
            ui.label(RichText::new("Envíe este Machine ID al desarrollador para recibir su license.key.\nCópielo en la carpeta de TeatroCue, junto a teatrocue-ui.exe, y vuelva a abrir la aplicación.").color(theme::TEXT_DIM));
            if can_reactivate && info.reactivable {
                ui.add_space(22.0);
                key = reactivation::show(ui, react, "lock");
            }
        });
        });
    });
    key
}
