//! Tabla central de cues, virtualizada, con las columnas de QLab:
//! Status/Flag · Número · Nombre · Destino · Pre-Wait · Acción · Post-Wait · Continuar.
//!
//! Solo se construyen las filas visibles, así que una lista de miles de cues cuesta lo mismo que una
//! de diez. El playhead va en ámbar, lo que suena en verde y lo que espera (pre/post-wait) en cian.

use eframe::egui::{self, pos2, vec2, Align2, Color32, FontId, Pos2, Rect, RichText, Sense, Shape, Stroke, Ui};
use egui_extras::{Column, TableBuilder};
use qlab_protocol::cue_dto::ContinueMode;
use qlab_protocol::ipc_msg::CueSummary;

use super::{Action, ViewCtx};
use crate::format::fmt_time;
use crate::model::{blink_on, CueUiPhase, CueUiState, RunningInfo, SliceTag};
use crate::theme;
use crate::widgets;

pub const ROW_HEIGHT: f32 = 26.0;

/// Columnas, en orden.
/// Ancho de la columna de estado: icono, rótulo VAMP/DEVAMP y bandera.
pub const STATUS_COL_WIDTH: f32 = 132.0;

pub const HEADERS: [&str; 8] = ["", "Q", "Nombre", "Destino", "Pre-Wait", "Acción", "Post-Wait", "Sig."];

pub fn show(ui: &mut Ui, v: &ViewCtx, actions: &mut Vec<Action>) {
    let rows = v.model.rows();
    let mut table = TableBuilder::new(ui)
        .id_salt("cue_table")
        .striped(false)
        .resizable(true)
        .sense(Sense::click())
        .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
        .min_scrolled_height(60.0)
        .column(Column::exact(STATUS_COL_WIDTH))
        .column(Column::exact(54.0))
        .column(Column::remainder().at_least(160.0).clip(true))
        .column(Column::initial(170.0).at_least(80.0).clip(true))
        .column(Column::initial(104.0).at_least(80.0))
        .column(Column::initial(190.0).at_least(110.0))
        .column(Column::initial(104.0).at_least(80.0))
        .column(Column::exact(44.0));
    if v.scroll_to_selected {
        if let Some(sel) = v.model.selected {
            if let Some(i) = rows.iter().position(|r| v.model.summaries[r.index].id == sel) {
                table = table.scroll_to_row(i, None);
            }
        }
    }
    table
        .header(24.0, |mut header| {
            for h in HEADERS {
                header.col(|ui| {
                    ui.label(RichText::new(h).small().color(theme::TEXT_DIM));
                });
            }
        })
        .body(|body| {
            body.rows(ROW_HEIGHT, rows.len(), |mut row| {
                let r = rows[row.index()];
                let c = &v.model.summaries[r.index];
                let state = CueUiState::parse(&c.state);
                let is_playhead = v.model.playhead == Some(c.id);
                let is_selected = v.model.selected == Some(c.id);
                let info = v.telemetry.running.get(&c.id).copied();
                let bg = theme::row_background(state, is_playhead, is_selected);
                let fg = theme::text_color(state, is_playhead, c.armed && !c.dead);
                let style = RowStyle { bg, fg, state, is_playhead, is_selected };

                row.col(|ui| {
                    paint_bg(ui, &style);
                    status_cell(ui, c, &style, info, v.telemetry.cue_level_db(c.id));
                });
                row.col(|ui| {
                    paint_bg(ui, &style);
                    ui.label(RichText::new(&c.number).monospace().color(fg));
                });
                row.col(|ui| {
                    paint_bg(ui, &style);
                    name_cell(ui, c, r.depth, r.has_children, v.model.collapsed.contains(&c.id), &style, actions);
                });
                row.col(|ui| {
                    paint_bg(ui, &style);
                    let color = if c.armed { theme::TEXT_DIM } else { theme::RED };
                    // La ruta del archivo solo se desplaza con el ratón encima: es larga casi siempre y no es lo que se lee en función.
                    let width = ui.available_width();
                    widgets::marquee(ui, &c.target, egui::TextStyle::Body.resolve(ui.style()), color, width, false);
                });
                row.col(|ui| {
                    paint_bg(ui, &style);
                    wait_cell(ui, c.pre_wait, info, WaitKind::Pre, &style);
                });
                row.col(|ui| {
                    paint_bg(ui, &style);
                    action_cell(ui, c, info, &style);
                });
                row.col(|ui| {
                    paint_bg(ui, &style);
                    wait_cell(ui, c.post_wait, info, WaitKind::Post, &style);
                });
                row.col(|ui| {
                    paint_bg(ui, &style);
                    if continue_button(ui, c.continue_mode, fg) {
                        actions.push(Action::CycleContinue(c.id));
                    }
                });

                let resp = row.response();
                if resp.double_clicked() {
                    actions.push(Action::FireCue(c.id));
                } else if resp.clicked() {
                    actions.push(Action::SelectCue(c.id));
                    if c.parent.is_none() {
                        actions.push(Action::SetPlayhead(c.id));
                    }
                }
                resp.context_menu(|ui| {
                    if c.parent.is_none() && ui.button("Poner el playhead aquí").clicked() {
                        actions.push(Action::SetPlayhead(c.id));
                        ui.close();
                    }
                    if ui.button("Disparar esta cue").clicked() {
                        actions.push(Action::FireCue(c.id));
                        ui.close();
                    }
                });
            });
        });
}

