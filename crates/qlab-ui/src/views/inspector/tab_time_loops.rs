//! Pestaña Time & Loops: forma de onda con marcadores de inicio/fin y slices, velocidad y bucles.
//!
//! La forma de onda se calcula en un hilo aparte ([`crate::waveform`]); mientras llega se dibuja la
//! barra de tiempo con los mismos marcadores, que ya se pueden arrastrar.

use std::path::PathBuf;

use eframe::egui::{self, pos2, vec2, Align2, Color32, FontId, Rect, RichText, Sense, Shape, Stroke, Ui};
use qlab_protocol::cue_dto::{AudioTimeAndLoopsData, MemoryMark, MAX_MEMORIES, MAX_MEMORY_NAME_CHARS, MAX_MEMORY_NOTE_CHARS, INFINITE_SLICE_LOOP, MAX_SLICES_PER_VOICE, MAX_SLICE_LABEL_CHARS, RATE_MAX, RATE_MIN};

use super::{labeled, time_drag, Draft, InspectorOutput, TabCtx};
use crate::format::fmt_time;
use crate::theme;
use crate::waveform::{Peaks, WaveState};
use crate::widgets;

const SLICE_COLOR: Color32 = Color32::from_rgb(0xb0, 0x7a, 0xff);
const MEMORY_COLOR: Color32 = theme::AMBER;
/// Alto fijo de la onda: no debe quitar espacio a los controles de Inicio, Fin, Velocidad y Slices.
const WAVE_HEIGHT: f32 = 110.0;
const RULER_HEIGHT: f32 = 16.0;
const HANDLE_HIT: f32 = 12.0;
/// Separación mínima entre marcadores, en segundos.
pub const MIN_GAP: f64 = 0.001;

/// Correspondencia entre segundos del archivo y píxeles del dibujo.
#[derive(Clone, Copy, Debug)]
pub struct TimeAxis {
    pub left: f32,
    pub width: f32,
    pub duration: f64,
    /// Tramo visible (zoom): desde `start` y `len` segundos. Sin zoom es todo el archivo.
    pub start: f64,
    pub len: f64,
}

impl TimeAxis {
    /// Posición en pantalla de un instante; fuera del tramo visible cae fuera del marco (lo recorta el pintor).
    pub fn x_of(&self, secs: f64) -> f32 {
        self.left + self.width * ((secs - self.start) / self.len.max(1e-9)) as f32
    }

    /// Instante bajo una posición de pantalla, siempre dentro del archivo.
    pub fn secs_of(&self, x: f32) -> f64 {
        (self.start + f64::from((x - self.left) / self.width.max(1.0)) * self.len).clamp(0.0, self.duration)
    }
}

/// Tramo del archivo que muestra la onda (el zoom, como en Pro Tools).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ViewWin {
    pub start: f64,
    pub len: f64,
}

/// Lo más que se acerca: 20 ms de pantalla.
const MIN_VIEW_SECS: f64 = 0.02;

impl ViewWin {
    pub fn full(duration: f64) -> Self {
        Self { start: 0.0, len: duration }
    }

    pub fn is_full(&self, duration: f64) -> bool {
        self.len >= duration * 0.999
    }

    pub fn clamped(self, duration: f64) -> Self {
        let len = if self.len.is_finite() { self.len } else { duration }.clamp(MIN_VIEW_SECS.min(duration), duration.max(1e-6));
        let start = if self.start.is_finite() { self.start } else { 0.0 }.clamp(0.0, (duration - len).max(0.0));
        Self { start, len }
    }

    /// Acerca (`factor` > 1) o aleja (< 1) manteniendo `anchor` (segundos) en el mismo lugar de la pantalla.
    pub fn zoomed(self, factor: f64, anchor: f64, duration: f64) -> Self {
        if !(factor.is_finite() && factor > 0.0) {
            return self;
        }
        let len = self.len / factor;
        let frac = ((anchor - self.start) / self.len.max(1e-9)).clamp(0.0, 1.0);
        Self { start: anchor - frac * len, len }.clamped(duration)
    }

    pub fn panned(self, delta_secs: f64, duration: f64) -> Self {
        Self { start: self.start + delta_secs, len: self.len }.clamped(duration)
    }

    /// Sigue la reproducción: si el cursor sale del tramo (o llega al borde derecho) pasa de «página» dejándolo a un 10 %.
    pub fn following(self, cursor: f64, duration: f64) -> Self {
        if cursor >= self.start && cursor <= self.start + self.len * 0.92 {
            return self;
        }
        Self { start: cursor - self.len * 0.1, len: self.len }.clamped(duration)
    }
}

/// Fin efectivo: `end_time_sec <= 0` significa "hasta el final del archivo".
pub fn effective_end(t: &AudioTimeAndLoopsData, duration: f64) -> f64 {
    if t.end_time_sec <= 0.0 { duration } else { t.end_time_sec.min(duration.max(t.end_time_sec)) }
}

/// Intervalo "agradable" entre marcas de la regla para que haya unas 8 en pantalla.
pub fn ruler_step(duration: f64) -> f64 {
    let raw = duration / 8.0;
    for step in [0.01, 0.02, 0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0, 1_800.0, 3_600.0] {
        if step >= raw {
            return step;
        }
    }
    3_600.0
}

/// Anchos de los grupos de la fila de controles (Velocidad, Bucles, Fade integrado).
const SPEED_BOX_W: f32 = 470.0;
const LOOPS_BOX_W: f32 = 330.0;
const FADE_BOX_W: f32 = 470.0;

/// Un grupo con ancho mínimo garantizado (si no, dentro de una fila con ajuste de línea se comprime hasta una columna de una letra).
fn boxed(ui: &mut Ui, width: f32, add: impl FnOnce(&mut Ui)) {
    ui.allocate_ui(egui::vec2(width, 0.0), |ui| {
        ui.set_min_width(width);
        ui.set_max_width(width);
        add(ui);
    });
}

/// Velocidades de ensayo de acceso directo.
pub const REHEARSAL_RATES: [(&str, f32); 4] = [("0.90× (-10%)", 0.90), ("0.95× (-5%)", 0.95), ("1.00× Normal", 1.0), ("1.05× (+5%)", 1.05)];

fn frame_of(secs: f64, sample_rate: u32) -> u64 {
    (secs.max(0.0) * sample_rate.max(1) as f64).round() as u64
}

fn secs_of_frame(frame: u64, sample_rate: u32) -> f64 {
    frame as f64 / sample_rate.max(1) as f64
}

/// Aviso de audio dañado bajo la onda, con la reparación: crear una copia sana hasta el punto dañado y usarla en la cue.
fn damaged_bar(ui: &mut Ui, original: &std::path::Path, from: f64, repair: Option<&crate::repair::RepairState>, out: &mut InspectorOutput) {
    use crate::repair::RepairState;
    egui::Frame::new().fill(Color32::from_rgb(0x3a, 0x12, 0x12)).corner_radius(4.0).inner_margin(egui::Margin::symmetric(8, 5)).show(ui, |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(format!("Audio dañado desde {}: ese tramo del archivo no se puede leer y suena en silencio.", fmt_time(from))).color(theme::RED));
            match repair {
                Some(RepairState::Running) => {
                    ui.spinner();
                    ui.label(RichText::new("Creando copia reparada…").color(theme::TEXT_DIM));
                }
                Some(RepairState::Done(copy)) => {
                    let name = copy.file_name().map_or_else(|| copy.display().to_string(), |n| n.to_string_lossy().into_owned());
                    ui.label(RichText::new(format!("Copia lista: {name}")).color(theme::GREEN));
                    if ui.button(RichText::new("Usar la copia en esta cue").strong()).on_hover_text("La cue pasa a usar la copia reparada (el original queda igual en su carpeta)").clicked() {
                        out.use_repaired = Some(copy.clone());
                    }
                }
                state => {
                    if let Some(RepairState::Failed(e)) = state {
                        ui.label(RichText::new(e).small().color(theme::AMBER));
                    }
                    let ok = qlab_engine::ffmpeg_pcm::available();
                    let cut = crate::repair::safe_cut(from);
                    let b = ui
                        .add_enabled(ok, egui::Button::new(RichText::new(format!("Crear copia reparada (hasta {})", fmt_time(cut))).strong()))
                        .on_hover_text("Escribe junto al original una copia que llega hasta donde el audio está sano, sin recodificar: suena y se ve igual, y cualquier programa la reproduce sin errores. El original no se toca.")
                        .on_disabled_hover_text("Hace falta ffmpeg (viene en el paquete de instalación)");
                    if b.clicked() {
                        out.repair = Some((original.to_path_buf(), cut));
                    }
                }
            }
        });
    });
}

