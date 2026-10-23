//! Pestaña Audio Levels: matriz de ruteo Entradas × Salidas con faders y cajas de dB (-120 a +12),
//! gangs, mute/solo por salida y arrastre en vivo hacia el motor.
//!
//! Disposición (igual que `AudioLevelMatrix`): la celda `(0,0)` es el master de la cue, `(0,j)` el
//! master de la salida `j`, `(i,0)` el trim de la entrada `i` y `(i,j)` el crosspoint `i → j`.
//! Para un `FadeCue` la pestaña edita en cambio las celdas que se van a modular.

use eframe::egui::{self, Color32, RichText, Ui};
use qlab_protocol::matrix::{AudioLevelMatrixData, GangId, MAX_VOLUME_DB, MIN_VOLUME_DB};
use qlab_protocol::show_format::{CueBody, FadeBody, FadeCellDto, FadeCurveDto, FadeModeDto};

use super::{Draft, InspectorOutput, TabCtx};
use crate::theme;
use crate::widgets;

pub const GANG_COLORS: [Color32; 8] = [
    Color32::from_rgb(0x4f, 0xc3, 0xf7),
    Color32::from_rgb(0xff, 0x8a, 0x65),
    Color32::from_rgb(0xba, 0x68, 0xc8),
    Color32::from_rgb(0xed, 0xb5, 0x3f),
    Color32::from_rgb(0x81, 0xc7, 0x84),
    Color32::from_rgb(0xff, 0xd5, 0x4f),
    Color32::from_rgb(0x90, 0xa4, 0xae),
    Color32::from_rgb(0xf0, 0x62, 0x92),
];

pub fn gang_color(id: GangId) -> Color32 {
    GANG_COLORS[(id.get() as usize - 1) % GANG_COLORS.len()]
}

#[derive(Debug, Clone, PartialEq)]
pub struct LevelsUi {
    /// Modo de asignación: pulsar una celda la mete (o saca) del gang elegido.
    pub gang_mode: bool,
    pub gang_id: u8,
}

impl Default for LevelsUi {
    fn default() -> Self {
        Self { gang_mode: false, gang_id: 1 }
    }
}

const CELL_W: f32 = 64.0;

pub fn show(ui: &mut Ui, d: &mut Draft, st: &mut LevelsUi, cx: &mut TabCtx, out: &mut InspectorOutput) {
    let id = d.id();
    match &mut d.node.body {
        CueBody::Fade(f) => fade_editor(ui, f, cx),
        CueBody::Group(_) => {
            ui.label(RichText::new("Los grupos no tienen niveles propios").color(theme::TEXT_DIM));
        }
        // Audio, o la pista de audio de un vídeo.
        body => match body.audio_mut().and_then(|a| a.levels.as_mut()) {
            Some(m) => matrix_editor(ui, id, m, st, out),
            None => {
                ui.label(RichText::new("Esta cue aún no tiene matriz de niveles").color(theme::TEXT_DIM));
            }
        },
    }
}

// ---------------------------------------------------------------------------------------------
// Matriz de audio
// ---------------------------------------------------------------------------------------------

/// Presets de la barra de herramientas.
pub fn set_all_silent(m: &mut AudioLevelMatrixData) {
    for i in 1..=m.inputs as usize {
        for j in 1..=m.outputs as usize {
            let _ = m.set_db(i, j, MIN_VOLUME_DB);
        }
    }
}

pub fn set_diagonal(m: &mut AudioLevelMatrixData) {
    for i in 1..=m.inputs as usize {
        for j in 1..=m.outputs as usize {
            let _ = m.set_db(i, j, if i == j { 0.0 } else { MIN_VOLUME_DB });
        }
    }
}

pub fn set_all_unity(m: &mut AudioLevelMatrixData) {
    for i in 1..=m.inputs as usize {
        for j in 1..=m.outputs as usize {
            let _ = m.set_db(i, j, 0.0);
        }
    }
}

