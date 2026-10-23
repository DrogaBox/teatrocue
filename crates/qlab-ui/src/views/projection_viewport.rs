//! Ventana secundaria de proyección (multi-viewport de egui).
//!
//! * **Modo ventana** (por defecto): ventana normal del sistema, con borde y barra de título «Proyección —
//!   Pantalla Secundaria»; se mueve, se redimensiona y se lleva a otro monitor como cualquier otra. Además se
//!   puede arrastrar desde cualquier punto del lienzo negro.
//! * **Pantalla completa** (`F`, doble clic o el botón): sin bordes ni barra de título. `Esc` sale de la
//!   pantalla completa y, estando ya en modo ventana, cierra la ventana.
//! * Una barra flotante con los controles aparece al mover el ratón y se oculta tras 3 s de inactividad (en
//!   pantalla completa también se oculta el cursor).
//!
//! egui no enumera monitores, así que en lugar de un selector solo se muestra la resolución del monitor donde
//! está la ventana; para cambiar de pantalla se arrastra la ventana (o se pulsa `F` una vez colocada).

use std::time::{Duration, Instant};

use eframe::egui::{self, pos2, Color32, RichText};
use qlab_protocol::show_format::VideoFit;

use crate::theme;
use crate::video::fit_rect;

pub const TITLE: &str = "Proyección — Pantalla Secundaria";
/// Tiempo sin mover el ratón tras el que se oculta la barra flotante.
pub const IDLE_HIDE: Duration = Duration::from_secs(3);
/// Duración del desvanecimiento al ocultarse.
pub const FADE: Duration = Duration::from_millis(400);