/// Tiempo de una memoria arrastrada: dentro del archivo y ajustado al frame exacto (lo que se muestra y lo que se guarda).
pub fn drag_memory_time(secs: f64, duration: f64, sample_rate: u32) -> f64 {
    crate::views::memories::snap_to_frame(if secs.is_finite() { secs.clamp(0.0, duration.max(0.0)) } else { 0.0 }, sample_rate)
}

/// Añade una memoria en `at` (segundos, dentro del archivo) en su sitio cronológico y devuelve su índice. `None` si ya hay el máximo.
pub fn add_memory_at(t: &mut AudioTimeAndLoopsData, duration: f64, at: f64) -> Option<usize> {
    if t.memories.len() >= MAX_MEMORIES {
        return None;
    }
    let secs = at.clamp(0.0, duration.max(0.0));
    let pos = t.memories.iter().filter(|m| m.secs <= secs).count();
    t.memories.insert(pos, MemoryMark { secs, name: String::new(), note: String::new() });
    normalize_memories(t, duration);
    t.memories.iter().position(|m| m.secs == secs && m.name.is_empty() && m.note.is_empty())
}

/// Deja las memorias dentro del archivo, con nombres y notas de largo acotado. No ordena: el orden de la lista es el que
/// se guarda (cronológico de fábrica, o el que el operador dejó arrastrando filas en el panel de Memorias).
pub fn normalize_memories(t: &mut AudioTimeAndLoopsData, duration: f64) {
    for m in &mut t.memories {
        m.secs = if m.secs.is_finite() { m.secs.clamp(0.0, duration.max(0.0)) } else { 0.0 };
        if m.name.chars().count() > MAX_MEMORY_NAME_CHARS {
            m.name = m.name.chars().take(MAX_MEMORY_NAME_CHARS).collect();
        }
        if m.note.chars().count() > MAX_MEMORY_NOTE_CHARS {
            m.note = m.note.chars().take(MAX_MEMORY_NOTE_CHARS).collect();
        }
    }
    t.memories.truncate(MAX_MEMORIES);
}

/// Deja los slices ordenados, sin repetidos y estrictamente entre inicio y fin.
pub fn normalize_slices(t: &mut AudioTimeAndLoopsData, end: f64) {
    let sr = t.file_sample_rate.max(1);
    let (lo, hi) = (frame_of(t.start_time_sec, sr), frame_of(end, sr));
    t.slices.retain(|s| s.frame > lo && s.frame < hi);
    t.slices.sort_by_key(|s| s.frame);
    t.slices.dedup_by_key(|s| s.frame);
    t.slices.truncate(MAX_SLICES_PER_VOICE - 1);
}

/// Texto flotante de un marcador: el nombre si lo tiene (o el número) y sus repeticiones.
pub fn slice_marker_label(index: usize, name: &str, repeats: &str) -> String {
    let name = name.trim();
    if name.is_empty() { format!("{} {repeats}", index + 1) } else { format!("{name} {repeats}") }
}

/// Dónde cae "Add Slice": el cursor de reproducción si suena, si no el de edición, y si no la mitad de la región.
pub fn slice_insert_point(t: &AudioTimeAndLoopsData, duration: f64, playing: Option<f64>, edit: Option<f64>) -> f64 {
    playing.or(edit).unwrap_or_else(|| (t.start_time_sec + effective_end(t, duration)) / 2.0)
}

pub fn slice_can_be_added(t: &AudioTimeAndLoopsData, duration: f64, at: f64) -> bool {
    let mut probe = t.clone();
    probe.add_slice(frame_of(at, t.file_sample_rate), 1, frame_of(duration, t.file_sample_rate)).is_some()
}

/// Añade un marcador de una pasada en `at` (segundos). Devuelve su índice si cabía.
pub fn add_slice_at(t: &mut AudioTimeAndLoopsData, duration: f64, at: f64) -> Option<usize> {
    let sr = t.file_sample_rate;
    t.add_slice(frame_of(at, sr), 1, frame_of(duration, sr))
}

pub fn file_path_of(d: &Draft) -> Option<PathBuf> {
    d.node.basics.file_target.as_ref().map(|t| PathBuf::from(&t.absolute_path)).filter(|p| !p.as_os_str().is_empty())
}

