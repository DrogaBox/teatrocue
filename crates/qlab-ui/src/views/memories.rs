//! Memorias al estilo de las «Ubicaciones de memoria» de Pro Tools: lista numerada acoplable a un lado (o cerrable),
//! con filtro de búsqueda, y el cuadro de nombre que aparece al marcar una memoria al vuelo.

use eframe::egui::{self, RichText, Ui};
use qlab_protocol::cue_dto::MemoryMark;
use qlab_protocol::ipc_msg::CueSummary;

use crate::theme;

/// `MM:SS.mmm` (con horas si pasa de una hora): el formato de la lista de ubicaciones de memoria.
pub fn fmt_mmss(secs: f64) -> String {
    if !secs.is_finite() || secs < 0.0 {
        return "--:--.---".into();
    }
    let total_ms = (secs * 1000.0).round() as u64;
    let (ms, s_total) = (total_ms % 1000, total_ms / 1000);
    let (s, m_total) = (s_total % 60, s_total / 60);
    if m_total >= 60 {
        format!("{}:{:02}:{:02}.{:03}", m_total / 60, m_total % 60, s, ms)
    } else {
        format!("{:02}:{:02}.{:03}", m_total, s, ms)
    }
}

/// Una fila de la lista.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    /// Número en la lista completa (no cambia al filtrar).
    pub number: usize,
    /// Posición de esta memoria en la lista de su cue (la que usan Renombrar y Quitar).
    pub index: usize,
    pub cue_id: u64,
    /// «número nombre» de la cue a la que pertenece.
    pub cue: String,
    pub secs: f64,
    pub name: String,
    pub note: String,
}

/// Todas las memorias, numeradas de corrido por cue y posición, ya filtradas. `live` reemplaza las memorias de una cue
/// por las del borrador abierto en el Inspector, así lo recién marcado aparece al instante (antes de que el daemon lo confirme).
pub fn rows(summaries: &[CueSummary], live: Option<(u64, &[MemoryMark])>, filter: &str) -> Vec<Row> {
    let needle = filter.trim().to_lowercase();
    let mut out = Vec::new();
    let mut number = 0;
    for c in summaries {
        let marks: &[MemoryMark] = match live {
            Some((id, m)) if id == c.id => m,
            _ => &c.memories,
        };
        for (index, m) in marks.iter().enumerate() {
            number += 1;
            let cue = format!("{} {}", c.number, c.name);
            let hay = format!("{} {} {}", m.name, m.note, cue).to_lowercase();
            if needle.is_empty() || hay.contains(&needle) {
                out.push(Row { number, index, cue_id: c.id, cue, secs: m.secs, name: m.name.clone(), note: m.note.clone() });
            }
        }
    }
    out
}

/// Estado de la lista (se conserva entre fotogramas).
#[derive(Default)]
pub struct PanelState {
    pub filter: String,
    /// `true` = acoplada a la derecha.
    pub right: bool,
    /// Pre-roll al saltar a una memoria: 0 = apagado, 2 o 4 segundos antes del marcador.
    pub preroll: u8,
}

pub enum PanelEvent {
    /// Un clic en una fila: ir a esa memoria.
    Activate { cue_id: u64, secs: f64, shift: bool },
    /// Se escribió otro tiempo para una memoria (`from` = el anterior, `to` = el nuevo, sin validar contra el archivo).
    SetTime { cue_id: u64, from: f64, to: f64 },
    /// Nombre nuevo para la memoria `index` de la cue (puede quedar vacío: «sin nombre»).
    Rename { cue_id: u64, index: usize, name: String },
    /// Quitar la memoria `index` de la cue. La numeración de la lista es la posición: las siguientes suben un puesto.
    Delete { cue_id: u64, index: usize },
    /// Mover la memoria `from` a la posición `to` de la lista de su cue (arrastrar la fila). `to` es el índice de
    /// inserción después de quitar la memoria `from`; las demás conservan su orden relativo. Solo hay destino dentro
    /// de la misma cue: una memoria pertenece al archivo de su cue.
    Reorder { cue_id: u64, from: usize, to: usize },
    /// Cambió el selector de pre-roll.
    Preroll(u8),
    /// Botón PFL de una fila: escuchar desde esa memoria en los auriculares.
    Pfl { cue_id: u64, secs: f64 },
    Close,
}

