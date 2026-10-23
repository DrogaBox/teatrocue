//! Pestaña EQ: ecualizador paramétrico de 4 bandas por cue. Curva de respuesta combinada (eje horizontal
//! logarítmico de 20 Hz a 20 kHz, vertical de −24 a +12 dB), un punto arrastrable por banda y una tabla numérica
//! con frecuencia, ganancia, Q y tipo de filtro.
//!
//! Los cambios van al borrador de la cue (se guardan solos) y de ahí al motor como un comando en vivo; no hay
//! temporizadores ni repintados continuos: la pantalla solo se dibuja cuando el operador interactúa.

use eframe::egui::{self, pos2, vec2, Align2, Color32, FontId, Rect, RichText, Sense, Shape, Stroke, Ui};
use qlab_protocol::cue_dto::{AudioEqData, EqBandData, EqFilterType, EQ_BANDS, EQ_MAX_GAIN_DB, EQ_MAX_HZ, EQ_MAX_Q, EQ_MIN_GAIN_DB, EQ_MIN_HZ, EQ_MIN_Q};

use super::Draft;
use crate::theme;

/// Frecuencia de muestreo con la que se calcula la curva dibujada (la forma apenas cambia con otra).
const CURVE_SR: f32 = 48_000.0;
/// Alto del gráfico: lo justo para que las 4 bandas de la tabla queden enteras en el Inspector de una pantalla chica.
const CANVAS_HEIGHT: f32 = 125.0;
const MARGIN_LEFT: f32 = 38.0;
const MARGIN_BOTTOM: f32 = 18.0;
const POINT_HIT: f32 = 20.0;
const BAND_COLORS: [Color32; EQ_BANDS] = [theme::CYAN, theme::GREEN, theme::AMBER, Color32::from_rgb(0xb0, 0x7a, 0xff)];
const FREQ_TICKS: [f32; 10] = [20.0, 50.0, 100.0, 200.0, 500.0, 1_000.0, 2_000.0, 5_000.0, 10_000.0, 20_000.0];
const GAIN_TICKS: [f32; 7] = [12.0, 6.0, 0.0, -6.0, -12.0, -18.0, -24.0];

// -- Conversión entre valores y píxeles (puras, con pruebas) -----------------------------------------

/// X de una frecuencia en un eje logarítmico de 20 Hz a 20 kHz.
pub fn x_of_freq(freq: f32, left: f32, width: f32) -> f32 {
    left + (freq.clamp(EQ_MIN_HZ, EQ_MAX_HZ) / EQ_MIN_HZ).ln() / (EQ_MAX_HZ / EQ_MIN_HZ).ln() * width
}

pub fn freq_of_x(x: f32, left: f32, width: f32) -> f32 {
    EQ_MIN_HZ * (EQ_MAX_HZ / EQ_MIN_HZ).powf(((x - left) / width.max(1.0)).clamp(0.0, 1.0))
}

/// Y de una ganancia: +12 dB arriba, −24 dB abajo.
pub fn y_of_gain(gain: f32, top: f32, height: f32) -> f32 {
    top + (EQ_MAX_GAIN_DB - gain.clamp(EQ_MIN_GAIN_DB, EQ_MAX_GAIN_DB)) / (EQ_MAX_GAIN_DB - EQ_MIN_GAIN_DB) * height
}

pub fn gain_of_y(y: f32, top: f32, height: f32) -> f32 {
    EQ_MAX_GAIN_DB - ((y - top) / height.max(1.0)).clamp(0.0, 1.0) * (EQ_MAX_GAIN_DB - EQ_MIN_GAIN_DB)
}

/// Q tras mover la rueda `scroll_y` puntos (hacia arriba = más estrecha). Cada «muesca» multiplica ~1,17.
pub fn scale_q(q: f32, scroll_y: f32) -> f32 {
    (q * (scroll_y * 0.0025).exp()).clamp(EQ_MIN_Q, EQ_MAX_Q)
}