pub fn show(ui: &mut Ui, d: &mut Draft, cx: &mut TabCtx, out: &mut InspectorOutput) {
    let cue_id = d.id();
    let path = file_path_of(d);
    let wave = match &path {
        Some(p) if p.is_file() => cx.waveforms.get(p),
        _ => WaveState::Failed("sin archivo de audio".into()),
    };
    let Some(a) = d.node.body.audio_mut() else { return };
    let t = &mut a.time_loops;

    let peaks = if let WaveState::Ready(p) = &wave { Some(p.clone()) } else { None };
    let duration = peaks.as_ref().map_or_else(|| t.end_time_sec.max(t.start_time_sec + 1.0), |p| p.duration_secs()).max(MIN_GAP * 2.0);
    // Posición actual de la lectura dentro del archivo (a la velocidad de la cue) si está sonando.
    let running = cx.telemetry.running.get(&cue_id).filter(|i| i.phase.is_some());
    let cursor = running.map(|i| i.cursor_secs(t.file_sample_rate, t.start_override_secs.unwrap_or(t.start_time_sec), t.rate));
    let edit_id = ui.scope_id().with(("edit_cursor", cue_id));
    let mut edit_cursor: Option<f64> = ui.data(|d| d.get_temp(edit_id));
    // Pedido de la ventana global de Memorias: llevar el cursor de edición a una memoria de esta cue.
    let goto_id = egui::Id::NULL.with("memory_goto");
    if let Some((id, secs)) = ui.ctx().data(|d| d.get_temp::<(u64, f64)>(goto_id)) {
        if id == cue_id {
            edit_cursor = Some(secs.clamp(0.0, duration));
            // Elegir una memoria deja la cue lista para dispararse desde ahí (GO / Espacio). Lo mismo se aplica por
            // `pending_memory_goto` cuando el borrador carga: este temporal es el camino rápido si la pestaña ya está abierta.
            t.start_override_secs = Some(secs.clamp(0.0, duration));
            ui.ctx().data_mut(|d| d.remove::<(u64, f64)>(goto_id));
        }
    }

    // Barra de slices: añadir en el cursor y escapar de un bucle infinito.
    ui.horizontal_wrapped(|ui| {
        let at = slice_insert_point(t, duration, cursor, edit_cursor);
        let can_add = slice_can_be_added(t, duration, at);
        let source = if cursor.is_some() { "cursor de reproducción" } else if edit_cursor.is_some() { "cursor de edición" } else { "mitad de la región" };
        let hint = if can_add { format!("Añade un marcador en {} ({source})", fmt_time(at)) } else { format!("No se puede añadir un slice en {} (fuera de la región, repetido o sin sitio)", fmt_time(at)) };
        if ui.add_enabled(can_add, egui::Button::new("Add Slice")).on_hover_text(hint).clicked() {
            add_slice_at(t, duration, at);
        }
        let mem_at = cursor.or(edit_cursor).unwrap_or(duration / 2.0);
        if ui.button(format!("Marcar memoria ({})", cx.memory_key)).on_hover_text(format!("Marca un punto en {} (cursor de reproducción, de edición o mitad) y pide su nombre", fmt_time(mem_at))).clicked() {
            ui.ctx().data_mut(|d| d.insert_temp(egui::Id::NULL.with("quick_memory_request"), (cue_id, mem_at)));
        }
        ui.separator();
        ui.label(RichText::new("Pre-roll").small().color(theme::TEXT_DIM));
        for (v, label) in [(0u8, "Off"), (2, "2s"), (4, "4s")] {
            if ui.selectable_label(cx.preroll == v, label).on_hover_text("Al saltar a una memoria, arranca estos segundos antes del marcador").clicked() && cx.preroll != v {
                out.preroll = Some(v);
            }
        }
        ui.separator();
        let can_devamp = running.is_some() && (!t.slices.is_empty() || t.infinite_loop);
        if ui.add_enabled(can_devamp, egui::Button::new("Devamp")).on_hover_text("Salir del bucle al final de la pasada en curso y seguir al siguiente tramo sin saltos").clicked() {
            out.devamp = Some(cue_id);
        }
    });

    // Zoom de la onda: el tramo visible se recuerda por cue; con zoom se pide el detalle de ese tramo en segundo plano.
    let view_id = ui.scope_id().with(("wave_view_win", cue_id));
    let view = ui.data(|d| d.get_temp::<ViewWin>(view_id)).unwrap_or_else(|| ViewWin::full(duration)).clamped(duration);
    let detail = match (&path, peaks.as_ref()) {
        (Some(p), Some(pk)) if !view.is_full(duration) => cx.waveforms.detail(p, view.start, view.len, duration, pk.sample_rate),
        _ => None,
    };
    let view = wave_view(ui, t, duration, peaks.as_deref(), detail.as_deref(), &wave, cursor, &mut edit_cursor, cue_id, view);
    ui.data_mut(|d| d.insert_temp(view_id, view));
    if let (Some(from), Some(original)) = (peaks.as_ref().and_then(|p| p.damaged_from), path.as_ref()) {
        damaged_bar(ui, original, from, cx.repair.as_ref(), out);
    }
    // El cursor de edición se publica para que M (en cualquier parte de la aplicación) marque ahí.
    ui.ctx().data_mut(|d| d.insert_temp(egui::Id::NULL.with("edit_cursor_pub"), (cue_id, edit_cursor)));
    ui.data_mut(|d| match edit_cursor {
        Some(c) => {
            d.insert_temp(edit_id, c);
        }
        None => d.remove::<f64>(edit_id),
    });

    ui.add_space(4.0);
    ui.horizontal_wrapped(|ui| {
        let end_before = effective_end(t, duration);
        let mut end = end_before;
        let mut start = t.start_time_sec;
        labeled(ui, "Inicio", |ui| time_drag(ui, &mut start, 0.0..=duration));
        labeled(ui, "Fin", |ui| time_drag(ui, &mut end, 0.0..=duration));
        if start != t.start_time_sec {
            t.start_time_sec = start.min((end - MIN_GAP).max(0.0));
        }
        if end != end_before {
            t.end_time_sec = if end >= duration - 1e-6 { duration } else { end.max(t.start_time_sec + MIN_GAP) };
        }
        let region = (effective_end(t, duration) - t.start_time_sec).max(0.0);
        let plays = if t.infinite_loop { None } else { Some(t.play_count.max(1) as f64) };
        let total = plays.map(|p| region / t.rate.max(RATE_MIN) as f64 * p);
        labeled(ui, "Duración", |ui| ui.label(RichText::new(total.map_or("inf".to_string(), fmt_time)).monospace().color(theme::AMBER)));
    });

    // Velocidad y Bucles comparten fila solo si caben; si no (panel de Memorias abierto, ventana chica) se apilan. Fade integrado
    // va en su propia fila. Cada grupo tiene un ancho fijo: nunca se aplasta contra el borde.
    let avail = ui.available_width();
    let side_by_side = avail >= SPEED_BOX_W + LOOPS_BOX_W + 24.0;
    let speed_ui = |ui: &mut Ui, t: &mut AudioTimeAndLoopsData| {
        widgets::section(ui, "Velocidad", |ui| {
            ui.horizontal_wrapped(|ui| {
                let r = ui.add(egui::DragValue::new(&mut t.rate).range(RATE_MIN..=RATE_MAX).speed(0.005).suffix("×").min_decimals(2).max_decimals(3));
                if r.double_clicked() {
                    t.rate = 1.0;
                }
                for (label, v) in [("½", 0.5), ("1×", 1.0), ("2×", 2.0)] {
                    if ui.small_button(label).clicked() {
                        t.rate = v;
                    }
                }
            });
            // Velocidades de ensayo: cambian la velocidad en caliente (la voz que suena sigue sonando, remuestreada con Catmull-Rom).
            ui.horizontal_wrapped(|ui| {
                for (label, v) in REHEARSAL_RATES {
                    let on = (t.rate - v).abs() < 1e-4;
                    let r = ui.selectable_label(on, label).on_hover_text("Velocidad de ensayo. Doble clic: vuelve a 1.000×");
                    if r.double_clicked() {
                        t.rate = 1.0;
                    } else if r.clicked() {
                        t.rate = v;
                    }
                }
            });
            ui.add_enabled(false, egui::Checkbox::new(&mut t.preserve_pitch, "Conservar tono")).on_disabled_hover_text("No disponible en esta versión: la velocidad cambia también el tono");
        });
    };
    let loops_ui = |ui: &mut Ui, t: &mut AudioTimeAndLoopsData| {
        widgets::section(ui, "Bucles", |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.add_enabled_ui(!t.infinite_loop, |ui| {
                    ui.label("Repeticiones");
                    let mut n = t.play_count.max(1);
                    if ui.add(egui::DragValue::new(&mut n).range(1..=9_999)).changed() {
                        t.play_count = n;
                    }
                });
                ui.checkbox(&mut t.infinite_loop, "Bucle infinito");
            });
        });
    };
    let speed_w = avail.min(SPEED_BOX_W);
    let loops_w = avail.min(LOOPS_BOX_W);
    if side_by_side {
        ui.horizontal_top(|ui| {
            boxed(ui, speed_w, |ui| speed_ui(ui, t));
            boxed(ui, loops_w, |ui| loops_ui(ui, t));
        });
    } else {
        boxed(ui, speed_w, |ui| speed_ui(ui, t));
        boxed(ui, loops_w, |ui| loops_ui(ui, t));
    }
    // Fila B: Fade integrado.
    boxed(ui, avail.min(FADE_BOX_W), |ui| {
        widgets::section(ui, "Fade integrado", |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.checkbox(&mut t.do_integrated_fade, "Fade sobre la forma de onda");
                ui.add_enabled(t.do_integrated_fade, egui::Checkbox::new(&mut t.lock_fade_to_cue, "Atar a la cue"));
            });
        });
    });

    slice_list(ui, t, duration);
    memory_list(ui, t, duration, &mut edit_cursor, edit_id, cx.preroll);
}

/// Lista de memorias: número, nombre, posición, nota, «Ir» (lleva el cursor de edición) y quitar.
fn memory_list(ui: &mut Ui, t: &mut AudioTimeAndLoopsData, duration: f64, edit_cursor: &mut Option<f64>, edit_id: egui::Id, preroll: u8) {
    let sr = t.file_sample_rate.max(1);
    egui::CollapsingHeader::new(RichText::new(format!("Memorias ({})", t.memories.len())).color(MEMORY_COLOR)).id_salt("memories_header").default_open(!t.memories.is_empty()).show(ui, |ui| {
        ui.label(RichText::new("«Marcar memoria» o la tecla M marcan un punto del audio (donde suena o donde está el cursor); póngale nombre y nota. No cambian la reproducción.").small().color(theme::TEXT_DIM));
        let mut remove = None;
        let mut go = None;
        let mut start_here = None;
        let mut clear_start = false;
        if let Some(s) = t.start_override_secs {
            let name = t.memories.iter().find(|m| (m.secs - s).abs() < 1e-6).map(|m| m.name.as_str()).filter(|n| !n.is_empty());
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(format!("Arranca en {}{}", fmt_time(s), name.map_or(String::new(), |n| format!(" ({n})")))).strong().color(theme::GREEN));
                if ui.small_button("Desde el inicio").on_hover_text("Quita el punto de arranque: la cue vuelve a empezar donde indica Inicio").clicked() {
                    clear_start = true;
                }
            });
        }
        for (i, m) in t.memories.iter_mut().enumerate() {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(format!("{}", i + 1)).color(MEMORY_COLOR).monospace());
                ui.add(egui::TextEdit::singleline(&mut m.name).desired_width(170.0).char_limit(MAX_MEMORY_NAME_CHARS).hint_text("nombre"));
                if let Some(v) = crate::views::memories::time_field(ui, ui.scope_id().with(("memory_time_edit", i)), m.secs, duration, sr) {
                    m.secs = v;
                }
                ui.add(egui::TextEdit::singleline(&mut m.note).desired_width(260.0).char_limit(MAX_MEMORY_NOTE_CHARS).hint_text("nota"));
                if ui.small_button("Ir").on_hover_text("Lleva el cursor de edición a este punto").clicked() {
                    go = Some(m.secs);
                }
                if ui.small_button("Arrancar aquí").on_hover_text("La cue empezará desde este punto al dispararla (GO / Espacio)").clicked() {
                    start_here = Some(m.secs);
                }
                if ui.small_button("x").on_hover_text("Quitar").clicked() {
                    remove = Some(i);
                }
            });
        }
        if let Some(i) = remove {
            t.memories.remove(i);
        }
        if clear_start {
            t.start_override_secs = None;
        }
        if let Some(s) = start_here {
            let s = crate::views::memories::preroll_start(s, preroll);
            t.start_override_secs = Some(s);
            *edit_cursor = Some(s);
            ui.data_mut(|d| d.insert_temp(edit_id, s));
        }
        if let Some(s) = go {
            *edit_cursor = Some(s);
            ui.data_mut(|d| d.insert_temp(edit_id, s));
        }
    });
    normalize_memories(t, duration);
}