struct RowStyle {
    bg: Option<Color32>,
    fg: Color32,
    state: CueUiState,
    is_playhead: bool,
    is_selected: bool,
}

/// Relleno de la celda hasta los bordes, para que la fila parezca continua.
fn paint_bg(ui: &mut Ui, s: &RowStyle) {
    let rect = ui.max_rect().expand2(vec2(4.0, 2.0));
    if let Some(bg) = s.bg {
        ui.painter().rect_filled(rect, 0.0, bg);
    }
    if s.is_playhead {
        // Borde izquierdo ámbar bien visible aunque la fila esté además seleccionada o sonando.
        ui.painter().rect_filled(Rect::from_min_size(rect.min, vec2(3.0, rect.height())), 0.0, theme::AMBER);
    } else if s.is_selected {
        ui.painter().rect_filled(Rect::from_min_size(rect.min, vec2(2.0, rect.height())), 0.0, theme::CYAN);
    }
}

fn triangle(p: &egui::Painter, c: Pos2, r: f32, color: Color32) {
    p.add(Shape::convex_polygon(vec![pos2(c.x - r * 0.6, c.y - r), pos2(c.x + r, c.y), pos2(c.x - r * 0.6, c.y + r)], color, Stroke::NONE));
}

/// Icono de estado y marca de flag, dibujados con primitivas (no dependen de la tipografía).
fn status_cell(ui: &mut Ui, c: &CueSummary, s: &RowStyle, info: Option<RunningInfo>, level_db: Option<f32>) {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width().max(40.0), ROW_HEIGHT - 4.0), Sense::hover());
    let p = ui.painter();
    let mid = rect.center();
    let icon = pos2(rect.left() + 12.0, mid.y);
    if s.is_playhead {
        triangle(p, pos2(rect.left() + 5.0, mid.y), 5.0, theme::AMBER);
    }
    match s.state {
        CueUiState::Running => triangle(p, icon, 5.0, theme::GREEN),
        CueUiState::PreWaiting | CueUiState::PostWaiting => {
            p.circle_stroke(icon, 5.0, Stroke::new(1.8, theme::CYAN));
            p.line_segment([icon, pos2(icon.x, icon.y - 3.5)], Stroke::new(1.5, theme::CYAN));
            p.line_segment([icon, pos2(icon.x + 2.5, icon.y)], Stroke::new(1.5, theme::CYAN));
        }
        CueUiState::Paused => {
            p.rect_filled(Rect::from_center_size(pos2(icon.x - 2.5, icon.y), vec2(3.0, 10.0)), 0.0, theme::AMBER_DIM);
            p.rect_filled(Rect::from_center_size(pos2(icon.x + 2.5, icon.y), vec2(3.0, 10.0)), 0.0, theme::AMBER_DIM);
        }
        CueUiState::Completed => {
            p.add(Shape::line(vec![pos2(icon.x - 4.0, icon.y), pos2(icon.x - 1.0, icon.y + 3.5), pos2(icon.x + 4.5, icon.y - 4.0)], Stroke::new(1.8, theme::TEXT_DIM)));
        }
        CueUiState::Armed => {
            p.circle_filled(icon, 2.5, theme::TEXT_DIM);
        }
        CueUiState::Idle => {}
    }
    let tag = info.and_then(|i| i.slice_tag());
    if let Some(tag) = tag {
        // VAMP parpadea en ámbar de alto contraste; DEVAMP queda fijo en cian mientras acaba la pasada.
        let (fg, bg) = match tag {
            SliceTag::Vamp if blink_on(ui.input(|i| i.time)) => (Color32::BLACK, theme::AMBER),
            SliceTag::Vamp => (theme::AMBER, theme::BG),
            SliceTag::Devamping => (Color32::BLACK, theme::CYAN),
        };
        let pill = Rect::from_min_size(pos2(rect.left() + 24.0, mid.y - 7.0), vec2(tag.label().len() as f32 * 6.2 + 8.0, 14.0));
        p.rect_filled(pill, 3.0, bg);
        p.rect_stroke(pill, 3.0, Stroke::new(1.0, if tag == SliceTag::Devamping { theme::CYAN } else { theme::AMBER }), egui::StrokeKind::Inside);
        p.text(pill.center(), Align2::CENTER_CENTER, tag.label(), FontId::monospace(10.0), fg);
    }
    // Vúmetro en miniatura: se ve qué cue está sacando audio en este instante.
    if let Some(db) = level_db {
        mini_meter(p, Rect::from_min_size(pos2(rect.left() + 76.0, mid.y - 4.0), vec2(34.0, 8.0)), db);
    }
    if !c.armed {
        // Desarmada: círculo tachado en rojo.
        let k = pos2(rect.left() + 12.0, mid.y);
        p.circle_stroke(k, 6.0, Stroke::new(1.5, theme::RED));
        p.line_segment([pos2(k.x - 4.2, k.y + 4.2), pos2(k.x + 4.2, k.y - 4.2)], Stroke::new(1.5, theme::RED));
    }
    if c.flagged {
        let f = pos2(rect.right() - 8.0, mid.y);
        p.line_segment([pos2(f.x - 3.0, f.y + 6.0), pos2(f.x - 3.0, f.y - 6.0)], Stroke::new(1.5, theme::RED));
        p.add(Shape::convex_polygon(vec![pos2(f.x - 3.0, f.y - 6.0), pos2(f.x + 5.0, f.y - 3.0), pos2(f.x - 3.0, f.y)], theme::RED, Stroke::NONE));
    }
    if let Some(color) = theme::cue_color(c.color) {
        p.rect_filled(Rect::from_min_size(pos2(rect.left() + 20.0, rect.bottom() - 3.0), vec2(14.0, 3.0)), 1.0, color);
    }
    let tip = match (c.armed, s.state) {
        (false, _) => "Desarmada",
        (_, CueUiState::Running) if tag == Some(SliceTag::Vamp) => "Atrapada en un bucle infinito: Devamp para salir",
        (_, CueUiState::Running) if tag == Some(SliceTag::Devamping) => "Devamp: termina la pasada en curso y sigue",
        (_, CueUiState::Running) => "Sonando",
        (_, CueUiState::PreWaiting) => "En pre-wait",
        (_, CueUiState::PostWaiting) => "En post-wait",
        (_, CueUiState::Paused) => "En pausa",
        (_, CueUiState::Completed) => "Terminada",
        (_, CueUiState::Armed) => "Cargada",
        (_, CueUiState::Idle) => "",
    };
    if !tip.is_empty() {
        resp.on_hover_text(tip);
    }
}

