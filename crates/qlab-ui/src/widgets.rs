//! Widgets propios: vúmetros, barras de progreso y celdas de nivel.

use eframe::egui::{self, vec2, Align2, Color32, CornerRadius, FontId, Rect, Sense, Stroke, StrokeKind, Ui};

use crate::format::{db_to_fader, fader_to_db, fmt_db, parse_db, MAX_DB, MIN_DB};
use crate::model::MeterState;
use crate::theme;

const RADIUS: CornerRadius = CornerRadius::same(2);

/// Velocidad del desplazamiento de un texto que no entra (px/s) y pausa en cada extremo (s).
pub const MARQUEE_SPEED: f32 = 40.0;
pub const MARQUEE_PAUSE: f32 = 1.4;

/// Cuánto se corre el texto a la hora `t` (s) si sobra `overflow` px: quieto al principio, avanza hasta mostrar el final, se queda
/// quieto y vuelve, en un vaivén suave (sin saltos).
pub fn marquee_offset(t: f64, overflow: f32, speed: f32, pause: f32) -> f32 {
    if overflow <= 0.0 || speed <= 0.0 {
        return 0.0;
    }
    let travel = overflow / speed;
    let period = 2.0 * (pause + travel);
    let x = (t.rem_euclid(f64::from(period))) as f32;
    if x < pause {
        0.0
    } else if x < pause + travel {
        (x - pause) * speed
    } else if x < 2.0 * pause + travel {
        overflow
    } else {
        overflow - (x - 2.0 * pause - travel) * speed
    }
    .clamp(0.0, overflow)
}

/// Duración de una vuelta completa (ida, pausa, vuelta, pausa) para un texto al que le sobran `overflow` px.
fn period_of(overflow: f32) -> f64 {
    f64::from(2.0 * (MARQUEE_PAUSE + overflow / MARQUEE_SPEED))
}

/// El texto más largo que entra en `width` con «…» al final (para cuando no se anima).
fn elide_to_width(ui: &Ui, text: &str, font: &FontId, width: f32) -> String {
    let fits = |t: &str| ui.painter().layout_no_wrap(t.to_string(), font.clone(), Color32::WHITE).size().x <= width;
    if fits(text) {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    let (mut lo, mut hi) = (0usize, chars.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        let cand: String = chars[..mid].iter().collect::<String>() + "…";
        if fits(&cand) {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    chars[..lo].iter().collect::<String>() + "…"
}

/// Vueltas completas (ida y vuelta) que da un texto largo cuando aparece o cambia, antes de quedarse quieto con «…».
pub const MARQUEE_CYCLES: f64 = 3.0;

/// Texto de una línea en un ancho fijo. Si no entra y `animate` es verdadero, **se desplaza de lado a lado** para que se lea
/// entero (como los nombres largos en una consola): da [`MARQUEE_CYCLES`] vueltas desde que aparece (o desde que `animate`
/// pasa a verdadero) y luego queda quieto cortado con «…», para que la ventana en reposo no gaste CPU; con el ratón encima
/// se mueve todo el rato. Nunca se parte en dos líneas ni empuja a los demás controles. Para la accesibilidad (y las pruebas)
/// su nombre es siempre el texto completo.
pub fn marquee(ui: &mut Ui, text: &str, font: FontId, color: Color32, width: f32, animate: bool) -> egui::Response {
    let height = ui.fonts_mut(|f| f.row_height(&font));
    let width = width.max(8.0);
    let (rect, resp) = ui.allocate_exact_size(vec2(width, height), Sense::hover());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Label, true, text));
    if !ui.is_rect_visible(rect) {
        return resp;
    }
    let galley = ui.painter().layout_no_wrap(text.to_string(), font.clone(), color);
    let overflow = galley.size().x - width;
    let painter = ui.painter_at(rect.expand2(vec2(0.0, 2.0)));
    if overflow <= 0.5 {
        painter.galley(rect.left_top(), galley, color);
        return resp;
    }
    // Cuándo empezó a moverse este texto en este sitio (un texto nuevo vuelve a empezar).
    let key = resp.id.with(("marquee", text));
    let now = ui.input(|i| i.time);
    let hovered = resp.hovered();
    let since = if animate || hovered {
        let start = ui.data_mut(|d| *d.get_temp_mut_or_insert_with(key, || now));
        let start = if hovered && now - start >= period_of(overflow) {
            // Con el ratón encima no se para: se reengancha al terminar cada vuelta, sin saltos.
            let s = now - (now - start).rem_euclid(period_of(overflow));
            ui.data_mut(|d| d.insert_temp(key, s));
            s
        } else {
            start
        };
        Some(now - start)
    } else {
        ui.data_mut(|d| d.remove::<f64>(key));
        None
    };
    if let Some(t) = since.filter(|&t| hovered || t < MARQUEE_CYCLES * period_of(overflow)) {
        let off = marquee_offset(t, overflow, MARQUEE_SPEED, MARQUEE_PAUSE);
        painter.galley(rect.left_top() - vec2(off, 0.0), galley, color);
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(33));
    } else {
        let short = elide_to_width(ui, text, &font, width);
        painter.text(rect.left_top(), Align2::LEFT_TOP, short, font, color);
    }
    resp
}