/// Tiempo escrito a mano: `MM:SS.mmm`, `MM:SS`, `H:MM:SS` o segundos decimales (`74.5`, `74,5`). `None` si no se entiende.
pub fn parse_memory_time(text: &str) -> Option<f64> {
    crate::format::parse_time(text)
}

/// Ajusta `secs` al frame de audio más cercano (`sample_rate` del archivo): la memoria cae en un frame exacto.
pub fn snap_to_frame(secs: f64, sample_rate: u32) -> f64 {
    let sr = f64::from(sample_rate.max(1));
    (secs.max(0.0) * sr).round() / sr
}

/// Valida un tiempo escrito: dentro de `0..=max_secs` (si hay máximo conocido) y ajustado a un frame. `None` si no sirve.
pub fn commit_memory_time(text: &str, max_secs: f64, sample_rate: u32) -> Option<f64> {
    let v = parse_memory_time(text)?;
    let clamped = if max_secs.is_finite() { v.min(max_secs.max(0.0)) } else { v };
    Some(snap_to_frame(clamped, sample_rate))
}

/// Etiqueta de tiempo `MM:SS.mmm` que se vuelve un campo de texto al hacer clic. Enter o perder el foco confirman (un texto que
/// no se entiende se descarta y queda el tiempo anterior); Esc cancela. Devuelve el tiempo nuevo **ya validado** si cambió.
pub fn time_field(ui: &mut Ui, id: egui::Id, secs: f64, max_secs: f64, sample_rate: u32) -> Option<f64> {
    let editing: Option<String> = ui.data(|d| d.get_temp(id));
    let Some(mut buf) = editing else {
        let r = ui
            .add(egui::Label::new(RichText::new(fmt_mmss(secs)).monospace().color(theme::AMBER)).sense(egui::Sense::click()))
            .on_hover_text("Clic para escribir el tiempo: MM:SS.mmm, MM:SS o segundos (74.5)");
        if r.clicked() || r.double_clicked() {
            ui.data_mut(|d| d.insert_temp(id, fmt_mmss(secs)));
            ui.memory_mut(|m| m.request_focus(id.with("edit")));
        }
        return None;
    };
    let edit_id = id.with("edit");
    let r = ui.add(egui::TextEdit::singleline(&mut buf).id(edit_id).font(egui::TextStyle::Monospace).desired_width(88.0));
    if !r.has_focus() && !r.lost_focus() {
        r.request_focus();
    }
    ui.data_mut(|d| d.insert_temp(id, buf.clone()));
    let cancel = ui.input(|i| i.key_pressed(egui::Key::Escape));
    if r.lost_focus() {
        ui.data_mut(|d| d.remove::<String>(id));
        if cancel {
            return None;
        }
        return commit_memory_time(&buf, max_secs, sample_rate).filter(|v| (v - secs).abs() > 1e-9);
    }
    None
}

/// Punto de arranque con pre-roll: `pre` segundos antes de la memoria, o el inicio del archivo si no hay tanto.
pub fn preroll_start(secs: f64, pre: u8) -> f64 {
    (secs - f64::from(pre)).max(0.0)
}

/// Ancho del botón PFL de cada fila.
const PFL_W: f32 = 30.0;
/// Ancho de los botones «Renombrar» y «Quitar» de cada fila.
const RENAME_W: f32 = 66.0;
const DEL_W: f32 = 48.0;