/// Cambia una celda arrastrando (con su gang) y devuelve las celdas que cambiaron `(fila, col, dB)`.
pub fn apply_cell_edit(m: &mut AudioLevelMatrixData, row: usize, col: usize, db: f32) -> Vec<(u8, u8, f32)> {
    let before: Vec<f32> = m.cells.iter().map(|c| c.db).collect();
    if m.set_db_ganged(row, col, db).is_err() {
        return Vec::new();
    }
    let cols = m.outputs as usize + 1;
    m.cells
        .iter()
        .enumerate()
        .filter(|(i, c)| c.db != before[*i])
        .map(|(i, c)| ((i / cols) as u8, (i % cols) as u8, c.db))
        .collect()
}

/// Mete la celda en el gang `id` o, si ya estaba en él, la saca.
pub fn toggle_gang(m: &mut AudioLevelMatrixData, row: usize, col: usize, id: u8) {
    let Some(gang) = GangId::new(id) else { return };
    if let Ok(cell) = m.cell(row, col) {
        let new = if cell.gang_id == Some(gang) { None } else { Some(gang) };
        let _ = m.set_gang(row, col, new);
    }
}

fn matrix_editor(ui: &mut Ui, cue_id: u64, m: &mut AudioLevelMatrixData, st: &mut LevelsUi, out: &mut InspectorOutput) {
    ui.horizontal_wrapped(|ui| {
        if ui.button("Todo en silencio").clicked() {
            set_all_silent(m);
        }
        if ui.button("1:1 diagonal").clicked() {
            set_diagonal(m);
        }
        if ui.button("Todo a 0 dB").clicked() {
            set_all_unity(m);
        }
        ui.separator();
        ui.toggle_value(&mut st.gang_mode, "Asignar gangs").on_hover_text("Pulsa las celdas para meterlas en el gang elegido. Las celdas de un gang se mueven juntas.");
        ui.add_enabled_ui(st.gang_mode, |ui| {
            for id in 1..=8u8 {
                let color = GANG_COLORS[id as usize - 1];
                let sel = st.gang_id == id;
                let txt = RichText::new(format!("{id}")).strong().color(if sel { Color32::BLACK } else { color });
                if ui.add(egui::Button::new(txt).fill(if sel { color } else { theme::PANEL_RAISED }).min_size(egui::vec2(22.0, 20.0))).clicked() {
                    st.gang_id = id;
                }
            }
        });
        ui.separator();
        ui.label(RichText::new(format!("{} entradas × {} salidas", m.inputs, m.outputs)).small().color(theme::TEXT_DIM));
    });
    ui.add_space(4.0);

    let (inputs, outputs) = (m.inputs as usize, m.outputs as usize);
    let mut gang_clicks: Vec<(usize, usize)> = Vec::new();
    egui::Grid::new(("levels_grid", cue_id)).spacing([4.0, 6.0]).show(ui, |ui| {
        // Cabecera: una columna por salida con sus botones de mute y solo.
        ui.label("");
        ui.label(RichText::new("Master / Trim").small().color(theme::TEXT_DIM));
        for j in 1..=outputs {
            ui.vertical(|ui| {
                ui.label(RichText::new(format!("Out {j}")).small().strong().color(theme::AMBER));
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    let bit = 1u64 << (j - 1);
                    let mut muted = m.mute_mask & bit != 0;
                    let mut solo = m.solo_mask & bit != 0;
                    if ui.add(egui::Button::new(RichText::new("M").small().strong().color(if muted { Color32::BLACK } else { theme::TEXT })).fill(if muted { theme::RED } else { theme::PANEL_RAISED }).min_size(egui::vec2(24.0, 16.0))).on_hover_text("Mute").clicked() {
                        muted = !muted;
                        let _ = m.set_mute(j, muted);
                    }
                    if ui.add(egui::Button::new(RichText::new("S").small().strong().color(if solo { Color32::BLACK } else { theme::TEXT })).fill(if solo { theme::AMBER } else { theme::PANEL_RAISED }).min_size(egui::vec2(24.0, 16.0))).on_hover_text("Solo").clicked() {
                        solo = !solo;
                        let _ = m.set_solo(j, solo);
                    }
                });
            });
        }
        ui.end_row();

        for i in 0..=inputs {
            ui.label(RichText::new(if i == 0 { "Master".to_string() } else { format!("In {i}") }).strong().color(theme::TEXT_DIM));
            for j in 0..=outputs {
                cell(ui, cue_id, m, i, j, st, out, &mut gang_clicks);
            }
            ui.end_row();
        }
    });
    for (r, c) in gang_clicks {
        toggle_gang(m, r, c, st.gang_id);
    }
}