/// Opacidad de la barra flotante tras `idle` sin actividad: 1 mientras hay actividad reciente, se desvanece en el
/// último tramo y vale 0 al cumplirse `IDLE_HIDE`.
pub fn overlay_alpha(idle: Duration) -> f32 {
    if idle >= IDLE_HIDE {
        0.0
    } else if idle + FADE > IDLE_HIDE {
        (IDLE_HIDE - idle).as_secs_f32() / FADE.as_secs_f32()
    } else {
        1.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EscAction {
    ExitFullscreen,
    Close,
}

/// Qué hace `Esc`: en pantalla completa vuelve al modo ventana; en modo ventana cierra.
pub fn esc_action(fullscreen: bool) -> EscAction {
    if fullscreen { EscAction::ExitFullscreen } else { EscAction::Close }
}

/// Lo que se proyecta: la textura del cuadro actual (si hay) y cómo se ajusta a la pantalla.
pub struct Content {
    pub texture: Option<egui::TextureHandle>,
    pub fit: VideoFit,
}

/// Tamaño inicial de la ventana: 960x540 (16:9), y nunca más del 80 % del monitor donde está la ventana principal (en una
/// pantalla de 1366x768 cabe sobrado). Sin saber el tamaño del monitor, 800x450.
pub fn safe_size(monitor: Option<egui::Vec2>) -> egui::Vec2 {
    let Some(m) = monitor.filter(|m| m.x > 100.0 && m.y > 100.0) else { return egui::vec2(800.0, 450.0) };
    let mut w = 960.0f32.min(m.x * 0.8);
    let mut h = w * 9.0 / 16.0;
    if h > m.y * 0.8 {
        h = m.y * 0.8;
        w = h * 16.0 / 9.0;
    }
    egui::vec2(w.floor().max(320.0), h.floor().max(180.0))
}

/// Posición inicial: centrada sobre la ventana principal (así cae en el mismo monitor que ella) y, si hay datos del monitor,
/// recortada para que quede entera a la vista. `None` = que la coloque el sistema operativo.
pub fn safe_position(size: egui::Vec2, main_outer: Option<egui::Rect>, monitor: Option<egui::Vec2>) -> Option<egui::Pos2> {
    let main = main_outer.filter(|r| r.is_finite() && r.width() > 50.0 && r.height() > 50.0)?;
    let mut p = main.center() - size / 2.0;
    if let Some(m) = monitor.filter(|m| m.x > 100.0 && m.y > 100.0) {
        p.x = p.x.clamp(0.0, (m.x - size.x).max(0.0));
        p.y = p.y.clamp(0.0, (m.y - size.y).max(0.0));
    }
    Some(pos2(p.x.max(0.0), p.y.max(0.0)))
}

/// Durante este tiempo tras abrirla se sigue pidiendo repintado de la ventana: así Windows y wgpu crean la superficie al
/// instante aunque el resto de la aplicación esté en reposo (0 % de CPU).
const SURFACE_WARMUP: Duration = Duration::from_millis(1500);

pub struct Projection {
    pub open: bool,
    pub fullscreen: bool,
    last_activity: Instant,
    opened_at: Instant,
    /// Tamaño y posición con que se abrió (se calculan una sola vez, al abrir: repetirlos cada fotograma movería la ventana).
    geometry: Option<(egui::Vec2, Option<egui::Pos2>)>,
}

impl Default for Projection {
    fn default() -> Self {
        Self { open: false, fullscreen: false, last_activity: Instant::now(), opened_at: Instant::now(), geometry: None }
    }
}

impl Projection {
    /// Abre la ventana (siempre en modo ventana, con bordes) o la cierra.
    pub fn toggle_open(&mut self) {
        self.open = !self.open;
        if self.open {
            self.fullscreen = false;
            self.last_activity = Instant::now();
            self.opened_at = Instant::now();
            self.geometry = None;
        }
    }

    pub fn toggle_fullscreen(&mut self) {
        self.fullscreen = !self.fullscreen;
        self.last_activity = Instant::now();
    }

    pub fn on_escape(&mut self) {
        match esc_action(self.fullscreen) {
            EscAction::ExitFullscreen => self.fullscreen = false,
            EscAction::Close => self.open = false,
        }
        self.last_activity = Instant::now();
    }

    pub fn id() -> egui::ViewportId {
        egui::ViewportId::from_hash_of("qlab_projector")
    }

    /// Dibuja la ventana (si está abierta) y aplica lo que pida el usuario en ella.
    pub fn show(&mut self, ctx: &egui::Context, content: &Content) {
        if !self.open {
            return;
        }
        let id = Self::id();
        let (size, pos) = *self.geometry.get_or_insert_with(|| {
            let (monitor, outer) = ctx.input(|i| (i.viewport().monitor_size, i.viewport().outer_rect));
            let size = safe_size(monitor);
            (size, safe_position(size, outer, monitor))
        });
        // `with_decorations` y `with_fullscreen` se repiten en cada fotograma: egui aplica solo lo que cambia.
        let mut builder = egui::ViewportBuilder::default()
            .with_title(TITLE)
            .with_inner_size(size)
            .with_min_inner_size([320.0, 180.0])
            .with_resizable(true)
            .with_decorations(!self.fullscreen)
            .with_fullscreen(self.fullscreen);
        if let Some(p) = pos {
            builder = builder.with_position(p);
        }
        if self.opened_at.elapsed() < SURFACE_WARMUP {
            ctx.request_repaint();
            ctx.request_repaint_of(id);
        }

        let fullscreen = self.fullscreen;
        let idle = self.last_activity.elapsed();
        let mut ev = Events::default();
        ctx.show_viewport_immediate(id, builder, |root, class| {
            let ctx = root.ctx().clone();
            let ctx = &ctx;
            if class == egui::ViewportClass::EmbeddedWindow {
                // Sin soporte de varias ventanas (y en las pruebas): una ventana flotante dentro de la principal.
                let mut open = true;
                egui::Window::new("Proyección").open(&mut open).default_size([480.0, 270.0]).show(ctx, |ui| {
                    let (rect, _) = ui.allocate_exact_size(ui.available_size(), egui::Sense::hover());
                    paint(ui, rect, content);
                });
                ev.close |= !open;
                return;
            }
            let (esc, f, close_req, moved, monitor) = ctx.input(|i| {
                (i.key_pressed(egui::Key::Escape), i.key_pressed(egui::Key::F), i.viewport().close_requested(), i.pointer.delta() != egui::Vec2::ZERO || i.pointer.any_pressed(), i.viewport().monitor_size)
            });
            ev.esc |= esc;
            ev.toggle_fs |= f;
            ev.close |= close_req;
            ev.activity |= moved || esc || f;

            egui::CentralPanel::default().frame(egui::Frame::new().fill(Color32::BLACK)).show(root, |ui| {
                let (rect, resp) = ui.allocate_exact_size(ui.available_size(), egui::Sense::click_and_drag());
                paint(ui, rect, content);
                if resp.double_clicked() {
                    ev.toggle_fs = true;
                }
                // Arrastre directo desde cualquier punto del fondo: mueve la ventana del sistema.
                if !fullscreen && resp.drag_started_by(egui::PointerButton::Primary) {
                    ctx.send_viewport_cmd_to(id, egui::ViewportCommand::StartDrag);
                }
            });

            let alpha = overlay_alpha(idle);
            if alpha > 0.0 {
                egui::Area::new(egui::Id::NULL.with("projection_bar")).order(egui::Order::Foreground).fixed_pos(pos2(0.0, 0.0)).show(ctx, |ui| {
                    ui.set_opacity(alpha);
                    let width = ctx.content_rect().width();
                    egui::Frame::new().fill(Color32::from_black_alpha(170)).inner_margin(egui::Margin::symmetric(10, 6)).show(ui, |ui| {
                        ui.set_width(width - 20.0);
                        ui.horizontal(|ui| {
                            if ui.button(if fullscreen { "Salir de pantalla completa (F)" } else { "Pantalla completa (F)" }).clicked() {
                                ev.toggle_fs = true;
                            }
                            if let Some(m) = monitor {
                                // egui no enumera monitores: se informa del actual; para cambiar se arrastra la ventana.
                                ui.label(RichText::new(format!("Monitor: {}×{}", m.x as u32, m.y as u32)).small().color(theme::TEXT_DIM))
                                    .on_hover_text("Arrastra la ventana a otro monitor y pulsa F");
                            }
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui.button(if fullscreen { "Cerrar (Esc: salir de pantalla completa)" } else { "Cerrar (Esc)" }).clicked() {
                                    ev.close = true;
                                }
                            });
                        });
                    });
                });
                // Vuelve a dibujar para completar el desvanecimiento y ocultar la barra.
                ctx.request_repaint_after((IDLE_HIDE.saturating_sub(idle)).max(Duration::from_millis(16)).min(Duration::from_millis(100)));
            } else if fullscreen {
                ctx.set_cursor_icon(egui::CursorIcon::None);
            }
        });

        if ev.activity {
            self.last_activity = Instant::now();
        }
        if ev.toggle_fs {
            self.toggle_fullscreen();
        }
        // `Esc` y el botón «Cerrar» cierran; la X del sistema también.
        if ev.esc {
            self.on_escape();
        } else if ev.close {
            self.open = false;
        }
    }
}