/// Arrastre en curso de una fila del panel: la fila de origen (índice en `rows`), la cue y la memoria que se mueve
/// (`from`, índice en la lista de la cue), el destino ya calculado y la altura de la guía de inserción.
#[derive(Clone, Copy, Debug, PartialEq)]
struct RowDrag {
    src: usize,
    cue_id: u64,
    from: usize,
    to: Option<usize>,
    guide_y: Option<f32>,
}

/// Contenido del panel. `start`: la memoria que es hoy el punto de arranque (se resalta).
pub fn show_panel(ui: &mut Ui, st: &mut PanelState, rows: &[Row], total: usize, start: Option<(u64, f64)>, pfl: Option<(u64, f64)>) -> Option<PanelEvent> {
    let mut event = None;
    ui.horizontal(|ui| {
        ui.label(RichText::new(format!("Memorias ({total})")).strong().color(theme::AMBER));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.small_button("x").on_hover_text("Cerrar el panel").clicked() {
                event = Some(PanelEvent::Close);
            }
            if ui.small_button(if st.right { "A la izquierda" } else { "A la derecha" }).on_hover_text("Acoplar el panel al otro lado").clicked() {
                st.right = !st.right;
            }
        });
    });
    ui.add(egui::TextEdit::singleline(&mut st.filter).hint_text("Buscar memoria…").desired_width(f32::INFINITY));
    ui.horizontal(|ui| {
        ui.label(RichText::new("Pre-roll").small().color(theme::TEXT_DIM));
        for (v, label) in [(0u8, "Off"), (2, "2s"), (4, "4s")] {
            if ui.selectable_label(st.preroll == v, label).on_hover_text("Al saltar a una memoria, arranca estos segundos antes del marcador. Shift+clic en una fila aplica 2 s a ese salto").clicked() && st.preroll != v {
                st.preroll = v;
                event = Some(PanelEvent::Preroll(v));
            }
        }
    });
    ui.separator();
    if total == 0 {
        ui.label(RichText::new("Todavía no hay memorias. Pulse M mientras suena la cue (o con el cursor sobre la onda) para marcar un punto con nombre.").color(theme::TEXT_DIM));
        return event;
    }
    if rows.is_empty() {
        ui.label(RichText::new("Ninguna memoria coincide con la búsqueda.").color(theme::TEXT_DIM));
        return event;
    }
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        // Reordenar arrastrando: cada fila tiene una capa de arrastre que solo reclama el arrastre (los clics siguen
        // en los botones de la fila). El destino se calcula con todas las filas ya medidas, tras la pasada.
        let drag_state_id = egui::Id::NULL.with("memory_row_drag_state");
        let mut drag_state: Option<RowDrag> = ui.data(|d| d.get_temp(drag_state_id));
        let mut row_rects: Vec<(egui::Rect, usize)> = Vec::new();
        let mut src_drag: Option<(usize, egui::Response)> = None;
        for (ri, r) in rows.iter().enumerate() {
            let name = if r.name.is_empty() { "(sin nombre)" } else { r.name.as_str() };
            let is_start = start.is_some_and(|(id, s)| id == r.cue_id && ((s - r.secs).abs() < 1e-6 || (s - preroll_start(r.secs, st.preroll)).abs() < 1e-6));
            let text = RichText::new(format!("#{:<2} {}", r.number, name)).monospace();
            // El renombrado en línea: mientras está abierto, el nombre de la fila es un campo de texto.
            let rename_id = egui::Id::NULL.with(("memory_rename", r.cue_id, r.index));
            let mut editing: Option<String> = ui.data(|d| d.get_temp(rename_id));
            // El campo de tiempo abierto también suspende el arrastre de su fila (el editor lo usa para seleccionar texto).
            let time_editing = ui.data(|d| d.get_temp::<String>(egui::Id::NULL.with(("memory_time", r.cue_id, r.number)))).is_some();
            let inner = ui.horizontal(|ui| {
                if let Some(to) = time_field(ui, egui::Id::NULL.with(("memory_time", r.cue_id, r.number)), r.secs, f64::INFINITY, 48_000) {
                    event = Some(PanelEvent::SetTime { cue_id: r.cue_id, from: r.secs, to });
                }
                // Nada de la fila puede pedir más ancho que el del panel (si no, el panel no se deja achicar y vuelve a
                // abrirse solo): el nombre de la memoria se corta con «…» y el de la cue se desplaza con el ratón encima.
                let gap = ui.spacing().item_spacing.x;
                let rest = (ui.available_width() - PFL_W - RENAME_W - DEL_W - 4.0 * gap).max(0.0);
                let cue_w = (rest * 0.45).min(260.0);
                let name_w = rest - cue_w;
                if let Some(buf) = editing.as_mut() {
                    let r_edit = ui.add(egui::TextEdit::singleline(buf).id(rename_id.with("edit")).font(egui::TextStyle::Small).desired_width(name_w.max(40.0)));
                    let typed = buf.clone();
                    ui.data_mut(|d| d.insert_temp(rename_id, typed.clone()));
                    if !r_edit.has_focus() && !r_edit.lost_focus() {
                        r_edit.request_focus();
                    }
                    let cancel = ui.input(|i| i.key_pressed(egui::Key::Escape));
                    if r_edit.lost_focus() {
                        ui.data_mut(|d| d.remove::<String>(rename_id));
                        if !cancel {
                            event = Some(PanelEvent::Rename { cue_id: r.cue_id, index: r.index, name: typed.trim().to_string() });
                        }
                    }
                } else {
                    let hover = format!("{name}\nIr a este punto: si la cue suena, salta en caliente y al volver a parar recobra su arranque; si no, mueve el cursor de edición (no cambia el arranque)");
                    let clicked = ui
                        .scope(|ui| {
                            ui.set_max_width(name_w);
                            ui.add(egui::Button::selectable(is_start, text).truncate()).on_hover_text(hover).clicked()
                        })
                        .inner;
                    if clicked {
                        event = Some(PanelEvent::Activate { cue_id: r.cue_id, secs: r.secs, shift: ui.input(|i| i.modifiers.shift) });
                    }
                }
                let pfl_on = pfl.is_some_and(|(id, s)| id == r.cue_id && (s - r.secs).abs() < 1e-6);
                if ui.add_sized([PFL_W, 18.0], egui::Button::selectable(pfl_on, RichText::new("PFL").small().strong())).on_hover_text("Escuchar desde este punto solo en los auriculares").clicked() {
                    event = Some(PanelEvent::Pfl { cue_id: r.cue_id, secs: r.secs });
                }
                if ui.add_sized([RENAME_W, 18.0], egui::Button::new(RichText::new("Renombrar").small())).on_hover_text(format!("Cambiar el nombre de «{name}»")).clicked() && editing.is_none() {
                    ui.data_mut(|d| d.insert_temp(rename_id, r.name.clone()));
                    ui.memory_mut(|m| m.request_focus(rename_id.with("edit")));
                }
                if ui.add_sized([DEL_W, 18.0], egui::Button::new(RichText::new("Quitar").small())).on_hover_text("Quitar esta memoria (Deshacer la devuelve)").clicked() {
                    event = Some(PanelEvent::Delete { cue_id: r.cue_id, index: r.index });
                }
                let w = ui.available_width();
                crate::widgets::marquee(ui, &r.cue, egui::TextStyle::Small.resolve(ui.style()), theme::TEXT_DIM, w, false);
            });
            row_rects.push((inner.response.rect, ri));
            let sense = if editing.is_some() || time_editing { egui::Sense::hover() } else { egui::Sense::drag() };
            // Sin texto al pasar el ratón: la capa cubre la fila entera y taparía los avisos de los botones.
            let drag = ui.interact(inner.response.rect, egui::Id::NULL.with(("memory_row_drag", r.cue_id, r.index)), sense);
            if drag.drag_started() {
                drag_state = Some(RowDrag { src: ri, cue_id: r.cue_id, from: r.index, to: None, guide_y: None });
            }
            if drag.drag_stopped() {
                if let Some(st) = drag_state.take() {
                    if st.src == ri {
                        if let Some(to) = st.to {
                            if to != st.from {
                                event = Some(PanelEvent::Reorder { cue_id: st.cue_id, from: st.from, to });
                            }
                        }
                    }
                }
            }
            if drag.dragged() && drag_state.as_ref().is_some_and(|s| s.src == ri) {
                src_drag = Some((ri, drag));
            }
            if !r.note.is_empty() {
                ui.label(RichText::new(&r.note).small().color(theme::TEXT_DIM));
            }
        }
        // Destino del arrastre: la frontera de la fila de la misma cue sobre la que está el puntero (medio superior =
        // insertar antes, medio inferior = insertar después). Fuera de su cue no hay destino.
        if let Some((_, drag)) = &src_drag {
            if let Some(st) = drag_state.as_mut() {
                st.to = None;
                st.guide_y = None;
                if let Some(py) = drag.interact_pointer_pos().map(|p| p.y) {
                    let same: Vec<&(egui::Rect, usize)> = row_rects.iter().filter(|(_, j)| rows[*j].cue_id == st.cue_id).collect();
                    if let (Some(first), Some(last)) = (same.first(), same.last()) {
                        if py >= first.0.top() - 6.0 && py <= last.0.bottom() + 6.0 {
                            let (pos_pre, y) = match same.iter().position(|(rect, _)| py < rect.center().y) {
                                Some(k) => (rows[same[k].1].index, Some(same[k].0.top())),
                                None => (rows[last.1].index + 1, Some(last.0.bottom())),
                            };
                            let to = if pos_pre > st.from { pos_pre - 1 } else { pos_pre };
                            if to != st.from {
                                st.to = Some(to);
                                st.guide_y = y;
                            }
                        }
                    }
                }
            }
        }
        match drag_state {
            Some(st) => ui.data_mut(|d| d.insert_temp(drag_state_id, st)),
            None => ui.data_mut(|d| d.remove::<RowDrag>(drag_state_id)),
        }
        if let Some(y) = drag_state.as_ref().and_then(|s| s.guide_y) {
            let (x0, x1) = row_rects.iter().fold((f32::MAX, f32::MIN), |(a, b), (r, _)| (a.min(r.left()), b.max(r.right())));
            ui.painter().line_segment([egui::pos2(x0, y), egui::pos2(x1, y)], egui::Stroke::new(2.0, theme::AMBER));
        }
    });
    event
}

