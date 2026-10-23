//! Opciones > Preferencias y atajos: retomar desde donde se paró, silencio de Windows y las teclas de cada orden.

use eframe::egui::{self, RichText};

use crate::prefs::{captured, Command, Prefs};
use crate::theme;

#[derive(Default)]
pub struct PrefsUi {
    pub open: bool,
    /// Esperando la tecla nueva para esta orden y ranura.
    pub capturing: Option<(Command, usize)>,
    /// Último aviso (texto, es un error).
    pub message: Option<(String, bool)>,
}

/// Dibuja la ventana. Devuelve `true` si algo cambió (hay que guardar y aplicar).
pub fn show(ctx: &egui::Context, st: &mut PrefsUi, prefs: &mut Prefs, system_sounds: Option<&crate::system_sounds::Status>) -> bool {
    if !st.open {
        return false;
    }
    let mut changed = false;
    // Captura del atajo nuevo: la tecla no llega a la aplicación (con esta ventana abierta las teclas son suyas).
    let was_capturing = st.capturing.is_some();
    if let Some((cmd, slot)) = st.capturing {
        if let Some(b) = ctx.input(captured) {
            st.capturing = None;
            if b.key == egui::Key::Escape && !b.ctrl && !b.shift && !b.alt && cmd != Command::Panic {
                st.message = Some(("Sin cambios".into(), false));
            } else {
                match prefs.keys.assign(cmd, slot, b) {
                    Ok(()) => {
                        st.message = Some((format!("«{}»: {}", cmd.label(), b.display()), false));
                        changed = true;
                    }
                    Err(e) => st.message = Some((e, true)),
                }
            }
        }
    }
    let modal = egui::Modal::new(egui::Id::NULL.with("prefs_modal")).show(ctx, |ui| {
        ui.set_width(680.0_f32.min(ctx.content_rect().width() - 60.0));
        ui.set_max_height((ctx.content_rect().height() - 120.0).max(260.0));
        ui.horizontal(|ui| {
            ui.heading("Preferencias");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Cerrar").clicked() {
                    st.open = false;
                    st.capturing = None;
                }
            });
        });
        ui.separator();
        egui::ScrollArea::vertical().id_salt("prefs_scroll").auto_shrink([false, true]).show(ui, |ui| {
            ui.label(RichText::new("Ensayos").strong().size(15.0).color(theme::AMBER));
            if ui.checkbox(&mut prefs.resume_after_stop, "Retomar desde donde se paró").changed() {
                changed = true;
            }
            ui.label(
                RichText::new(
                    "Al parar con PANIC o «Parar todo», la música que sonaba queda lista en ese punto y el próximo GO la retoma desde ahí (con el pre-roll del panel de Memorias: Off, 2 s o 4 s). «Desde el inicio», en Time & Loops, la vuelve a su comienzo. Con el Bloqueo de función activo no actúa: en función cada cue arranca donde corresponde.",
                )
                .small()
                .color(theme::TEXT_DIM),
            );
            ui.label(RichText::new("Pausa (otra forma de frenar): congela la música en ese frame y la misma tecla la hace seguir.").small().color(theme::TEXT_DIM));
            ui.add_space(8.0);

            ui.label(RichText::new("Windows").strong().size(15.0).color(theme::AMBER));
            if crate::system_sounds::SUPPORTED {
                if ui.checkbox(&mut prefs.mute_system_sounds, "Silenciar los sonidos de Windows mientras TeatroCue está abierto").changed() {
                    changed = true;
                }
                ui.label(RichText::new("Notificaciones, avisos y errores de Windows no salen por los parlantes. El audio del show no se toca, y al cerrar TeatroCue todo queda como estaba.").small().color(theme::TEXT_DIM));
                if let Some(s) = system_sounds {
                    match &s.error {
                        Some(e) => ui.label(RichText::new(format!("No se pudo: {e}")).small().color(theme::RED)),
                        None if prefs.mute_system_sounds => ui.label(RichText::new(format!("En silencio en {} salida(s)", s.muted_sessions)).small().color(theme::GREEN)),
                        None => ui.label(RichText::new("Los sonidos de Windows suenan normalmente").small().color(theme::TEXT_DIM)),
                    };
                }
            } else {
                ui.label(RichText::new("Silenciar los sonidos del sistema solo está disponible en Windows.").small().color(theme::TEXT_DIM));
            }
            ui.add_space(8.0);

            ui.horizontal(|ui| {
                ui.label(RichText::new("Atajos de teclado").strong().size(15.0).color(theme::AMBER));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Restaurar todos").on_hover_text("Vuelve a las teclas de fábrica").clicked() {
                        prefs.keys.reset_all();
                        st.capturing = None;
                        st.message = Some(("Teclas de fábrica".into(), false));
                        changed = true;
                    }
                });
            });
            ui.label(RichText::new("Clic en una tecla y pulse la nueva (Esc cancela). Los números y el punto quedan para el salto a memoria «. N .».").small().color(theme::TEXT_DIM));
            match &st.message {
                Some((m, true)) => {
                    ui.label(RichText::new(m).color(theme::RED));
                }
                Some((m, false)) => {
                    ui.label(RichText::new(m).color(theme::GREEN));
                }
                None => {}
            }
            egui::Grid::new("prefs_keys").num_columns(4).spacing([10.0, 6.0]).striped(true).show(ui, |ui| {
                for cmd in Command::ALL {
                    ui.label(cmd.label());
                    let slots = prefs.keys.get(cmd);
                    for (i, b) in slots.iter().enumerate() {
                        ui.horizontal(|ui| {
                            let waiting = st.capturing == Some((cmd, i));
                            let text = if waiting {
                                RichText::new("pulse una tecla…").color(theme::AMBER)
                            } else {
                                match b {
                                    Some(b) => RichText::new(b.display()).monospace().strong().color(theme::GREEN),
                                    None => RichText::new(if i == 0 { "(sin tecla)" } else { "+ otra" }).color(theme::TEXT_DIM),
                                }
                            };
                            if ui.add(egui::Button::selectable(waiting, text).min_size(egui::vec2(120.0, 0.0))).on_hover_text(format!("Cambiar la tecla de «{}»", cmd.label())).clicked() {
                                st.capturing = if waiting { None } else { Some((cmd, i)) };
                                st.message = None;
                            }
                            if b.is_some() && ui.small_button("x").on_hover_text("Quitar esta tecla").clicked() {
                                match prefs.keys.clear(cmd, i) {
                                    Ok(()) => changed = true,
                                    Err(e) => st.message = Some((e, true)),
                                }
                            }
                        });
                    }
                    if prefs.keys.get(cmd) != cmd.defaults() {
                        if ui.small_button("Restaurar").on_hover_text("La tecla de fábrica").clicked() {
                            prefs.keys.reset(cmd);
                            changed = true;
                        }
                    } else {
                        ui.label("");
                    }
                    ui.end_row();
                }
            });
        });
    });
    // Esc mientras se espera una tecla cancela la captura, no cierra la ventana.
    if modal.should_close() && !was_capturing && st.capturing.is_none() {
        st.open = false;
    }
    changed
}