#[derive(Default)]
struct Events {
    esc: bool,
    toggle_fs: bool,
    close: bool,
    activity: bool,
}

fn paint(ui: &egui::Ui, rect: egui::Rect, content: &Content) {
    ui.painter().rect_filled(rect, 0.0, Color32::BLACK);
    if let Some(t) = &content.texture {
        let size = t.size_vec2();
        let (r, uv) = fit_rect(rect, (size.x, size.y), content.fit);
        ui.painter().image(t.id(), r, uv, Color32::WHITE);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bar_stays_for_three_seconds_then_fades_out() {
        assert_eq!(overlay_alpha(Duration::ZERO), 1.0);
        assert_eq!(overlay_alpha(Duration::from_millis(2_500)), 1.0);
        let mid = overlay_alpha(Duration::from_millis(2_800));
        assert!((mid - 0.5).abs() < 0.01, "{mid}");
        assert_eq!(overlay_alpha(IDLE_HIDE), 0.0);
        assert_eq!(overlay_alpha(Duration::from_secs(60)), 0.0);
        // Nunca sube al avanzar el tiempo sin actividad.
        let mut prev = 1.0f32;
        for ms in (0..3_500).step_by(50) {
            let a = overlay_alpha(Duration::from_millis(ms));
            assert!(a <= prev + 1e-6 && (0.0..=1.0).contains(&a));
            prev = a;
        }
    }

    #[test]
    fn escape_leaves_fullscreen_first_and_closes_only_from_window_mode() {
        let mut p = Projection::default();
        p.toggle_open();
        assert!(p.open && !p.fullscreen, "se abre en modo ventana, con bordes");
        p.toggle_fullscreen();
        assert!(p.fullscreen);
        p.on_escape();
        assert!(p.open && !p.fullscreen, "Esc en pantalla completa vuelve a la ventana");
        p.on_escape();
        assert!(!p.open, "Esc en modo ventana la cierra");
        assert_eq!((esc_action(true), esc_action(false)), (EscAction::ExitFullscreen, EscAction::Close));
    }

    #[test]
    fn reopening_always_starts_in_window_mode() {
        let mut p = Projection::default();
        p.toggle_open();
        p.toggle_fullscreen();
        p.toggle_open(); // cerrar estando en pantalla completa
        assert!(!p.open);
        p.toggle_open();
        assert!(p.open && !p.fullscreen);
    }

    #[test]
    fn the_window_draws_without_a_texture_and_with_one_in_both_modes() {
        let ctx = egui::Context::default();
        let mut p = Projection::default();
        p.toggle_open();
        let blank = Content { texture: None, fit: VideoFit::Fit };
        for fullscreen in [false, true] {
            p.fullscreen = fullscreen;
            for _ in 0..3 {
                ctx.run_pass(egui::RawInput::default(), |ctx| p.show(ctx, &blank)).textures_delta.clear();
            }
        }
        let tex = ctx.load_texture("t", egui::ColorImage::filled([4, 2], Color32::RED), Default::default());
        let with = Content { texture: Some(tex), fit: VideoFit::Fill };
        ctx.run_pass(egui::RawInput::default(), |ctx| p.show(ctx, &with)).textures_delta.clear();
        assert!(p.open, "dibujar no la cierra");
    }

    #[test]
    fn the_window_opens_at_a_safe_size_and_centered_on_the_main_window_never_off_screen() {
        use egui::{pos2, vec2, Rect};
        // 1366x768: 960x540 cabe sobrado (80 % = 1092x614).
        assert_eq!(safe_size(Some(vec2(1366.0, 768.0))), vec2(960.0, 540.0));
        // Monitores chicos: nunca más del 80 % y siempre 16:9.
        let s = safe_size(Some(vec2(1024.0, 600.0)));
        assert!(s.x <= 1024.0 * 0.8 && s.y <= 600.0 * 0.8 && (s.x / s.y - 16.0 / 9.0).abs() < 0.03, "{s:?}");
        let s = safe_size(Some(vec2(800.0, 480.0)));
        assert!(s.x <= 640.0 && s.y <= 384.0, "{s:?}");
        assert_eq!(safe_size(None), vec2(800.0, 450.0));
        assert_eq!(safe_size(Some(vec2(0.0, 0.0))), vec2(800.0, 450.0), "datos de monitor absurdos");
        // Centrada sobre la ventana principal.
        let size = vec2(960.0, 540.0);
        let main = Rect::from_min_size(pos2(0.0, 0.0), vec2(1366.0, 768.0));
        assert_eq!(safe_position(size, Some(main), Some(vec2(1366.0, 768.0))), Some(pos2(203.0, 114.0)));
        // Una ventana principal que se sale de la pantalla no arrastra a la de proyección fuera: se recorta.
        let off = Rect::from_min_size(pos2(1200.0, 600.0), vec2(900.0, 500.0));
        let p = safe_position(size, Some(off), Some(vec2(1366.0, 768.0))).unwrap();
        assert!(p.x + size.x <= 1366.0 && p.y + size.y <= 768.0 && p.x >= 0.0 && p.y >= 0.0, "{p:?}");
        let neg = Rect::from_min_size(pos2(-600.0, -300.0), vec2(500.0, 300.0));
        let p = safe_position(size, Some(neg), Some(vec2(1366.0, 768.0))).unwrap();
        assert!(p.x >= 0.0 && p.y >= 0.0, "{p:?}");
        // Sin datos de la ventana principal: que la ubique el sistema operativo.
        assert_eq!(safe_position(size, None, Some(vec2(1366.0, 768.0))), None);
    }
}