/// Frecuencia, ganancia y Q llevados a su rejilla de ajuste fino al arrastrar (0,1 dB; frecuencia con 3 cifras).
pub fn snapped(freq: f32, gain: f32) -> (f32, f32) {
    let f = freq.clamp(EQ_MIN_HZ, EQ_MAX_HZ);
    let mag = 10f32.powf(f.log10().floor() - 2.0);
    ((f / mag).round() * mag, (gain * 10.0).round() / 10.0)
}

fn tick_label(f: f32) -> String {
    if f >= 1_000.0 { format!("{}k", f / 1_000.0) } else { format!("{f}") }
}

/// Parámetros del motor para dibujar la curva.
fn engine_eq(eq: &AudioEqData) -> qlab_engine::biquad::EqParams {
    qlab_core::cue::eq_params(eq)
}

pub fn show(ui: &mut Ui, d: &mut Draft, playing: bool) {
    let applied = !d.dirty();
    let Some(a) = d.node.body.audio_mut() else { return };
    let eq = &mut a.eq;

    ui.horizontal_wrapped(|ui| {
        ui.checkbox(&mut eq.enabled, RichText::new("Ecualizador activo").strong()).on_hover_text("Bypass: desactivado, la señal pasa sin tocar y no cuesta nada de CPU");
        if ui.button("Restablecer bandas").on_hover_text("Vuelve las 4 bandas a 0 dB").clicked() {
            let enabled = eq.enabled;
            *eq = AudioEqData::default();
            eq.enabled = enabled;
        }
        // Estado del cambio: «aplicado» quiere decir que el motor ya lo tiene (con la cue sonando se oye al instante).
        let (text, color) = match (applied, eq.enabled, playing) {
            (false, _, _) => ("Enviando al motor…", theme::AMBER),
            (true, false, _) => ("EQ apagado: la señal pasa sin tocar", theme::TEXT_DIM),
            (true, true, true) => ("EQ aplicado: suena en vivo", theme::GREEN),
            (true, true, false) => ("EQ aplicado (se oye al disparar la cue)", theme::GREEN),
        };
        ui.label(RichText::new(text).strong().color(color));
        ui.label(RichText::new("Arrastre los puntos: horizontal = frecuencia, vertical = ganancia. Rueda sobre un punto = Q. Doble clic = activar/desactivar la banda.").small().color(theme::TEXT_DIM));
    });
    ui.add_space(4.0);

    let sel_id = ui.scope_id().with("eq_selected");
    let mut selected: usize = ui.data(|d| d.get_temp(sel_id)).unwrap_or(0);
    canvas(ui, eq, &mut selected);
    ui.data_mut(|d| d.insert_temp(sel_id, selected));
    ui.add_space(6.0);
    table(ui, eq, &mut selected);
    ui.data_mut(|d| d.insert_temp(sel_id, selected));
    *eq = eq.clamped();
}