fn slice_list(ui: &mut Ui, t: &mut AudioTimeAndLoopsData, duration: f64) {
    let sr = t.file_sample_rate.max(1);
    let end = effective_end(t, duration);
    egui::CollapsingHeader::new(RichText::new(format!("Slices ({})", t.slices.len())).color(theme::AMBER)).id_salt("slices_header").default_open(!t.slices.is_empty()).show(ui, |ui| {
        ui.label(RichText::new("Add Slice o doble clic en la onda añaden un marcador; clic en un marcador alterna 1x / 2x / Infinite; clic derecho lo quita. Un tramo Infinite se repite hasta Devamp.").small().color(theme::TEXT_DIM));
        let mut remove = None;
        for (i, s) in t.slices.iter_mut().enumerate() {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(format!("#{}", i + 1)).color(SLICE_COLOR));
                ui.add(egui::TextEdit::singleline(&mut s.label).desired_width(150.0).char_limit(MAX_SLICE_LABEL_CHARS).hint_text("nombre del tramo"));
                let mut secs = secs_of_frame(s.frame, sr);
                if time_drag(ui, &mut secs, 0.0..=end).changed() {
                    s.frame = frame_of(secs, sr);
                }
                for (label, count) in [("1x", 1u32), ("2x", 2), ("Infinite", INFINITE_SLICE_LOOP)] {
                    if ui.selectable_label(s.play_count == count, label).clicked() {
                        s.play_count = count;
                    }
                }
                if s.play_count != INFINITE_SLICE_LOOP && s.play_count > 2 {
                    let mut n = s.play_count;
                    if ui.add(egui::DragValue::new(&mut n).range(1..=9_999).suffix(" veces")).changed() {
                        s.play_count = n;
                    }
                }
                if ui.small_button("x").on_hover_text("Quitar").clicked() {
                    remove = Some(i);
                }
            });
        }
        if let Some(i) = remove {
            t.slices.remove(i);
        }
    });
    normalize_slices(t, end);
}

/// Dibuja la onda (o la barra de tiempo) y gestiona los marcadores.
#[allow(clippy::too_many_arguments)]
/// Rombo sobre la regla y rótulo con el tiempo exacto de un cursor (el de edición, que deja el clic, y el de reproducción).
/// `edit`: el del clic va arriba; el de reproducción, abajo, para que no se tapen si coinciden.
fn cursor_tag(p: &egui::Painter, rect: Rect, wave_rect: Rect, x: f32, secs: f64, color: Color32, edit: bool) {
    let c = pos2(x, rect.top() + RULER_HEIGHT * 0.5);
    let d = 6.0;
    if edit {
        p.add(Shape::convex_polygon(vec![pos2(c.x, c.y - d), pos2(c.x + d, c.y), pos2(c.x, c.y + d), pos2(c.x - d, c.y)], color, Stroke::new(1.0, Color32::BLACK)));
    } else {
        // La reproducción lleva un triángulo hacia abajo (la cabeza de lectura), no un rombo.
        p.add(Shape::convex_polygon(vec![pos2(c.x - d - 1.0, c.y - d), pos2(c.x + d + 1.0, c.y - d), pos2(c.x, c.y + d + 2.0)], color, Stroke::new(1.0, Color32::BLACK)));
    }
    let galley = p.layout_no_wrap(fmt_time(secs), FontId::monospace(12.0), Color32::BLACK);
    let (w, h) = (galley.size().x + 10.0, galley.size().y + 4.0);
    let left = if x + 9.0 + w > rect.right() { x - 9.0 - w } else { x + 9.0 };
    let top = if edit { wave_rect.top() + 3.0 } else { wave_rect.bottom() - h - 3.0 };
    let pill = Rect::from_min_size(pos2(left.max(rect.left()), top), vec2(w, h));
    p.rect_filled(pill, 3.0, color);
    p.galley(pos2(pill.left() + 5.0, pill.top() + 2.0), galley, Color32::BLACK);
}