/// Barra horizontal compacta de -60 a 0 dBFS: verde, ámbar por encima de -12 y rojo por encima de -3.
fn mini_meter(p: &egui::Painter, rect: Rect, db: f32) {
    p.rect_filled(rect, 2.0, theme::BG);
    let frac = ((db + 60.0) / 60.0).clamp(0.0, 1.0);
    if frac > 0.0 {
        p.rect_filled(Rect::from_min_size(rect.min, vec2(rect.width() * frac, rect.height())), 2.0, widgets::meter_color(db));
    }
    p.rect_stroke(rect, 2.0, Stroke::new(1.0, theme::GRID), egui::StrokeKind::Inside);
}

fn name_cell(ui: &mut Ui, c: &CueSummary, depth: u8, has_children: bool, collapsed: bool, s: &RowStyle, actions: &mut Vec<Action>) {
    ui.add_space(depth as f32 * 16.0);
    if has_children {
        let (rect, resp) = ui.allocate_exact_size(vec2(14.0, 14.0), Sense::click());
        let p = ui.painter();
        let c0 = rect.center();
        let pts = if collapsed {
            vec![pos2(c0.x - 2.5, c0.y - 4.5), pos2(c0.x + 3.5, c0.y), pos2(c0.x - 2.5, c0.y + 4.5)]
        } else {
            vec![pos2(c0.x - 4.5, c0.y - 2.5), pos2(c0.x + 4.5, c0.y - 2.5), pos2(c0.x, c0.y + 3.5)]
        };
        p.add(Shape::convex_polygon(pts, theme::TEXT_DIM, Stroke::NONE));
        if resp.clicked() {
            actions.push(Action::ToggleGroup(c.id));
        }
    } else {
        ui.add_space(14.0);
    }
    long_text(ui, &c.name, s.fg, s);
}

