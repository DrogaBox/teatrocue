//! Pruebas con ratón y teclado reales sobre el árbol de accesibilidad (`egui_kittest`): se buscan los
//! widgets por su etiqueta, se pulsan, y se comprueba el efecto en el daemon.

use std::time::{Duration, Instant};

use eframe::egui::{self, Modifiers};
use egui_kittest::kittest::{NodeT, Queryable};
use egui_kittest::Harness;
use qlab_daemon::backend_audio::BackendKind;
use qlab_daemon::{Daemon, DaemonConfig};
use qlab_protocol::cue_dto::{CueBasicsData, CueId, MediaTarget};
use qlab_protocol::show_format::{AudioBody, CueBody, CueNodeDto, ShowFile, SHOW_FORMAT};

use crate::app::{AppConfig, QlabApp};
use crate::model::CueUiState;
use crate::views::inspector::Tab;
use crate::waveform::write_test_wav;

const WAIT: Duration = Duration::from_secs(8);

type H = Harness<'static, Option<QlabApp>>;

fn node(id: u64, wav: &std::path::Path) -> CueNodeDto {
    CueNodeDto {
        basics: CueBasicsData {
            unique_id: CueId(id),
            number: id.to_string(),
            name: format!("Pista {id}"),
            file_target: Some(MediaTarget { absolute_path: wav.to_string_lossy().into(), ..Default::default() }),
            ..Default::default()
        },
        triggers: Default::default(),
        dead: false,
        body: CueBody::Audio(AudioBody { time_loops: Default::default(), levels: None, eq: Default::default() }),
    }
}

struct Fixture {
    h: H,
    _daemon: Daemon,
    files: Vec<std::path::PathBuf>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for f in &self.files {
            let _ = std::fs::remove_file(f);
        }
    }
}

fn harness(cfg: AppConfig) -> H {
    Harness::builder().with_size(egui::vec2(1400.0, 1200.0)).with_accessibility_check(false).build_ui_state(
        move |ui, st: &mut Option<QlabApp>| {
            let app = st.get_or_insert_with(|| QlabApp::new(ui.ctx(), cfg.clone()));
            app.logic_step(&ui.ctx().clone());
            app.show(ui);
        },
        None,
    )
}

fn app(h: &H) -> &QlabApp {
    h.state().as_ref().expect("la app se crea en el primer fotograma")
}

fn pump(h: &mut H, what: &str, mut cond: impl FnMut(&QlabApp) -> bool) {
    let t0 = Instant::now();
    loop {
        h.step();
        if h.state().as_ref().is_some_and(&mut cond) {
            return;
        }
        assert!(t0.elapsed() < WAIT, "tiempo agotado esperando: {what}");
        std::thread::sleep(Duration::from_millis(4));
    }
}

fn fixture(tag: &str) -> Fixture {
    fixture_with(tag, false)
}

/// `vamp`: la cue 1 lleva un slice infinito desde 0,5 s (la pista dura 2 s).
fn fixture_with(tag: &str, vamp: bool) -> Fixture {
    let socket = format!("/tmp/qlab-ui-click-{}-{tag}.sock", std::process::id());
    let wav = write_test_wav(&format!("click-{tag}"), 2, 96_000, 48_000, |_, _| 0.3);
    let show = std::env::temp_dir().join(format!("qlab-ui-click-{}-{tag}.json", std::process::id()));
    let mut first = node(1, &wav);
    if let (true, CueBody::Audio(a)) = (vamp, &mut first.body) {
        a.time_loops.slices = vec![qlab_protocol::cue_dto::AudioSliceMarker::new(24_000, qlab_protocol::cue_dto::INFINITE_SLICE_LOOP)];
    }
    let cues = vec![first, node(2, &wav), node(3, &wav)];
    std::fs::write(&show, ShowFile { format: SHOW_FORMAT.into(), name: "Click".into(), sample_rate: 48_000, carts: Default::default(), cues }.to_json()).unwrap();
    let mut dc = DaemonConfig::default();
    dc.socket = socket.clone();
    dc.shm_name = format!("qlab_uiclick_{}_{tag}", std::process::id());
    dc.backend.kind = BackendKind::Null;
    dc.lock_memory = false;
    dc.disk_threads = 2;
    dc.show = Some(show.clone());
    let daemon = Daemon::start(dc).unwrap();
    let mut h = harness(AppConfig::new(socket));
    pump(&mut h, "conexión", |a| a.is_connected() && a.model.summaries.len() == 3 && a.inspector.draft.is_some());
    Fixture { h, _daemon: daemon, files: vec![wav, show] }
}

#[test]
fn clicking_the_go_button_fires_the_standby_cue() {
    let mut f = fixture("go");
    f.h.get_by_label("GO").click();
    pump(&mut f.h, "la cue 1 suena", |a| a.model.state_of(1) == CueUiState::Running);
    assert_eq!(app(&f.h).model.selected, Some(2), "la selección avanzó a la cue en espera");
    pump(&mut f.h, "el playhead pasa a la 2", |a| a.model.playhead == Some(2));
}

#[test]
fn clicking_a_row_selects_it_and_moves_the_playhead() {
    let mut f = fixture("row");
    f.h.get_by_label("Pista 3").click();
    pump(&mut f.h, "selección en la cue 3", |a| a.model.selected == Some(3) && a.inspector.draft.as_ref().is_some_and(|d| d.id() == 3));
    pump(&mut f.h, "el daemon mueve el playhead", |a| a.telemetry.playhead() == Some(3));
    assert_eq!(app(&f.h).model.playhead, Some(3));
    // El Inspector muestra la cue elegida.
    assert_eq!(app(&f.h).inspector.draft.as_ref().unwrap().node.basics.name, "Pista 3");
}

#[test]
fn the_panic_button_is_soft_and_a_second_press_is_hard() {
    let mut f = fixture("panic");
    f.h.get_by_label("GO").click();
    pump(&mut f.h, "suena", |a| a.model.state_of(1) == CueUiState::Running);
    f.h.get_by_label("PANIC").click();
    f.h.step();
    assert!(!app(&f.h).toasts.iter().any(|t| t.text.contains("Parada dura")));
    f.h.get_by_label("PANIC").click();
    f.h.step();
    assert!(app(&f.h).toasts.iter().any(|t| t.text.contains("Parada dura")));
    pump(&mut f.h, "la cue termina", |a| a.model.state_of(1) == CueUiState::Completed);
}

#[test]
fn transport_buttons_reach_the_daemon() {
    let mut f = fixture("transport");
    f.h.get_by_label("GO").click();
    pump(&mut f.h, "suena", |a| a.model.state_of(1) == CueUiState::Running);
    f.h.get_by_label("Pausar todo").click();
    pump(&mut f.h, "en pausa", |a| a.model.state_of(1) == CueUiState::Paused);
    f.h.get_by_label("Reanudar todo").click();
    pump(&mut f.h, "suena otra vez", |a| a.model.state_of(1) == CueUiState::Running);
    f.h.get_by_label("Parar todo").click();
    pump(&mut f.h, "parada", |a| a.model.state_of(1) == CueUiState::Completed);
}

#[test]
fn inspector_tabs_switch_by_clicking() {
    let mut f = fixture("tabs");
    for (label, tab) in [("Triggers", Tab::Triggers), ("Time & Loops", Tab::TimeLoops), ("Audio Levels", Tab::Levels), ("Basics", Tab::Basics)] {
        f.h.get_by_label(label).click();
        f.h.step();
        f.h.step();
        assert_eq!(app(&f.h).inspector.tab, tab, "{label}");
    }
}

#[test]
fn typing_in_the_name_field_saves_when_focus_leaves() {
    let mut f = fixture("name");
    // Basics muestra Número y Nombre; el nombre es el segundo campo de una sola línea.
    let inputs: Vec<_> = f.h.query_all_by_role(egui::accesskit::Role::TextInput).collect();
    assert!(inputs.len() >= 2, "campos de texto: {}", inputs.len());
    let name = inputs.into_iter().nth(1).unwrap();
    name.focus();
    f.h.step();
    f.h.key_press_modifiers(Modifiers::COMMAND, egui::Key::A);
    f.h.step();
    f.h.get_all_by_role(egui::accesskit::Role::TextInput).nth(1).unwrap().type_text("Apertura");
    f.h.step();
    assert!(app(&f.h).inspector.draft.as_ref().unwrap().dirty(), "el borrador cambia mientras se escribe");
    assert!(app(&f.h).inspector.in_flight.is_none(), "pero no se envía mientras el campo tiene el foco");
    // Enter confirma el campo.
    f.h.key_press(egui::Key::Enter);
    pump(&mut f.h, "guardado en el daemon", |a| a.model.summary(1).is_some_and(|c| c.name == "Apertura"));
}

#[test]
fn the_reconnect_screen_buttons_work() {
    let socket = format!("/tmp/qlab-ui-click-reconnect-{}.sock", std::process::id());
    let mut h = harness(AppConfig::new(socket));
    pump(&mut h, "pantalla de reconexión", |a| matches!(a.conn, crate::app::ConnState::Down { .. }));
    h.step();
    assert!(h.query_by_label("Reintentar ahora").is_some());
    assert!(h.query_by_label("Iniciar motor embebido sin audio").is_some());
    h.get_by_label("Iniciar motor embebido sin audio").click();
    pump(&mut h, "conectado al motor embebido", |a| a.is_connected());
    // Ya conectada, la pantalla principal reemplaza a la de reconexión.
    h.step();
    assert!(h.query_by_label("Reintentar ahora").is_none());
    assert!(h.query_by_label("GO").is_some() || h.query_by_label("FIN").is_some());
    h.state_mut().as_mut().unwrap().on_exit_for_test();
}

#[test]
fn space_goes_through_the_keyboard_path_too() {
    let mut f = fixture("space");
    f.h.key_press(egui::Key::Space);
    pump(&mut f.h, "la cue 1 suena con Espacio", |a| a.model.state_of(1) == CueUiState::Running);
    assert_eq!(app(&f.h).model.selected, Some(2));
}

fn slices_of(a: &QlabApp) -> Vec<(u64, u32)> {
    match &a.inspector.draft.as_ref().unwrap().node.body {
        CueBody::Audio(b) => b.time_loops.slices.iter().map(|s| (s.frame, s.play_count)).collect(),
        _ => vec![],
    }
}

#[test]
fn add_slice_button_inserts_a_marker_and_saves_it() {
    let mut f = fixture("addslice");
    f.h.get_by_label("Time & Loops").click();
    f.h.step();
    f.h.step();
    assert!(slices_of(app(&f.h)).is_empty());
    f.h.get_by_label("Add Slice").click();
    pump(&mut f.h, "slice guardado", |a| slices_of(a).len() == 1 && !a.inspector.draft.as_ref().unwrap().dirty());
    let (frame, count) = slices_of(app(&f.h))[0];
    assert!(frame > 0 && frame < 96_000, "dentro de la pista: {frame}");
    assert_eq!(count, 1, "una pasada por defecto");
}

#[test]
fn devamp_button_releases_a_cue_trapped_in_an_infinite_slice() {
    let mut f = fixture_with("devamp", true);
    f.h.get_by_label("Time & Loops").click();
    f.h.get_by_label("GO").click();
    pump(&mut f.h, "la cue 1 suena", |a| a.model.state_of(1) == CueUiState::Running);
    // La pista dura 2 s: pasado ese tiempo sigue sonando porque el slice infinito la retiene.
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_millis(2600) {
        f.h.step();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(app(&f.h).model.state_of(1), CueUiState::Running, "atrapada en el bucle");
    pump(&mut f.h, "indicador VAMP", |a| a.telemetry.running.get(&1).is_some_and(|i| i.slice_tag() == Some(crate::model::SliceTag::Vamp)));
    f.h.get_by_label("Pista 1").click();
    pump(&mut f.h, "Inspector en la cue 1", |a| a.inspector.draft.as_ref().is_some_and(|d| d.id() == 1));
    f.h.get_by_label("Time & Loops").click();
    f.h.step();
    f.h.step();
    f.h.get_by_label("Devamp").click();
    // El indicador cambia en cuanto se emite el Devamp y desaparece al terminar.
    pump(&mut f.h, "indicador DEVAMP", |a| a.telemetry.running.get(&1).is_some_and(|i| i.slice_tag() == Some(crate::model::SliceTag::Devamping)));
    pump(&mut f.h, "termina tras el Devamp", |a| a.model.state_of(1) == CueUiState::Completed);
    pump(&mut f.h, "el indicador desaparece", |a| a.telemetry.running.get(&1).is_none_or(|i| i.slice_tag().is_none()));
}

#[test]
fn the_published_position_wraps_inside_the_infinite_slice() {
    let mut f = fixture_with("wrap", true);
    f.h.get_by_label("GO").click();
    pump(&mut f.h, "la cue 1 suena", |a| a.model.state_of(1) == CueUiState::Running);
    // Pista de 2 s con un slice infinito en [0,5 s, 2 s): en 3,2 s el tiempo transcurrido supera la pista.
    let mut samples: Vec<(f32, u64)> = Vec::new();
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_millis(3200) {
        f.h.step();
        if let Some(i) = app(&f.h).telemetry.running.get(&1) {
            samples.push((i.action_elapsed, i.source_frame));
        }
        std::thread::sleep(Duration::from_millis(8));
    }
    assert!(samples.last().unwrap().0 > 2.2, "el tiempo transcurrido ya superó la duración de la pista");
    let frames: Vec<u64> = samples.iter().map(|s| s.1).collect();
    // Puede valer justo el final (96 000) en el instante previo a que el motor cierre la vuelta del bucle.
    assert!(frames.iter().all(|&p| p <= 96_000), "el cursor nunca pasa del final de la pista: {:?}", frames.iter().max());
    // Una vez dentro del tramo infinito no vuelve a salir por debajo de su inicio.
    let entered = frames.iter().position(|&p| p >= 24_000).expect("entra en el tramo infinito");
    assert!(frames[entered..].iter().all(|&p| p >= 24_000), "salió por debajo del inicio del slice");
    // Y la posición retrocedió (el bucle dio al menos una vuelta) a pesar de que el tiempo sigue creciendo.
    let wraps = frames.windows(2).filter(|w| w[1] + 10_000 < w[0]).count();
    assert!(wraps >= 1, "la posición debía volver al inicio del slice; muestras: {} wraps: {wraps}", frames.len());
    // El cursor que dibuja la pestaña es esa posición real, no el tiempo acumulado.
    let info = app(&f.h).telemetry.running[&1];
    assert!(info.cursor_secs(48_000, 0.0, 1.0) < 2.0);
}

#[test]
fn the_inspector_keeps_its_height_when_the_selection_changes() {
    let mut f = fixture("height");
    f.h.get_by_label("Time & Loops").click();
    for _ in 0..5 {
        f.h.step();
    }
    let base = app(&f.h).inspector_height;
    assert!(base >= 340.0, "altura inicial {base}");
    let mut seen = vec![base];
    // GO avanza la selección (y el Inspector recarga su borrador); luego saltos manuales entre filas.
    f.h.get_by_label("GO").click();
    for _ in 0..40 {
        f.h.step();
        seen.push(app(&f.h).inspector_height);
        std::thread::sleep(Duration::from_millis(8));
    }
    for name in ["Pista 3", "Pista 1", "Pista 2"] {
        f.h.get_by_label(name).click();
        for _ in 0..30 {
            f.h.step();
            seen.push(app(&f.h).inspector_height);
            std::thread::sleep(Duration::from_millis(8));
        }
    }
    let (min, max) = seen.iter().fold((f32::MAX, 0.0f32), |(lo, hi), &h| (lo.min(h), hi.max(h)));
    assert!((max - min) < 0.5, "la altura del Inspector varió entre {min} y {max}");
}

#[test]
fn time_and_loops_controls_stay_visible_below_the_waveform() {
    let mut f = fixture("visible");
    f.h.get_by_label("Time & Loops").click();
    for _ in 0..5 {
        f.h.step();
    }
    let window_bottom = 1200.0;
    for label in ["Add Slice", "Inicio", "Fin", "Velocidad", "Bucles"] {
        let r = f.h.get_by_label(label).rect();
        assert!(r.max.y <= window_bottom, "«{label}» queda fuera de la ventana: {r:?}");
        assert!(r.min.y >= 1200.0 - app(&f.h).inspector_height - 1.0, "«{label}» queda fuera del panel del Inspector: {r:?}");
    }
    // Y tras un GO sigue igual.
    f.h.get_by_label("GO").click();
    for _ in 0..30 {
        f.h.step();
        std::thread::sleep(Duration::from_millis(8));
    }
    let r = f.h.get_by_label("Velocidad").rect();
    assert!(r.max.y <= window_bottom, "tras GO: {r:?}");
}

#[test]
fn trigger_cards_form_two_columns_and_never_overflow_the_right_edge() {
    let mut f = fixture("triggers");
    f.h.get_by_label("Triggers").click();
    for width in [1400.0f32, 900.0] {
        f.h.set_size(egui::vec2(width, 1200.0));
        for _ in 0..6 {
            f.h.step();
        }
        let right = |l: &str| f.h.get_by_label(l).rect().max.x;
        let left = |l: &str| f.h.get_by_label(l).rect().min.x;
        // Columna 1: disparadores. Columna 2: comportamiento.
        for l in ["Hotkey", "MIDI", "Reloj de pared"] {
            assert!(left(l) < width / 2.0, "«{l}» debe estar en la columna izquierda ({width} px)");
        }
        for l in ["Fade & Stop Others (al disparar esta cue)", "Duck / Boost Others", "Segundo disparo"] {
            assert!(left(l) >= width / 2.0 - 20.0, "«{l}» debe estar en la columna derecha ({width} px)");
        }
        for l in ["Segundo disparo", "Reloj de pared", "Al soltar la tecla / nota", "Disparar a una hora", "Atenuar (o realzar) a las demás mientras suene"] {
            assert!(right(l) <= width, "«{l}» se corta por el margen derecho a {width} px: {}", right(l));
        }
    }
}

// -- Autoría: alta, baja, orden, guardado, apertura y archivos --------------------------------

use crate::native_dialog::{DialogKind, DialogOutcome};

fn cue_count(a: &QlabApp) -> usize {
    a.model.summaries.len()
}

#[test]
fn toolbar_adds_cues_after_the_selection_and_selects_them() {
    let mut f = fixture("tb_add");
    assert!(!app(&f.h).show_dirty);
    f.h.get_by_label("+ Audio Cue").click();
    pump(&mut f.h, "cue de audio nueva", |a| cue_count(a) == 4 && a.model.selected == Some(4));
    f.h.get_by_label("+ Fade Cue").click();
    pump(&mut f.h, "cue de fade nueva", |a| cue_count(a) == 5 && a.model.selected == Some(5));
    f.h.get_by_label("+ Group Cue").click();
    pump(&mut f.h, "grupo nuevo", |a| cue_count(a) == 6 && a.model.selected == Some(6));
    let order: Vec<u64> = app(&f.h).model.summaries.iter().map(|c| c.id).collect();
    // La selección inicial es la 1: la audio entra tras ella y cada nueva tras la anterior.
    assert_eq!(order, [1, 4, 5, 6, 2, 3]);
    let kinds: Vec<&str> = app(&f.h).model.summaries.iter().skip(1).take(3).map(|c| c.kind.as_str()).collect();
    assert_eq!(kinds, ["audio", "fade", "group"]);
    assert!(app(&f.h).show_dirty && app(&f.h).window_title().contains('*'));
    pump(&mut f.h, "el Inspector muestra la nueva", |a| a.inspector.draft.as_ref().is_some_and(|d| d.id() == 6));
}

#[test]
fn delete_key_and_trash_button_remove_the_selected_cue() {
    let mut f = fixture("tb_del");
    f.h.get_by_label("Pista 2").click();
    pump(&mut f.h, "selección en la 2", |a| a.model.selected == Some(2));
    f.h.key_press(egui::Key::Delete);
    pump(&mut f.h, "la 2 desaparece", |a| cue_count(a) == 2 && a.model.summary(2).is_none());
    // La selección pasa a una cue que existe y el Inspector la sigue.
    pump(&mut f.h, "nueva selección", |a| a.model.selected.is_some_and(|s| a.model.summary(s).is_some()));
    f.h.get_by_label("Eliminar la cue seleccionada (Supr)").click();
    pump(&mut f.h, "otra baja con el botón", |a| cue_count(a) == 1);
    pump(&mut f.h, "queda una cue seleccionada", |a| a.model.selected.is_some() && a.inspector.draft.is_some());
    f.h.key_press(egui::Key::Backspace);
    pump(&mut f.h, "lista vacía", |a| cue_count(a) == 0 && a.model.selected.is_none());
    assert!(app(&f.h).inspector.draft.is_none());
}

#[test]
fn arrow_buttons_reorder_the_selected_cue() {
    let mut f = fixture("tb_move");
    f.h.get_by_label("Pista 3").click();
    pump(&mut f.h, "selección en la 3", |a| a.model.selected == Some(3));
    f.h.get_by_label("Subir la cue seleccionada").click();
    pump(&mut f.h, "la 3 sube", |a| a.model.summaries.iter().map(|c| c.id).collect::<Vec<_>>() == [1, 3, 2]);
    f.h.get_by_label("Subir la cue seleccionada").click();
    pump(&mut f.h, "la 3 sube otra vez", |a| a.model.summaries.iter().map(|c| c.id).collect::<Vec<_>>() == [3, 1, 2]);
    assert_eq!(app(&f.h).model.selected, Some(3), "la selección sigue a la cue");
    f.h.get_by_label("Bajar la cue seleccionada").click();
    pump(&mut f.h, "la 3 baja", |a| a.model.summaries.iter().map(|c| c.id).collect::<Vec<_>>() == [1, 3, 2]);
}

#[test]
fn save_new_and_open_roundtrip_through_the_ui_with_title_and_confirmation() {
    let mut f = fixture("persist");
    let path = std::env::temp_dir().join(format!("qlab-ui-persist-{}.json", std::process::id()));
    let _ = std::fs::remove_file(&path);
    f.files.push(path.clone());

    f.h.get_by_label("+ Audio Cue").click();
    pump(&mut f.h, "alta", |a| cue_count(a) == 4);
    assert!(app(&f.h).show_dirty);

    // Guardar como (resultado del diálogo nativo): el título refleja el nombre del archivo.
    f.h.state_mut().as_mut().unwrap().on_dialog_outcome(DialogOutcome { kind: DialogKind::SaveShowAs, path: Some(path.clone()), more: Vec::new() });
    let stem = path.file_stem().unwrap().to_string_lossy().into_owned();
    pump(&mut f.h, "guardado", |a| !a.show_dirty && a.show_path.as_deref() == Some(path.as_path()));
    assert_eq!(app(&f.h).window_title(), format!("{stem} — TeatroCue"));
    let saved = qlab_protocol::show_format::ShowFile::from_json(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!((saved.cue_count(), saved.name.as_str()), (4, stem.as_str()));

    // Un cambio posterior marca el show como modificado; Guardar (Ctrl+S) lo escribe en el mismo archivo.
    f.h.get_by_label("+ Fade Cue").click();
    pump(&mut f.h, "otra alta", |a| cue_count(a) == 5 && a.show_dirty);
    assert!(app(&f.h).window_title().contains('*'));
    f.h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::S);
    pump(&mut f.h, "guardado con Ctrl+S", |a| !a.show_dirty);
    assert_eq!(qlab_protocol::show_format::ShowFile::from_json(&std::fs::read_to_string(&path).unwrap()).unwrap().cue_count(), 5);

    // Show nuevo con cambios sin guardar: pide confirmación; Cancelar no toca nada.
    f.h.get_by_label("+ Group Cue").click();
    pump(&mut f.h, "grupo", |a| cue_count(a) == 6 && a.show_dirty);
    f.h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::N);
    pump(&mut f.h, "aparece la confirmación", |a| a.confirm.is_some());
    f.h.get_by_label("Cancelar").click();
    pump(&mut f.h, "se cierra", |a| a.confirm.is_none());
    assert_eq!(cue_count(app(&f.h)), 6, "Cancelar conserva el show");
    // Descartar limpia el show y el título vuelve a «Sin título».
    f.h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::N);
    pump(&mut f.h, "confirmación otra vez", |a| a.confirm.is_some());
    f.h.get_by_label("Descartar cambios").click();
    pump(&mut f.h, "show vacío", |a| cue_count(a) == 0 && !a.show_dirty && a.show_path.is_none());
    assert_eq!(app(&f.h).window_title(), "Sin título — TeatroCue");

    // Abrir lo guardado: vuelven las 5 cues (la última versión) y el título con su nombre.
    f.h.state_mut().as_mut().unwrap().on_dialog_outcome(DialogOutcome { kind: DialogKind::OpenShow, path: Some(path.clone()), more: Vec::new() });
    pump(&mut f.h, "show abierto", |a| cue_count(a) == 5 && a.show_path.as_deref() == Some(path.as_path()) && !a.show_dirty);
    assert_eq!(app(&f.h).window_title(), format!("{stem} — TeatroCue"));
}