#[allow(clippy::too_many_arguments)]
fn wave_view(ui: &mut Ui, t: &mut AudioTimeAndLoopsData, duration: f64, peaks: Option<&Peaks>, detail: Option<&crate::waveform::Detail>, wave: &WaveState, cursor: Option<f64>, edit_cursor: &mut Option<f64>, cue_id: u64, view: ViewWin) -> ViewWin {
    let mut view = view.clamped(duration);
    let follow_id = ui.scope_id().with(("wave_follow", cue_id));
    let mut follow = ui.data(|d| d.get_temp::<bool>(follow_id)).unwrap_or(true);
    let end_now = effective_end(t, duration);
    // Barra de zoom, como en Pro Tools: acercar, alejar, ver todo, ver la región y seguir la reproducción.
    ui.horizontal_wrapped(|ui| {
        let center = cursor.or(*edit_cursor).unwrap_or(view.start + view.len * 0.5);
        if ui.button("−").on_hover_text("Alejar (Ctrl + rueda del ratón)").clicked() {
            view = view.zoomed(0.5, center.clamp(view.start, view.start + view.len), duration);
        }
        if ui.button("+").on_hover_text("Acercar (Ctrl + rueda del ratón sobre la onda: se acerca donde está el puntero)").clicked() {
            view = view.zoomed(2.0, center.clamp(view.start, view.start + view.len), duration);
        }
        if ui.add_enabled(!view.is_full(duration), egui::Button::new("Todo")).on_hover_text("Ver el archivo entero").clicked() {
            view = ViewWin::full(duration);
        }
        if ui.add_enabled(end_now - t.start_time_sec > MIN_GAP, egui::Button::new("Región")).on_hover_text("Acercar a la región (de Inicio a Fin)").clicked() {
            view = ViewWin { start: t.start_time_sec, len: end_now - t.start_time_sec }.clamped(duration);
        }
        ui.checkbox(&mut follow, "Seguir").on_hover_text("Con zoom, la onda sigue a la reproducción");
        ui.label(RichText::new(format!("Vista {} de {} · Mayús + rueda desplaza", fmt_time(view.len), fmt_time(duration))).small().color(theme::TEXT_DIM));
    });
    ui.data_mut(|d| d.insert_temp(follow_id, follow));
    // El lienzo respeta el ancho disponible del panel: nada de lo que se dibuja (marcadores, banderas, etiquetas) sale del marco.
    let width = ui.available_width().max(120.0);
    let (rect, bg) = ui.allocate_exact_size(vec2(width, WAVE_HEIGHT), Sense::click());
    // Rueda sobre la onda: Ctrl acerca/aleja donde está el puntero; Mayús (o rueda horizontal) desplaza.
    if ui.rect_contains_pointer(rect) {
        let probe = TimeAxis { left: rect.left(), width: rect.width(), duration, start: view.start, len: view.len };
        let (zoom, scroll, shift, pointer) = ui.input(|i| (i.zoom_delta(), i.smooth_scroll_delta, i.modifiers.shift, i.pointer.hover_pos()));
        if (zoom - 1.0).abs() > 1e-4 {
            let anchor = pointer.map_or(view.start + view.len * 0.5, |p| probe.secs_of(p.x));
            view = view.zoomed(f64::from(zoom), anchor, duration);
        } else if scroll.x.abs() > 0.0 || (shift && scroll.y.abs() > 0.0) {
            let px = if scroll.x.abs() > 0.0 { scroll.x } else { scroll.y };
            view = view.panned(-f64::from(px) / f64::from(rect.width().max(1.0)) * view.len, duration);
        }
    }
    // Con zoom, la onda acompaña a la reproducción (si está activado y no se acaba de mover a mano).
    if let (Some(c), true) = (cursor, follow && !view.is_full(duration)) {
        let moved_by_hand = ui.rect_contains_pointer(rect) && ui.input(|i| i.smooth_scroll_delta != egui::Vec2::ZERO || (i.zoom_delta() - 1.0).abs() > 1e-4);
        if !moved_by_hand {
            view = view.following(c, duration);
        }
    }
    let axis = TimeAxis { left: rect.left(), width: rect.width(), duration, start: view.start, len: view.len };
    let wave_rect = Rect::from_min_max(pos2(rect.left(), rect.top() + RULER_HEIGHT), rect.max);
    let sr = t.file_sample_rate.max(1);
    let end = effective_end(t, duration);

    if ui.is_rect_visible(rect) {
        let p = ui.painter_at(rect);
        p.rect_filled(rect, 2.0, theme::BG);
        // Regla de tiempo.
        let step = ruler_step(view.len);
        let mut tick = (view.start / step).ceil() * step;
        while tick <= (view.start + view.len).min(duration) + 1e-9 {
            let x = axis.x_of(tick);
            p.line_segment([pos2(x, rect.top() + RULER_HEIGHT - 5.0), pos2(x, rect.top() + RULER_HEIGHT)], Stroke::new(1.0, theme::TEXT_DIM));
            p.text(pos2(x + 3.0, rect.top() + 1.0), Align2::LEFT_TOP, fmt_time(tick), FontId::monospace(10.0), theme::TEXT_DIM);
            tick += step;
        }
        p.line_segment([pos2(rect.left(), wave_rect.top()), pos2(rect.right(), wave_rect.top())], Stroke::new(1.0, theme::GRID));
        let mid = wave_rect.center().y;
        let half = wave_rect.height() * 0.46;
        p.line_segment([pos2(rect.left(), mid), pos2(rect.right(), mid)], Stroke::new(1.0, theme::GRID));
        match (peaks, wave) {
            (Some(pk), _) => {
                let (x_start, x_end) = (axis.x_of(t.start_time_sec), axis.x_of(end));
                for px in 0..rect.width() as usize {
                    let (t0, t1) = (axis.start + px as f64 / f64::from(rect.width()) * axis.len, axis.start + (px + 1) as f64 / f64::from(rect.width()) * axis.len);
                    // Con el detalle del tramo (zoom) se ven las crestas reales; mientras llega, la vista general ampliada.
                    let (lo, hi) = match detail.filter(|d| d.covers(t0, t1)) {
                        Some(d) => d.range(t0, t1),
                        None => pk.range((t0 / duration) as f32, (t1 / duration) as f32),
                    };
                    let x = rect.left() + px as f32 + 0.5;
                    let inside = x >= x_start && x <= x_end;
                    let color = if inside { theme::CYAN } else { Color32::from_rgb(0x1c, 0x4a, 0x52) };
                    p.line_segment([pos2(x, mid - hi.clamp(-1.0, 1.0) * half), pos2(x, (mid - lo.clamp(-1.0, 1.0) * half).max(mid - hi * half + 1.0))], Stroke::new(1.0, color));
                }
            }
            (None, WaveState::Pending) => {
                p.text(wave_rect.center(), Align2::CENTER_CENTER, "calculando forma de onda…", FontId::proportional(13.0), theme::TEXT_DIM);
            }
            (None, WaveState::Failed(e)) => {
                p.text(wave_rect.center(), Align2::CENTER_CENTER, e, FontId::proportional(13.0), theme::TEXT_DIM);
            }
            _ => {}
        }
        // Audio dañado: desde ahí el archivo no se puede decodificar y suena en silencio. Se marca en rojo y se avisa.
        if let Some(from) = peaks.and_then(|pk| pk.damaged_from).filter(|&f| f < duration) {
            let x0 = axis.x_of(from);
            p.rect_filled(Rect::from_min_max(pos2(x0, wave_rect.top()), wave_rect.max), 0.0, Color32::from_rgba_unmultiplied(0xd0, 0x30, 0x30, 46));
            p.line_segment([pos2(x0, wave_rect.top()), pos2(x0, wave_rect.bottom())], Stroke::new(1.5, theme::RED));
            let text = format!("Audio dañado desde {}: el resto suena en silencio", fmt_time(from));
            let galley = p.layout_no_wrap(text, FontId::proportional(12.0), theme::RED);
            let w = galley.size().x + 10.0;
            let left = if x0 + 8.0 + w > rect.right() { (x0 - 8.0 - w).max(rect.left() + 2.0) } else { x0 + 8.0 };
            p.rect_filled(Rect::from_min_size(pos2(left, wave_rect.top() + 4.0), vec2(w, galley.size().y + 4.0)), 3.0, Color32::from_black_alpha(210));
            p.galley(pos2(left + 5.0, wave_rect.top() + 6.0), galley, theme::RED);
        }
        // Zonas fuera de la región.
        let shade = Color32::from_black_alpha(130);
        p.rect_filled(Rect::from_min_max(wave_rect.min, pos2(axis.x_of(t.start_time_sec), wave_rect.bottom())), 0.0, shade);
        p.rect_filled(Rect::from_min_max(pos2(axis.x_of(end), wave_rect.top()), wave_rect.max), 0.0, shade);
        if let Some(c) = cursor {
            // Seguimiento de la reproducción: lo ya sonado se aclara, y el punto actual es una línea verde gruesa con un
            // triángulo sobre la regla y el tiempo en un rótulo, que se distinguen del cursor ámbar del clic.
            let x = axis.x_of(c);
            let from = axis.x_of(t.start_override_secs.unwrap_or(t.start_time_sec)).min(x);
            p.rect_filled(Rect::from_min_max(pos2(from, wave_rect.top()), pos2(x, wave_rect.bottom())), 0.0, Color32::from_rgba_unmultiplied(0x40, 0xe0, 0x70, 34));
            p.line_segment([pos2(x, wave_rect.top()), pos2(x, wave_rect.bottom())], Stroke::new(4.0, Color32::from_black_alpha(160)));
            p.line_segment([pos2(x, wave_rect.top()), pos2(x, wave_rect.bottom())], Stroke::new(2.0, theme::GREEN));
            cursor_tag(&p, rect, wave_rect, x, c, theme::GREEN, false);
        }
        if let Some(c) = *edit_cursor {
            let x = axis.x_of(c);
            p.line_segment([pos2(x, wave_rect.top()), pos2(x, wave_rect.bottom())], Stroke::new(1.0, theme::AMBER));
            cursor_tag(&p, rect, wave_rect, x, c, theme::AMBER, true);
        }
    }
    // Un clic en la onda coloca el cursor de edición (el doble clic añade un slice).
    if bg.clicked() {
        if let Some(pos) = bg.interact_pointer_pos() {
            *edit_cursor = Some(axis.secs_of(pos.x));
        }
    }

    // El rombo del cursor de edición (el que deja el clic) se agarra y se arrastra: el tiempo del rótulo lo sigue en vivo.
    if let Some(c) = *edit_cursor {
        let cx_ = axis.x_of(c);
        let hit = if cx_ > rect.left() - HANDLE_HIT && cx_ < rect.right() + HANDLE_HIT { Rect::from_center_size(pos2(cx_, rect.top() + RULER_HEIGHT * 0.5), vec2(HANDLE_HIT, RULER_HEIGHT)) } else { Rect::from_center_size(pos2(-1.0e5, -1.0e5), vec2(1.0, 1.0)) };
        let r = ui.interact(hit, ui.scope_id().with(("edit_cursor_grab", cue_id)), Sense::click_and_drag());
        r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, "Cursor de edición"));
        if r.dragged() {
            if let Some(pos) = r.interact_pointer_pos() {
                *edit_cursor = Some(axis.secs_of(pos.x).clamp(0.0, duration));
            }
        }
        r.on_hover_cursor(egui::CursorIcon::ResizeHorizontal).on_hover_text("Arrastre para mover el cursor");
    }

    // Marcadores arrastrables. Cada uno tiene su propia zona de agarre.
    let mut dragged_slice = None;
    let start_id = ui.scope_id().with("marker_start");
    let end_id = ui.scope_id().with("marker_end");
    // Un marcador fuera del tramo visible no se agarra: su zona de agarre va lejos para no tapar a los vecinos.
    let on_screen = |x: f32| x > rect.left() - HANDLE_HIT && x < rect.right() + HANDLE_HIT;
    let far = Rect::from_center_size(pos2(-1.0e5, -1.0e5), vec2(1.0, 1.0));
    let grab = |x: f32| if on_screen(x) { Rect::from_center_size(pos2(x, wave_rect.center().y), vec2(HANDLE_HIT, wave_rect.height())) } else { far };

    let r_start = ui.interact(grab(axis.x_of(t.start_time_sec)), start_id, Sense::drag());
    let r_end = ui.interact(grab(axis.x_of(end)), end_id, Sense::drag());
    if let Some(pos) = r_start.interact_pointer_pos().filter(|_| r_start.dragged()) {
        t.start_time_sec = axis.secs_of(pos.x).min((end - MIN_GAP).max(0.0));
    }
    if let Some(pos) = r_end.interact_pointer_pos().filter(|_| r_end.dragged()) {
        let e = axis.secs_of(pos.x).max(t.start_time_sec + MIN_GAP);
        t.end_time_sec = if e >= duration - duration * 0.001 { duration } else { e };
    }
    let slice_count = t.slices.len();
    let mut remove_slice = None;
    for i in 0..slice_count {
        let x = axis.x_of(secs_of_frame(t.slices[i].frame, sr));
        let r = ui.interact(grab(x), ui.scope_id().with(("marker_slice", i)), Sense::click_and_drag());
        if let Some(pos) = r.interact_pointer_pos().filter(|_| r.dragged()) {
            t.slices[i].frame = frame_of(axis.secs_of(pos.x), sr);
            dragged_slice = Some(i);
        }
        if r.secondary_clicked() {
            remove_slice = Some(i);
        }
        if r.clicked() {
            t.slices[i].play_count = t.slices[i].next_play_count();
        }
        r.on_hover_text(format!("Slice #{} · clic derecho para quitar", i + 1));
    }
    if let Some(i) = remove_slice {
        t.slices.remove(i);
    }
    // Doble clic en la onda: nuevo slice (si cabe y hay sitio).
    if bg.double_clicked() && t.slices.len() + 1 < MAX_SLICES_PER_VOICE {
        if let Some(pos) = bg.interact_pointer_pos() {
            add_slice_at(t, duration, axis.secs_of(pos.x));
        }
    }
    if dragged_slice.is_some() || bg.double_clicked() {
        normalize_slices(t, end);
    }

    // Banderas de memoria sobre la regla: rombo ámbar con el nombre. Un clic salta a ese punto; sostener y arrastrar la mueve
    // (como las ubicaciones de memoria de Pro Tools): una línea guía vertical cruza toda la onda siguiendo el cursor y un
    // rótulo muestra el nombre y el tiempo exacto en vivo. Al soltar se confirma el frame nuevo (la lista no se reordena: el orden solo cambia arrastrando filas en el panel de Memorias).
    let mut clicked_memory = None;
    let drag_state_id = ui.scope_id().with("memory_drag_state");
    let mut drag_state: Option<(usize, f64)> = ui.data(|d| d.get_temp(drag_state_id));
    let mut drag_commit: Option<(usize, f64)> = None;
    let mut guide: Option<(f32, String)> = None;
    for (i, m) in t.memories.iter().enumerate() {
        let live = drag_state.filter(|(j, _)| *j == i).map_or(m.secs, |(_, v)| v);
        let center = pos2(axis.x_of(live), rect.top() + RULER_HEIGHT * 0.5);
        let hit = if on_screen(center.x) { Rect::from_center_size(center, vec2(HANDLE_HIT, RULER_HEIGHT)) } else { far };
        let r = ui.interact(hit, ui.scope_id().with(("memory_flag", i)), Sense::click_and_drag());
        let name = if m.name.is_empty() { format!("Memoria {}", i + 1) } else { m.name.clone() };
        r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, format!("Memoria {}: {}", i + 1, name)));
        if r.dragged() {
            if let Some(pos) = r.interact_pointer_pos() {
                let v = drag_memory_time(axis.secs_of(pos.x), duration, sr);
                drag_state = Some((i, v));
                guide = Some((axis.x_of(v), format!("{}: {}", name, fmt_time(v))));
            }
        } else {
            r.clone().on_hover_cursor(egui::CursorIcon::ResizeHorizontal).on_hover_text(if m.note.is_empty() { format!("{} · {}\nArrastre para moverla", name, fmt_time(m.secs)) } else { format!("{} · {}\n{}\nArrastre para moverla", name, fmt_time(m.secs), m.note) });
        }
        if r.drag_stopped() {
            if let Some((j, v)) = drag_state.filter(|(j, _)| *j == i) {
                drag_commit = Some((j, v));
            }
            drag_state = None;
        }
        if r.clicked() {
            clicked_memory = Some(m.secs);
        }
        if ui.is_rect_visible(rect) {
            let p = ui.painter_at(rect);
            let hot = r.hovered() || r.dragged();
            let color = if hot { Color32::WHITE } else { MEMORY_COLOR };
            let d = if hot { 7.0 } else { 6.0 };
            p.add(Shape::convex_polygon(vec![pos2(center.x, center.y - d), pos2(center.x + d, center.y), pos2(center.x, center.y + d), pos2(center.x - d, center.y)], color, Stroke::new(1.0, Color32::BLACK)));
            p.line_segment([pos2(center.x, rect.top() + RULER_HEIGHT), pos2(center.x, rect.bottom())], Stroke::new(1.0, MEMORY_COLOR.gamma_multiply(0.55)));
            let galley = p.layout_no_wrap(format!("{} {}", i + 1, name), FontId::proportional(11.0), MEMORY_COLOR);
            let w = galley.size().x + 8.0;
            let left = if center.x + 9.0 + w > rect.right() { center.x - 9.0 - w } else { center.x + 9.0 };
            p.rect_filled(Rect::from_min_size(pos2(left, rect.top() + 1.0), vec2(w, RULER_HEIGHT - 2.0)), 3.0, Color32::from_black_alpha(215));
            p.galley(pos2(left + 4.0, rect.top() + 1.0 + (RULER_HEIGHT - 2.0 - galley.size().y) * 0.5), galley, MEMORY_COLOR);
        }
    }
    match drag_state {
        Some(st) => ui.data_mut(|d| {
            d.insert_temp(drag_state_id, st);
        }),
        None => ui.data_mut(|d| d.remove::<(usize, f64)>(drag_state_id)),
    }
    if let Some((x, text)) = guide {
        // Línea guía roja de punta a punta de la onda (alinear con transitorios) y el rótulo junto al cursor.
        let p = ui.painter_at(rect);
        p.line_segment([pos2(x, rect.top()), pos2(x, rect.bottom())], Stroke::new(1.5, theme::RED));
        let galley = p.layout_no_wrap(text, FontId::monospace(12.0), Color32::WHITE);
        let (w, h) = (galley.size().x + 10.0, galley.size().y + 6.0);
        let left = if x + 12.0 + w > rect.right() { x - 12.0 - w } else { x + 12.0 };
        let top = wave_rect.top() + 6.0;
        p.rect_filled(Rect::from_min_size(pos2(left, top), vec2(w, h)), 4.0, Color32::from_black_alpha(230));
        p.rect_stroke(Rect::from_min_size(pos2(left, top), vec2(w, h)), 4.0, Stroke::new(1.0, theme::RED), egui::StrokeKind::Inside);
        p.galley(pos2(left + 5.0, top + 3.0), galley, Color32::WHITE);
    }
    if let Some((i, v)) = drag_commit {
        if i < t.memories.len() {
            t.memories[i].secs = v;
            // El borrador cambió: el Inspector lo guarda y marca el show con cambios.
        }
    }
    if let Some(secs) = clicked_memory {
        *edit_cursor = Some(secs);
        ui.ctx().data_mut(|d| d.insert_temp(egui::Id::NULL.with("memory_activate"), (cue_id, secs)));
    }

    if ui.is_rect_visible(rect) {
        let p = ui.painter_at(rect);
        let marker = |x: f32, color: Color32, label: &str, up: bool| {
            p.line_segment([pos2(x, wave_rect.top()), pos2(x, wave_rect.bottom())], Stroke::new(2.0, color));
            let y = if up { wave_rect.top() } else { wave_rect.bottom() };
            let dir = if up { 1.0 } else { -1.0 };
            p.add(Shape::convex_polygon(vec![pos2(x - 6.0, y), pos2(x + 6.0, y), pos2(x, y + dir * 9.0)], color, Stroke::NONE));
            if !label.is_empty() {
                // Etiqueta flotante con fondo para que se lea sobre la onda; si no cabe a la derecha del corte, va a su izquierda.
                let galley = p.layout_no_wrap(label.to_string(), FontId::monospace(10.0), color);
                let w = galley.size().x + 8.0;
                let left = if x + 4.0 + w > rect.right() { x - 4.0 - w } else { x + 4.0 };
                let top = if up { wave_rect.top() + 2.0 } else { wave_rect.bottom() - galley.size().y - 6.0 };
                let pill = Rect::from_min_size(pos2(left, top), vec2(w, galley.size().y + 4.0));
                p.rect_filled(pill, 3.0, Color32::from_black_alpha(190));
                p.galley(pos2(left + 4.0, top + 2.0), galley, color);
            }
        };
        marker(axis.x_of(t.start_time_sec), theme::GREEN, "", true);
        marker(axis.x_of(end), theme::RED, "", false);
        if let Some(s) = t.start_override_secs {
            marker(axis.x_of(s), theme::GREEN, "arranque", true);
        }
        for (i, s) in t.slices.iter().enumerate() {
            let repeats = if s.play_count == INFINITE_SLICE_LOOP { "inf".to_string() } else { format!("×{}", s.play_count) };
            let label = slice_marker_label(i, &s.label, &repeats);
            marker(axis.x_of(secs_of_frame(s.frame, sr)), SLICE_COLOR, &label, true);
        }
    }

    // Tira de navegación (solo con zoom): el archivo entero en pequeño y el tramo visible como una ventana que se arrastra.
    if !view.is_full(duration) {
        let (strip, resp) = ui.allocate_exact_size(vec2(width, 14.0), Sense::click_and_drag());
        let to_x = |secs: f64| strip.left() + strip.width() * (secs / duration.max(1e-9)) as f32;
        if let Some(pos) = resp.interact_pointer_pos().filter(|_| resp.dragged() || resp.clicked()) {
            // El centro de la ventana sigue al puntero (clic o arrastre).
            let secs = f64::from((pos.x - strip.left()) / strip.width().max(1.0)) * duration;
            view = ViewWin { start: secs - view.len * 0.5, len: view.len }.clamped(duration);
        }
        if ui.is_rect_visible(strip) {
            let p = ui.painter_at(strip);
            p.rect_filled(strip, 2.0, theme::BG);
            if let Some(pk) = peaks {
                for px in 0..strip.width() as usize {
                    let (lo, hi) = pk.range(px as f32 / strip.width(), (px + 1) as f32 / strip.width());
                    let x = strip.left() + px as f32 + 0.5;
                    let m = strip.center().y;
                    p.line_segment([pos2(x, m - hi.clamp(0.0, 1.0) * 6.0), pos2(x, m - lo.clamp(-1.0, 0.0) * 6.0)], Stroke::new(1.0, Color32::from_rgb(0x1c, 0x4a, 0x52)));
                }
            }
            let win = Rect::from_min_max(pos2(to_x(view.start), strip.top()), pos2(to_x(view.start + view.len).max(to_x(view.start) + 4.0), strip.bottom()));
            p.rect_filled(win, 2.0, Color32::from_rgba_unmultiplied(0xff, 0xb0, 0x20, 50));
            p.rect_stroke(win, 2.0, Stroke::new(1.5, theme::AMBER), egui::StrokeKind::Inside);
            if let Some(c) = cursor {
                p.line_segment([pos2(to_x(c), strip.top()), pos2(to_x(c), strip.bottom())], Stroke::new(1.5, theme::GREEN));
            }
        }
        resp.on_hover_cursor(egui::CursorIcon::Grab).on_hover_text("El archivo entero: arrastre la ventana para recorrerlo");
    }
    view
}