fn canvas(ui: &mut Ui, eq: &mut AudioEqData, selected: &mut usize) {
    let width = ui.available_width().max(260.0);
    let (rect, _bg) = ui.allocate_exact_size(vec2(width, CANVAS_HEIGHT), Sense::hover());
    let plot = Rect::from_min_max(pos2(rect.left() + MARGIN_LEFT, rect.top() + 6.0), pos2(rect.right() - 8.0, rect.bottom() - MARGIN_BOTTOM));
    let (left, w, top, h) = (plot.left(), plot.width(), plot.top(), plot.height());

    if ui.is_rect_visible(rect) {
        let p = ui.painter_at(rect);
        p.rect_filled(rect, 2.0, theme::BG);
        for f in FREQ_TICKS {
            let x = x_of_freq(f, left, w);
            p.line_segment([pos2(x, top), pos2(x, plot.bottom())], Stroke::new(1.0, theme::GRID));
            p.text(pos2(x, plot.bottom() + 2.0), Align2::CENTER_TOP, tick_label(f), FontId::monospace(10.0), theme::TEXT_DIM);
        }
        for g in GAIN_TICKS {
            let y = y_of_gain(g, top, h);
            p.line_segment([pos2(left, y), pos2(plot.right(), y)], Stroke::new(if g == 0.0 { 1.6 } else { 1.0 }, if g == 0.0 { theme::AMBER_DIM } else { theme::GRID }));
            p.text(pos2(left - 4.0, y), Align2::RIGHT_CENTER, format!("{g:+.0}"), FontId::monospace(10.0), theme::TEXT_DIM);
        }
        // Curva combinada de las bandas activas (calculada por el mismo código que filtra el audio).
        let params = engine_eq(eq);
        let points: Vec<_> = (0..=240)
            .map(|i| {
                let f = EQ_MIN_HZ * (EQ_MAX_HZ / EQ_MIN_HZ).powf(i as f32 / 240.0);
                let db = qlab_engine::biquad::combined_response_db(&params, CURVE_SR, f);
                pos2(x_of_freq(f, left, w), y_of_gain(db, top, h))
            })
            .collect();
        p.add(Shape::line(points, Stroke::new(2.2, if eq.enabled { theme::AMBER } else { theme::TEXT_DIM })));
    }

    // Puntos arrastrables, uno por banda (los dibuja el mismo bucle que los atiende).
    let p = ui.painter_at(rect);
    for i in 0..EQ_BANDS {
        let b = eq.bands[i];
        let center = pos2(x_of_freq(b.frequency_hz, left, w), y_of_gain(if b.filter_type.uses_gain() { b.gain_db } else { 0.0 }, top, h));
        let r = ui.interact(Rect::from_center_size(center, vec2(POINT_HIT, POINT_HIT)), ui.scope_id().with(("eq_point", i)), Sense::click_and_drag());
        r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, format!("EQ banda {}", i + 1)));
        let r = r.on_hover_text(format!("Banda {} · {} · {:.0} Hz · {:+.1} dB · Q {:.2}", i + 1, b.filter_type.label(), b.frequency_hz, b.gain_db, b.q_factor));
        if r.dragged() {
            if let Some(pos) = r.interact_pointer_pos() {
                let (f, g) = snapped(freq_of_x(pos.x, left, w), if b.filter_type.uses_gain() { gain_of_y(pos.y, top, h) } else { b.gain_db });
                eq.bands[i].frequency_hz = f;
                eq.bands[i].gain_db = g;
                *selected = i;
            }
        }
        if r.clicked() {
            *selected = i;
        }
        if r.double_clicked() {
            eq.bands[i].enabled = !eq.bands[i].enabled;
        }
        if r.hovered() {
            let scroll = ui.input(|inp| inp.smooth_scroll_delta.y);
            if scroll != 0.0 {
                eq.bands[i].q_factor = scale_q(eq.bands[i].q_factor, scroll);
                *selected = i;
                // La rueda es de la banda: no debe además desplazar el Inspector.
                ui.ctx().input_mut(|inp| inp.smooth_scroll_delta = egui::Vec2::ZERO);
            }
        }
        let color = BAND_COLORS[i];
        let (radius, active) = (if *selected == i || r.hovered() { 8.0 } else { 6.5 }, eq.bands[i].enabled && eq.enabled);
        if active {
            p.circle_filled(center, radius, color);
        } else {
            p.circle_stroke(center, radius, Stroke::new(1.8, color.gamma_multiply(0.5)));
        }
        p.text(center + vec2(0.0, -radius - 8.0), Align2::CENTER_CENTER, format!("{}", i + 1), FontId::monospace(10.0), color);
    }
}