#[test]
fn browse_assigns_an_audio_file_to_a_new_cue_and_arms_it() {
    let mut f = fixture("browse");
    let wav = f.files[0].clone();
    f.h.get_by_label("+ Audio Cue").click();
    pump(&mut f.h, "cue nueva", |a| cue_count(a) == 4 && a.inspector.draft.as_ref().is_some_and(|d| d.id() == 4));
    assert!(!app(&f.h).inspector.draft.as_ref().unwrap().node.basics.armed, "nace desarmada");
    assert!(f.h.query_by_label("Examinar…").is_some());
    // Resultado del diálogo nativo «Examinar…».
    f.h.state_mut().as_mut().unwrap().on_dialog_outcome(DialogOutcome { kind: DialogKind::PickAudio, path: Some(wav.clone()), more: Vec::new() });
    pump(&mut f.h, "guardada en el daemon con duración real", |a| {
        a.inspector.draft.as_ref().is_some_and(|d| {
            let CueBody::Audio(b) = &d.baseline.body else { return false };
            d.id() == 4 && d.baseline.basics.armed && !d.dirty() && (b.time_loops.end_time_sec - 2.0).abs() < 1e-6
        })
    });
    let c = app(&f.h).model.summary(4).unwrap().clone();
    assert!(c.armed && c.target.ends_with(".wav"), "{c:?}");
}

#[test]
fn a_fade_cue_opens_on_its_own_tab_with_target_duration_curve_and_levels() {
    let mut f = fixture("fadetab");
    f.h.get_by_label("+ Fade Cue").click();
    pump(&mut f.h, "fade nuevo en el Inspector", |a| a.inspector.draft.as_ref().is_some_and(|d| d.id() == 4 && matches!(d.node.body, CueBody::Fade(_))));
    for _ in 0..4 {
        f.h.step();
    }
    assert_eq!(app(&f.h).inspector.tab, Tab::Fade);
    for label in ["Cue destino", "Duración", "Curva", "Celdas que se modulan", "+ Celda"] {
        assert!(f.h.query_by_label(label).is_some(), "falta «{label}» en la pestaña Fade");
    }
    // Un fade no tiene pestañas de audio, y al volver a una cue de audio se sale de la pestaña Fade.
    assert!(f.h.query_by_label("Time & Loops").is_none() && f.h.query_by_label("Audio Levels").is_none());
    f.h.get_by_label("Pista 1").click();
    pump(&mut f.h, "Inspector en la 1", |a| a.inspector.draft.as_ref().is_some_and(|d| d.id() == 1));
    f.h.step();
    assert_eq!(app(&f.h).inspector.tab, Tab::Basics);
    // La duración y las celdas editadas se guardan en el daemon.
    f.h.state_mut().as_mut().unwrap().select(4);
    pump(&mut f.h, "de vuelta en el fade", |a| a.inspector.draft.as_ref().is_some_and(|d| d.id() == 4));
    f.h.step();
    f.h.get_by_label("+ Master a -inf").click();
    pump(&mut f.h, "celda guardada", |a| {
        a.inspector.draft.as_ref().is_some_and(|d| matches!(&d.baseline.body, CueBody::Fade(b) if b.cells.len() == 1) && !d.dirty())
    });
}

fn continue_modes(a: &QlabApp) -> Vec<qlab_protocol::cue_dto::ContinueMode> {
    a.model.summaries.iter().map(|c| c.continue_mode).collect()
}

#[test]
fn the_next_column_cycles_the_continue_mode_and_saves_it() {
    use qlab_protocol::cue_dto::ContinueMode::*;
    let mut f = fixture("sig");
    assert_eq!(continue_modes(app(&f.h)), [DoNotContinue; 3]);
    // Cue 2 (no seleccionada): el daemon recibe el cambio vía detalle + UpdateCue.
    f.h.get_all_by_label("No continuar (Do Not Continue)").nth(1).unwrap().click();
    pump(&mut f.h, "la 2 pasa a Auto-continue", |a| continue_modes(a) == [DoNotContinue, AutoContinue, DoNotContinue]);
    // Y queda guardado en el daemon (no solo en la tabla).
    pump(&mut f.h, "confirmado por el daemon", |a| a.show_dirty);
    // Segunda pulsación: Auto-follow; tercera: vuelve a No continuar.
    f.h.step();
    f.h.step();
    f.h.get_by_label("Auto-continue: dispara la siguiente tras el pre-wait + post-wait").click();
    pump(&mut f.h, "Auto-follow", |a| continue_modes(a) == [DoNotContinue, AutoFollow, DoNotContinue]);
    f.h.step();
    f.h.step();
    f.h.get_by_label("Auto-follow: dispara la siguiente cuando esta termina").click();
    pump(&mut f.h, "otra vez No continuar", |a| continue_modes(a) == [DoNotContinue; 3]);
    f.h.step();
    f.h.step();
    // Cue 1 (en el Inspector): se edita su borrador y se guarda sola.
    f.h.get_all_by_label("No continuar (Do Not Continue)").next().unwrap().click();
    pump(&mut f.h, "la 1 pasa a Auto-continue y se guarda", |a| {
        a.inspector.draft.as_ref().is_some_and(|d| d.id() == 1 && d.baseline.basics.continue_mode == AutoContinue && !d.dirty())
    });
    assert_eq!(app(&f.h).model.selected, Some(1), "pulsar la celda no cambia la selección");
}

#[test]
fn a_slice_name_is_saved_with_the_cue_and_survives_save_and_open() {
    let mut f = fixture("slicename");
    let path = std::env::temp_dir().join(format!("qlab-ui-slicename-{}.json", std::process::id()));
    f.files.push(path.clone());
    f.h.get_by_label("Time & Loops").click();
    f.h.step();
    f.h.step();
    f.h.get_by_label("Add Slice").click();
    pump(&mut f.h, "slice guardado", |a| slices_of(a).len() == 1 && !a.inspector.draft.as_ref().unwrap().dirty());
    // El nombre se escribe en el borrador (como el campo de la lista) y viaja con la cue.
    if let CueBody::Audio(b) = &mut f.h.state_mut().as_mut().unwrap().inspector.draft.as_mut().unwrap().node.body {
        b.time_loops.slices[0].label = "Danza en puntas".into();
    }
    let named = |a: &QlabApp| match &a.inspector.draft.as_ref().unwrap().baseline.body {
        CueBody::Audio(b) => b.time_loops.slices.first().is_some_and(|s| s.label == "Danza en puntas"),
        _ => false,
    };
    pump(&mut f.h, "nombre guardado en el daemon", |a| named(a) && !a.inspector.draft.as_ref().unwrap().dirty());
    // Guardar el show, vaciarlo y abrirlo: el nombre sigue ahí.
    f.h.state_mut().as_mut().unwrap().on_dialog_outcome(DialogOutcome { kind: DialogKind::SaveShowAs, path: Some(path.clone()), more: Vec::new() });
    pump(&mut f.h, "show guardado", |a| !a.show_dirty && a.show_path.is_some());
    assert!(std::fs::read_to_string(&path).unwrap().contains("Danza en puntas"));
    f.h.state_mut().as_mut().unwrap().begin_new_show();
    pump(&mut f.h, "show vacío", |a| a.model.summaries.is_empty());
    f.h.state_mut().as_mut().unwrap().on_dialog_outcome(DialogOutcome { kind: DialogKind::OpenShow, path: Some(path), more: Vec::new() });
    pump(&mut f.h, "show abierto con el nombre del slice", |a| cue_count(a) == 3 && a.inspector.draft.as_ref().is_some_and(|d| d.id() == 1) && named(a));
}

#[test]
fn adding_a_cue_to_an_empty_list_inserts_it_and_opens_the_inspector() {
    let mut f = fixture("empty_add");
    f.h.state_mut().as_mut().unwrap().begin_new_show();
    pump(&mut f.h, "lista vacía", |a| a.model.summaries.is_empty() && a.model.selected.is_none());
    f.h.step();
    f.h.get_by_label("+ Audio Cue").click();
    pump(&mut f.h, "la cue entra y se selecciona", |a| cue_count(a) == 1 && a.model.selected == Some(1));
    pump(&mut f.h, "el Inspector se abre con ella", |a| a.inspector.draft.as_ref().is_some_and(|d| d.id() == 1));
    // Sin selección activa (p. ej. tras deseleccionar) también va al final.
    f.h.state_mut().as_mut().unwrap().model.selected = None;
    f.h.state_mut().as_mut().unwrap().inspector.draft = None;
    f.h.step();
    f.h.get_by_label("+ Fade Cue").click();
    pump(&mut f.h, "al final y seleccionada", |a| cue_count(a) == 2 && a.model.selected == Some(2));
    assert_eq!(app(&f.h).model.summaries.iter().map(|c| c.id).collect::<Vec<_>>(), [1, 2]);
}

#[test]
fn a_freshly_started_empty_daemon_accepts_the_first_cue() {
    let socket = format!("/tmp/qlab-ui-click-{}-cold.sock", std::process::id());
    let mut dc = DaemonConfig::default();
    dc.socket = socket.clone();
    dc.shm_name = format!("qlab_uiclick_{}_cold", std::process::id());
    dc.backend.kind = BackendKind::Null;
    dc.lock_memory = false;
    dc.disk_threads = 2;
    let _daemon = Daemon::start(dc).unwrap();
    let mut h = harness(AppConfig::new(socket));
    pump(&mut h, "conexión", |a| a.is_connected());
    for _ in 0..10 {
        h.step();
    }
    assert!(app(&h).model.summaries.is_empty());
    h.get_by_label("+ Audio Cue").click();
    pump(&mut h, "primera cue creada y seleccionada", |a| cue_count(a) == 1 && a.model.selected == Some(1) && a.inspector.draft.as_ref().is_some_and(|d| d.id() == 1));
    h.get_by_label("+ Group Cue").click();
    pump(&mut h, "segunda al final", |a| cue_count(a) == 2 && a.model.selected == Some(2));
}

// -- Arrastrar y soltar -------------------------------------------------------------------------------

#[derive(Debug)]
struct TestDrop(std::path::PathBuf);

impl egui::DroppedFile for TestDrop {
    fn path(&self) -> &std::path::Path {
        &self.0
    }
    fn bytes(&self) -> Result<Vec<u8>, String> {
        std::fs::read(&self.0).map_err(|e| e.to_string())
    }
}

fn drop_files(h: &mut H, at: egui::Pos2, files: &[std::path::PathBuf]) {
    h.input_mut().events.push(egui::Event::PointerMoved(at));
    h.step();
    for f in files {
        h.input_mut().dropped_files.push(std::sync::Arc::new(TestDrop(f.clone())));
    }
    h.step();
}

#[test]
fn dropping_media_files_creates_armed_named_cues_and_selects_the_first() {
    let mut f = fixture("dnd_new");
    let named = |tag: &str| {
        let src = write_test_wav(tag, 2, 48_000, 48_000, |_, _| 0.1);
        let dst = std::env::temp_dir().join(format!("{tag}-{}.wav", std::process::id()));
        std::fs::rename(&src, &dst).unwrap();
        dst
    };
    let (a, b) = (named("Entrada coro"), named("Danza en puntas"));
    let txt = std::env::temp_dir().join(format!("notas-{}.txt", std::process::id()));
    std::fs::write(&txt, "x").unwrap();
    f.files.extend([a.clone(), b.clone(), txt.clone()]);

    // Sobre la tabla (arriba): se crean cues nuevas tras la seleccionada (la 1); el .txt se ignora.
    drop_files(&mut f.h, egui::pos2(500.0, 200.0), &[a.clone(), txt, b.clone()]);
    pump(&mut f.h, "dos cues importadas", |a| cue_count(a) == 5 && a.model.summaries.iter().all(|c| c.state != "Idle" || c.armed));
    let s = &app(&f.h).model.summaries;
    assert_eq!(s.iter().map(|c| c.id).collect::<Vec<_>>(), [1, 4, 5, 2, 3], "entran en orden, justo después de la seleccionada");
    assert_eq!((s[1].name.as_str(), s[2].name.as_str()), (format!("Entrada coro-{}", std::process::id()).as_str(), format!("Danza en puntas-{}", std::process::id()).as_str()));
    assert!(s[1].armed && s[2].armed);
    assert!(s[1].target.ends_with(".wav") && s[2].target.ends_with(".wav"));
    pump(&mut f.h, "se selecciona la primera importada", |a| a.model.selected == Some(4) && a.inspector.draft.as_ref().is_some_and(|d| d.id() == 4));
}

#[test]
fn dropping_a_file_on_the_inspector_retargets_the_selected_cue() {
    let mut f = fixture("dnd_inspector");
    let wav2 = write_test_wav("dnd-retarget", 2, 24_000, 48_000, |_, _| 0.1);
    f.files.push(wav2.clone());
    for _ in 0..4 {
        f.h.step();
    }
    let inspector = app(&f.h).inspector_rect;
    assert!(inspector.height() > 100.0, "{inspector:?}");
    drop_files(&mut f.h, inspector.center(), &[wav2.clone()]);
    let want = wav2.to_string_lossy().into_owned();
    pump(&mut f.h, "la cue 1 apunta al archivo soltado", |a| {
        a.inspector.draft.as_ref().is_some_and(|d| d.id() == 1 && d.baseline.basics.file_target.as_ref().is_some_and(|t| t.absolute_path == want) && !d.dirty())
    });
    assert_eq!(cue_count(app(&f.h)), 3, "no se creó ninguna cue nueva");
}

#[test]
fn hovering_files_draws_the_overlay_without_side_effects() {
    let mut f = fixture("dnd_hover");
    f.h.input_mut().hovered_files.push(egui::HoveredFile { path: Some("/x/a.wav".into()), mime: String::new() });
    for _ in 0..3 {
        f.h.step();
    }
    assert_eq!(cue_count(app(&f.h)), 3);
}