#[cfg(test)]
mod tests {
    use super::*;
    use qlab_protocol::cue_dto::AudioSliceMarker;

    fn axis() -> TimeAxis {
        TimeAxis { left: 10.0, width: 200.0, duration: 20.0, start: 0.0, len: 20.0 }
    }

    #[test]
    fn axis_maps_both_ways_and_clamps() {
        let a = axis();
        assert_eq!(a.x_of(0.0), 10.0);
        assert_eq!(a.x_of(20.0), 210.0);
        assert_eq!(a.x_of(10.0), 110.0);
        assert_eq!(a.x_of(30.0), 310.0, "fuera del tramo cae fuera del marco");
        assert_eq!(a.secs_of(110.0), 10.0);
        assert_eq!(a.secs_of(-50.0), 0.0);
        assert_eq!(a.secs_of(9_999.0), 20.0);
        for secs in [0.0, 1.25, 7.5, 19.9] {
            assert!((a.secs_of(a.x_of(secs)) - secs).abs() < 1e-3);
        }
    }

    #[test]
    fn end_zero_means_end_of_file() {
        let mut t = AudioTimeAndLoopsData::default();
        assert_eq!(effective_end(&t, 12.0), 12.0);
        t.end_time_sec = 5.0;
        assert_eq!(effective_end(&t, 12.0), 5.0);
    }