#[allow(clippy::too_many_arguments)]
fn cell(ui: &mut Ui, cue_id: u64, m: &mut AudioLevelMatrixData, row: usize, col: usize, st: &LevelsUi, out: &mut InspectorOutput, gang_clicks: &mut Vec<(usize, usize)>) {
    let Ok(current) = m.cell(row, col).map(|c| (c.db, c.gang_id)) else { return };
    let (db, gang) = current;
    let tint = gang.map(gang_color);
    if st.gang_mode {
        let label = gang.map_or("—".to_string(), |g| format!("G{}", g.get()));
        let b = egui::Button::new(RichText::new(label).strong().color(tint.map_or(theme::TEXT_DIM, |_| Color32::BLACK))).fill(tint.unwrap_or(theme::PANEL_RAISED)).min_size(egui::vec2(CELL_W, 34.0));
        if ui.add(b).clicked() {
            gang_clicks.push((row, col));
        }
        return;
    }
    let mut v = db;
    let r = widgets::level_cell(ui, &mut v, CELL_W, tint, true);
    if r.changed && v != db {
        let changed = apply_cell_edit(m, row, col, v);
        if r.live || !changed.is_empty() {
            out.drags.extend(changed.into_iter().map(|(rr, cc, level)| (cue_id, rr, cc, level)));
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Fade
// ---------------------------------------------------------------------------------------------

pub fn curve_label(c: FadeCurveDto) -> &'static str {
    match c {
        FadeCurveDto::Linear => "Lineal",
        FadeCurveDto::SCurve => "Curva S",
        FadeCurveDto::Exponential { .. } => "Exponencial",
    }
}

/// Valor 0..1 de la curva en `t` (misma fórmula que el motor, para la vista previa).
pub fn curve_value(c: FadeCurveDto, t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    match c {
        FadeCurveDto::Linear => t,
        FadeCurveDto::SCurve => 0.5 * (1.0 - (std::f32::consts::PI * t).cos()),
        FadeCurveDto::Exponential { steepness: k } => {
            if k.abs() < 1e-4 { t } else { ((k * t).exp() - 1.0) / (k.exp() - 1.0) }
        }
    }
}

pub fn fade_editor(ui: &mut Ui, f: &mut FadeBody, cx: &TabCtx) {
    ui.horizontal_wrapped(|ui| {
        widgets::section(ui, "Modo y curva", |ui| {
            ui.horizontal(|ui| {
                ui.radio_value(&mut f.mode, FadeModeDto::Absolute, "Absoluto");
                ui.radio_value(&mut f.mode, FadeModeDto::Relative, "Relativo (suma dB)");
            });
            ui.horizontal(|ui| {
                ui.label("Curva");
                egui::ComboBox::from_id_salt("fade_curve").selected_text(curve_label(f.curve)).width(120.0).show_ui(ui, |ui| {
                    for c in [FadeCurveDto::Linear, FadeCurveDto::SCurve, FadeCurveDto::Exponential { steepness: 3.0 }] {
                        let selected = std::mem::discriminant(&f.curve) == std::mem::discriminant(&c);
                        if ui.selectable_label(selected, curve_label(c)).clicked() && !selected {
                            f.curve = c;
                        }
                    }
                });
                if let FadeCurveDto::Exponential { steepness } = &mut f.curve {
                    ui.add(egui::DragValue::new(steepness).range(-8.0..=8.0).speed(0.05).prefix("k = "));
                }
            });
            curve_preview(ui, f.curve);
        });
        widgets::section(ui, "Al terminar", |ui| {
            ui.checkbox(&mut f.stop_target_when_done, "Parar la cue destino");
            ui.checkbox(&mut f.honor_gangs, "Respetar los gangs de la matriz");
        });
    });
    widgets::section(ui, "Celdas que se modulan", |ui| {
        let mut remove = None;
        egui::Grid::new("fade_cells").spacing([8.0, 6.0]).show(ui, |ui| {
            for h in ["", "Fila", "Col", "Nivel destino", ""] {
                ui.label(RichText::new(h).small().color(theme::TEXT_DIM));
            }
            ui.end_row();
            for (i, c) in f.cells.iter_mut().enumerate() {
                ui.checkbox(&mut c.active, "");
                ui.add(egui::DragValue::new(&mut c.row).range(0..=32));
                ui.add(egui::DragValue::new(&mut c.col).range(0..=64));
                widgets::level_cell(ui, &mut c.target_db, 70.0, None, c.active);
                if ui.small_button("x").clicked() {
                    remove = Some(i);
                }
                ui.end_row();
            }
        });
        if let Some(i) = remove {
            f.cells.remove(i);
        }
        ui.horizontal(|ui| {
            if ui.button("+ Celda").clicked() {
                f.cells.push(FadeCellDto { row: 1, col: 1, target_db: MIN_VOLUME_DB, active: true });
            }
            if ui.button("+ Master a -inf").on_hover_text("Fade-out del master de la cue destino").clicked() {
                f.cells.push(FadeCellDto { row: 0, col: 0, target_db: MIN_VOLUME_DB, active: true });
            }
        });
        let target_ok = cx.model.selected.is_some();
        if f.cells.iter().all(|c| !c.active) && target_ok {
            ui.label(RichText::new("Sin celdas activas: el fade no hará nada").small().color(theme::AMBER));
        }
        ui.label(RichText::new(format!("Rango de nivel: {MIN_VOLUME_DB:.0} dB (-inf) a +{MAX_VOLUME_DB:.0} dB.")).small().color(theme::TEXT_DIM));
    });
}

fn curve_preview(ui: &mut Ui, curve: FadeCurveDto) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(160.0, 56.0), egui::Sense::hover());
    let p = ui.painter_at(rect);
    p.rect_filled(rect, 2.0, theme::BG);
    let pts: Vec<egui::Pos2> = (0..=48)
        .map(|k| {
            let t = k as f32 / 48.0;
            egui::pos2(rect.left() + 4.0 + t * (rect.width() - 8.0), rect.bottom() - 4.0 - curve_value(curve, t) * (rect.height() - 8.0))
        })
        .collect();
    p.add(egui::Shape::line(pts, egui::Stroke::new(2.0, theme::AMBER)));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matrix(m: usize, n: usize) -> AudioLevelMatrixData {
        AudioLevelMatrixData::new_one_to_one(m, n).unwrap()
    }

    #[test]
    fn presets_leave_masters_and_trims_alone() {
        let mut m = matrix(3, 3);
        m.set_db(0, 0, -6.0).unwrap();
        m.set_db(2, 0, -3.0).unwrap();
        set_all_unity(&mut m);
        assert!((1..=3).all(|i| (1..=3).all(|j| m.db(i, j).unwrap() == 0.0)));
        set_all_silent(&mut m);
        assert!((1..=3).all(|i| (1..=3).all(|j| m.db(i, j).unwrap() == MIN_VOLUME_DB)));
        set_diagonal(&mut m);
        assert_eq!(m.db(2, 2).unwrap(), 0.0);
        assert_eq!(m.db(1, 2).unwrap(), MIN_VOLUME_DB);
        assert_eq!((m.db(0, 0).unwrap(), m.db(2, 0).unwrap()), (-6.0, -3.0), "masters y trims intactos");
    }

    #[test]
    fn editing_a_ganged_cell_reports_every_cell_that_moved() {
        let mut m = matrix(2, 2);
        m.set_db(1, 1, -10.0).unwrap();
        m.set_db(2, 2, -4.0).unwrap();
        toggle_gang(&mut m, 1, 1, 1);
        toggle_gang(&mut m, 2, 2, 1);
        let mut moved = apply_cell_edit(&mut m, 1, 1, -4.0); // +6 dB para todo el gang
        moved.sort_by(|a, b| a.partial_cmp(b).unwrap());
        assert_eq!(moved, vec![(1, 1, -4.0), (2, 2, 2.0)]);
        // Fuera de gang solo cambia la celda tocada.
        let moved = apply_cell_edit(&mut m, 1, 2, -20.0);
        assert_eq!(moved, vec![(1, 2, -20.0)]);
        // Sin cambio real o fuera de la matriz: nada.
        assert!(apply_cell_edit(&mut m, 1, 2, -20.0).is_empty());
        assert!(apply_cell_edit(&mut m, 9, 9, 0.0).is_empty());
    }

    #[test]
    fn gang_membership_toggles_and_has_distinct_colors() {
        let mut m = matrix(2, 2);
        toggle_gang(&mut m, 1, 2, 3);
        assert_eq!(m.cell(1, 2).unwrap().gang_id, GangId::new(3));
        toggle_gang(&mut m, 1, 2, 3);
        assert_eq!(m.cell(1, 2).unwrap().gang_id, None);
        toggle_gang(&mut m, 1, 2, 3);
        toggle_gang(&mut m, 1, 2, 5);
        assert_eq!(m.cell(1, 2).unwrap().gang_id, GangId::new(5), "cambiar de gang no pasa por vacío");
        toggle_gang(&mut m, 1, 2, 0); // id inválido
        assert_eq!(m.cell(1, 2).unwrap().gang_id, GangId::new(5));
        let set: std::collections::HashSet<_> = (1..=8).map(|i| gang_color(GangId::new(i).unwrap())).collect();
        assert_eq!(set.len(), 8);
    }

    #[test]
    fn curve_preview_matches_the_engine_formulas() {
        for c in [FadeCurveDto::Linear, FadeCurveDto::SCurve, FadeCurveDto::Exponential { steepness: 3.0 }, FadeCurveDto::Exponential { steepness: -2.0 }] {
            assert!(curve_value(c, 0.0).abs() < 1e-6);
            assert!((curve_value(c, 1.0) - 1.0).abs() < 1e-5);
            assert_eq!(curve_value(c, -3.0), curve_value(c, 0.0));
            assert!(!curve_label(c).is_empty());
        }
        assert!((curve_value(FadeCurveDto::SCurve, 0.5) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn matrix_editor_renders_headless_for_big_and_small_matrices() {
        let ctx = egui::Context::default();
        theme::apply(&ctx);
        for (m, n) in [(1, 1), (2, 2), (8, 16)] {
            let mut mat = matrix(m, n);
            mat.set_mute(1, true).unwrap();
            let mut st = LevelsUi::default();
            let mut out = InspectorOutput::default();
            for gang_mode in [false, true] {
                st.gang_mode = gang_mode;
                let o = ctx.run_ui(Default::default(), |ui| matrix_editor(ui, 1, &mut mat, &mut st, &mut out));
                o.drop_without_applying_deltas();
            }
            assert!(out.drags.is_empty(), "sin interacción no hay arrastres");
        }
    }
}