fn table(ui: &mut Ui, eq: &mut AudioEqData, selected: &mut usize) {
    egui::Grid::new("eq_table").num_columns(6).spacing([14.0, 5.0]).striped(true).show(ui, |ui| {
        for h in ["Banda", "Activa", "Tipo", "Frecuencia", "Ganancia", "Q"] {
            ui.label(RichText::new(h).small().color(theme::TEXT_DIM));
        }
        ui.end_row();
        for i in 0..EQ_BANDS {
            let b: &mut EqBandData = &mut eq.bands[i];
            if ui.selectable_label(*selected == i, RichText::new(format!("{}", i + 1)).strong().color(BAND_COLORS[i])).clicked() {
                *selected = i;
            }
            ui.checkbox(&mut b.enabled, "");
            egui::ComboBox::from_id_salt(("eq_type", i)).selected_text(b.filter_type.label()).width(96.0).show_ui(ui, |ui| {
                for t in EqFilterType::ALL {
                    ui.selectable_value(&mut b.filter_type, t, t.label());
                }
            });
            let speed = (b.frequency_hz * 0.004).max(0.05);
            ui.add(egui::DragValue::new(&mut b.frequency_hz).range(EQ_MIN_HZ..=EQ_MAX_HZ).speed(speed).suffix(" Hz").max_decimals(1));
            ui.add_enabled(b.filter_type.uses_gain(), egui::DragValue::new(&mut b.gain_db).range(EQ_MIN_GAIN_DB..=EQ_MAX_GAIN_DB).speed(0.1).suffix(" dB").fixed_decimals(1));
            ui.add(egui::DragValue::new(&mut b.q_factor).range(EQ_MIN_Q..=EQ_MAX_Q).speed(0.01).fixed_decimals(2));
            ui.end_row();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_frequency_axis_is_logarithmic_and_the_gain_axis_spans_minus_24_to_plus_12() {
        let (left, w, top, h) = (40.0, 600.0, 10.0, 180.0);
        assert_eq!(x_of_freq(20.0, left, w), 40.0);
        assert!((x_of_freq(20_000.0, left, w) - 640.0).abs() < 1e-3);
        assert!((x_of_freq(632.455_5, left, w) - 340.0).abs() < 0.5, "la mitad del eje es la media geométrica (632 Hz)");
        assert_eq!(x_of_freq(1.0, left, w), 40.0, "se recorta a 20 Hz");
        for f in [20.0f32, 55.0, 440.0, 1_000.0, 9_999.0, 20_000.0] {
            assert!((freq_of_x(x_of_freq(f, left, w), left, w) / f - 1.0).abs() < 1e-3, "{f}");
        }
        assert_eq!(y_of_gain(12.0, top, h), 10.0);
        assert_eq!(y_of_gain(-24.0, top, h), 190.0);
        assert!((y_of_gain(0.0, top, h) - 70.0).abs() < 1e-4, "0 dB está a un tercio desde arriba");
        for g in [-24.0f32, -6.5, 0.0, 3.3, 12.0] {
            assert!((gain_of_y(y_of_gain(g, top, h), top, h) - g).abs() < 1e-3);
        }
        assert_eq!(gain_of_y(-500.0, top, h), 12.0);
        assert_eq!(gain_of_y(9_999.0, top, h), -24.0);
    }

    #[test]
    fn the_wheel_changes_q_within_range_and_dragging_snaps_to_fine_values() {
        assert!(scale_q(1.0, 40.0) > 1.0 && scale_q(1.0, -40.0) < 1.0);
        assert_eq!(scale_q(9.9, 10_000.0), EQ_MAX_Q);
        assert_eq!(scale_q(0.11, -10_000.0), EQ_MIN_Q);
        assert_eq!(scale_q(2.0, 0.0), 2.0);
        assert_eq!(snapped(1_234.56, 3.333), (1_230.0, 3.3));
        assert_eq!(snapped(57.34, -6.04), (57.3, -6.0), "tres cifras significativas");
        assert_eq!(snapped(1.0, 99.0).0, 20.0);
        assert_eq!(snapped(99_999.0, 0.0).0, 20_000.0);
    }
}