    #[test]
    fn ruler_steps_are_round_numbers() {
        assert_eq!(ruler_step(0.4), 0.05);
        assert_eq!(ruler_step(8.0), 1.0);
        assert_eq!(ruler_step(60.0), 10.0);
        assert_eq!(ruler_step(1.0e9), 3_600.0);
        for d in [0.5, 3.0, 17.0, 240.0, 5_000.0] {
            let n = d / ruler_step(d);
            assert!((1.0..=9.0).contains(&n), "{d}: {n} marcas");
        }
    }

    #[test]
    fn slices_are_kept_sorted_unique_and_inside_the_region() {
        let mut t = AudioTimeAndLoopsData { file_sample_rate: 1_000, start_time_sec: 1.0, ..Default::default() };
        t.slices = vec![
            AudioSliceMarker::new(5_000, 2),
            AudioSliceMarker::new(500, 2),   // antes del inicio
            AudioSliceMarker::new(3_000, 3),
            AudioSliceMarker::new(3_000, 9), // repetido
            AudioSliceMarker::new(8_000, 2), // en el fin
            AudioSliceMarker::new(9_000, 2), // después del fin
        ];
        normalize_slices(&mut t, 8.0);
        assert_eq!(t.slices.iter().map(|s| s.frame).collect::<Vec<_>>(), vec![3_000, 5_000]);
        assert_eq!(t.slices[0].play_count, 3);
        t.slices = (1..=100).map(|i| AudioSliceMarker::new(1_000 + i * 10, 1)).collect();
        normalize_slices(&mut t, 8.0);
        assert_eq!(t.slices.len(), MAX_SLICES_PER_VOICE - 1);
    }