/// Barra de progreso con texto centrado (`frac` 0..=1).
pub fn progress_bar(ui: &mut Ui, size: egui::Vec2, frac: f32, fill: Color32, text: &str, text_color: Color32) {
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    if !ui.is_rect_visible(rect) {
        return;
    }
    let p = ui.painter();
    p.rect_filled(rect, RADIUS, theme::BG);
    let frac = if frac.is_finite() { frac.clamp(0.0, 1.0) } else { 0.0 };
    if frac > 0.0 {
        p.rect_filled(Rect::from_min_size(rect.min, vec2(rect.width() * frac, rect.height())), RADIUS, fill);
    }
    p.rect_stroke(rect, RADIUS, Stroke::new(1.0, theme::GRID), StrokeKind::Inside);
    if !text.is_empty() {
        p.text(rect.center(), Align2::CENTER_CENTER, text, FontId::monospace(12.0), text_color);
    }
}

/// Color del segmento del vúmetro a `db`: verde hasta -12, ámbar hasta -3, rojo por encima.
pub fn meter_color(db: f32) -> Color32 {
    if db > -3.0 {
        theme::RED
    } else if db > -12.0 {
        theme::AMBER
    } else {
        theme::GREEN
    }
}

/// Vúmetro horizontal de -60 a 0 dBFS con pico mantenido y luz de clip.
pub fn meter_bar(ui: &mut Ui, size: egui::Vec2, m: &MeterState) {
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    if !ui.is_rect_visible(rect) {
        return;
    }
    let p = ui.painter();
    p.rect_filled(rect, RADIUS, theme::BG);
    let to_x = |db: f32| rect.left() + rect.width() * ((db + 60.0) / 60.0).clamp(0.0, 1.0);
    let level = m.level_db;
    if level > -60.0 {
        // Por tramos para que el color cambie a lo largo de la barra, como un vúmetro de segmentos.
        let mut from = -60.0;
        for to in [-12.0, -3.0, 0.0] {
            let seg_to = level.min(to);
            if seg_to > from {
                let r = Rect::from_min_max(egui::pos2(to_x(from), rect.top()), egui::pos2(to_x(seg_to), rect.bottom()));
                p.rect_filled(r, 0.0, meter_color(seg_to - 0.01));
            }
            from = to;
        }
    }
    if m.hold_db > -60.0 {
        let x = to_x(m.hold_db);
        p.line_segment([egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())], Stroke::new(2.0, meter_color(m.hold_db)));
    }
    p.rect_stroke(rect, RADIUS, Stroke::new(1.0, theme::GRID), StrokeKind::Inside);
    if m.clip_left > 0.0 {
        let r = Rect::from_min_size(egui::pos2(rect.right() - 6.0, rect.top()), vec2(6.0, rect.height()));
        p.rect_filled(r, RADIUS, theme::RED);
    }
}