/// Nombre en una sola línea. Si no entra, en la fila del playhead, la seleccionada o la que tiene el ratón encima **se
/// desplaza** para leerse entero; en el resto se corta con «…» (para que la lista no sea un hormiguero).
fn long_text(ui: &mut Ui, text: &str, color: Color32, s: &RowStyle) {
    let width = ui.available_width();
    let hovered = ui.rect_contains_pointer(ui.max_rect().expand2(vec2(4.0, 2.0)));
    let font = egui::TextStyle::Body.resolve(ui.style());
    widgets::marquee(ui, text, font, color, width, s.is_playhead || s.is_selected || hovered);
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WaitKind {
    Pre,
    Post,
}

fn wait_cell(ui: &mut Ui, total: f64, info: Option<RunningInfo>, kind: WaitKind, s: &RowStyle) {
    let size = vec2(ui.available_width().min(96.0), ROW_HEIGHT - 8.0);
    if total <= 0.0 {
        ui.label(RichText::new("—").color(theme::TEXT_DIM));
        return;
    }
    let (elapsed, active_phase) = match (kind, info) {
        (WaitKind::Pre, Some(i)) => (i.pre_wait_elapsed as f64, i.phase == Some(CueUiPhase::PreWait)),
        (WaitKind::Post, Some(i)) => (i.post_wait_elapsed as f64, matches!(i.phase, Some(CueUiPhase::Action | CueUiPhase::PostWait))),
        _ => (0.0, false),
    };
    let done = matches!(s.state, CueUiState::Completed) || (kind == WaitKind::Pre && info.is_some_and(|i| i.phase != Some(CueUiPhase::PreWait)));
    if active_phase || (done && info.is_some()) {
        let frac = (elapsed / total) as f32;
        let remaining = (total - elapsed).max(0.0);
        let color = if active_phase { theme::CYAN } else { theme::TEXT_DIM };
        widgets::progress_bar(ui, size, frac, Color32::from_rgb(0x0c, 0x4a, 0x56), &fmt_time(remaining), color);
    } else {
        ui.label(RichText::new(fmt_time(total)).monospace().color(if s.state.is_active() { theme::TEXT } else { theme::TEXT_DIM }));
    }
}

fn action_cell(ui: &mut Ui, c: &CueSummary, info: Option<RunningInfo>, s: &RowStyle) {
    let size = vec2(ui.available_width().min(176.0), ROW_HEIGHT - 8.0);
    match (info, c.duration) {
        (Some(i), duration) if matches!(i.phase, Some(CueUiPhase::Action | CueUiPhase::TailingOut | CueUiPhase::PostWait)) => {
            let elapsed = i.action_elapsed as f64;
            match duration {
                Some(d) if d > 0.0 => {
                    let text = format!("{} / {}", fmt_time(elapsed), fmt_time(d));
                    widgets::progress_bar(ui, size, (elapsed / d) as f32, theme::GREEN_DARK, &text, theme::GREEN);
                }
                _ => {
                    widgets::progress_bar(ui, size, 1.0, theme::GREEN_DARK, &format!("{} / inf", fmt_time(elapsed)), theme::GREEN);
                }
            }
        }
        (_, Some(d)) => {
            ui.label(RichText::new(fmt_time(d)).monospace().color(if s.state == CueUiState::Completed { theme::TEXT_DIM } else { s.fg }));
        }
        (_, None) => {
            ui.label(RichText::new(if c.kind == "group" { "grupo" } else { "inf" }).color(theme::TEXT_DIM));
        }
    }
}

/// Siguiente modo al pulsar la celda: No continuar → Auto-continue → Auto-follow → No continuar.
pub fn next_continue(mode: ContinueMode) -> ContinueMode {
    match mode {
        ContinueMode::DoNotContinue => ContinueMode::AutoContinue,
        ContinueMode::AutoContinue => ContinueMode::AutoFollow,
        ContinueMode::AutoFollow => ContinueMode::DoNotContinue,
    }
}

/// Celda «Sig.»: un botón que cicla el modo de continuación. Se dibuja un guion (no continuar), dos flechas
/// seguidas (auto-continue) o una flecha con barra (auto-follow). Devuelve `true` si se pulsó.
fn continue_button(ui: &mut Ui, mode: ContinueMode, color: Color32) -> bool {
    let (rect, resp) = ui.allocate_exact_size(vec2(38.0, ROW_HEIGHT - 6.0), Sense::click());
    let p = ui.painter();
    if resp.hovered() {
        p.rect_filled(rect, 3.0, Color32::from_white_alpha(22));
    }
    let c = rect.center();
    let arrow = |x: f32, col: Color32| {
        // Punta de flecha hacia la derecha con su asta.
        p.line_segment([pos2(x - 5.0, c.y), pos2(x + 3.0, c.y)], Stroke::new(1.8, col));
        p.line_segment([pos2(x - 0.5, c.y - 4.0), pos2(x + 4.0, c.y)], Stroke::new(1.8, col));
        p.line_segment([pos2(x - 0.5, c.y + 4.0), pos2(x + 4.0, c.y)], Stroke::new(1.8, col));
    };
    match mode {
        ContinueMode::DoNotContinue => {
            p.line_segment([pos2(c.x - 5.0, c.y), pos2(c.x + 5.0, c.y)], Stroke::new(1.5, theme::TEXT_DIM));
        }
        ContinueMode::AutoContinue => {
            arrow(c.x - 6.0, color);
            arrow(c.x + 6.0, color);
        }
        ContinueMode::AutoFollow => {
            arrow(c.x - 4.0, color);
            p.line_segment([pos2(c.x + 9.0, c.y - 6.0), pos2(c.x + 9.0, c.y + 6.0)], Stroke::new(2.0, color));
        }
    }
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, continue_label(mode)));
    let clicked = resp.clicked();
    resp.on_hover_text(format!("{}\nClic para cambiar", continue_label(mode)));
    clicked
}

pub fn continue_label(mode: ContinueMode) -> &'static str {
    match mode {
        ContinueMode::DoNotContinue => "No continuar (Do Not Continue)",
        ContinueMode::AutoContinue => "Auto-continue: dispara la siguiente tras el pre-wait + post-wait",
        ContinueMode::AutoFollow => "Auto-follow: dispara la siguiente cuando esta termina",
    }
}

#[cfg(test)]
mod continue_tests {
    use super::*;

    #[test]
    fn continue_mode_cycles_through_all_three() {
        let mut m = ContinueMode::DoNotContinue;
        let seen: Vec<ContinueMode> = (0..4).map(|_| { m = next_continue(m); m }).collect();
        assert_eq!(seen, [ContinueMode::AutoContinue, ContinueMode::AutoFollow, ContinueMode::DoNotContinue, ContinueMode::AutoContinue]);
    }
}