#[test]
fn bundle_show_copies_media_next_to_a_relative_show_file() {
    let mut f = fixture("bundle_ui");
    let out = std::env::temp_dir().join(format!("qlab-ui-bundle-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&out);
    f.h.state_mut().as_mut().unwrap().on_dialog_outcome(DialogOutcome { kind: DialogKind::PickBundleFolder, path: Some(out.clone()), more: Vec::new() });
    pump(&mut f.h, "aviso de show empaquetado", |a| a.toasts.iter().any(|t| t.text.contains("Show empaquetado")));
    let audio: Vec<_> = std::fs::read_dir(out.join("audio")).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert_eq!(audio.len(), 1, "las 3 cues comparten un solo archivo: {audio:?}");
    let json = std::fs::read_dir(&out).unwrap().filter_map(|e| e.ok()).find(|e| e.path().extension().is_some_and(|x| x == "teatroshow")).expect("show .teatroshow en la carpeta");
    let text = std::fs::read_to_string(json.path()).unwrap();
    assert!(text.contains("\"audio/") && !text.contains(&out.to_string_lossy().to_string()));
    let _ = std::fs::remove_dir_all(&out);
}

#[test]
fn the_table_shows_a_per_cue_meter_only_for_the_cue_that_is_sounding() {
    let mut f = fixture("cue_meter");
    assert!(app(&f.h).telemetry.cue_level_db(1).is_none());
    f.h.get_by_label("GO").click();
    pump(&mut f.h, "el vúmetro de la cue 1 mide la señal real (0,3 ≈ -10,5 dBFS)", |a| a.telemetry.cue_level_db(1).is_some_and(|db| (-12.0..-9.0).contains(&db)));
    assert!(app(&f.h).telemetry.cue_level_db(2).is_none() && app(&f.h).telemetry.cue_level_db(3).is_none(), "las que no suenan no marcan");
    // Al terminar (la pista dura 2 s) el vúmetro cae y desaparece.
    pump(&mut f.h, "cue terminada", |a| a.model.state_of(1) == CueUiState::Completed);
    pump(&mut f.h, "el vúmetro desaparece", |a| a.telemetry.cue_level_db(1).is_none());
}

// -- Vídeo ---------------------------------------------------------------------------------------

/// Vídeo de prueba de 2 s (imagen + tono) con ffmpeg; `None` si no está instalado.
fn test_video(tag: &str) -> Option<std::path::PathBuf> {
    let path = std::env::temp_dir().join(format!("qlab-ui-{tag}-{}.mp4", std::process::id()));
    let ok = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc=duration=2:size=320x180:rate=25", "-f", "lavfi", "-i", "sine=frequency=440:duration=2:sample_rate=44100", "-c:v", "mpeg4", "-c:a", "aac", "-shortest"])
        .arg(&path)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    ok.then_some(path)
}

/// Crea una cue de vídeo con el archivo dado (por la barra de herramientas y «Examinar») y espera a que se guarde.
fn add_video_cue(f: &mut Fixture, video: &std::path::Path) -> u64 {
    f.h.get_by_label("+ Video Cue").click();
    pump(&mut f.h, "cue de vídeo nueva", |a| a.inspector.draft.as_ref().is_some_and(|d| d.node.body.is_video()));
    let id = app(&f.h).inspector.draft.as_ref().unwrap().id();
    f.h.state_mut().as_mut().unwrap().on_dialog_outcome(DialogOutcome { kind: DialogKind::PickAudio, path: Some(video.to_path_buf()), more: Vec::new() });
    pump(&mut f.h, "vídeo guardado con su duración", |a| {
        a.inspector.draft.as_ref().is_some_and(|d| d.id() == id && !d.dirty() && matches!(&d.baseline.body, CueBody::Video(v) if (v.audio.time_loops.end_time_sec - 2.0).abs() < 0.1))
    });
    id
}

#[test]
fn a_video_cue_is_created_from_the_toolbar_and_opens_on_its_video_tab() {
    let Some(video) = test_video("toolbar") else { return eprintln!("ffmpeg no está instalado: se omite") };
    let mut f = fixture("video_toolbar");
    f.files.push(video.clone());
    let id = add_video_cue(&mut f, &video);
    assert_eq!(app(&f.h).inspector.tab, Tab::Video);
    assert_eq!(app(&f.h).model.summary(id).unwrap().kind, "video");
    assert!(app(&f.h).model.summary(id).unwrap().armed, "con archivo queda armada");
    assert_eq!(app(&f.h).model.summary(id).unwrap().duration.map(|d| d.round()), Some(2.0), "la duración sale de la pista de audio");
    for l in ["Vista previa", "Abrir pantalla de proyección"] {
        pump(&mut f.h, l, |_| true);
        assert!(f.h.query_by_label(l).is_some(), "falta «{l}»");
    }
    // Las pestañas de audio también están: la pista de audio tiene tiempos, bucles y niveles.
    assert!(f.h.query_by_label("Time & Loops").is_some() && f.h.query_by_label("Audio Levels").is_some());
}

#[test]
fn the_inspector_shows_a_thumbnail_of_the_video_even_when_it_is_not_playing() {
    let Some(video) = test_video("thumb") else { return eprintln!("ffmpeg no está instalado: se omite") };
    let mut f = fixture("video_thumb");
    f.files.push(video.clone());
    let id = add_video_cue(&mut f, &video);
    pump(&mut f.h, "miniatura decodificada", |a| a.video.texture_of(id).is_some());
    let tex = app(&f.h).video.texture_of(id).unwrap().size_vec2();
    assert_eq!((tex.x as u32, tex.y as u32), (320, 180));
    let info = app(&f.h).video.players[&id].info.clone().unwrap();
    assert_eq!((info.width, info.height, info.fps.round() as u32), (320, 180, 25));
}

#[test]
fn playing_a_video_cue_follows_the_audio_clock_and_projects_it() {
    let Some(video) = test_video("play") else { return eprintln!("ffmpeg no está instalado: se omite") };
    let mut f = fixture("video_play");
    f.files.push(video.clone());
    let id = add_video_cue(&mut f, &video);
    pump(&mut f.h, "miniatura", |a| a.video.texture_of(id).is_some());
    // El playhead va a la cue de vídeo y GO la arranca.
    f.h.state_mut().as_mut().unwrap().apply(crate::views::Action::SetPlayhead(id), &egui::Context::default());
    f.h.state_mut().as_mut().unwrap().go();
    pump(&mut f.h, "suena", |a| a.model.state_of(id) == CueUiState::Running);
    pump(&mut f.h, "la proyección elige esta cue", |a| a.projector_cue == Some(id));
    f.h.state_mut().as_mut().unwrap().open_projector();
    // Durante la reproducción el cuadro mostrado nunca se desvía mucho del reloj de audio.
    let t0 = Instant::now();
    let (mut checked, mut worst) = (0, 0.0f64);
    while t0.elapsed() < Duration::from_millis(1500) {
        f.h.step();
        let a = app(&f.h);
        if let (Some(info), Some(p)) = (a.telemetry.running.get(&id), a.video.players.get(&id)) {
            let meta = &p.meta;
            let clock = info.cursor_secs(meta.sample_rate, meta.start_time_sec, 1.0);
            if let Some(shown) = p.shown_pts() {
                worst = worst.max((clock - shown).abs());
                checked += 1;
            }
        }
        std::thread::sleep(Duration::from_millis(8));
    }
    assert!(checked > 30, "se comprobó el sincronismo en {checked} fotogramas");
    assert!(worst < 0.25, "el cuadro mostrado se desvió {worst:.3} s del reloj de audio");
    // Al terminar la cue la proyección se queda a oscuras y la miniatura vuelve al primer cuadro.
    pump(&mut f.h, "termina", |a| a.model.state_of(id) == CueUiState::Completed);
    pump(&mut f.h, "la proyección se apaga", |a| a.projector_cue.is_none());
    assert!(app(&f.h).projection.open);
}

#[test]
fn dropping_a_video_file_creates_an_armed_video_cue() {
    let Some(video) = test_video("drop") else { return eprintln!("ffmpeg no está instalado: se omite") };
    let mut f = fixture("video_drop");
    f.files.push(video.clone());
    drop_files(&mut f.h, egui::pos2(500.0, 200.0), &[video.clone()]);
    pump(&mut f.h, "cue de vídeo importada", |a| a.model.summaries.iter().any(|c| c.kind == "video" && c.armed && c.name.starts_with("qlab-ui-drop")));
    pump(&mut f.h, "queda seleccionada con la pestaña Video", |a| a.inspector.draft.as_ref().is_some_and(|d| d.node.body.is_video()) && a.inspector.tab == Tab::Video);
}

// -- Licencia ------------------------------------------------------------------------------------

#[test]
fn an_invalid_license_shows_only_the_lock_screen_with_the_machine_id() {
    use crate::license_gate::LicenseInfo;
    use qlab_license::{LicenseError, MachineId, Status};
    let id = MachineId::from_guid("pc-sin-licencia");
    let status = Status { machine: Some(id.clone()), result: Err(LicenseError::NotFound("C:/TeatroCue/license.key".into())), path: "C:/TeatroCue/license.key".into(), now: 0, effective_expires_unix: 0 };
    let mut cfg = AppConfig::new(format!("/tmp/qlab-ui-click-{}-lock.sock", std::process::id()));
    cfg.embedded_on_start = Some(false); // la pantalla de bloqueo impide incluso el motor embebido
    cfg.license = LicenseInfo::from_status(&status);
    let mut h = harness(cfg);
    for _ in 0..6 {
        h.step();
    }
    assert!(h.query_by_label("Licencia no válida en esta PC").is_some());
    assert!(h.query_by_label(&id.to_string()).is_some(), "se muestra el Machine ID para enviarlo al desarrollador");
    assert!(h.query_by_label("Copiar Machine ID").is_some());
    // Nada de la aplicación está accesible: ni barra de herramientas, ni GO, ni menús.
    for l in ["+ Audio Cue", "GO", "Archivo"] {
        assert!(h.query_by_label(l).is_none(), "«{l}» no debe aparecer bloqueado");
    }
    let a = app(&h);
    assert!(a.license.is_locked() && !a.is_connected());
    assert!(!a.embedded_running(), "sin licencia no arranca el motor embebido");
}

#[test]
fn a_valid_license_shows_the_normal_interface() {
    use crate::license_gate::{LicenseInfo, LicenseState};
    let mut cfg = AppConfig::new(format!("/tmp/qlab-ui-click-{}-nolock.sock", std::process::id()));
    cfg.license = LicenseInfo { state: LicenseState::Valid { licensee: "Ballet Oficial de Córdoba".into(), issued_unix: 1, expires_unix: 0, days_left: None }, machine_id: Some("AAAA-BBBB-CCCC-DDDD".into()), lock_text: None, reactivable: false };
    let mut h = harness(cfg);
    for _ in 0..4 {
        h.step();
    }
    assert!(h.query_by_label("Licencia no válida en esta PC").is_none());
    // Sin daemon cerca se ve la pantalla de reconexión normal, no la de bloqueo.
    assert!(h.query_by_label("Reintentar ahora").is_some());
}

// -- Ayuda ----------------------------------------------------------------------------------------

#[test]
fn f1_opens_the_manual_with_three_tabs_and_the_legal_authorization() {
    let mut f = fixture("help");
    f.h.key_press(egui::Key::F1);
    for _ in 0..4 {
        f.h.step();
    }
    assert!(app(&f.h).help.open);
    for tab in ["Atajos de Teclado", "Operación de Escena", "Autorización Legal y Licencia"] {
        assert!(f.h.query_by_label(tab).is_some(), "falta la pestaña «{tab}»");
    }
    // Atajos: los pedidos están descritos.
    for k in ["Espacio", "Esc, Esc", "Supr / Retroceso", "Ctrl+N", "Ctrl+Mayús+S"] {
        assert!(f.h.query_by_label(k).is_some(), "falta el atajo {k}");
    }
    f.h.get_by_label("Operación de Escena").click();
    f.h.step();
    f.h.step();
    for h in ["Slices con etiquetas coreográficas", "Salida limpia con Devamp", "Audio Levels: matriz, Gangs y Mute/Solo", "Vídeo esclavo del audio"] {
        assert!(f.h.query_by_label(h).is_some(), "falta «{h}»");
    }
    f.h.get_by_label("Autorización Legal y Licencia").click();
    f.h.step();
    f.h.step();
    assert!(f.h.query_by_label("AUTORIZACIÓN DE USO EXCLUSIVO:").is_some());
    assert!(f.h.query_by_label(crate::views::help_modal::AUTHORIZATION_TEXT).is_some(), "el texto legal se muestra completo");
    assert!(f.h.query_by_label("Ballet Oficial de Córdoba").is_some(), "Licenciado a");
    let id = app(&f.h).license.machine_id.clone();
    if let Some(id) = id {
        assert!(f.h.query_by_label(&id).is_some(), "Machine ID visible");
    }
    // Mientras el manual está abierto, Esc lo cierra y NO dispara un Panic.
    f.h.key_press(egui::Key::Escape);
    for _ in 0..4 {
        f.h.step();
    }
    assert!(!app(&f.h).help.open);
    assert!(!app(&f.h).toasts.iter().any(|t| t.text.contains("Parada dura")));
}

#[test]
fn the_help_menu_opens_the_manual_and_about_with_a_link_to_the_legal_tab() {
    let mut f = fixture("help_menu");
    f.h.get_by_label("Ayuda").click();
    f.h.step();
    f.h.step();
    f.h.get_by_label("Acerca de TeatroCue").click();
    for _ in 0..4 {
        f.h.step();
    }
    assert!(app(&f.h).help.about_open);
    assert!(f.h.query_by_label("Reproductor de audio y vídeo para escena.").is_some());
    f.h.get_by_label("Autorización legal").click();
    for _ in 0..4 {
        f.h.step();
    }
    assert!(app(&f.h).help.open && app(&f.h).help.tab == crate::views::help_modal::HelpTab::Legal && !app(&f.h).help.about_open);
    f.h.key_press(egui::Key::F1);
    for _ in 0..3 {
        f.h.step();
    }
    assert!(!app(&f.h).help.open, "F1 alterna");
    f.h.get_by_label("Ayuda").click();
    f.h.step();
    f.h.step();
    f.h.get_by_label("Manual de Uso (F1)").click();
    for _ in 0..3 {
        f.h.step();
    }
    assert!(app(&f.h).help.open);
}

// -- Reactivación de emergencia -------------------------------------------------------------------

/// PC de pruebas con una licencia de 30 días ya vencida (hoy = +40 días) y marcas de reloj en archivos.
struct Expired {
    sk: ed25519_dalek::SigningKey,
    setup: qlab_license::activation::Setup,
    dir: std::path::PathBuf,
    now: u64,
}

impl Expired {
    fn new(tag: &str) -> Self {
        use qlab_license::clock::{self, FileStore, MarkStore};
        let dir = std::env::temp_dir().join(format!("teatrocue-ui-react-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sk = ed25519_dalek::SigningKey::from_bytes(&[3u8; 32]);
        let machine = qlab_license::MachineId::from_guid("pc-del-teatro");
        let t0 = clock::BUILD_UNIX + 100 * 86_400;
        let path = dir.join("license.key");
        std::fs::write(&path, qlab_license::issue("Ballet Oficial de Córdoba", &machine, t0, t0 + 30 * 86_400, &sk)).unwrap();
        let stores: Vec<Box<dyn MarkStore>> = (0..3).map(|i| Box::new(FileStore(dir.join(format!("mark{i}.dat")))) as Box<dyn MarkStore>).collect();
        let lic = qlab_license::verify_with(&std::fs::read_to_string(&path).unwrap(), &sk.verifying_key(), &machine).unwrap();
        clock::reset(t0, &lic, &stores);
        let now = t0 + 40 * 86_400;
        let setup = qlab_license::activation::Setup { license_path: path, stores: std::sync::Arc::new(stores), anchors: vec![], public: Some(sk.verifying_key()), machine: Ok(machine), now: Some(now) };
        Self { sk, setup, dir, now }
    }

    fn cfg(&self, tag: &str) -> AppConfig {
        let mut cfg = AppConfig::new(format!("/tmp/qlab-ui-click-{}-{tag}.sock", std::process::id()));
        cfg.license = crate::license_gate::LicenseInfo::from_status(&self.setup.status());
        cfg.license_setup = Some(self.setup.clone());
        cfg
    }

    fn key(&self, days: u32) -> String {
        qlab_license::activation::sign_challenge(&self.setup.challenge().unwrap(), days, self.now, &self.sk).1
    }
}

impl Drop for Expired {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn an_expired_license_offers_emergency_reactivation_and_a_valid_key_unlocks_the_app() {
    let pc = Expired::new("lock");
    let mut cfg = pc.cfg("react-lock");
    cfg.embedded_on_start = Some(false);
    let mut h = harness(cfg);
    for _ in 0..4 {
        h.step();
    }
    assert!(app(&h).license.is_locked() && app(&h).license.reactivable);
    assert!(h.query_by_label("Reactivación de Emergencia").is_some());
    let code = pc.setup.challenge().unwrap().to_string();
    assert!(h.query_by_label(&code).is_some(), "se muestra el código de solicitud REQ-…");
    assert!(code.starts_with("REQ-"));
    assert!(h.query_by_label("Copiar código").is_some() && h.query_by_label("Activar").is_some());

    // Una clave inválida: error descriptivo, la pantalla de bloqueo sigue y la aplicación no se cierra.
    h.get_all_by_role(egui::accesskit::Role::MultilineTextInput).next().unwrap().click();
    h.step();
    h.get_all_by_role(egui::accesskit::Role::MultilineTextInput).next().unwrap().type_text("RESP-ABCD-1234");
    h.step();
    assert_eq!(app(&h).react.input, "RESP-ABCD-1234", "el texto pegado llegó al campo");
    h.get_by_label("Activar").click();
    h.step();
    h.step();
    assert!(matches!(&app(&h).react.outcome, Some(crate::views::reactivation::Outcome::Err(m)) if m.contains("No se pudo activar")), "{:?}", app(&h).react.outcome);
    assert!(app(&h).license.is_locked(), "una clave mala no desbloquea");
    assert!(h.query_by_label("Licencia no válida en esta PC").is_some());
    assert!(!app(&h).embedded_running());

    // Una clave auténtica: se extiende, se cierra el bloqueo y se habilitan los controles.
    app_mut(&mut h).react.input = pc.key(15);
    h.step();
    h.get_by_label("Activar").click();
    pump(&mut h, "aplicación desbloqueada con el motor embebido", |a| !a.license.is_locked() && a.embedded_running() && a.is_connected());
    let until = crate::license_gate::fmt_dmy(pc.now + 15 * 86_400);
    pump(&mut h, "aviso de licencia extendida", |a| a.toasts.iter().any(|t| t.text == format!("Licencia extendida hasta: {until}")));
    // La barra superior vuelve (el botón principal dice GO, o FIN si el show está vacío) y también la de cues.
    assert!(h.query_by_label("GO").is_some() || h.query_by_label("FIN").is_some(), "el botón GO debe estar disponible tras reactivar");
    assert!(h.query_by_label("+ Audio Cue").is_some());
    assert!(h.query_by_label("Licencia no válida en esta PC").is_none());
    assert!(matches!(&app(&h).license.state, crate::license_gate::LicenseState::Valid { expires_unix, .. } if *expires_unix == pc.now + 15 * 86_400));
}

fn app_mut(h: &mut H) -> &mut QlabApp {
    h.state_mut().as_mut().expect("la app se crea en el primer fotograma")
}

#[test]
fn the_help_license_tab_has_the_reactivation_block_and_a_reused_key_is_rejected() {
    let pc = Expired::new("help");
    // Licencia aún vigente en pantalla (a mitad de función) pero que vence pronto: el bloque está en Ayuda > Licencia.
    let early = qlab_license::activation::Setup { now: Some(pc.now - 20 * 86_400), ..pc.setup.clone() };
    let mut cfg = AppConfig::new(format!("/tmp/qlab-ui-click-{}-react-help.sock", std::process::id()));
    cfg.license = crate::license_gate::LicenseInfo::from_status(&early.status());
    cfg.license_setup = Some(early);
    assert!(!cfg.license.is_locked(), "a +20 días todavía vale: {:?}", cfg.license.state);
    let mut h = harness(cfg);
    h.step();
    app_mut(&mut h).help.open = true;
    app_mut(&mut h).help.tab = crate::views::help_modal::HelpTab::Legal;
    h.step();
    h.step();
    assert!(h.query_by_label("Reactivación de Emergencia").is_some());
    assert!(h.query_by_label("Copiar código").is_some());
    // Reactivar sin daemon conectado se canjea en local; la misma clave dos veces falla (anti-replay).
    let key = pc.key(15);
    app_mut(&mut h).react.input = key.clone();
    h.step();
    h.get_by_label("Activar").click();
    h.step();
    h.step();
    assert!(matches!(&app(&h).react.outcome, Some(crate::views::reactivation::Outcome::Ok(m)) if m.starts_with("Licencia extendida hasta: ")), "{:?}", app(&h).react.outcome);
    app_mut(&mut h).react.input = key;
    h.step();
    h.get_by_label("Activar").click();
    h.step();
    h.step();
    assert!(matches!(&app(&h).react.outcome, Some(crate::views::reactivation::Outcome::Err(m)) if m.contains("ya fue usada")), "{:?}", app(&h).react.outcome);
}

// -- Configuración de audio -----------------------------------------------------------------------

#[test]
fn the_audio_settings_window_saves_the_choice_for_the_next_engine_start() {
    let dir = std::env::temp_dir().join(format!("teatrocue-ui-audio-{}", std::process::id()));
    let path = dir.join("audio.json");
    let _ = std::fs::remove_dir_all(&dir);
    let mut cfg = AppConfig::new(format!("/tmp/qlab-ui-click-{}-audio.sock", std::process::id()));
    cfg.settings_path = Some(path.clone());
    cfg.embedded_on_start = Some(false); // motor sin audio: el cambio no debe reiniciarlo con la tarjeta real
    let mut h = harness(cfg);
    pump(&mut h, "motor conectado", |a| a.is_connected());
    app_mut(&mut h).audio_ui.open_with(&crate::settings::AudioSettings::default());
    app_mut(&mut h).audio_ui.set_devices_for_test(vec![("wasapi".into(), "Focusrite USB".into())]);
    h.step();
    h.step();
    assert!(h.query_by_label("Configuración de audio").is_some());
    assert!(h.query_by_label("Aplicar y reiniciar motor").is_some());
    app_mut(&mut h).audio_ui.draft.channels = 8;
    app_mut(&mut h).audio_ui.draft.device = Some("Focusrite USB".into());
    app_mut(&mut h).audio_ui.draft.host = Some("wasapi".into());
    h.step();
    h.get_by_label("Aplicar y reiniciar motor").click();
    h.step();
    h.step();
    let saved = crate::settings::load(&path);
    assert_eq!((saved.channels, saved.device.as_deref(), saved.host.as_deref()), (8, Some("Focusrite USB"), Some("wasapi")));
    assert!(!app(&h).audio_ui.open, "la ventana se cierra al aplicar");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_control_cue_stops_another_cue_when_fired() {
    let mut f = fixture("control");
    f.h.get_by_label("+ Control Cue").click();
    pump(&mut f.h, "cue de control en el Inspector", |a| a.inspector.draft.as_ref().is_some_and(|d| d.id() == 4 && matches!(d.node.body, CueBody::Control(_))));
    for _ in 0..4 {
        f.h.step();
    }
    assert_eq!(app(&f.h).inspector.tab, Tab::Control);
    for label in ["Cue destino", "Stop", "Pause", "Resume", "Devamp", "Fundido de salida"] {
        assert!(f.h.query_all_by_label(label).next().is_some(), "falta «{label}» en la pestaña Control");
    }
    // Apunta a la cue 1 y se guarda en el daemon.
    app_mut(&mut f.h).inspector.draft.as_mut().unwrap().node.basics.cue_target_id = Some(CueId(1));
    pump(&mut f.h, "destino guardado", |a| a.model.summary(4).is_some_and(|c| c.target.starts_with("Stop")) && a.inspector.draft.as_ref().is_some_and(|d| !d.dirty()));
    // Orden de la lista: 1, 4 (control), 2, 3. El primer GO dispara la 1; el segundo, el control.
    f.h.get_by_label("GO").click();
    pump(&mut f.h, "la cue 1 suena", |a| a.model.state_of(1) == CueUiState::Running);
    pump(&mut f.h, "el playhead llega al control", |a| a.model.playhead == Some(4));
    f.h.get_by_label("GO").click();
    pump(&mut f.h, "el control detiene la cue 1", |a| a.model.state_of(1) != CueUiState::Running);
}

#[test]
fn a_memo_cue_waits_its_duration_and_auto_continue_chains_the_next_cue() {
    use qlab_protocol::cue_dto::ContinueMode;
    let mut f = fixture("memo");
    f.h.get_by_label("+ Memo/Espera").click();
    pump(&mut f.h, "memo en el Inspector", |a| a.inspector.draft.as_ref().is_some_and(|d| d.id() == 4 && matches!(d.node.body, CueBody::Memo)));
    for _ in 0..3 {
        f.h.step();
    }
    assert!(f.h.query_by_label("Duración").is_some(), "el memo muestra su duración");
    {
        let d = app_mut(&mut f.h).inspector.draft.as_mut().unwrap();
        d.node.basics.action_duration = 0.2;
        d.node.basics.continue_mode = ContinueMode::AutoContinue;
    }
    pump(&mut f.h, "memo guardado", |a| a.inspector.draft.as_ref().is_some_and(|d| !d.dirty()) && a.model.summary(4).is_some_and(|c| c.kind == "memo"));
    // Lista: 1, 4 (memo), 2, 3. Disparar el memo: dura 0,2 s y encadena la cue siguiente.
    f.h.get_by_label("Pista 1").click();
    pump(&mut f.h, "playhead en 1", |a| a.telemetry.playhead() == Some(1));
    f.h.get_by_label("GO").click();
    pump(&mut f.h, "la cue 1 suena", |a| a.model.state_of(1) == CueUiState::Running);
    f.h.get_by_label("GO").click();
    pump(&mut f.h, "el memo corre y luego encadena la cue 2", |a| a.model.state_of(2) == CueUiState::Running);
}

#[test]
fn ctrl_z_and_ctrl_shift_z_undo_and_redo_a_cue_edit() {
    let mut f = fixture("undo");
    let name_of = |a: &QlabApp| a.model.summary(1).map(|c| c.name.clone()).unwrap_or_default();
    assert_eq!(name_of(app(&f.h)), "Pista 1");
    // Edita el nombre de la cue 1 (borrador) y espera a que el daemon lo confirme.
    app_mut(&mut f.h).inspector.draft.as_mut().unwrap().node.basics.name = "Entrada coro".into();
    pump(&mut f.h, "nombre guardado", |a| a.model.summary(1).is_some_and(|c| c.name == "Entrada coro") && a.inspector.draft.as_ref().is_some_and(|d| !d.dirty()));
    // Ctrl+Z vuelve al nombre anterior en el daemon y en el Inspector.
    f.h.key_press_modifiers(Modifiers::COMMAND, egui::Key::Z);
    pump(&mut f.h, "deshecho", |a| a.model.summary(1).is_some_and(|c| c.name == "Pista 1") && a.inspector.draft.as_ref().is_some_and(|d| d.node.basics.name == "Pista 1" && !d.dirty()));
    assert!(app(&f.h).toasts.iter().any(|t| t.text == "Deshecho"));
    // Ctrl+Mayús+Z lo rehace.
    f.h.key_press_modifiers(Modifiers::COMMAND | Modifiers::SHIFT, egui::Key::Z);
    pump(&mut f.h, "rehecho", |a| a.model.summary(1).is_some_and(|c| c.name == "Entrada coro") && a.inspector.draft.as_ref().is_some_and(|d| d.node.basics.name == "Entrada coro" && !d.dirty()));
    // Sin más historial hacia atrás, Ctrl+Z dos veces avisa en lugar de fallar.
    f.h.key_press_modifiers(Modifiers::COMMAND, egui::Key::Z);
    pump(&mut f.h, "otra vez deshecho", |a| a.model.summary(1).is_some_and(|c| c.name == "Pista 1"));
    f.h.key_press_modifiers(Modifiers::COMMAND, egui::Key::Z);
    pump(&mut f.h, "aviso de nada que deshacer", |a| a.toasts.iter().any(|t| t.text == "No hay nada que deshacer"));
}

#[test]
fn a_goto_control_cue_sends_the_playhead_back_so_a_section_can_repeat() {
    use qlab_protocol::show_format::ControlActionDto;
    let mut f = fixture("goto");
    f.h.get_by_label("+ Control Cue").click();
    pump(&mut f.h, "control en el Inspector", |a| a.inspector.draft.as_ref().is_some_and(|d| d.id() == 4 && matches!(d.node.body, CueBody::Control(_))));
    f.h.step();
    {
        let d = app_mut(&mut f.h).inspector.draft.as_mut().unwrap();
        d.node.basics.cue_target_id = Some(CueId(1));
        if let CueBody::Control(c) = &mut d.node.body {
            c.action = ControlActionDto::GoTo;
        }
    }
    pump(&mut f.h, "guardado como Go to", |a| a.model.summary(4).is_some_and(|c| c.target.starts_with("Go to")) && a.inspector.draft.as_ref().is_some_and(|d| !d.dirty()));
    // Lista: 1, 4 (go to 1), 2, 3. GO dispara la 1, GO dispara el Go to y el playhead vuelve a la 1.
    f.h.get_by_label("GO").click();
    pump(&mut f.h, "suena la 1 y el playhead pasa al control", |a| a.model.state_of(1) == CueUiState::Running && a.model.playhead == Some(4));
    f.h.get_by_label("GO").click();
    pump(&mut f.h, "el playhead vuelve a la cue 1", |a| a.model.playhead == Some(1));
}

#[test]
fn a_load_control_cue_can_be_configured_and_fired_without_errors() {
    use qlab_protocol::show_format::ControlActionDto;
    let mut f = fixture("loadctl");
    f.h.get_by_label("+ Control Cue").click();
    pump(&mut f.h, "control en el Inspector", |a| a.inspector.draft.as_ref().is_some_and(|d| d.id() == 4 && matches!(d.node.body, CueBody::Control(_))));
    f.h.step();
    {
        let d = app_mut(&mut f.h).inspector.draft.as_mut().unwrap();
        d.node.basics.cue_target_id = Some(CueId(3));
        if let CueBody::Control(c) = &mut d.node.body {
            c.action = ControlActionDto::Load;
        }
    }
    pump(&mut f.h, "guardado como Load", |a| a.model.summary(4).is_some_and(|c| c.target.starts_with("Load")) && a.inspector.draft.as_ref().is_some_and(|d| !d.dirty()));
    f.h.get_by_label("GO").click(); // dispara la 1
    pump(&mut f.h, "playhead en el control", |a| a.model.playhead == Some(4));
    f.h.get_by_label("GO").click(); // dispara el Load
    pump(&mut f.h, "el control corre y avanza el playhead", |a| a.model.playhead == Some(2));
    assert!(!app(&f.h).toasts.iter().any(|t| t.text.to_lowercase().contains("error")), "{:?}", app(&f.h).toasts.iter().map(|t| &t.text).collect::<Vec<_>>());
}

#[test]
fn ctrl_d_duplicates_the_selected_cue_with_its_file_and_a_new_id() {
    let mut f = fixture("dup");
    let original = app(&f.h).inspector.draft.as_ref().unwrap().baseline.clone();
    assert_eq!(original.basics.unique_id.0, 1);
    f.h.key_press_modifiers(Modifiers::COMMAND, egui::Key::D);
    pump(&mut f.h, "copia creada y seleccionada", |a| a.model.summaries.len() == 4 && a.model.selected == Some(4) && a.inspector.draft.as_ref().is_some_and(|d| d.id() == 4));
    let copy = app(&f.h).inspector.draft.as_ref().unwrap().baseline.clone();
    assert_eq!(copy.basics.name, "Pista 1 (copia)");
    assert_eq!(copy.basics.file_target, original.basics.file_target, "mismo archivo");
    assert_ne!(copy.basics.number, original.basics.number, "número propio");
    assert!(matches!(copy.body, CueBody::Audio(_)));
    // Queda justo después de la original.
    let order: Vec<u64> = app(&f.h).model.summaries.iter().map(|c| c.id).collect();
    assert_eq!(order, vec![1, 4, 2, 3], "{order:?}");
    assert!(app(&f.h).toasts.iter().any(|t| t.text == "Cue duplicada"));
}

#[test]
fn an_osc_cue_is_created_edited_and_sends_its_message() {
    use qlab_protocol::show_format::OscBody;
    let receiver = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    receiver.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let port = receiver.local_addr().unwrap().port();
    let mut f = fixture("oscui");
    f.h.get_by_label("+ OSC Cue").click();
    pump(&mut f.h, "cue OSC en el Inspector", |a| a.inspector.draft.as_ref().is_some_and(|d| d.id() == 4 && matches!(d.node.body, CueBody::Osc(_))));
    for _ in 0..4 {
        f.h.step();
    }
    assert_eq!(app(&f.h).inspector.tab, Tab::Osc);
    for label in ["Destino", "Mensaje", "Duración"] {
        assert!(f.h.query_all_by_label(label).next().is_some(), "falta «{label}» en la pestaña OSC");
    }
    app_mut(&mut f.h).inspector.draft.as_mut().unwrap().node.body = CueBody::Osc(OscBody { destination: format!("127.0.0.1:{port}"), message: "/prueba 42".into() });
    pump(&mut f.h, "guardada", |a| a.model.summary(4).is_some_and(|c| c.kind == "osc" && c.target.starts_with("127.0.0.1")) && a.inspector.draft.as_ref().is_some_and(|d| !d.dirty()));
    // Lista: 1, 4 (osc), 2, 3: GO dispara la 1 y GO otra vez dispara el OSC.
    f.h.get_by_label("GO").click();
    pump(&mut f.h, "playhead en el OSC", |a| a.model.playhead == Some(4));
    f.h.get_by_label("GO").click();
    pump(&mut f.h, "el OSC se disparó y el playhead avanzó", |a| a.model.playhead == Some(2));
    let mut buf = [0u8; 256];
    let n = receiver.recv(&mut buf).expect("llega el mensaje");
    assert!(buf[..n].starts_with(b"/prueba\0"), "{:?}", &buf[..n]);
}

#[test]
fn the_mark_memory_button_asks_for_a_name_and_stores_it_with_the_note() {
    let mut f = fixture("memory");
    app_mut(&mut f.h).inspector.tab = Tab::TimeLoops;
    for _ in 0..4 {
        f.h.step();
    }
    f.h.get_by_label("Marcar memoria (M)").click();
    pump(&mut f.h, "aparece el cuadro de nombre", |a| a.quick_memory.is_some());
    f.h.step();
    f.h.step();
    assert!(f.h.query_by_label("Nombre de la memoria:").is_some());
    // Enter sin escribir nada acepta el nombre por defecto («Memoria 1»).
    f.h.key_press(egui::Key::Enter);
    pump(&mut f.h, "memoria guardada en el daemon", |a| a.inspector.draft.as_ref().is_some_and(|d| d.baseline.body.audio().is_some_and(|b| b.time_loops.memories.len() == 1) && !d.dirty()));
    assert_eq!(app(&f.h).inspector.draft.as_ref().unwrap().baseline.body.audio().unwrap().time_loops.memories[0].name, "Memoria 1");
    // Se le pone una nota al estilo de las ubicaciones de memoria de Pro Tools.
    {
        let d = app_mut(&mut f.h).inspector.draft.as_mut().unwrap();
        let m = &mut d.node.body.audio_mut().unwrap().time_loops.memories[0];
        m.name = "giselle baile medio".into();
        m.note = "entra el coro por la izquierda".into();
    }
    pump(&mut f.h, "nombre y nota guardados", |a| {
        a.inspector.draft.as_ref().is_some_and(|d| !d.dirty() && d.baseline.body.audio().is_some_and(|b| b.time_loops.memories.first().is_some_and(|m| m.name == "giselle baile medio" && m.note.starts_with("entra el coro"))))
    });
    let json = serde_json::to_string(&app(&f.h).inspector.draft.as_ref().unwrap().baseline).unwrap();
    assert!(json.contains("giselle baile medio") && json.contains("memories"), "{json}");
}

#[test]
fn pressing_m_while_the_cue_plays_captures_that_point_names_it_and_shows_it_in_the_panel_and_on_the_wave() {
    let mut f = fixture("quickm");
    f.h.get_by_label("GO").click();
    pump(&mut f.h, "suena con posición de lectura", |a| a.telemetry.running.get(&1).is_some_and(|i| i.source_frame > 0));
    app_mut(&mut f.h).inspector.tab = Tab::TimeLoops;
    f.h.key_press(egui::Key::M);
    pump(&mut f.h, "cuadro de nombre sobre la cue que suena", |a| a.quick_memory.as_ref().is_some_and(|q| q.cue_id == 1 && q.frame.is_some()));
    let captured = app(&f.h).quick_memory.as_ref().unwrap().frame.unwrap();
    app_mut(&mut f.h).quick_memory.as_mut().unwrap().name = "1er levantada".into();
    f.h.key_press(egui::Key::Enter);
    pump(&mut f.h, "memoria guardada con el nombre", |a| a.inspector.draft.as_ref().is_some_and(|d| d.baseline.body.audio().is_some_and(|b| b.time_loops.memories.len() == 1) && !d.dirty()));
    let m = app(&f.h).inspector.draft.as_ref().unwrap().baseline.body.audio().unwrap().time_loops.memories[0].clone();
    assert_eq!(m.name, "1er levantada");
    assert!((m.secs - captured as f64 / 48_000.0).abs() < 1e-6, "la memoria queda en el frame capturado: {} vs {}", m.secs, captured as f64 / 48_000.0);
    // Aparece en el panel (que se abre solo) y como bandera sobre la regla de la onda.
    pump(&mut f.h, "panel abierto con la memoria", |a| a.memories_open);
    f.h.step();
    f.h.step();
    assert!(f.h.query_all_by_label_contains("1er levantada").next().is_some(), "en el panel");
    assert!(f.h.query_by_label("Memoria 1: 1er levantada").is_some(), "bandera sobre la onda");
}

#[test]
fn the_memories_panel_filters_docks_to_either_side_and_a_wave_flag_click_arms_that_point() {
    let mut f = fixture("mempanel");
    {
        let d = app_mut(&mut f.h).inspector.draft.as_mut().unwrap();
        d.node.body.audio_mut().unwrap().time_loops.memories = vec![
            qlab_protocol::cue_dto::MemoryMark { secs: 0.5, name: "diagonal".into(), note: String::new() },
            qlab_protocol::cue_dto::MemoryMark { secs: 1.0, name: "reloj circulo".into(), note: String::new() },
        ];
    }
    pump(&mut f.h, "memorias guardadas", |a| a.model.summary(1).is_some_and(|c| c.memories.len() == 2) && a.inspector.draft.as_ref().is_some_and(|d| !d.dirty()));
    app_mut(&mut f.h).memories_open = true;
    app_mut(&mut f.h).inspector.tab = Tab::TimeLoops;
    f.h.step();
    f.h.step();
    // Lista numerada con tiempo MM:SS.mmm y nombre.
    assert!(f.h.query_all_by_label_contains("#1  diagonal").next().is_some() && f.h.query_all_by_label("00:00.500").next().is_some());
    assert!(f.h.query_all_by_label_contains("#2  reloj circulo").next().is_some() && f.h.query_all_by_label("00:01.000").next().is_some());
    // El filtro deja solo lo que coincide, sin renumerar.
    app_mut(&mut f.h).memories_panel.filter = "reloj".into();
    f.h.step();
    f.h.step();
    assert!(f.h.query_all_by_label_contains("#1  diagonal").next().is_none(), "la fila no coincide: no se lista");
    assert!(f.h.query_all_by_label_contains("#2  reloj circulo").next().is_some(), "conserva el número 2");
    assert!(f.h.query_by_label("Memoria 1: diagonal").is_some(), "la onda sigue mostrando todas las banderas");
    app_mut(&mut f.h).memories_panel.filter.clear();
    // Se acopla al otro lado.
    assert!(!app(&f.h).memories_panel.right);
    f.h.step();
    f.h.get_by_label("A la derecha").click();
    f.h.step();
    assert!(app(&f.h).memories_panel.right, "ahora a la derecha");
    // Un clic en la bandera de la onda deja la cue lista para arrancar desde ese punto.
    f.h.get_by_label("Memoria 2: reloj circulo").click_accesskit();
    pump(&mut f.h, "arranque guardado", |a| a.model.playhead == Some(1) && a.inspector.draft.as_ref().is_some_and(|d| !d.dirty() && d.baseline.body.audio().is_some_and(|b| b.time_loops.start_override_secs == Some(1.0))));
}

#[test]
fn the_global_memories_window_lists_every_cue_memory_and_jumps_to_it() {
    let mut f = fixture("memwin");
    // Dos memorias con nombre en la cue 1.
    {
        let d = app_mut(&mut f.h).inspector.draft.as_mut().unwrap();
        let t = &mut d.node.body.audio_mut().unwrap().time_loops;
        t.memories = vec![
            qlab_protocol::cue_dto::MemoryMark { secs: 0.5, name: "cruces".into(), note: "diagonal desde el fondo".into() },
            qlab_protocol::cue_dto::MemoryMark { secs: 1.0, name: "reloj circulo".into(), note: String::new() },
        ];
    }
    pump(&mut f.h, "memorias en el resumen de la cue", |a| a.model.summary(1).is_some_and(|c| c.memories.len() == 2));
    // Se selecciona otra cue y se abre la ventana global.
    f.h.get_by_label("Pista 3").click();
    pump(&mut f.h, "cue 3 seleccionada", |a| a.model.selected == Some(3) && a.inspector.draft.as_ref().is_some_and(|d| d.id() == 3));
    app_mut(&mut f.h).memories_open = true;
    f.h.step();
    f.h.step();
    assert!(f.h.query_all_by_label_contains("cruces").next().is_some(), "la ventana lista la memoria «cruces»");
    assert!(f.h.query_all_by_label_contains("reloj circulo").next().is_some());
    assert!(f.h.query_by_label("diagonal desde el fondo").is_some(), "y su nota");
    // Un clic en una memoria lleva a su cue y abre Time & Loops.
    f.h.get_all_by_label_contains("reloj circulo").next().unwrap().click();
    pump(&mut f.h, "salta a la cue 1 en Time & Loops", |a| a.model.selected == Some(1) && a.inspector.draft.as_ref().is_some_and(|d| d.id() == 1) && a.inspector.tab == Tab::TimeLoops);
    // Elegir la memoria deja la cue lista: el playhead está en ella y arrancará desde 1,0 s al pulsar Espacio.
    pump(&mut f.h, "playhead en la cue 1 y arranque guardado", |a| {
        a.model.playhead == Some(1) && a.inspector.draft.as_ref().is_some_and(|d| !d.dirty() && d.baseline.body.audio().is_some_and(|b| b.time_loops.start_override_secs == Some(1.0)))
    });
    f.h.key_press(egui::Key::Space);
    pump(&mut f.h, "la cue 1 suena", |a| a.model.state_of(1) == CueUiState::Running);
    // La cue suena desde el punto elegido (la pista dura 2 s: el cursor no puede estar antes de 1,0 s).
    pump(&mut f.h, "el cursor real arranca en la memoria", |a| a.telemetry.running.get(&1).is_some_and(|i| i.source_frame >= 48_000));
}

#[test]
fn picking_a_memory_while_the_cue_plays_jumps_there_and_keeps_playing() {
    let mut f = fixture("memjump");
    {
        let d = app_mut(&mut f.h).inspector.draft.as_mut().unwrap();
        d.node.body.audio_mut().unwrap().time_loops.memories = vec![qlab_protocol::cue_dto::MemoryMark { secs: 1.0, name: "giselle al medio".into(), note: String::new() }];
    }
    pump(&mut f.h, "memoria guardada", |a| a.model.summary(1).is_some_and(|c| c.memories.len() == 1) && a.inspector.draft.as_ref().is_some_and(|d| !d.dirty()));
    // La cue 1 (2 s de audio) suena desde el principio.
    f.h.get_by_label("GO").click();
    let t0 = Instant::now();
    pump(&mut f.h, "suena", |a| a.model.state_of(1) == CueUiState::Running);
    app_mut(&mut f.h).memories_open = true;
    f.h.step();
    f.h.step();
    f.h.get_all_by_label_contains("giselle al medio").next().unwrap().click();
    // Salta a 1,0 s y sigue sonando desde ahí: la lectura pasa de 1,0 s mucho antes de lo que tardaría sonando desde el principio.
    pump(&mut f.h, "relanzada desde la memoria", |a| a.telemetry.running.get(&1).is_some_and(|i| i.source_frame >= 48_000) && a.model.state_of(1) == CueUiState::Running);
    assert!(t0.elapsed() < Duration::from_millis(900), "{:?}", t0.elapsed());
    assert!(app(&f.h).inspector.draft.as_ref().is_some_and(|d| d.baseline.body.audio().is_some_and(|b| b.time_loops.start_override_secs == Some(1.0))));
}

#[test]
fn the_master_fader_shows_in_the_top_bar_with_its_value() {
    let mut f = fixture("master");
    assert!(f.h.query_by_label_contains("MASTER").is_some(), "el control está en la barra superior");
    assert_eq!(app(&f.h).master_db, 0.0, "empieza en 0 dB");
    app_mut(&mut f.h).master_db = -3.0;
    f.h.step();
    assert!(f.h.query_by_label_contains("-3.0 dB").is_some(), "la etiqueta muestra el valor");
}

#[test]
fn enter_marks_a_memory_only_when_there_is_something_playing_and_is_silent_otherwise() {
    let mut f = fixture("mementer");
    // Sin nada sonando ni cursor de edición: Enter no hace nada y no molesta con avisos.
    f.h.key_press(egui::Key::Enter);
    for _ in 0..3 {
        f.h.step();
    }
    assert!(app(&f.h).quick_memory.is_none());
    assert!(!app(&f.h).toasts.iter().any(|t| t.text.contains("Para marcar")), "Enter nunca avisa");
    // Con la cue sonando, Enter abre el cuadro de nombre sobre ella.
    f.h.get_by_label("GO").click();
    pump(&mut f.h, "suena con posición de lectura", |a| a.telemetry.running.get(&1).is_some_and(|i| i.source_frame > 0));
    f.h.key_press(egui::Key::Enter);
    pump(&mut f.h, "cuadro de nombre", |a| a.quick_memory.as_ref().is_some_and(|q| q.cue_id == 1 && q.frame.is_some()));
}

#[test]
fn on_a_1366x768_cabin_screen_the_top_bars_fit_without_clipping_overlapping_or_squashing_text() {
    let mut f = fixture("layout1366");
    f.h.set_size(egui::vec2(1366.0, 768.0));
    for _ in 0..6 {
        f.h.step();
    }
    let screen = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1366.0, 768.0));
    let rect_of = |h: &H, label: &str| h.get_all_by_label_contains(label).next().unwrap_or_else(|| panic!("falta «{label}»")).rect();
    let must_show = ["GO", "STANDBY", "Pausar todo", "Reanudar todo", "Parar todo", "PANIC", "MASTER", "+ Audio Cue", "+ Group Cue", "Proyección", "Subir la cue seleccionada", "Eliminar la cue seleccionada (Supr)"];
    for l in must_show {
        let r = rect_of(&f.h, l);
        assert!(screen.contains_rect(r), "«{l}» se sale de la pantalla de 1366x768: {r:?}");
        assert!(r.width() > 10.0 && r.height() > 8.0, "«{l}» quedó aplastado: {r:?}");
    }
    // Primera fila: transporte; segunda: MASTER. Sin solapamientos entre bloques.
    let (pause, panic_btn, master) = (rect_of(&f.h, "Pausar todo"), rect_of(&f.h, "PANIC"), rect_of(&f.h, "MASTER"));
    assert!(pause.center().y < master.center().y && panic_btn.center().y < master.center().y, "dos filas: transporte arriba, MASTER abajo");
    assert!(!pause.intersects(panic_btn), "los botones de transporte no se pisan");
    // «MASTER» se lee de corrido (una línea de texto), no en una columna de letras.
    assert!(master.height() < 28.0 && master.width() > 40.0, "MASTER aplastado en vertical: {master:?}");
    // La lectura numérica tiene ancho fijo y legible.
    let reading = rect_of(&f.h, "0.0 dB");
    assert!(reading.width() >= 60.0 && reading.height() < 28.0, "lectura de dB mal dimensionada: {reading:?}");
    // El deslizador queda entre MASTER y la lectura, en horizontal.
    assert!(master.right() < reading.left(), "orden: MASTER, deslizador, lectura");
    // La toolbar no desborda: todos sus botones tienen su sitio dentro del ancho (si no caben, bajan de línea).
    let (audio, group, trash) = (rect_of(&f.h, "+ Audio Cue"), rect_of(&f.h, "+ Group Cue"), rect_of(&f.h, "Eliminar la cue seleccionada (Supr)"));
    assert!(audio.right() <= 1366.0 && group.right() <= 1366.0 && trash.right() <= 1366.0);
}

#[test]
fn the_eq_tab_shows_the_curve_points_and_table_and_edits_reach_the_daemon() {
    use qlab_protocol::cue_dto::{AudioEqData, EqFilterType};
    let mut f = fixture("eqtab");
    assert!(f.h.query_by_label("EQ").is_some(), "la pestaña EQ aparece en las cues de audio");
    f.h.get_by_label("EQ").click();
    pump(&mut f.h, "pestaña EQ abierta", |a| a.inspector.tab == Tab::Eq);
    f.h.step();
    f.h.step();
    for l in ["Ecualizador activo", "Restablecer bandas", "EQ banda 1", "EQ banda 2", "EQ banda 3", "EQ banda 4", "Frecuencia", "Ganancia", "Banda", "Activa", "Tipo"] {
        assert!(f.h.query_all_by_label_contains(l).next().is_some(), "falta «{l}» en la pestaña EQ");
    }
    // Activar el bypass general y poner un paso alto en la banda 1: se guarda en el daemon.
    f.h.get_by_label("Ecualizador activo").click_accesskit();
    pump(&mut f.h, "EQ activado en el borrador", |a| a.inspector.draft.as_ref().is_some_and(|d| d.node.body.audio().is_some_and(|b| b.eq.enabled)));
    {
        let d = app_mut(&mut f.h).inspector.draft.as_mut().unwrap();
        let eq = &mut d.node.body.audio_mut().unwrap().eq;
        eq.bands[0].filter_type = EqFilterType::LowCut;
        eq.bands[0].frequency_hz = 120.0;
        eq.bands[2].gain_db = -4.5;
    }
    pump(&mut f.h, "EQ guardado en el daemon", |a| {
        a.inspector.draft.as_ref().is_some_and(|d| !d.dirty() && d.baseline.body.audio().is_some_and(|b| b.eq.enabled && b.eq.bands[0].filter_type == EqFilterType::LowCut && b.eq.bands[2].gain_db == -4.5))
    });
    // Restablecer deja las bandas planas pero conserva el estado de activación.
    f.h.get_by_label("Restablecer bandas").click_accesskit();
    pump(&mut f.h, "bandas restablecidas", |a| {
        a.inspector.draft.as_ref().is_some_and(|d| !d.dirty() && d.baseline.body.audio().is_some_and(|b| b.eq.enabled && b.eq.bands == AudioEqData::default().bands))
    });
    // La pestaña EQ no existe en las cues sin audio.
    f.h.get_by_label("+ Fade Cue").click();
    pump(&mut f.h, "fade en el Inspector", |a| a.inspector.draft.as_ref().is_some_and(|d| matches!(d.node.body, CueBody::Fade(_))));
    f.h.step();
    assert!(f.h.query_by_label("EQ").is_none(), "un fade no tiene EQ");
}

#[test]
fn autosave_writes_dirty_shows_recovers_after_a_crash_and_is_removed_on_clean_exit() {
    let mut f = fixture("autosave");
    let dir = std::env::temp_dir().join(format!("qlab-ui-autosave-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let show = dir.join("Obra.teatroshow");
    f.files.push(show.clone());
    app_mut(&mut f.h).enable_autosave_for_test(1, &dir);
    // Se guarda a mano una vez para que el show tenga nombre y carpeta.
    f.h.state_mut().as_mut().unwrap().on_dialog_outcome(DialogOutcome { kind: DialogKind::SaveShowAs, path: Some(show.clone()), more: Vec::new() });
    pump(&mut f.h, "guardado", |a| !a.show_dirty && a.show_path.as_deref() == Some(show.as_path()));
    let auto = crate::autosave::path_for(Some(&show), &dir);
    assert!(!auto.exists(), "sin cambios no hay autoguardado");

    // Un cambio sin guardar se autoguarda solo, sin tocar el estado «modificado» ni el show manual.
    f.h.get_by_label("+ Fade Cue").click();
    pump(&mut f.h, "autoguardado escrito", |a| a.show_dirty && auto.exists());
    assert!(app(&f.h).show_dirty, "el autoguardado no limpia la marca de cambios");
    assert_eq!(qlab_protocol::show_format::ShowFile::from_json(&std::fs::read_to_string(&auto).unwrap()).unwrap().cue_count(), 4);
    assert_eq!(qlab_protocol::show_format::ShowFile::from_json(&std::fs::read_to_string(&show).unwrap()).unwrap().cue_count(), 3, "el guardado manual sigue intacto");

    // «Caída»: otra instancia arranca y ofrece recuperar; el autoguardado es más nuevo que el show.
    let t = std::time::SystemTime::now() - Duration::from_secs(60);
    std::fs::File::options().write(true).open(&show).unwrap().set_modified(t).unwrap();
    assert!(crate::autosave::find_recoverable(&[show.clone()], &dir).is_some());
    app_mut(&mut f.h).recent = vec![show.clone()];
    app_mut(&mut f.h).enable_autosave_for_test(3600, &dir);
    pump(&mut f.h, "diálogo de recuperación", |a| a.recovery.is_some());
    f.h.step();
    assert!(f.h.query_all_by_label("Recuperar cambios").next().is_some());
    f.h.get_by_label("Recuperar cambios").click();
    pump(&mut f.h, "sesión recuperada con 4 cues y cambios pendientes", |a| a.recovery.is_none() && cue_count(a) == 4 && a.show_dirty && a.show_path.as_deref() == Some(show.as_path()));

    // Cierre limpio: el autoguardado desaparece.
    assert!(auto.exists());
    f.h.state_mut().as_mut().unwrap().on_exit_for_test();
    assert!(!auto.exists(), "el cierre limpio borra el autoguardado");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn exporting_the_cue_sheet_writes_a_printable_html_with_every_cue() {
    let mut f = fixture("cuesheet");
    let out = std::env::temp_dir().join(format!("qlab-ui-cuesheet-{}.html", std::process::id()));
    let _ = std::fs::remove_file(&out);
    f.files.push(out.clone());
    f.h.state_mut().as_mut().unwrap().on_dialog_outcome(DialogOutcome { kind: DialogKind::ExportCueSheet, path: Some(out.clone()), more: Vec::new() });
    pump(&mut f.h, "aviso de exportación", |a| a.toasts.iter().any(|t| t.text.starts_with("Guion técnico exportado (3 cues)")));
    let html = std::fs::read_to_string(&out).unwrap();
    assert!(html.contains("Ballet Oficial de Córdoba") && html.contains("@media print"));
    for n in ["Pista 1", "Pista 2", "Pista 3"] {
        assert!(html.contains(n), "falta {n}");
    }
    assert_eq!(html.matches("<tr class=").count(), 3);
    // Las duraciones que conoce la interfaz (el wav de prueba dura 2 s) llegan a la planilla.
    assert!(html.contains("00:02"), "falta la duración");
}

#[test]
fn the_pfl_button_asks_for_a_headphone_output_when_none_is_configured_and_the_panel_rows_have_one() {
    let mut f = fixture("pfl");
    assert!(f.h.query_all_by_label("PFL").next().is_some(), "el Inspector de una cue de audio tiene el botón PFL");
    f.h.get_by_label("PFL").click();
    pump(&mut f.h, "aviso de que no hay auriculares", |a| a.toasts.iter().any(|t| t.text.contains("auriculares")) && a.pfl.is_none());
    // Con memorias, cada fila del panel trae su PFL.
    app_mut(&mut f.h).memories_open = true;
    if let Some(a) = app_mut(&mut f.h).inspector.draft.as_mut().and_then(|d| d.node.body.audio_mut()) {
        a.time_loops.memories = vec![qlab_protocol::cue_dto::MemoryMark { secs: 0.5, name: "uno".into(), note: String::new() }];
    }
    for _ in 0..4 {
        f.h.step();
    }
    assert!(f.h.query_all_by_label("PFL").count() >= 2, "botón del Inspector y de la memoria");
}

fn cart_pad(path: &std::path::Path, name: &str, key: &str, mode: qlab_protocol::cue_dto::CartMode) -> qlab_protocol::cue_dto::CartPad {
    qlab_protocol::cue_dto::CartPad { path: path.to_string_lossy().into(), name: name.into(), key: key.into(), mode, ..Default::default() }
}

#[test]
fn the_cart_board_fires_pads_by_click_and_key_without_moving_the_standby_and_is_saved_with_the_show() {
    use qlab_protocol::cue_dto::CartMode;
    let mut f = fixture("carts");
    let fx = write_test_wav("click-carts-fx", 1, 96_000, 48_000, |_, _| 0.2);
    f.files.push(fx.clone());
    let standby = app(&f.h).model.playhead;
    assert!(standby.is_some());

    // F3 abre la ventana; el menú también la ofrece.
    f.h.key_press(egui::Key::F3);
    pump(&mut f.h, "ventana de Carts", |a| a.carts.open);
    assert!(f.h.query_all_by_label("Pad 1").next().is_some() && f.h.query_all_by_label("Pad 16").next().is_some(), "cuadrícula de 16 pads");

    // El editor del pad (clic derecho) muestra todos sus campos.
    app_mut(&mut f.h).carts.editing = Some(0);
    for _ in 0..3 {
        f.h.step();
    }
    for label in ["Nombre", "Archivo", "Color", "Volumen", "Atajo", "Modo"] {
        assert!(f.h.query_all_by_label(label).next().is_some(), "el editor del pad no muestra «{label}»");
    }
    app_mut(&mut f.h).carts.editing = None;

    // Asignar tres pads (como si se editaran en el panel) y guardarlos en el daemon.
    {
        let a = app_mut(&mut f.h);
        a.carts.bank = qlab_protocol::cue_dto::CartBank::default().clamped();
        a.carts.bank.pads[0] = cart_pad(&fx, "Aplauso", "1", CartMode::Retrigger);
        a.carts.bank.pads[1] = cart_pad(&fx, "Golpe", "F5", CartMode::Toggle);
        a.carts.bank.pads[2] = cart_pad(&fx, "Sirena", "", CartMode::Hold);
    }
    for i in 0..3 {
        app_mut(&mut f.h).cart_edited_for_test(i);
    }
    pump(&mut f.h, "pads preparados", |a| a.show_dirty && a.carts.duration[0].is_some() && a.carts.duration[2].is_some());

    // Clic en el pad: suena y el Standby no se mueve.
    f.h.get_by_label("Pad 1").click();
    pump(&mut f.h, "el pad 1 suena", |a| a.carts.playing(0, std::time::Instant::now()));
    assert_eq!(app(&f.h).model.playhead, standby, "el pad no mueve el playhead");
    assert!(!app(&f.h).model.any_running(), "ninguna cue de la lista arrancó");

    // Atajo F5 (modo alternar): suena; otra vez, se detiene.
    f.h.key_press(egui::Key::F5);
    pump(&mut f.h, "el pad 2 suena por su atajo", |a| a.carts.playing(1, std::time::Instant::now()));
    f.h.key_press(egui::Key::F5);
    pump(&mut f.h, "el segundo F5 lo detiene", |a| !a.carts.playing(1, std::time::Instant::now()));

    // Modo mantener: suena solo mientras se pulsa.
    app_mut(&mut f.h).cart_press(2);
    assert!(app(&f.h).carts.playing(2, std::time::Instant::now()));
    app_mut(&mut f.h).cart_release(2);
    assert!(!app(&f.h).carts.playing(2, std::time::Instant::now()));
    assert_eq!(app(&f.h).model.playhead, standby);

    // Se guardan con el show y vuelven al abrirlo.
    let out = std::env::temp_dir().join(format!("qlab-ui-carts-{}.json", std::process::id()));
    f.files.push(out.clone());
    f.h.state_mut().as_mut().unwrap().on_dialog_outcome(DialogOutcome { kind: DialogKind::SaveShowAs, path: Some(out.clone()), more: Vec::new() });
    pump(&mut f.h, "guardado", |a| !a.show_dirty && a.show_path.as_deref() == Some(out.as_path()));
    let saved = qlab_protocol::show_format::ShowFile::from_json(&std::fs::read_to_string(&out).unwrap()).unwrap();
    assert_eq!((saved.carts.pads[0].name.as_str(), saved.carts.pads[1].mode), ("Aplauso", CartMode::Toggle));
    app_mut(&mut f.h).carts.bank = Default::default();
    f.h.state_mut().as_mut().unwrap().on_dialog_outcome(DialogOutcome { kind: DialogKind::OpenShow, path: Some(out), more: Vec::new() });
    pump(&mut f.h, "los pads vuelven", |a| a.carts.bank.pads.len() == 16 && a.carts.bank.pads[1].key == "F5");
}

fn with_memory(f: &mut Fixture, secs: f64, name: &str) {
    {
        let d = app_mut(&mut f.h).inspector.draft.as_mut().unwrap();
        d.node.body.audio_mut().unwrap().time_loops.memories = vec![qlab_protocol::cue_dto::MemoryMark { secs, name: name.into(), note: String::new() }];
    }
    pump(&mut f.h, "memoria guardada", |a| a.model.summary(1).is_some_and(|c| !c.memories.is_empty()) && a.inspector.draft.as_ref().is_some_and(|d| !d.dirty()));
}

fn start_override(a: &QlabApp) -> Option<f64> {
    a.inspector.draft.as_ref().and_then(|d| d.baseline.body.audio().and_then(|b| b.time_loops.start_override_secs))
}

#[test]
fn the_preroll_selector_moves_the_jump_before_the_marker_shows_a_notice_and_is_remembered() {
    let mut f = fixture("preroll");
    with_memory(&mut f, 1.5, "1er levantada");
    app_mut(&mut f.h).memories_open = true;
    f.h.step();
    f.h.step();
    // El selector está en el panel y en Time & Loops.
    assert!(f.h.query_all_by_label("Pre-roll").count() >= 1);
    f.h.get_all_by_label("4s").next().unwrap().click();
    pump(&mut f.h, "pre-roll de 4 s", |a| a.memories_panel.preroll == 4);
    // La pista dura 2 s: 1,5 s − 4 s se queda en el inicio del archivo, y el aviso dice cuánto se adelantó.
    f.h.get_all_by_label_contains("1er levantada").next().unwrap().click();
    pump(&mut f.h, "arranque desde el inicio", |a| start_override(a) == Some(0.0));
    assert_eq!(app(&f.h).hud_text(), Some("Pre-roll: -1.5s -> Memoria [1er levantada]"));
    // Con Off salta exacto al marcador.
    f.h.get_all_by_label("Off").next().unwrap().click();
    pump(&mut f.h, "pre-roll apagado", |a| a.memories_panel.preroll == 0);
    f.h.get_all_by_label_contains("1er levantada").next().unwrap().click();
    pump(&mut f.h, "arranque exacto", |a| start_override(a) == Some(1.5));
    // Shift+clic aplica 2 s a ese salto aunque esté apagado.
    f.h.get_all_by_label_contains("1er levantada").next().unwrap().click_modifiers(egui::Modifiers::SHIFT);
    pump(&mut f.h, "shift: 2 s antes", |a| start_override(a).is_some_and(|s| (s - 0.0).abs() < 1e-9));
    assert!(app(&f.h).hud_text().is_some_and(|t| t.starts_with("Pre-roll: -1.5s")));
}

#[test]
fn a_memory_jump_starts_the_cue_from_the_chosen_point_with_the_micro_fade() {
    let mut f = fixture("prefade");
    with_memory(&mut f, 1.0, "mitad");
    {
        let ctx = f.h.ctx.clone();
        app_mut(&mut f.h).activate_memory_for_test(&ctx, 1, 1.0);
    }
    pump(&mut f.h, "arranque desde 1 s guardado", |a| start_override(a) == Some(1.0));
    f.h.key_press(egui::Key::Space);
    pump(&mut f.h, "suena desde la memoria", |a| a.telemetry.running.get(&1).is_some_and(|i| i.source_frame >= 48_000));
    // La pista dura 2 s y arranca en 1 s: cuando se ve la lectura puede haber terminado ya.
    assert!(matches!(app(&f.h).model.state_of(1), CueUiState::Running | CueUiState::Completed));
}

#[test]
fn the_rehearsal_speed_buttons_change_the_rate_live_without_restarting_the_cue() {
    let mut f = fixture("varispeed");
    app_mut(&mut f.h).inspector.tab = Tab::TimeLoops;
    f.h.step();
    f.h.step();
    for label in ["0.90× (-10%)", "0.95× (-5%)", "1.00× Normal", "1.05× (+5%)"] {
        assert!(f.h.query_by_label(label).is_some(), "falta el botón {label}");
    }
    f.h.get_by_label("GO").click();
    pump(&mut f.h, "suena", |a| a.model.state_of(1) == CueUiState::Running);
    let before = app(&f.h).telemetry.running.get(&1).map_or(0, |i| i.source_frame);
    f.h.get_by_label("0.95× (-5%)").click();
    pump(&mut f.h, "velocidad guardada en caliente", |a| a.inspector.draft.as_ref().is_some_and(|d| d.baseline.body.audio().is_some_and(|b| (b.time_loops.rate - 0.95).abs() < 1e-6)));
    // Sigue sonando, sin reiniciar: la lectura no vuelve atrás.
    assert!(app(&f.h).model.state_of(1) == CueUiState::Running || app(&f.h).model.state_of(1) == CueUiState::Completed);
    assert!(app(&f.h).telemetry.running.get(&1).map_or(u64::MAX, |i| i.source_frame) >= before);
    f.h.get_by_label("1.00× Normal").click();
    pump(&mut f.h, "vuelve a 1×", |a| a.inspector.draft.as_ref().is_some_and(|d| d.baseline.body.audio().is_some_and(|b| (b.time_loops.rate - 1.0).abs() < 1e-6)));
}

#[test]
fn the_numeric_sequence_dot_n_dot_jumps_to_a_memory_number_and_shows_a_badge() {
    let mut f = fixture("numnav");
    with_memory(&mut f, 1.0, "reloj");
    f.h.key_press(egui::Key::Period);
    f.h.step();
    assert_eq!(app(&f.h).hud_text(), Some("Memoria #…"));
    f.h.key_press(egui::Key::Num1);
    f.h.step();
    assert_eq!(app(&f.h).hud_text(), Some("Memoria #1…"));
    f.h.key_press(egui::Key::Period);
    pump(&mut f.h, "salta a la memoria 1", |a| start_override(a) == Some(1.0));
    assert!(app(&f.h).hud_text().is_some_and(|t| t.starts_with("Memoria #1")));
    // «. 1 Enter» también cierra la secuencia; un número inexistente avisa.
    f.h.key_press(egui::Key::Period);
    f.h.key_press(egui::Key::Num9);
    f.h.key_press(egui::Key::Enter);
    f.h.step();
    assert_eq!(app(&f.h).hud_text(), Some("No existe la memoria #9"));
    assert!(app(&f.h).quick_memory.is_none(), "el Enter de la secuencia no abre el cuadro de memoria");
    // Los dígitos sueltos (sin el punto) no hacen nada.
    f.h.key_press(egui::Key::Num2);
    f.h.step();
    assert!(app(&f.h).hud_text().is_some_and(|t| !t.starts_with("Memoria #2")));
}

#[test]
fn show_lock_blocks_editing_but_keeps_go_panic_master_and_memory_jumps_working() {
    let mut f = fixture("lock");
    with_memory(&mut f, 1.0, "mitad");
    assert!(!app(&f.h).window_title().contains("BLOQUEADA"));
    f.h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::L);
    pump(&mut f.h, "bloqueado", |a| a.locked);
    assert!(app(&f.h).window_title().contains("FUNCIÓN BLOQUEADA"));
    assert!(f.h.query_by_label("Función BLOQUEADA").is_some());
    // Bloqueado: no borra, no añade, no duplica, no marca memorias.
    let cues = cue_count(app(&f.h));
    f.h.key_press(egui::Key::Delete);
    f.h.step();
    app_mut(&mut f.h).apply_for_test(crate::views::Action::AddCue("audio"));
    f.h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::D);
    f.h.step();
    f.h.step();
    assert_eq!(cue_count(app(&f.h)), cues, "el show no se altera");
    // Sigue operativo: GO, saltos de memoria, MASTER y Panic.
    {
        let ctx = f.h.ctx.clone();
        app_mut(&mut f.h).activate_memory_for_test(&ctx, 1, 1.0);
    }
    pump(&mut f.h, "salto de memoria", |a| start_override(a) == Some(1.0));
    f.h.key_press(egui::Key::Space);
    pump(&mut f.h, "GO suena", |a| a.model.state_of(1) == CueUiState::Running);
    app_mut(&mut f.h).apply_for_test(crate::views::Action::SetMasterDb(-6.0));
    assert_eq!(app(&f.h).master_db, -6.0);
    f.h.key_press(egui::Key::Escape);
    pump(&mut f.h, "Panic detiene", |a| a.model.state_of(1) != CueUiState::Running);
    // Desbloquear devuelve la edición.
    f.h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::L);
    pump(&mut f.h, "desbloqueado", |a| !a.locked);
    app_mut(&mut f.h).apply_for_test(crate::views::Action::AddCue("audio"));
    pump(&mut f.h, "ahora sí se añade", |a| cue_count(a) == cues + 1);
}

#[test]
fn the_giant_clock_counts_down_the_running_track_and_is_dashed_when_idle() {
    let mut f = fixture("clock");
    f.h.step();
    assert!(f.h.query_by_label("--:--.-").is_some(), "parado: guiones");
    f.h.get_by_label("GO").click();
    pump(&mut f.h, "suena", |a| a.model.state_of(1) == CueUiState::Running);
    pump(&mut f.h, "cuenta regresiva visible", |a| crate::views::clock::countdown(&a.model, &a.telemetry).is_some());
    f.h.step();
    assert!(f.h.query_all_by_label_contains("-00:0").next().is_some() || f.h.query_all_by_label_contains("-00:1").next().is_some(), "el reloj muestra -mm:ss.d");
    assert!(f.h.query_by_label("--:--.-").is_none());
    let cd = crate::views::clock::countdown(&app(&f.h).model, &app(&f.h).telemetry).unwrap();
    assert_eq!(crate::views::clock::alert_of(cd.remaining), crate::views::clock::Alert::Critical, "una pista de 2 s está en rojo");
}

#[test]
fn the_headphone_selector_offers_channel_pairs_only_with_four_channels_and_any_other_system_output() {
    let mut cfg = AppConfig::new(format!("/tmp/qlab-ui-click-{}-phones.sock", std::process::id()));
    cfg.embedded_on_start = Some(false);
    let mut h = harness(cfg);
    pump(&mut h, "motor conectado", |a| a.is_connected());
    let mut cur = crate::settings::AudioSettings::default();
    cur.device = Some("Focusrite USB".into());
    cur.host = Some("wasapi".into());
    app_mut(&mut h).audio_ui.open_with(&cur);
    app_mut(&mut h).audio_ui.set_devices_for_test(vec![("wasapi".into(), "Focusrite USB".into()), ("wasapi".into(), "Realtek High Definition Audio".into()), ("wasapi".into(), "Auriculares USB".into())]);
    h.step();
    h.step();
    // Placa principal estéreo: no hay pares de canales, pero sí las otras placas del sistema.
    h.get_by_value("Sin salida de auriculares").click();
    h.step();
    h.step();
    assert!(h.query_by_label("Realtek High Definition Audio").is_some() && h.query_by_label("Auriculares USB").is_some());
    assert!(h.query_by_label("Canales 3-4 de la placa principal").is_none(), "con 2 canales no hay par sobrante");
    assert!(h.query_by_label_contains("Otras salidas del sistema").is_some());
    h.get_by_label("Auriculares USB").click();
    h.step();
    assert_eq!(app(&h).audio_ui.draft.monitor_device.as_deref(), Some("Auriculares USB"));
    assert_eq!(app(&h).audio_ui.draft.monitor_first, None);
    // Con 4 canales aparece además el par 3-4 de la placa principal, y elegirlo quita la otra placa.
    app_mut(&mut h).audio_ui.draft.channels = 4;
    h.step();
    h.step();
    h.get_by_value("Auriculares USB").click();
    h.step();
    h.step();
    assert!(h.query_by_label("Canales 3-4 de la placa principal").is_some());
    h.get_by_label("Canales 3-4 de la placa principal").click();
    h.step();
    assert_eq!((app(&h).audio_ui.draft.monitor_first, app(&h).audio_ui.draft.monitor_device.clone()), (Some(2), None));
}

fn rect_of(h: &H, label: &str) -> egui::Rect {
    h.get_all_by_label_contains(label).next().unwrap_or_else(|| panic!("falta «{label}»")).rect()
}

#[test]
fn time_and_loops_controls_never_collapse_even_with_the_memories_panel_open_on_a_small_screen() {
    let mut f = fixture("tlwrap");
    f.h.set_size(egui::vec2(1366.0, 768.0));
    app_mut(&mut f.h).inspector.tab = Tab::TimeLoops;
    app_mut(&mut f.h).memories_open = true; // el panel lateral le quita ~340 px al Inspector
    for _ in 0..6 {
        f.h.step();
    }
    let inspector = app(&f.h).inspector_rect;
    for label in ["Velocidad", "Bucles", "Fade integrado", "Fade sobre la forma de onda", "Atar a la cue", "Bucle infinito", "Repeticiones", "0.90× (-10%)", "1.05× (+5%)"] {
        let r = rect_of(&f.h, label);
        assert!(r.right() <= inspector.right() + 1.0 && r.left() >= inspector.left() - 1.0, "«{label}» se sale del Inspector: {r:?} vs {inspector:?}");
        assert!(r.height() < 30.0, "«{label}» se partió en varias líneas: {r:?}");
    }
    // «Fade integrado» y su casilla se leen de corrido, no en una columna de una letra de ancho.
    assert!(rect_of(&f.h, "Fade sobre la forma de onda").width() > 150.0);
    assert!(rect_of(&f.h, "Fade integrado").width() > 60.0);
    // Fade va en una fila propia, debajo de Velocidad.
    assert!(rect_of(&f.h, "Fade integrado").top() > rect_of(&f.h, "Velocidad").top());
}

#[test]
fn the_eq_tab_fits_all_four_bands_inside_the_inspector_on_a_small_screen() {
    let mut f = fixture("eqfit");
    f.h.set_size(egui::vec2(1366.0, 768.0));
    app_mut(&mut f.h).inspector.tab = Tab::Eq;
    for _ in 0..6 {
        f.h.step();
    }
    let inspector = app(&f.h).inspector_rect;
    let bands: Vec<egui::Rect> = f.h.query_all_by_label("4").map(|n| n.rect()).collect();
    assert!(!bands.is_empty(), "no se encontró la fila de la Banda 4");
    let b4 = bands.iter().max_by(|a, b| a.top().total_cmp(&b.top())).unwrap();
    assert!(b4.bottom() <= inspector.bottom() - 2.0, "la Banda 4 queda cortada: {b4:?} vs {inspector:?}");
    for l in ["1", "2", "3"] {
        assert!(f.h.query_all_by_label(l).next().is_some());
    }
}

#[test]
fn a_memory_time_can_be_typed_in_the_list_and_the_panel_reordering_the_memories_and_moving_the_flag() {
    let mut f = fixture("memtime");
    {
        let d = app_mut(&mut f.h).inspector.draft.as_mut().unwrap();
        d.node.body.audio_mut().unwrap().time_loops.memories = vec![
            qlab_protocol::cue_dto::MemoryMark { secs: 0.5, name: "uno".into(), note: String::new() },
            qlab_protocol::cue_dto::MemoryMark { secs: 1.0, name: "dos".into(), note: String::new() },
        ];
    }
    pump(&mut f.h, "memorias guardadas", |a| a.model.summary(1).is_some_and(|c| c.memories.len() == 2) && a.inspector.draft.as_ref().is_some_and(|d| !d.dirty()));
    app_mut(&mut f.h).inspector.tab = Tab::TimeLoops;
    app_mut(&mut f.h).memories_open = true;
    for _ in 0..4 {
        f.h.step();
    }
    let mems = |a: &QlabApp| a.inspector.draft.as_ref().map(|d| d.node.body.audio().unwrap().time_loops.memories.iter().map(|m| (m.name.clone(), m.secs)).collect::<Vec<_>>()).unwrap_or_default();
    // Panel lateral: clic en el tiempo de «uno» (00:00.500), escribir 1.5 s con segundos decimales y Enter.
    let first = f.h.query_all_by_label("00:00.500").next().expect("hay una etiqueta de tiempo editable").rect().center();
    let _ = first;
    f.h.get_all_by_label("00:00.500").next().unwrap().click();
    f.h.step();
    f.h.step();
    f.h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::A);
    f.h.get_all_by_role(egui::accesskit::Role::TextInput).next().expect("campo de texto en línea").type_text("1.5");
    f.h.key_press(egui::Key::Enter);
    f.h.step();
    f.h.step();
    pump(&mut f.h, "«uno» pasó a 1,5 s y la lista se reordenó", |a| mems(a) == vec![("dos".to_string(), 1.0), ("uno".to_string(), 1.5)]);
    // La bandera de la onda siguió al nuevo tiempo (la lista y la onda salen de los mismos datos).
    assert!(f.h.query_by_label("Memoria 2: uno").is_some(), "la bandera de «uno» ahora es la segunda");
}

#[test]
fn memory_times_are_parsed_flexibly_clamped_to_the_file_and_snapped_to_an_exact_frame() {
    use crate::views::memories::{commit_memory_time, parse_memory_time, snap_to_frame};
    assert_eq!(parse_memory_time("01:14.500"), Some(74.5));
    assert_eq!(parse_memory_time("1:14"), Some(74.0));
    assert_eq!(parse_memory_time("74.5"), Some(74.5));
    assert_eq!(parse_memory_time("74,5"), Some(74.5));
    assert_eq!(parse_memory_time("  0:00.250 "), Some(0.25));
    for bad in ["", "abc", "1:2:3:4", "-3", "1:xx"] {
        assert_eq!(parse_memory_time(bad), None, "{bad}");
    }
    // Se recorta al archivo y cae en un frame exacto.
    assert_eq!(commit_memory_time("500", 120.0, 48_000), Some(120.0));
    assert_eq!(commit_memory_time("1.00001", f64::INFINITY, 1_000), Some(1.0));
    assert_eq!(snap_to_frame(0.123_456_7, 48_000), 5_926.0 / 48_000.0);
    assert_eq!(commit_memory_time("basura", 10.0, 48_000), None);
}

#[test]
fn the_top_bar_shrinks_to_fit_so_the_engine_status_is_never_clipped_at_1366_1280_or_narrower() {
    let mut f = fixture("topbarfit");
    for (w, h) in [(1366.0f32, 768.0f32), (1280.0, 720.0), (1100.0, 700.0), (960.0, 640.0)] {
        f.h.set_size(egui::vec2(w, h));
        for _ in 0..6 {
            f.h.step();
        }
        let screen = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(w, h));
        let labels = ["GO", "STANDBY", "RESTANTE", "Pausar todo", "Parar todo", "PANIC", "MASTER", "0.0 dB", "XR 0", "CPU"];
        for l in labels {
            let r = rect_of(&f.h, l);
            assert!(screen.contains_rect(r), "a {w} px «{l}» se sale de la pantalla: {r:?}");
        }
        // El estado del motor queda entero a la derecha de los controles, sin pisarlos y en dos líneas compactas.
        let (panic_btn, xr, cpu, master) = (rect_of(&f.h, "PANIC"), rect_of(&f.h, "XR 0"), rect_of(&f.h, "CPU"), rect_of(&f.h, "MASTER"));
        assert!(panic_btn.right() <= xr.left() + 0.5, "a {w} px el estado pisa el botón PANIC: {panic_btn:?} / {xr:?}");
        assert!(cpu.right() <= w - 2.0, "a {w} px «CPU» queda cortado: {cpu:?}");
        assert!(xr.height() < 24.0 && cpu.height() < 24.0, "texto de estado partido en varias líneas");
        assert!(master.width() > 40.0 && master.height() < 28.0, "MASTER aplastado a {w} px: {master:?}");
        // Los vúmetros y el deslizador siguen a la derecha de MASTER, dentro de la pantalla.
        assert!(rect_of(&f.h, "0.0 dB").right() <= xr.left() + 60.0 || rect_of(&f.h, "0.0 dB").bottom() <= xr.top() + 40.0);
    }
    // Los niveles de ancho cubren la barra con holgura en cada ventana típica.
    use crate::views::top_bar::Metrics;
    for (w, ch) in [(1366.0f32, 2usize), (1280.0, 2), (1100.0, 2), (1366.0, 8)] {
        let m = Metrics::for_width(w - 16.0);
        assert!(m.total_width(ch) <= w - 16.0, "{w}px/{ch}ch: la barra necesita {} px", m.total_width(ch));
    }
}

#[test]
fn the_projection_button_is_a_clear_toggle_green_and_labeled_when_active_and_a_second_click_closes_it() {
    let mut f = fixture("projtoggle");
    assert!(!app(&f.h).projection.open);
    assert!(f.h.query_by_label("Proyección").is_some() && f.h.query_by_label("Proyección activa").is_none());
    f.h.get_by_label("Proyección").click();
    pump(&mut f.h, "proyección abierta", |a| a.projection.open);
    f.h.step();
    f.h.step();
    assert!(f.h.query_by_label("Proyección activa").is_some(), "el botón avisa que la proyección está activa");
    f.h.get_by_label("Proyección activa").click_accesskit(); // en las pruebas la ventana flotante embebida tapa la barra
    pump(&mut f.h, "proyección cerrada", |a| !a.projection.open);
    f.h.step();
    f.h.step();
    assert!(f.h.query_by_label("Proyección").is_some() && f.h.query_by_label("Proyección activa").is_none());
}

#[test]
fn dragging_a_memory_flag_moves_it_with_a_guide_line_snaps_to_a_frame_reorders_the_list_and_a_plain_click_still_jumps() {
    let mut f = fixture("flagdrag");
    {
        let d = app_mut(&mut f.h).inspector.draft.as_mut().unwrap();
        d.node.body.audio_mut().unwrap().time_loops.memories = vec![
            qlab_protocol::cue_dto::MemoryMark { secs: 0.5, name: "uno".into(), note: String::new() },
            qlab_protocol::cue_dto::MemoryMark { secs: 1.0, name: "dos".into(), note: String::new() },
        ];
    }
    pump(&mut f.h, "memorias guardadas", |a| a.model.summary(1).is_some_and(|c| c.memories.len() == 2) && a.inspector.draft.as_ref().is_some_and(|d| !d.dirty()));
    app_mut(&mut f.h).inspector.tab = Tab::TimeLoops;
    for _ in 0..4 {
        f.h.step();
    }
    let mems = |a: &QlabApp| a.inspector.draft.as_ref().map(|d| d.node.body.audio().unwrap().time_loops.memories.iter().map(|m| (m.name.clone(), m.secs)).collect::<Vec<_>>()).unwrap_or_default();
    let (c1, c2) = (rect_of(&f.h, "Memoria 1: uno").center(), rect_of(&f.h, "Memoria 2: dos").center());
    let px_per_sec = (c2.x - c1.x) / 0.5;
    assert!(px_per_sec > 20.0, "la regla tiene escala: {px_per_sec}");

    // Arrastrar «uno» de 0,5 s a 1,5 s: mientras se arrastra, el dato no cambia (se confirma al soltar).
    f.h.hover_at(c1);
    f.h.step();
    f.h.drag_at(c1);
    f.h.step();
    let target = egui::pos2(c1.x + 1.0 * px_per_sec, c1.y);
    for k in 1..=4 {
        f.h.hover_at(egui::pos2(c1.x + (target.x - c1.x) * k as f32 / 4.0, c1.y + 3.0));
        f.h.step();
    }
    assert_eq!(mems(app(&f.h)), vec![("uno".to_string(), 0.5), ("dos".to_string(), 1.0)], "sin soltar no se guarda nada");
    f.h.hover_at(target);
    f.h.step();
    f.h.drop_at(target);
    f.h.step();
    f.h.step();
    let after = mems(app(&f.h));
    // Reordenadas cronológicamente, con el tiempo del soltado (≈1,5 s) ajustado a un frame exacto de 48 kHz.
    assert_eq!(after.iter().map(|m| m.0.as_str()).collect::<Vec<_>>(), vec!["dos", "uno"], "{after:?}");
    let moved = after[1].1;
    assert!((moved - 1.5).abs() < 0.05, "{moved}");
    assert!(((moved * 48_000.0) - (moved * 48_000.0).round()).abs() < 1e-6, "cae en un frame exacto: {moved}");
    // Se guarda en el daemon y el show queda con cambios pendientes.
    pump(&mut f.h, "guardado y show marcado como modificado", |a| a.show_dirty && a.inspector.draft.as_ref().is_some_and(|d| !d.dirty()) && a.model.summary(1).is_some_and(|c| c.memories.iter().any(|m| m.name == "uno" && (m.secs - moved).abs() < 1e-9)));
    assert!(app(&f.h).window_title().contains('*'));

    // Un clic simple (sin arrastrar) sigue saltando a la memoria.
    let flag = rect_of(&f.h, "Memoria 1: dos").center();
    f.h.hover_at(flag);
    f.h.step();
    f.h.drag_at(flag);
    f.h.step();
    f.h.drop_at(flag);
    f.h.step();
    pump(&mut f.h, "el clic arma «dos» como punto de arranque", |a| start_override(a) == Some(1.0));
}

#[test]
fn a_dragged_memory_time_is_clamped_to_the_file_and_snapped_to_a_frame() {
    use crate::views::inspector::tab_time_loops::drag_memory_time;
    assert_eq!(drag_memory_time(-3.0, 120.0, 48_000), 0.0);
    assert_eq!(drag_memory_time(500.0, 120.0, 48_000), 120.0);
    assert_eq!(drag_memory_time(f64::NAN, 120.0, 48_000), 0.0);
    let v = drag_memory_time(50.153_217, 3_534.0, 48_000);
    assert!((v * 48_000.0 - (v * 48_000.0).round()).abs() < 1e-6 && (v - 50.153_217).abs() < 1.0 / 48_000.0, "{v}");
}

#[test]
fn p_pauses_freezing_the_playhead_on_the_waveform_lets_m_mark_that_frame_and_p_resumes_from_it() {
    use crate::app::{pause_decision, PauseToggle};
    // La decisión de la tecla, sin motor.
    assert_eq!(pause_decision(&[CueUiState::Idle, CueUiState::Armed]), None);
    assert_eq!(pause_decision(&[CueUiState::Running, CueUiState::Idle]), Some(PauseToggle::Pause));
    assert_eq!(pause_decision(&[CueUiState::PreWaiting]), Some(PauseToggle::Pause));
    assert_eq!(pause_decision(&[CueUiState::Paused, CueUiState::Running]), Some(PauseToggle::Resume));
    assert_eq!(pause_decision(&[CueUiState::Completed]), None);

    // Con el motor: la cue 1 tiene un bucle infinito desde 0,5 s, así que suena todo el tiempo que haga falta.
    let mut f = fixture_with("pausekey", true);
    app_mut(&mut f.h).inspector.tab = Tab::TimeLoops;
    f.h.key_press(egui::Key::Space);
    pump(&mut f.h, "suena", |a| a.model.state_of(1) == CueUiState::Running && a.telemetry.running.get(&1).is_some_and(|i| i.source_frame > 0));
    std::thread::sleep(Duration::from_millis(120));

    // P pausa: el estado pasa a Paused y el cabezal queda quieto.
    f.h.key_press(egui::Key::P);
    pump(&mut f.h, "en pausa", |a| a.model.state_of(1) == CueUiState::Paused);
    std::thread::sleep(Duration::from_millis(150)); // la rampa de 5 ms ya terminó
    f.h.step();
    let frozen = app(&f.h).telemetry.running.get(&1).expect("la cue en pausa sigue en la lista de las activas").source_frame;
    assert!(frozen > 0);
    for _ in 0..5 {
        std::thread::sleep(Duration::from_millis(60));
        f.h.step();
        let i = app(&f.h).telemetry.running.get(&1).expect("el cursor sigue visible en pausa");
        assert!(i.phase.is_some() && i.source_frame == frozen, "el cabezal se movió estando en pausa: {} vs {frozen}", i.source_frame);
    }
    assert_eq!(app(&f.h).model.state_of(1), CueUiState::Paused);

    // Estando en pausa, M clava una memoria en ese frame exacto.
    f.h.key_press(egui::Key::M);
    pump(&mut f.h, "cuadro de nombre de memoria", |a| a.quick_memory.is_some());
    assert_eq!(app(&f.h).quick_memory.as_ref().and_then(|q| q.frame), Some(frozen), "la memoria cae en el frame congelado");
    f.h.key_press(egui::Key::Escape); // cancela el nombre (no hace Panic: el cuadro lo consume)
    pump(&mut f.h, "cuadro cerrado", |a| a.quick_memory.is_none());
    assert_eq!(app(&f.h).model.state_of(1), CueUiState::Paused, "cancelar el nombre no tocó la pausa");

    // P otra vez reanuda desde el mismo frame, sin reiniciar la pista.
    f.h.step();
    f.h.step();
    f.h.key_press(egui::Key::P);
    pump(&mut f.h, "reanudada", |a| a.model.state_of(1) == CueUiState::Running);
    pump(&mut f.h, "el cabezal avanza desde el frame congelado", |a| a.telemetry.running.get(&1).is_some_and(|i| i.source_frame != frozen));
    let now = app(&f.h).telemetry.running.get(&1).unwrap().source_frame;
    // Con el bucle infinito el frame puede volver al inicio del tramo, pero nunca antes de él (0,5 s = 24 000) si no se reinició la pista.
    assert!(now >= 24_000 || now > frozen, "se reinició la pista: {now}");
}

#[test]
fn the_pause_key_does_not_fire_while_typing_in_a_text_field() {
    let mut f = fixture_with("pausetyping", true);
    f.h.key_press(egui::Key::Space);
    pump(&mut f.h, "suena", |a| a.model.state_of(1) == CueUiState::Running);
    // Con el foco en el campo de nombre, una «p» es una letra y no pausa.
    app_mut(&mut f.h).inspector.tab = Tab::Basics;
    f.h.step();
    f.h.step();
    f.h.get_all_by_role(egui::accesskit::Role::TextInput).next().expect("hay un campo de texto").click();
    f.h.step();
    f.h.key_press(egui::Key::P);
    f.h.step();
    std::thread::sleep(Duration::from_millis(100));
    f.h.step();
    assert_eq!(app(&f.h).model.state_of(1), CueUiState::Running, "escribir «p» no pausa");
}

#[test]
fn escape_in_the_memory_name_box_closes_it_without_firing_a_panic_while_a_cue_plays() {
    let mut f = fixture_with("escmem", true);
    app_mut(&mut f.h).inspector.tab = Tab::TimeLoops;
    f.h.key_press(egui::Key::Space);
    pump(&mut f.h, "suena", |a| a.model.state_of(1) == CueUiState::Running && a.telemetry.running.get(&1).is_some_and(|i| i.source_frame > 0));
    f.h.key_press(egui::Key::M);
    pump(&mut f.h, "cuadro de nombre", |a| a.quick_memory.is_some());
    f.h.key_press(egui::Key::Escape);
    pump(&mut f.h, "cuadro cerrado", |a| a.quick_memory.is_none());
    for _ in 0..8 {
        f.h.step();
        std::thread::sleep(Duration::from_millis(40));
    }
    assert!(!app(&f.h).panic_fired_for_test(), "Esc para cancelar el nombre no puede ser un PANIC en pleno show");
    assert_eq!(app(&f.h).model.state_of(1), CueUiState::Running);
    // Un Esc suelto (sin cuadro abierto) sigue siendo PANIC.
    f.h.key_press(egui::Key::Escape);
    f.h.step();
    assert!(app(&f.h).panic_fired_for_test());
}

#[test]
fn a_damaged_file_shows_a_repair_button_that_writes_a_clean_copy_and_the_cue_can_switch_to_it() {
    let Some(damaged) = crate::waveform::tests_support::damaged_m4a("repairui") else { return eprintln!("ffmpeg no está instalado: se omite") };
    let mut f = fixture("repairui");
    f.files.push(damaged.clone());
    // La cue 1 pasa a usar el archivo dañado.
    {
        let d = app_mut(&mut f.h).inspector.draft.as_mut().unwrap();
        d.node.basics.file_target.as_mut().unwrap().absolute_path = damaged.to_string_lossy().into_owned();
        let t = &mut d.node.body.audio_mut().unwrap().time_loops;
        (t.start_time_sec, t.end_time_sec) = (0.0, 0.0);
    }
    pump(&mut f.h, "archivo asignado", |a| a.inspector.draft.as_ref().is_some_and(|d| !d.dirty() && d.baseline.basics.file_target.as_ref().is_some_and(|t| t.absolute_path.ends_with(".m4a"))));
    app_mut(&mut f.h).inspector.tab = Tab::TimeLoops;
    // La onda se calcula, detecta el daño, avisa y ofrece la reparación.
    pump(&mut f.h, "aviso de audio dañado", |a| a.toasts.iter().any(|t| t.text.starts_with("Audio dañado en")));
    f.h.step();
    f.h.step();
    let button = f.h.get_all_by_label_contains("Crear copia reparada (hasta").next().expect("botón de reparación").rect();
    let _ = button;
    f.h.get_all_by_label_contains("Crear copia reparada (hasta").next().unwrap().click();
    pump(&mut f.h, "copia creada", |a| matches!(a.repairs.get(&damaged), Some(crate::repair::RepairState::Done(_))));
    let Some(crate::repair::RepairState::Done(copy)) = app(&f.h).repairs.get(&damaged).cloned() else { unreachable!() };
    f.files.push(copy.clone());
    assert!(copy.is_file() && copy.to_string_lossy().contains("(reparado hasta"));
    assert!(app(&f.h).toasts.iter().any(|t| t.text.starts_with("Copia reparada creada")));
    // Usar la copia en la cue: el archivo cambia, se guarda, y la onda ya no marca daño.
    f.h.step();
    f.h.step();
    f.h.get_by_label("Usar la copia en esta cue").click();
    pump(&mut f.h, "la cue usa la copia", |a| a.inspector.draft.as_ref().is_some_and(|d| !d.dirty() && d.baseline.basics.file_target.as_ref().is_some_and(|t| std::path::Path::new(&t.absolute_path) == copy.as_path())));
    pump(&mut f.h, "onda de la copia, sin daño", |a| matches!(a.waveforms.peek(&copy), Some(crate::waveform::WaveState::Ready(p)) if p.damaged_from.is_none()));
    assert!(f.h.query_all_by_label_contains("Crear copia reparada").next().is_none(), "con la copia ya no hay nada que reparar");
}

#[test]
fn choosing_a_file_names_the_cue_from_its_tags_or_file_name_but_never_overwrites_a_typed_name() {
    let mut f = fixture("autoname");
    let dir = std::env::temp_dir().join(format!("qlab-autoname-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // Un archivo sin etiquetas, con número de pista en el nombre.
    let plain = dir.join("04 - Variacion_de_Kitri.wav");
    std::fs::rename(write_test_wav("autoname-a", 2, 48_000, 48_000, |_, _| 0.2), &plain).unwrap();
    f.files.push(plain.clone());
    let name_of = |a: &QlabApp| a.inspector.draft.as_ref().map(|d| d.node.basics.name.clone()).unwrap_or_default();
    f.h.get_by_label("+ Audio Cue").click();
    pump(&mut f.h, "cue nueva", |a| cue_count(a) == 4 && a.inspector.draft.as_ref().is_some_and(|d| d.id() == 4));
    f.h.state_mut().as_mut().unwrap().on_dialog_outcome(DialogOutcome { kind: DialogKind::PickAudio, path: Some(plain.clone()), more: Vec::new() });
    pump(&mut f.h, "nombre desde el archivo", |a| name_of(a) == "Variacion de Kitri" && a.inspector.draft.as_ref().is_some_and(|d| !d.dirty()));
    // Con etiquetas (si hay ffmpeg para crearlas): cambiar el archivo actualiza el nombre que se había puesto solo.
    let tagged = dir.join("pista_07.mp3");
    let made = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-f", "lavfi", "-i", "sine=duration=1", "-metadata", "title=Entrada de Basilio", "-metadata", "artist=Ludwig Minkus"])
        .arg(&tagged)
        .status()
        .is_ok_and(|s| s.success());
    if made {
        f.files.push(tagged.clone());
        f.h.state_mut().as_mut().unwrap().on_dialog_outcome(DialogOutcome { kind: DialogKind::PickAudio, path: Some(tagged.clone()), more: Vec::new() });
        pump(&mut f.h, "nombre desde las etiquetas", |a| name_of(a) == "Entrada de Basilio — Ludwig Minkus" && a.inspector.draft.as_ref().is_some_and(|d| !d.dirty()));
    }
    // Un nombre escrito a mano se respeta al cambiar de archivo.
    app_mut(&mut f.h).inspector.draft.as_mut().unwrap().node.basics.name = "Kitri — solo de abanico".into();
    pump(&mut f.h, "nombre propio guardado", |a| a.inspector.draft.as_ref().is_some_and(|d| !d.dirty()));
    f.h.state_mut().as_mut().unwrap().on_dialog_outcome(DialogOutcome { kind: DialogKind::PickAudio, path: Some(plain.clone()), more: Vec::new() });
    pump(&mut f.h, "archivo cambiado", |a| a.inspector.draft.as_ref().is_some_and(|d| !d.dirty() && d.baseline.basics.file_target.as_ref().is_some_and(|t| t.absolute_path.ends_with("Kitri.wav"))));
    assert_eq!(name_of(app(&f.h)), "Kitri — solo de abanico");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_long_cue_name_stays_on_one_line_inside_its_box_and_scrolls_instead_of_spilling_out() {
    let mut f = fixture("marquee");
    let long = "(2 Hour Nature Relaxation Video) A Day in the Forest with Birdsong and River Sounds for Sleep";
    app_mut(&mut f.h).inspector.draft.as_mut().unwrap().node.basics.name = long.into();
    pump(&mut f.h, "nombre guardado", |a| a.model.summary(1).is_some_and(|c| c.name == long) && a.inspector.draft.as_ref().is_some_and(|d| !d.dirty()));
    f.h.set_size(egui::vec2(1366.0, 768.0));
    for _ in 0..6 {
        f.h.step();
    }
    let body = egui::TextStyle::Body.resolve(&f.h.ctx.global_style());
    let row = f.h.ctx.fonts_mut(|fo| fo.row_height(&body));
    // STANDBY, la fila de la tabla y la cabecera del Inspector: los tres llevan el nombre completo como etiqueta.
    let standby = f.h.get_by_label(&format!("1  {long}")).rect();
    let cell = f.h.get_by_label(long).rect();
    let header = f.h.get_by_label(&format!("1 · {long} · audio")).rect();
    for (what, r) in [("standby", standby), ("tabla", cell), ("inspector", header)] {
        assert!(r.height() <= 26.0 && r.height() >= row - 1.0, "una sola línea: {what} {r:?}");
        assert!(r.right() <= 1366.0, "no se sale de la ventana: {what} {r:?}");
    }
    assert!(standby.width() < 600.0, "STANDBY no pasa del ancho de su recuadro: {standby:?}");
    let destino = f.h.get_by_label("Destino").rect();
    assert!(cell.right() <= destino.left(), "el nombre no invade la columna Destino: {cell:?} {destino:?}");
}

#[test]
fn the_news_window_opens_on_the_first_run_of_a_new_version_only_once_and_again_after_an_update() {
    let dir = std::env::temp_dir().join(format!("qlab-ui-news-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let settings = dir.join("audio.json");
    let open_app = || {
        let mut cfg = AppConfig::new(format!("/tmp/qlab-ui-news-{}.sock", std::process::id()));
        cfg.settings_path = Some(settings.clone());
        let mut h = harness(cfg);
        for _ in 0..4 {
            h.step();
        }
        h
    };
    // Primera vez con esta versión: el cuadro sale solo, con el texto de CAMBIOS.txt.
    let mut h = open_app();
    assert!(app(&h).news_open_for_test(), "sale al abrir por primera vez");
    h.get_by_label("Novedades de esta versión");
    h.get_by_label("1. Pausa con la tecla P");
    // Esc lo cierra y no es un PANIC; tampoco dispara nada.
    h.key_press(egui::Key::Escape);
    h.step();
    h.step();
    assert!(!app(&h).news_open_for_test(), "Esc lo cierra");
    assert!(!app(&h).panic_fired_for_test(), "Esc en el cuadro de novedades no es un PANIC");
    drop(h);
    // Segunda vez: no sale.
    let h = open_app();
    assert!(!app(&h).news_open_for_test(), "ya se vieron");
    drop(h);
    // Actualización: la versión anterior había anotado otra huella, y vuelve a salir.
    std::fs::write(crate::novedades::seen_path(&settings), "0000000000000000\n").unwrap();
    let mut h = open_app();
    assert!(app(&h).news_open_for_test(), "después de actualizar sale otra vez");
    h.get_by_label("Entendido").click();
    h.step();
    h.step();
    assert!(!app(&h).news_open_for_test());
    drop(h);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_news_can_be_opened_again_from_the_help_menu() {
    let mut f = fixture("newsmenu");
    assert!(!app(&f.h).news_open_for_test(), "sin ajustes (pruebas) no sale sola");
    f.h.get_by_label("Ayuda").click();
    f.h.step();
    f.h.step();
    f.h.get_by_label("Novedades de esta versión").click();
    for _ in 0..3 {
        f.h.step();
    }
    assert!(app(&f.h).news_open_for_test());
    f.h.get_by_label("Entendido").click();
    for _ in 0..3 {
        f.h.step();
    }
    assert!(!app(&f.h).news_open_for_test());
}

#[test]
fn a_long_cue_name_does_not_widen_the_memories_panel_and_the_panel_can_be_dragged_narrower_and_wider() {
    let mut f = fixture("mempanelw");
    let long = "(2 Hour Nature Relaxation Video) A Day in Big Sur, California 1080p Relaxation Video Pure Nature — Nature Relaxation Films";
    app_mut(&mut f.h).inspector.draft.as_mut().unwrap().node.basics.name = long.into();
    pump(&mut f.h, "nombre guardado", |a| a.model.summary(1).is_some_and(|c| c.name == long) && a.inspector.draft.as_ref().is_some_and(|d| !d.dirty()));
    with_memory(&mut f, 0.5, "popo con un nombre de memoria bastante largo también");
    f.h.set_size(egui::vec2(1366.0, 768.0));
    app_mut(&mut f.h).memories_open = true;
    for _ in 0..6 {
        f.h.step();
    }
    // El panel está a la izquierda: el Inspector empieza donde termina el panel.
    let edge = |h: &H| app(h).inspector_rect.left();
    let start = edge(&f.h);
    assert!(start < 400.0, "el panel se abre con su ancho normal aunque el nombre de la cue sea larguísimo: {start}");
    // Arrastrar el borde: a 280 px queda en 280 (no vuelve a abrirse solo) y después se puede ensanchar.
    drag_panel_edge(&mut f.h, start - 1.0, 280.0);
    let narrow = edge(&f.h);
    assert!(narrow < start - 40.0 && narrow < 320.0, "se achicó: {start} -> {narrow}");
    for _ in 0..10 {
        f.h.step();
    }
    assert!((edge(&f.h) - narrow).abs() < 2.0, "y se queda así, no vuelve a ensancharse: {narrow} -> {}", edge(&f.h));
    // La fila de la memoria entra en el panel angosto: nada se sale por la derecha.
    let right = edge(&f.h);
    let in_panel: Vec<_> = f.h.query_all_by_label_contains("PFL").map(|n| n.rect()).chain(f.h.query_all_by_label_contains(long).map(|n| n.rect())).filter(|r| r.left() < right && r.top() > 160.0).collect(); // debajo de la barra superior (STANDBY también lleva el nombre)
    assert!(in_panel.len() >= 2, "{in_panel:?}");
    for r in &in_panel {
        assert!(r.right() <= right + 1.0, "se sale del panel: {r:?} borde {right}");
    }
    drag_panel_edge(&mut f.h, narrow - 1.0, 520.0);
    assert!(edge(&f.h) > 480.0, "también se puede ensanchar: {}", edge(&f.h));
}

fn drag_panel_edge(h: &mut H, from: f32, to: f32) {
    let y = 400.0;
    h.hover_at(egui::pos2(from, y));
    h.step();
    h.drag_at(egui::pos2(from, y));
    h.step();
    for k in 1..=4 {
        h.hover_at(egui::pos2(from + (to - from) * k as f32 / 4.0, y));
        h.step();
    }
    h.drop_at(egui::pos2(to, y));
    for _ in 0..4 {
        h.step();
    }
}

fn start_point(a: &QlabApp, id: u64) -> Option<f64> {
    a.inspector.draft.as_ref().filter(|d| d.id() == id && !d.dirty()).and_then(|d| d.baseline.body.audio().and_then(|b| b.time_loops.start_override_secs))
}

/// La cue 1 suena hasta pasar `secs` del archivo; devuelve dónde estaba en el momento de parar.
fn play_cue_1_until(f: &mut Fixture, secs: f64) -> f64 {
    f.h.key_press(egui::Key::Space);
    pump(&mut f.h, "la cue 1 pasa el punto", |a| a.telemetry.running.get(&1).is_some_and(|i| i.source_frame as f64 / 48_000.0 >= secs));
    app(&f.h).telemetry.running.get(&1).map(|i| i.source_frame as f64 / 48_000.0).unwrap()
}

#[test]
fn with_resume_after_stop_a_panic_in_rehearsal_leaves_the_music_ready_to_continue_from_there_with_go() {
    let mut f = fixture("resume");
    app_mut(&mut f.h).prefs.resume_after_stop = true;
    app_mut(&mut f.h).memories_panel.preroll = 0;
    let at = play_cue_1_until(&mut f, 0.6);
    assert_eq!(app(&f.h).model.playhead, Some(2), "el GO ya pasó a la siguiente");
    f.h.key_press(egui::Key::Escape); // «¡paren!»
    f.h.step();
    pump(&mut f.h, "la cue 1 queda lista donde se paró", |a| a.model.playhead == Some(1) && start_point(a, 1).is_some_and(|s| s >= at - 0.05 && s < at + 0.3));
    let ready = start_point(app(&f.h), 1).unwrap();
    assert!(app(&f.h).hud_text().is_some_and(|t| t.starts_with("Retoma en")), "{:?}", app(&f.h).hud_text());
    pump(&mut f.h, "la parada termina", |a| a.telemetry.running.get(&1).is_none_or(|i| i.phase.is_none()));
    // «Desde ahí»: GO retoma desde ese punto, no desde el principio.
    f.h.key_press(egui::Key::Space);
    pump(&mut f.h, "vuelve a sonar", |a| a.telemetry.running.get(&1).is_some_and(|i| i.source_frame > 0));
    let first = app(&f.h).telemetry.running.get(&1).unwrap().source_frame as f64 / 48_000.0;
    assert!(first >= ready - 0.02, "retoma en {ready:.2} s, no desde el principio (leyó desde {first:.2} s)");
    // Sirve una vez: al terminar la música, la cue vuelve a arrancar desde su inicio (nada del ensayo queda para la función).
    pump(&mut f.h, "termina y vuelve a su inicio", |a| a.model.state_of(1) == CueUiState::Completed && a.resume_points_for_test() == 0 && a.model.summary(1).is_some_and(|c| c.start_override_secs.is_none()));
}

#[test]
fn turning_on_the_show_lock_clears_rehearsal_resume_points_and_saving_never_writes_them_to_the_show() {
    let mut f = fixture("resumeclear");
    app_mut(&mut f.h).prefs.resume_after_stop = true;
    app_mut(&mut f.h).memories_panel.preroll = 0;
    play_cue_1_until(&mut f, 0.5);
    f.h.key_press(egui::Key::Escape);
    f.h.step();
    pump(&mut f.h, "punto puesto", |a| a.model.summary(1).is_some_and(|c| c.start_override_secs.is_some()));
    // Guardar: el archivo del show sale sin el punto de ensayo, y el ensayo sigue con su punto.
    let out = std::env::temp_dir().join(format!("qlab-ui-resume-save-{}.json", std::process::id()));
    app_mut(&mut f.h).save_show_as_for_test(&out);
    pump(&mut f.h, "guardado", |_| out.is_file());
    let saved = std::fs::read_to_string(&out).unwrap();
    assert!(!saved.contains("start_override_secs"), "el show guardado no lleva puntos de ensayo");
    pump(&mut f.h, "el punto sigue para el ensayo", |a| a.model.summary(1).is_some_and(|c| c.start_override_secs.is_some()));
    // Bloqueo de función (función): la cue vuelve a su inicio.
    app_mut(&mut f.h).toggle_lock();
    pump(&mut f.h, "vuelve a su inicio", |a| a.resume_points_for_test() == 0 && a.model.summary(1).is_some_and(|c| c.start_override_secs.is_none()));
    let _ = std::fs::remove_file(&out);
}

#[test]
fn resume_after_stop_uses_the_preroll_and_does_nothing_with_the_show_lock_or_when_switched_off() {
    // Con pre-roll de 2 s la música vuelve un poco antes del punto de parada (aquí, al principio del archivo).
    let mut f = fixture("resumepre");
    app_mut(&mut f.h).prefs.resume_after_stop = true;
    app_mut(&mut f.h).memories_panel.preroll = 2;
    play_cue_1_until(&mut f, 0.5);
    f.h.key_press(egui::Key::Escape);
    f.h.step();
    pump(&mut f.h, "lista con pre-roll", |a| a.model.playhead == Some(1) && start_point(a, 1) == Some(0.0));
    assert!(app(&f.h).hud_text().is_some_and(|t| t.contains("pre-roll")));
    drop(f);

    // En función (Bloqueo activo): parar no cambia nada; el playhead sigue en la siguiente cue.
    let mut f = fixture("resumelock");
    app_mut(&mut f.h).prefs.resume_after_stop = true;
    play_cue_1_until(&mut f, 0.4);
    app_mut(&mut f.h).toggle_lock();
    f.h.key_press(egui::Key::Escape);
    for _ in 0..20 {
        f.h.step();
    }
    assert_eq!(app(&f.h).pending_resume_for_test(), None);
    assert_eq!(app(&f.h).model.playhead, Some(2));
    drop(f);

    // Opción apagada (de fábrica): igual que siempre.
    let mut f = fixture("resumeoff");
    assert!(!app(&f.h).prefs.resume_after_stop);
    play_cue_1_until(&mut f, 0.4);
    f.h.key_press(egui::Key::Escape);
    for _ in 0..20 {
        f.h.step();
    }
    assert_eq!(app(&f.h).model.playhead, Some(2));
}

#[test]
fn the_go_key_can_be_changed_in_preferences_and_esc_while_choosing_a_key_is_not_a_panic() {
    let mut f = fixture("hotkeys");
    f.h.get_by_label("Opciones").click();
    f.h.step();
    f.h.step();
    f.h.get_by_label("Preferencias y atajos de teclado…").click();
    for _ in 0..3 {
        f.h.step();
    }
    assert!(app(&f.h).prefs_ui.open);
    // Clic en la tecla de GO («Espacio») y pulsar F12.
    f.h.get_by_label("Espacio").click();
    f.h.step();
    f.h.key_press(egui::Key::F12);
    f.h.step();
    f.h.step();
    assert!(app(&f.h).prefs.keys.pressed(crate::prefs::Command::Go, egui::Key::F12, egui::Modifiers::NONE, false));
    // Elegir tecla para Pausa y arrepentirse con Esc: no hay PANIC ni se cierra la ventana.
    f.h.get_by_label("P").click();
    f.h.step();
    f.h.key_press(egui::Key::Escape);
    f.h.step();
    f.h.step();
    assert!(app(&f.h).prefs_ui.open, "Esc cancela la elección, no cierra");
    assert!(!app(&f.h).panic_fired_for_test());
    assert_eq!(app(&f.h).prefs.keys.primary(crate::prefs::Command::Pause), "P");
    // Una tecla ocupada se rechaza con un aviso.
    f.h.get_by_label("P").click();
    f.h.step();
    f.h.key_press(egui::Key::F12);
    f.h.step();
    f.h.step();
    assert!(f.h.query_by_label_contains("ya es la tecla de «GO").is_some());
    f.h.get_by_label("Cerrar").click();
    for _ in 0..3 {
        f.h.step();
    }
    assert!(!app(&f.h).prefs_ui.open);
    // Espacio ya no dispara; F12 sí.
    f.h.key_press(egui::Key::Space);
    for _ in 0..10 {
        f.h.step();
    }
    assert_ne!(app(&f.h).model.state_of(1), CueUiState::Running);
    f.h.key_press(egui::Key::F12);
    pump(&mut f.h, "F12 es el nuevo GO", |a| a.model.state_of(1) == CueUiState::Running);
}

#[test]
fn the_import_audio_window_converts_files_with_ffmpeg_and_adds_the_results_to_the_show_as_cues() {
    if !crate::audio_tools::ffmpeg_available() {
        eprintln!("sin ffmpeg: se omite");
        return;
    }
    let mut f = fixture("tools");
    let wav = crate::waveform::write_test_wav("tools-src", 2, 96_000, 48_000, |i, _| ((i as f32) * 0.03).sin() * 0.4);
    let dest = std::env::temp_dir().join(format!("qlab-ui-tools-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dest);
    // Archivo > Importar audio…
    f.h.get_all_by_label("Archivo").next().unwrap().click();
    f.h.step();
    f.h.step();
    f.h.get_by_label("Importar audio: convertir / copiar de CD…").click();
    for _ in 0..3 {
        f.h.step();
    }
    assert!(app(&f.h).audio_tools.open);
    f.h.get_by_label_contains("Convertir archivos");
    // Como si el usuario eligiera el archivo y la carpeta en los diálogos del sistema.
    f.h.state_mut().as_mut().unwrap().on_dialog_outcome(DialogOutcome { kind: DialogKind::PickConvertFiles, path: Some(wav.clone()), more: Vec::new() });
    f.h.state_mut().as_mut().unwrap().on_dialog_outcome(DialogOutcome { kind: DialogKind::PickToolFolder, path: Some(dest.clone()), more: Vec::new() });
    app_mut(&mut f.h).audio_tools.opts.format = crate::audio_tools::Format::Flac;
    app_mut(&mut f.h).audio_tools.add_as_cues = true;
    for _ in 0..3 {
        f.h.step();
    }
    f.h.get_by_label("Convertir").click();
    pump(&mut f.h, "termina la conversión y se agrega la cue", |a| !a.audio_tools.busy() && a.model.summaries.len() == 4);
    let out: Vec<_> = std::fs::read_dir(&dest).unwrap().map(|e| e.unwrap().path()).collect();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].extension().unwrap(), "flac");
    pump(&mut f.h, "la cue nueva apunta al FLAC", |a| a.model.summaries.iter().any(|c| c.target.ends_with(".flac")));
    assert!(f.h.query_by_label_contains("Listo: 1 archivo(s)").is_some());
    let _ = std::fs::remove_dir_all(&dest);
    let _ = std::fs::remove_file(&wav);
}

#[test]
fn the_cd_tab_says_so_when_there_is_no_drive_instead_of_failing() {
    let mut f = fixture("tools-cd");
    app_mut(&mut f.h).audio_tools.open = true;
    app_mut(&mut f.h).audio_tools.tab = crate::views::audio_tools::Tab::Cd;
    for _ in 0..4 {
        f.h.step();
    }
    assert!(f.h.query_by_label_contains("solo está disponible en Windows").is_some() || f.h.query_by_label_contains("No se encontró ninguna lectora").is_some());
}

#[test]
fn the_diamond_left_by_a_click_on_the_waveform_can_be_dragged_and_its_time_follows() {
    let mut f = fixture("cursordrag");
    with_memory(&mut f, 1.0, "ref");
    app_mut(&mut f.h).inspector.tab = Tab::TimeLoops;
    for _ in 0..5 {
        f.h.step();
    }
    // La bandera de la memoria dice dónde está la regla y qué escala tiene; el clic cae en la onda, debajo y a la izquierda.
    let flag = rect_of(&f.h, "Memoria 1: ref").center();
    let click = egui::pos2(flag.x - 60.0, flag.y + 45.0);
    f.h.hover_at(click);
    f.h.step();
    f.h.drag_at(click);
    f.h.step();
    f.h.drop_at(click);
    for _ in 0..3 {
        f.h.step();
    }
    let flag = f.h.query_by_label("Cursor de edición").map(|n| n.rect().center());
    let c = flag.expect("el clic en la onda deja el rombo del cursor");
    let before = c.x;
    let target = egui::pos2(c.x + 120.0, c.y);
    f.h.hover_at(c);
    f.h.step();
    f.h.drag_at(c);
    f.h.step();
    for k in 1..=4 {
        f.h.hover_at(egui::pos2(c.x + 30.0 * k as f32, c.y + 2.0));
        f.h.step();
    }
    f.h.drop_at(target);
    for _ in 0..3 {
        f.h.step();
    }
    let after = f.h.get_by_label("Cursor de edición").rect().center().x;
    assert!(after > before + 80.0, "el rombo se movió con el arrastre: {before} -> {after}");
}

#[test]
fn changing_the_equalizer_from_the_inspector_while_the_cue_is_playing_is_accepted() {
    use qlab_protocol::cue_dto::{EqBandData, EqFilterType};
    let mut f = fixture_with("eqplaying", true); // la cue 1 queda en bucle infinito: suena hasta que se la pare
    f.h.key_press(egui::Key::Space);
    pump(&mut f.h, "suena la cue 1", |a| a.telemetry.running.get(&1).is_some_and(|i| i.phase.is_some()));
    app_mut(&mut f.h).inspector.tab = Tab::Eq;
    {
        let d = app_mut(&mut f.h).inspector.draft.as_mut().unwrap();
        let eq = &mut d.node.body.audio_mut().unwrap().eq;
        eq.enabled = true;
        eq.bands[0] = EqBandData { filter_type: EqFilterType::LowCut, frequency_hz: 4_000.0, gain_db: 0.0, q_factor: 0.707, enabled: true };
    }
    pump(&mut f.h, "el daemon acepta el EQ con la cue sonando", |a| a.inspector.draft.as_ref().is_some_and(|d| !d.dirty() && d.baseline.body.audio().is_some_and(|b| b.eq.enabled)));
    assert!(!app(&f.h).toasts.iter().any(|t| t.text.contains("No se guardó")), "{:?}", app(&f.h).toasts.iter().map(|t| t.text.clone()).collect::<Vec<_>>());
    // Pasada una vuelta, sigue aceptado y la cue sigue sonando con el EQ puesto.
    for _ in 0..20 {
        f.h.step();
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(app(&f.h).inspector.draft.as_ref().is_some_and(|d| !d.dirty() && d.baseline.body.audio().is_some_and(|b| b.eq.enabled)), "el EQ no se revirtió");
    assert!(app(&f.h).telemetry.running.contains_key(&1));
}

#[test]
fn the_waveform_zoom_buttons_narrow_and_widen_the_view_and_the_navigation_strip_appears_only_when_zoomed() {
    let mut f = fixture("wavezoom");
    app_mut(&mut f.h).inspector.tab = Tab::TimeLoops;
    f.h.set_size(egui::vec2(1366.0, 768.0));
    for _ in 0..6 {
        f.h.step();
    }
    let text_of = |h: &H| -> String { h.get_all_by_label_contains("Vista ").next().map(|n| { let d = format!("{:?}", n.accesskit_node()); let a = d.find("Vista ").unwrap_or(0); d[a..].split(" · ").next().unwrap_or_default().to_string() }).unwrap_or_default() };
    let before = text_of(&f.h);
    assert!(before.contains("Vista"), "{before}");
    f.h.get_by_label("+").click();
    for _ in 0..3 {
        f.h.step();
    }
    let zoomed = text_of(&f.h);
    assert_ne!(before, zoomed, "la vista se acercó");
    // Con zoom aparece «Todo» activo y vuelve a la vista entera.
    f.h.get_by_label("Todo").click();
    for _ in 0..3 {
        f.h.step();
    }
    assert_eq!(text_of(&f.h), before, "Todo vuelve a la vista completa");
}

#[test]
fn dragging_an_equalizer_point_sends_the_change_while_the_mouse_is_still_down_so_it_is_heard_live() {
    let mut f = fixture_with("eqlive", true);
    f.h.set_size(egui::vec2(1366.0, 768.0));
    {
        let d = app_mut(&mut f.h).inspector.draft.as_mut().unwrap();
        d.node.body.audio_mut().unwrap().eq.enabled = true;
    }
    pump(&mut f.h, "EQ activado y guardado", |a| a.inspector.draft.as_ref().is_some_and(|d| !d.dirty() && d.baseline.body.audio().is_some_and(|b| b.eq.enabled)));
    app_mut(&mut f.h).inspector.tab = Tab::Eq;
    for _ in 0..5 {
        f.h.step();
    }
    f.h.key_press(egui::Key::Space);
    pump(&mut f.h, "suena la cue 1", |a| a.telemetry.running.get(&1).is_some_and(|i| i.phase.is_some()));
    let gain_of = |a: &QlabApp| a.inspector.draft.as_ref().and_then(|d| d.baseline.body.audio().map(|b| b.eq.bands[2].gain_db)).unwrap_or(0.0);
    assert_eq!(gain_of(app(&f.h)), 0.0);
    let p = f.h.get_all_by_label_contains("EQ banda 3").next().expect("el punto de la banda 3").rect().center();
    f.h.hover_at(p);
    f.h.step();
    f.h.drag_at(p);
    f.h.step();
    // Se arrastra hacia arriba (más ganancia) SIN soltar: el motor tiene que recibir el cambio igual.
    let t0 = std::time::Instant::now();
    let mut k: f32 = 1.0;
    while gain_of(app(&f.h)) == 0.0 {
        k += 1.0;
        f.h.hover_at(egui::pos2(p.x, p.y - 2.0 * k.min(30.0)));
        f.h.step();
        std::thread::sleep(std::time::Duration::from_millis(15));
        assert!(t0.elapsed() < WAIT, "el cambio no llegó al motor con el mouse apretado");
    }
    assert!(gain_of(app(&f.h)) > 0.0);
    let end = egui::pos2(p.x, p.y - 60.0);
    f.h.drop_at(end);
    for _ in 0..4 {
        f.h.step();
    }
}