pub fn led(ui: &mut Ui, color: Color32, on: bool, tooltip: &str) {
    let (rect, resp) = ui.allocate_exact_size(vec2(12.0, 12.0), Sense::hover());
    let c = if on { color } else { theme::GRID };
    ui.painter().circle_filled(rect.center(), 5.0, c);
    if !tooltip.is_empty() {
        resp.on_hover_text(tooltip);
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LevelResponse {
    /// El valor cambió en este fotograma (arrastre o texto).
    pub changed: bool,
    /// El usuario está arrastrando: hay que mandar el valor en vivo.
    pub live: bool,
    /// Terminó la edición (soltar el ratón o confirmar el texto): conviene guardar.
    pub commit: bool,
}

/// Celda de nivel: caja de dB (arrastrar o escribir) y un fader fino debajo.
/// `tint` colorea el borde de las celdas con gang.
pub fn level_cell(ui: &mut Ui, db: &mut f32, width: f32, tint: Option<Color32>, enabled: bool) -> LevelResponse {
    let mut out = LevelResponse::default();
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 1.0;
        ui.set_width(width);
        let before = *db;
        let r = ui.add_enabled(
            enabled,
            egui::DragValue::new(db)
                .range(MIN_DB..=MAX_DB)
                .speed(0.15)
                .min_decimals(1)
                .custom_formatter(|v, _| fmt_db(v as f32))
                .custom_parser(|s| parse_db(s).map(f64::from)),
        );
        if r.changed() || *db != before {
            out.changed = true;
        }
        out.live |= r.dragged();
        out.commit |= r.drag_stopped() || r.lost_focus();

        // Fader: clic o arrastre fijan el nivel de forma absoluta (Mayús = ajuste fino relativo).
        let (rect, f) = ui.allocate_exact_size(vec2(width, 7.0), Sense::click_and_drag());
        let fine = ui.input(|i| i.modifiers.shift);
        if enabled && (f.dragged() || f.clicked()) {
            if let Some(pos) = f.interact_pointer_pos() {
                let target = ((pos.x - rect.left()) / rect.width()).clamp(0.0, 1.0);
                let new_db = if fine && f.dragged() {
                    let cur = db_to_fader(*db);
                    fader_to_db(cur + f.drag_delta().x / rect.width() * 0.1)
                } else {
                    fader_to_db(target)
                };
                let new_db = if new_db <= MIN_DB + 0.05 { MIN_DB } else { new_db };
                if (new_db - *db).abs() > f32::EPSILON {
                    *db = new_db;
                    out.changed = true;
                }
            }
        }
        out.live |= f.dragged();
        out.commit |= f.drag_stopped() || f.clicked();
        if ui.is_rect_visible(rect) {
            let p = ui.painter();
            p.rect_filled(rect, 1.0, theme::BG);
            let x = rect.left() + rect.width() * db_to_fader(*db);
            let fill = if *db <= MIN_DB { theme::GRID } else { meter_color(*db - 6.0) };
            p.rect_filled(Rect::from_min_max(rect.min, egui::pos2(x.max(rect.left()), rect.bottom())), 1.0, fill);
            // Marca de 0 dB.
            let zero = rect.left() + rect.width() * db_to_fader(0.0);
            p.line_segment([egui::pos2(zero, rect.top()), egui::pos2(zero, rect.bottom())], Stroke::new(1.0, theme::TEXT_DIM));
            p.rect_stroke(rect, 1.0, Stroke::new(1.0, tint.unwrap_or(theme::GRID)), StrokeKind::Inside);
        }
    });
    out
}

