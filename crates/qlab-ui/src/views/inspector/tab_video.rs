//! Pestaña Video: vista previa del fotograma (sincronizada con el audio cuando suena), ajuste a la pantalla de
//! proyección e información del archivo.

use eframe::egui::{self, vec2, Color32, RichText, Sense, Ui};
use qlab_protocol::show_format::{CueBody, VideoFit};

use super::{labeled, Draft, InspectorOutput, TabCtx};
use crate::theme;
use crate::video::fit_rect;
use crate::widgets;

const PREVIEW_W: f32 = 320.0;
const PREVIEW_H: f32 = 180.0;

pub fn fit_label(f: VideoFit) -> &'static str {
    match f {
        VideoFit::Fit => "Ajustar (barras si hace falta)",
        VideoFit::Fill => "Llenar (recorta los bordes)",
        VideoFit::Stretch => "Estirar",
    }
}

pub fn show(ui: &mut Ui, d: &mut Draft, cx: &mut TabCtx, out: &mut InspectorOutput) {
    let id = d.id();
    let CueBody::Video(v) = &mut d.node.body else { return };
    let fit = v.fit;
    ui.horizontal_wrapped(|ui| {
        widgets::section(ui, "Vista previa", |ui| {
            let (rect, _) = ui.allocate_exact_size(vec2(PREVIEW_W, PREVIEW_H), Sense::hover());
            ui.painter().rect_filled(rect, 2.0, Color32::BLACK);
            let player = cx.video.players.get(&id);
            match player.and_then(|p| p.texture.as_ref()) {
                Some(tex) => {
                    let size = tex.size_vec2();
                    let (r, uv) = fit_rect(rect, (size.x, size.y), fit);
                    ui.painter().image(tex.id(), r, uv, Color32::WHITE);
                }
                None => {
                    let msg = player.and_then(|p| p.error.clone()).unwrap_or_else(|| if d.node.basics.file_target.is_none() { "Asigna un archivo de vídeo en Basics".into() } else { "cargando…".into() });
                    ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, msg, egui::FontId::proportional(12.0), theme::TEXT_DIM);
                }
            }
        });
        widgets::section(ui, "Proyección", |ui| {
            labeled(ui, "Ajuste", |ui| {
                egui::ComboBox::from_id_salt("video_fit").selected_text(fit_label(v.fit)).width(210.0).show_ui(ui, |ui| {
                    for f in [VideoFit::Fit, VideoFit::Fill, VideoFit::Stretch] {
                        ui.selectable_value(&mut v.fit, f, fit_label(f));
                    }
                });
            });
            if ui.button("Abrir pantalla de proyección").on_hover_text("Ventana sin bordes para el proyector; F alterna pantalla completa y Esc la cierra").clicked() {
                out.open_projector = true;
            }
            if let Some(info) = cx.video.players.get(&id).and_then(|p| p.info.as_ref()) {
                ui.label(RichText::new(format!("{}×{} · {:.2} fps · {}", info.width, info.height, info.fps, crate::format::fmt_time(info.duration))).small().color(theme::TEXT_DIM));
            }
            if let Some(p) = cx.video.players.get(&id) {
                let (dropped, restarts) = p.stats();
                if dropped > 0 || restarts > 1 {
                    ui.label(RichText::new(format!("{dropped} cuadros descartados · {restarts} reinicios del decodificador")).small().color(theme::TEXT_DIM));
                }
            }
            ui.label(RichText::new("El audio manda: si la imagen se retrasa, se descartan cuadros para no desfasar la música.").small().color(theme::TEXT_DIM));
        });
    });
}