/// Memoria que se está nombrando.
pub struct Quick {
    pub cue_id: u64,
    /// Posición ya capturada (segundos del archivo) o, si se capturó un frame de lectura, ese frame.
    pub secs: Option<f64>,
    pub frame: Option<u64>,
    pub name: String,
    pub default_name: String,
    /// Primer fotograma: el campo toma el foco.
    pub first: bool,
}

pub enum QuickOutcome {
    Pending,
    Save(String),
    Cancel,
}

/// Cuadro «Nombre de la memoria:» con el campo ya enfocado. Enter guarda (vacío = nombre por defecto), Esc cancela.
pub fn show_quick(ctx: &egui::Context, q: &mut Quick, at_label: &str) -> QuickOutcome {
    let mut outcome = QuickOutcome::Pending;
    let id = egui::Id::NULL.with("quick_memory_name");
    egui::Modal::new(egui::Id::NULL.with("quick_memory_modal")).show(ctx, |ui| {
        ui.set_width(380.0);
        ui.label(RichText::new("Nombre de la memoria:").strong());
        let r = ui.add(egui::TextEdit::singleline(&mut q.name).id(id).hint_text(q.default_name.as_str()).desired_width(f32::INFINITY));
        if q.first {
            q.first = false;
            r.request_focus();
        }
        ui.label(RichText::new(format!("Punto: {at_label}  ·  Enter guarda, Esc cancela")).small().color(theme::TEXT_DIM));
        if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            let name = q.name.trim();
            outcome = QuickOutcome::Save(if name.is_empty() { q.default_name.clone() } else { name.to_string() });
        } else if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            outcome = QuickOutcome::Cancel;
        }
    });
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mark(secs: f64, name: &str, note: &str) -> MemoryMark {
        MemoryMark { secs, name: name.into(), note: note.into() }
    }

    fn cue(id: u64, name: &str, memories: Vec<MemoryMark>) -> CueSummary {
        let mut c = crate::model::tests::summary(id, None, "Idle");
        c.name = name.into();
        c.memories = memories;
        c
    }

    #[test]
    fn times_use_minutes_seconds_and_milliseconds() {
        assert_eq!(fmt_mmss(0.0), "00:00.000");
        assert_eq!(fmt_mmss(83.4567), "01:23.457");
        assert_eq!(fmt_mmss(3_725.0), "1:02:05.000");
        assert_eq!(fmt_mmss(f64::NAN), "--:--.---");
        assert_eq!(fmt_mmss(-1.0), "--:--.---");
    }

    #[test]
    fn rows_are_numbered_across_cues_and_keep_their_number_when_filtered() {
        let cues = vec![cue(1, "Giselle acto 1", vec![mark(5.0, "1er levantada", ""), mark(20.0, "diagonal", "desde el fondo")]), cue(2, "Variación", vec![mark(3.0, "reloj circulo", "")]), cue(3, "Sin marcas", vec![])];
        let all = rows(&cues, None, "");
        assert_eq!(all.iter().map(|r| (r.number, r.name.as_str())).collect::<Vec<_>>(), vec![(1, "1er levantada"), (2, "diagonal"), (3, "reloj circulo")]);
        // El índice es la posición de la memoria en la lista de su cue (Renombrar y Quitar la usan) y no cambia al filtrar.
        assert_eq!(all.iter().map(|r| r.index).collect::<Vec<_>>(), vec![0, 1, 0]);
        assert_eq!(rows(&cues, None, "DIAG")[0].index, 1);
        assert_eq!(rows(&cues, None, "variación")[0].index, 0);
        // El filtro busca en nombre, nota y cue, sin distinguir mayúsculas, y no renumera.
        assert_eq!(rows(&cues, None, "DIAG").iter().map(|r| r.number).collect::<Vec<_>>(), vec![2]);
        assert_eq!(rows(&cues, None, "fondo").len(), 1, "busca en la nota");
        assert_eq!(rows(&cues, None, "variación").iter().map(|r| r.number).collect::<Vec<_>>(), vec![3], "busca en la cue");
        assert!(rows(&cues, None, "zzz").is_empty());
        assert_eq!(rows(&cues, None, "  ").len(), 3, "un filtro en blanco no filtra");
    }

    #[test]
    fn the_open_draft_replaces_the_cue_memories_so_new_marks_show_up_at_once() {
        let cues = vec![cue(1, "A", vec![mark(1.0, "vieja", "")])];
        let live = [mark(1.0, "vieja", ""), mark(2.0, "recién marcada", "")];
        let r = rows(&cues, Some((1, &live)), "");
        assert_eq!(r.len(), 2);
        assert_eq!(r[1].name, "recién marcada");
        assert_eq!(r[1].index, 1, "el índice apunta a la lista del borrador abierto");
        assert_eq!(rows(&cues, Some((9, &live)), "").len(), 1, "un borrador de otra cue no cuenta");
    }

    #[test]
    fn the_preroll_starts_before_the_marker_and_never_before_the_beginning_of_the_file() {
        assert_eq!(preroll_start(10.0, 0), 10.0);
        assert_eq!(preroll_start(10.0, 2), 8.0);
        assert_eq!(preroll_start(10.0, 4), 6.0);
        assert_eq!(preroll_start(1.5, 4), 0.0, "si no hay tanto, desde 00:00.00");
        assert_eq!(preroll_start(0.0, 2), 0.0);
    }
}