/// Recuadro de sección con título, como los grupos del Inspector de QLab.
pub fn section(ui: &mut Ui, title: &str, add: impl FnOnce(&mut Ui)) {
    egui::Frame::group(ui.style()).fill(theme::PANEL).show(ui, |ui| {
        ui.label(egui::RichText::new(title).color(theme::AMBER).strong());
        ui.add_space(2.0);
        add(ui);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meter_colors_follow_the_usual_zones() {
        assert_eq!(meter_color(-30.0), theme::GREEN);
        assert_eq!(meter_color(-6.0), theme::AMBER);
        assert_eq!(meter_color(-1.0), theme::RED);
    }

    #[test]
    fn widgets_render_headless_without_panicking() {
        let ctx = egui::Context::default();
        theme::apply(&ctx);
        let mut db = -6.0f32;
        let mut meter = MeterState::default();
        meter.push(0.5, 0.016);
        let out = ctx.run_ui(Default::default(), |ui| {
            progress_bar(ui, vec2(100.0, 14.0), 0.4, theme::GREEN, "00:01.00", theme::TEXT);
            progress_bar(ui, vec2(100.0, 14.0), f32::NAN, theme::GREEN, "", theme::TEXT);
            meter_bar(ui, vec2(120.0, 10.0), &meter);
            led(ui, theme::RED, true, "xruns");
            let r = level_cell(ui, &mut db, 60.0, Some(theme::CYAN), true);
            assert_eq!(r, LevelResponse::default(), "sin interacción no hay cambios");
            level_cell(ui, &mut db, 60.0, None, false);
            section(ui, "Sección", |ui| {
                ui.label("x");
            });
        });
        out.drop_without_applying_deltas();
        assert_eq!(db, -6.0);
    }

    #[test]
    fn the_marquee_pauses_at_both_ends_and_moves_smoothly_between_them() {
        let (overflow, speed, pause) = (80.0, 40.0, 1.0); // 2 s de recorrido, ciclo de 6 s
        assert_eq!(marquee_offset(0.0, overflow, speed, pause), 0.0);
        assert_eq!(marquee_offset(0.9, overflow, speed, pause), 0.0, "pausa al principio");
        assert!((marquee_offset(2.0, overflow, speed, pause) - 40.0).abs() < 1e-3, "a mitad de camino");
        assert_eq!(marquee_offset(3.5, overflow, speed, pause), 80.0, "pausa al final, mostrando el final del texto");
        assert!((marquee_offset(5.0, overflow, speed, pause) - 40.0).abs() < 1e-3, "vuelve");
        assert!((marquee_offset(6.0, overflow, speed, pause)).abs() < 1e-3, "y se repite");
        // Sin saltos: entre dos instantes muy cercanos el texto se mueve como mucho velocidad × tiempo.
        let mut prev = marquee_offset(0.0, overflow, speed, pause);
        for k in 1..6_000 {
            let now = marquee_offset(f64::from(k) * 0.001, overflow, speed, pause);
            assert!((now - prev).abs() <= speed * 0.001 + 1e-3, "salto en t={}", f64::from(k) * 0.001);
            prev = now;
        }
        assert_eq!(marquee_offset(3.0, 0.0, speed, pause), 0.0, "si entra, quieto");
    }

    #[test]
    fn a_long_text_scrolls_a_few_times_then_rests_so_an_idle_window_uses_no_cpu_and_hovering_wakes_it() {
        let ctx = egui::Context::default();
        theme::apply(&ctx);
        let long = "(2 Hour Nature Relaxation Video) A Day in the Forest with Birdsong";
        let frame = |t: f64, pointer: Option<egui::Pos2>| {
            let mut input = egui::RawInput { time: Some(t), ..Default::default() };
            input.screen_rect = Some(Rect::from_min_size(egui::Pos2::ZERO, vec2(400.0, 100.0)));
            if let Some(p) = pointer {
                input.events.push(egui::Event::PointerMoved(p));
            }
            let mut rect = Rect::NOTHING;
            let out = ctx.run_ui(input, |ui| {
                rect = marquee(ui, long, FontId::proportional(14.0), theme::TEXT, 120.0, true).rect;
                marquee(ui, "corto", FontId::proportional(14.0), theme::TEXT, 120.0, true);
            });
            let delay = out.viewport_output.get(&egui::ViewportId::ROOT).map_or(std::time::Duration::MAX, |v| v.repaint_delay);
            out.drop_without_applying_deltas();
            (rect, delay)
        };
        let (rect, d0) = frame(0.0, None);
        assert_eq!(rect.size(), vec2(120.0, ctx.fonts_mut(|f| f.row_height(&FontId::proportional(14.0)))), "ancho fijo, una línea");
        assert!(d0 <= std::time::Duration::from_millis(40), "al aparecer se mueve: {d0:?}");
        let (_, d1) = frame(5.0, None);
        assert!(d1 <= std::time::Duration::from_millis(40), "sigue en las primeras vueltas: {d1:?}");
        let (_, rest) = frame(600.0, None);
        assert_eq!(rest, std::time::Duration::MAX, "pasadas las vueltas, quieto y sin pedir repintados");
        let (_, _) = frame(601.0, Some(rect.center()));
        let (_, hover) = frame(601.1, Some(rect.center()));
        assert!(hover <= std::time::Duration::from_millis(40), "con el ratón encima vuelve a moverse: {hover:?}");
        let (_, hover_late) = frame(900.0, Some(rect.center()));
        assert!(hover_late <= std::time::Duration::from_millis(40), "y no se para mientras siga encima: {hover_late:?}");
    }
}

// ---------------------------------------------------------------------------------------------
// Botones de icono dibujados (la fuente por defecto no trae flechas ni papelera)
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icon {
    Up,
    Down,
    Trash,
}

/// Botón cuadrado con un icono vectorial. Devuelve la respuesta; desactivado no reacciona.
pub fn icon_button(ui: &mut Ui, icon: Icon, tip: &str, enabled: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(28.0, 24.0), if enabled { Sense::click() } else { Sense::hover() });
    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        let hovered = enabled && resp.hovered();
        p.rect_filled(rect, 3.0, if hovered { theme::PANEL_RAISED } else { theme::BG });
        p.rect_stroke(rect, 3.0, Stroke::new(1.0, theme::GRID), StrokeKind::Inside);
        let color = match (enabled, icon) {
            (false, _) => theme::TEXT_DIM.gamma_multiply(0.5),
            (true, Icon::Trash) => theme::RED,
            (true, _) => theme::AMBER,
        };
        let c = rect.center();
        match icon {
            Icon::Up => {
                p.add(egui::Shape::convex_polygon(vec![egui::pos2(c.x - 5.0, c.y + 3.5), egui::pos2(c.x + 5.0, c.y + 3.5), egui::pos2(c.x, c.y - 4.5)], color, Stroke::NONE));
            }
            Icon::Down => {
                p.add(egui::Shape::convex_polygon(vec![egui::pos2(c.x - 5.0, c.y - 3.5), egui::pos2(c.x + 5.0, c.y - 3.5), egui::pos2(c.x, c.y + 4.5)], color, Stroke::NONE));
            }
            Icon::Trash => {
                let s = Stroke::new(1.5, color);
                // Tapa, asa y cuerpo con dos ranuras.
                p.line_segment([egui::pos2(c.x - 6.0, c.y - 4.0), egui::pos2(c.x + 6.0, c.y - 4.0)], s);
                p.line_segment([egui::pos2(c.x - 2.0, c.y - 4.0), egui::pos2(c.x - 2.0, c.y - 6.5)], s);
                p.line_segment([egui::pos2(c.x + 2.0, c.y - 4.0), egui::pos2(c.x + 2.0, c.y - 6.5)], s);
                p.line_segment([egui::pos2(c.x - 2.0, c.y - 6.5), egui::pos2(c.x + 2.0, c.y - 6.5)], s);
                p.rect_stroke(Rect::from_min_max(egui::pos2(c.x - 5.0, c.y - 4.0), egui::pos2(c.x + 5.0, c.y + 6.5)), 1.0, s, StrokeKind::Inside);
                p.line_segment([egui::pos2(c.x - 1.5, c.y - 1.5), egui::pos2(c.x - 1.5, c.y + 4.0)], s);
                p.line_segment([egui::pos2(c.x + 1.5, c.y - 1.5), egui::pos2(c.x + 1.5, c.y + 4.0)], s);
            }
        }
    }
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, enabled, tip));
    resp.on_hover_text(tip)

}