    #[test]
    fn add_slice_uses_playing_then_edit_cursor_then_the_middle() {
        let mut t = AudioTimeAndLoopsData { file_sample_rate: 1_000, start_time_sec: 2.0, end_time_sec: 10.0, ..Default::default() };
        assert_eq!(slice_insert_point(&t, 20.0, Some(5.0), Some(7.0)), 5.0);
        assert_eq!(slice_insert_point(&t, 20.0, None, Some(7.0)), 7.0);
        assert_eq!(slice_insert_point(&t, 20.0, None, None), 6.0);
        assert_eq!(add_slice_at(&mut t, 20.0, 7.0), Some(0));
        assert_eq!(add_slice_at(&mut t, 20.0, 3.0), Some(0), "se inserta ordenado");
        assert_eq!(t.slices.iter().map(|s| s.frame).collect::<Vec<_>>(), [3_000, 7_000]);
        assert!(t.slices.iter().all(|s| s.play_count == 1));
        assert!(!slice_can_be_added(&t, 20.0, 7.0), "ya hay uno ahí");
        assert!(!slice_can_be_added(&t, 20.0, 1.0), "antes del inicio");
        assert!(!slice_can_be_added(&t, 20.0, 12.0), "después del fin");
        assert!(slice_can_be_added(&t, 20.0, 5.0));
    }

    #[test]
    fn marker_label_prefers_the_name_over_the_number() {
        assert_eq!(slice_marker_label(0, "", "inf"), "1 inf");
        assert_eq!(slice_marker_label(2, "  ", "×2"), "3 ×2");
        assert_eq!(slice_marker_label(1, "Danza en puntas", "×2"), "Danza en puntas ×2");
    }

    #[test]
    fn frames_and_seconds_roundtrip() {
        for sr in [44_100u32, 48_000, 96_000] {
            for secs in [0.0, 0.001, 1.5, 123.456] {
                let back = secs_of_frame(frame_of(secs, sr), sr);
                assert!((back - secs).abs() <= 0.5 / sr as f64 + 1e-12);
            }
        }
        assert_eq!(frame_of(-1.0, 48_000), 0);
    }
    #[test]
    fn memories_are_kept_inside_the_file_and_bounded_and_their_order_is_preserved() {
        let mut t = AudioTimeAndLoopsData::default();
        assert_eq!(add_memory_at(&mut t, 10.0, 7.0), Some(0));
        assert_eq!(add_memory_at(&mut t, 10.0, 2.0), Some(0), "la nueva queda primera por posición");
        t.memories[0].name = "x".repeat(200);
        t.memories[1].secs = f64::NAN;
        t.memories.push(MemoryMark { secs: 99.0, name: String::new(), note: "n".repeat(5_000) });
        normalize_memories(&mut t, 10.0);
        assert_eq!(t.memories.iter().map(|m| m.secs).collect::<Vec<_>>(), vec![2.0, 0.0, 10.0], "normalize no ordena: conserva el orden de la lista (manual o cronológico)");
        assert!(t.memories.iter().all(|m| m.name.chars().count() <= MAX_MEMORY_NAME_CHARS && m.note.chars().count() <= MAX_MEMORY_NOTE_CHARS));
        assert_eq!(add_memory_at(&mut t, 10.0, -5.0).map(|i| t.memories[i].secs), Some(0.0), "se recorta al inicio");
        for _ in 0..(MAX_MEMORIES + 10) {
            add_memory_at(&mut t, 10.0, 1.0);
        }
        assert_eq!(t.memories.len(), MAX_MEMORIES);
        assert_eq!(add_memory_at(&mut t, 10.0, 1.0), None);
    }


    /// Textos que se dibujaron en una pasada de `wave_view` con los cursores dados.
    fn painted_texts(edit: Option<f64>, play: Option<f64>) -> Vec<String> {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let mut t = AudioTimeAndLoopsData::default();
        t.file_sample_rate = 48_000;
        t.end_time_sec = 200.0;
        let mut edit = edit;
        let out = ctx.run_ui(egui::RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(900.0, 400.0))), ..Default::default() }, |ui| {
            wave_view(ui, &mut t, 200.0, None, None, &WaveState::Pending, play, &mut edit, 1, ViewWin::full(200.0));
        });
        let mut texts = Vec::new();
        for c in &out.shapes {
            collect_text(&c.shape, &mut texts);
        }
        out.drop_without_applying_deltas();
        texts
    }

    fn collect_text(s: &egui::epaint::Shape, out: &mut Vec<String>) {
        match s {
            egui::epaint::Shape::Text(t) => out.push(t.galley.text().to_string()),
            egui::epaint::Shape::Vec(v) => v.iter().for_each(|x| collect_text(x, out)),
            _ => {}
        }
    }

    #[test]
    fn the_edit_cursor_and_the_playing_cursor_show_their_exact_time_next_to_a_diamond() {
        let texts = painted_texts(Some(75.5), Some(120.25));
        assert!(texts.iter().any(|t| t == "01:15.50"), "el cursor del clic muestra su tiempo: {texts:?}");
        assert!(texts.iter().any(|t| t == "02:00.25"), "el de reproducción también: {texts:?}");
        let none = painted_texts(None, None);
        assert!(!none.iter().any(|t| t == "01:15.50"));
    }

    #[test]
    fn zooming_keeps_the_anchor_under_the_pointer_and_never_leaves_the_file() {
        let full = ViewWin::full(100.0);
        // Acercar x4 con el puntero en el segundo 40: el segundo 40 queda en el mismo lugar relativo.
        let z = full.zoomed(4.0, 40.0, 100.0);
        assert!((z.len - 25.0).abs() < 1e-9);
        assert!(((40.0 - z.start) / z.len - 0.4).abs() < 1e-9, "{z:?}");
        // Acercar en el borde no se sale: queda pegado.
        let edge = full.zoomed(10.0, 0.0, 100.0);
        assert_eq!((edge.start, edge.len), (0.0, 10.0));
        let edge = full.zoomed(10.0, 100.0, 100.0);
        assert!((edge.start + edge.len - 100.0).abs() < 1e-9);
        // Alejar vuelve a todo, sin pasarse; y no se acerca más de 20 ms.
        assert!(z.zoomed(0.001, 40.0, 100.0).is_full(100.0));
        assert!((full.zoomed(1.0e9, 50.0, 100.0).len - MIN_VIEW_SECS).abs() < 1e-12);
        // Desplazar se detiene en los extremos; valores locos se corrigen.
        assert_eq!(z.panned(-1.0e6, 100.0).start, 0.0);
        assert!((z.panned(1.0e6, 100.0).start + z.len - 100.0).abs() < 1e-9);
        assert_eq!(ViewWin { start: f64::NAN, len: f64::INFINITY }.clamped(100.0), full);
        assert_eq!(full.zoomed(f64::NAN, 1.0, 100.0), full);
        // Un archivo más corto que el mínimo no rompe nada.
        assert!(ViewWin::full(0.01).clamped(0.01).len <= 0.01);
    }

    #[test]
    fn the_view_follows_the_playback_by_pages_and_leaves_it_alone_while_it_is_visible() {
        let v = ViewWin { start: 20.0, len: 10.0 };
        assert_eq!(v.following(25.0, 100.0), v, "visible: no se mueve");
        let p = v.following(31.0, 100.0);
        assert!((p.start - 30.0).abs() < 1e-9 && p.len == 10.0, "pasa de página dejando el cursor al 10 %: {p:?}");
        let back = v.following(5.0, 100.0);
        assert!((back.start - 4.0).abs() < 1e-9);
        assert_eq!(ViewWin { start: 85.0, len: 10.0 }.following(99.0, 100.0).start, 90.0, "no pasa del final");
    }

    #[test]
    fn the_axis_with_zoom_maps_the_visible_window_to_the_whole_width() {
        let a = TimeAxis { left: 10.0, width: 200.0, duration: 100.0, start: 40.0, len: 20.0 };
        assert_eq!(a.x_of(40.0), 10.0);
        assert_eq!(a.x_of(60.0), 210.0);
        assert_eq!(a.x_of(50.0), 110.0);
        assert!(a.x_of(0.0) < 10.0 && a.x_of(100.0) > 210.0, "lo de afuera cae fuera del marco");
        assert_eq!(a.secs_of(110.0), 50.0);
        assert_eq!(a.secs_of(-1.0e6), 0.0);
        assert_eq!(a.secs_of(1.0e6), 100.0);
    }
}
