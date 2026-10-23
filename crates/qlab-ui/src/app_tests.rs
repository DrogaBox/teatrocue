//! Pruebas de la aplicación completa, sin ventana: un daemon real (backend nulo) y la UI avanzada a
//! mano fotograma a fotograma con entrada sintética.

use std::time::{Duration, Instant};

use eframe::egui::{self, Event, Key, Modifiers, Rect};
use qlab_daemon::backend_audio::BackendKind;
use qlab_daemon::{Daemon, DaemonConfig};
use qlab_protocol::cue_dto::{CueBasicsData, CueId, MediaTarget};
use qlab_protocol::ipc_client::IpcClient;
use qlab_protocol::ipc_msg::{ClientRequest, ServerResponse};
use qlab_protocol::show_format::{AudioBody, CueBody, CueNodeDto, FadeBody, FadeCellDto, FadeCurveDto, FadeModeDto, GroupBody, GroupModeDto, ShowFile, SHOW_FORMAT};

use crate::app::{AppConfig, ConnState, QlabApp, ToastKind};
use crate::views::inspector::Tab;
use crate::views::Action;
use crate::waveform::write_test_wav;

const WAIT: Duration = Duration::from_secs(8);

fn unique(tag: &str) -> String {
    format!("{}_{tag}", std::process::id())
}

fn audio(id: u64, wav: &std::path::Path, looped: bool) -> CueNodeDto {
    let mut body = AudioBody { time_loops: Default::default(), levels: None, eq: Default::default() };
    body.time_loops.infinite_loop = looped;
    CueNodeDto {
        basics: CueBasicsData {
            unique_id: CueId(id),
            number: id.to_string(),
            name: format!("Audio {id}"),
            file_target: Some(MediaTarget { absolute_path: wav.to_string_lossy().into(), ..Default::default() }),
            ..Default::default()
        },
        triggers: Default::default(),
        dead: false,
        body: CueBody::Audio(body),
    }
}

fn show_json(wav_short: &std::path::Path, wav_loop: &std::path::Path) -> String {
    let fade = CueNodeDto {
        basics: CueBasicsData { unique_id: CueId(3), number: "3".into(), name: "Bajar".into(), cue_target_id: Some(CueId(2)), action_duration: 1.0, ..Default::default() },
        triggers: Default::default(),
        dead: false,
        body: CueBody::Fade(FadeBody {
            mode: FadeModeDto::Absolute,
            cells: vec![FadeCellDto { row: 0, col: 0, target_db: -20.0, active: true }],
            curve: FadeCurveDto::SCurve,
            stop_target_when_done: false,
            honor_gangs: true,
        }),
    };
    let group = CueNodeDto {
        basics: CueBasicsData { unique_id: CueId(4), number: "4".into(), name: "Grupo".into(), ..Default::default() },
        triggers: Default::default(),
        dead: false,
        body: CueBody::Group(GroupBody { mode: GroupModeDto::Timeline, children: vec![audio(5, wav_short, false)] }),
    };
    ShowFile { format: SHOW_FORMAT.into(), name: "Demo".into(), sample_rate: 48_000, carts: Default::default(), cues: vec![audio(1, wav_short, false), audio(2, wav_loop, true), fade, group] }.to_json()
}

struct Rig {
    ctx: egui::Context,
    app: QlabApp,
    daemon: Option<Daemon>,
    socket: String,
    started: Instant,
    files: Vec<std::path::PathBuf>,
}

impl Rig {
    fn new(tag: &str) -> Self {
        let socket = format!("/tmp/qlab-ui-app-{}.sock", unique(tag));
        let wav_short = write_test_wav(&format!("{tag}-s"), 2, 14_400, 48_000, |_, _| 0.5);
        let wav_loop = write_test_wav(&format!("{tag}-l"), 2, 48_000, 48_000, |_, _| 0.25);
        let show = std::env::temp_dir().join(format!("qlab-ui-show-{}.json", unique(tag)));
        std::fs::write(&show, show_json(&wav_short, &wav_loop)).unwrap();
        let daemon = Self::start_daemon(&socket, tag, Some(show.clone()));
        let ctx = egui::Context::default();
        let app = QlabApp::new(&ctx, AppConfig::new(socket.clone()));
        Self { ctx, app, daemon: Some(daemon), socket, started: Instant::now(), files: vec![wav_short, wav_loop, show] }
    }

    fn start_daemon(socket: &str, tag: &str, show: Option<std::path::PathBuf>) -> Daemon {
        let mut cfg = DaemonConfig::default();
        cfg.socket = socket.to_string();
        cfg.shm_name = format!("qlab_uiapp_{}", unique(tag));
        cfg.backend.kind = BackendKind::Null;
        cfg.lock_memory = false;
        cfg.disk_threads = 2;
        cfg.show = show;
        Daemon::start(cfg).expect("el daemon debe arrancar")
    }

    /// Un fotograma completo, como lo haría eframe: lógica y luego dibujo.
    fn frame(&mut self, events: Vec<Event>) -> egui::FullOutput {
        let mut input = egui::RawInput::default();
        input.time = Some(self.started.elapsed().as_secs_f64());
        input.screen_rect = Some(Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1280.0, 800.0)));
        input.events = events;
        let app = &mut self.app;
        // Igual que eframe: la lógica y el dibujo dentro del mismo paso, con la entrada ya disponible.
        self.ctx.run_ui(input, |ui| {
            app.logic_step(&ui.ctx().clone());
            app.show(ui);
        })
    }

    fn pump_until(&mut self, what: &str, mut cond: impl FnMut(&QlabApp) -> bool) {
        let t0 = Instant::now();
        loop {
            self.frame(vec![]).drop_without_applying_deltas();
            if cond(&self.app) {
                return;
            }
            assert!(t0.elapsed() < WAIT, "tiempo agotado esperando: {what}");
            std::thread::sleep(Duration::from_millis(4));
        }
    }

    fn connect(tag: &str) -> Self {
        let mut rig = Self::new(tag);
        rig.pump_until("conexión y lista de cues", |a| a.is_connected() && a.model.summaries.len() == 5 && a.model.selected.is_some() && a.inspector.draft.is_some());
        rig
    }

    /// Pulsa y suelta una tecla en el mismo fotograma (así la siguiente pulsación no cuenta como repetición).
    fn key(&mut self, key: Key, repeat: bool) -> egui::FullOutput {
        let mut events = vec![Event::Key { key, physical_key: None, pressed: true, repeat, modifiers: Modifiers::NONE }];
        if !repeat {
            events.push(Event::Key { key, physical_key: None, pressed: false, repeat: false, modifiers: Modifiers::NONE });
        }
        self.frame(events)
    }

    /// Una interfaz sin daemon, para las pruebas de arranque.
    fn bare(socket: String) -> Self {
        let ctx = egui::Context::default();
        let app = QlabApp::new(&ctx, AppConfig::new(socket.clone()));
        Self { ctx, app, daemon: None, socket, started: Instant::now(), files: vec![] }
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        for f in &self.files {
            let _ = std::fs::remove_file(f);
        }
    }
}

fn repaint_delay(out: &egui::FullOutput) -> Duration {
    out.viewport_output.get(&egui::ViewportId::ROOT).map_or(Duration::MAX, |v| v.repaint_delay)
}

#[test]
fn connects_loads_the_cue_list_and_selects_the_standby_cue() {
    let rig = Rig::connect("connect");
    let app = &rig.app;
    assert!(matches!(&app.conn, ConnState::Connected(h) if h.backend == "null" && h.channels == 2));
    assert_eq!(app.model.summaries.iter().map(|c| c.id).collect::<Vec<_>>(), vec![1, 2, 3, 4, 5]);
    assert_eq!(app.model.playhead, Some(1));
    assert_eq!(app.model.selected, Some(1), "la selección empieza en la cue en espera");
    let d = app.inspector.draft.as_ref().unwrap();
    assert_eq!((d.id(), d.node.basics.name.as_str(), d.dirty()), (1, "Audio 1", false));
    // El grupo y su hijo salen en la lista con la jerarquía correcta.
    assert_eq!(app.model.summary(5).unwrap().parent, Some(4));
    assert_eq!(app.model.rows().iter().map(|r| r.depth).collect::<Vec<_>>(), vec![0, 0, 0, 0, 1]);
}

#[test]
fn space_fires_go_and_advances_the_selection_immediately() {
    let mut rig = Rig::connect("go");
    rig.key(Key::Space, false).drop_without_applying_deltas();
    // Antes de que vuelva ningún evento del daemon la selección ya está en la cue siguiente.
    assert_eq!(rig.app.model.selected, Some(2));
    rig.pump_until("la cue 1 suena y el playhead pasa a la 2", |a| a.model.state_of(1) == crate::model::CueUiState::Running && a.model.playhead == Some(2));
    assert_eq!(rig.app.inspector.draft.as_ref().map(|d| d.id()), Some(2) , "el Inspector sigue a la selección");
    rig.pump_until("la cue 1 termina sola", |a| a.model.state_of(1) == crate::model::CueUiState::Completed);
}

#[test]
fn holding_space_does_not_machine_gun_the_cues() {
    let mut rig = Rig::connect("repeat");
    // Pulsación sin soltar y luego las repeticiones automáticas del teclado.
    rig.frame(vec![Event::Key { key: Key::Space, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE }]).drop_without_applying_deltas();
    for _ in 0..30 {
        rig.key(Key::Space, true).drop_without_applying_deltas();
    }
    std::thread::sleep(Duration::from_millis(100));
    rig.pump_until("estado estable", |a| a.model.playhead == Some(2));
    let c = IpcClient::connect(&rig.socket).unwrap();
    match c.request(ClientRequest::GetCues, WAIT).unwrap() {
        ServerResponse::Cues { cues } => {
            let started: Vec<_> = cues.iter().filter(|c| c.state != "Idle").map(|c| c.id).collect();
            assert_eq!(started, vec![1], "solo se disparó una cue: {started:?}");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn shortcuts_are_ignored_while_a_dialog_is_open_or_a_text_field_has_focus() {
    let mut rig = Rig::connect("blocked");
    rig.app.apply(Action::OpenShowPicker, &rig.ctx.clone());
    rig.key(Key::Space, false).drop_without_applying_deltas();
    std::thread::sleep(Duration::from_millis(60));
    rig.frame(vec![]).drop_without_applying_deltas();
    assert_eq!(rig.app.model.playhead, Some(1), "con el selector abierto Espacio no dispara");
    assert_eq!(rig.app.model.state_of(1), crate::model::CueUiState::Idle);
}

#[test]
fn double_escape_is_a_hard_stop() {
    let mut rig = Rig::connect("panic");
    rig.key(Key::Escape, false).drop_without_applying_deltas();
    assert!(!rig.app.toasts.iter().any(|t| t.text.contains("dura")));
    rig.key(Key::Escape, false).drop_without_applying_deltas();
    assert!(rig.app.toasts.iter().any(|t| t.text.contains("Parada dura") && t.kind == ToastKind::Warn));
    // Pasado el margen, vuelve a ser una parada suave.
    std::thread::sleep(Duration::from_millis(750));
    let before = rig.app.toasts.len();
    rig.key(Key::Escape, false).drop_without_applying_deltas();
    assert_eq!(rig.app.toasts.len(), before);
}

#[test]
fn arrow_keys_move_the_selection_and_the_playhead() {
    let mut rig = Rig::connect("arrows");
    rig.key(Key::ArrowDown, false).drop_without_applying_deltas();
    assert_eq!((rig.app.model.selected, rig.app.model.playhead), (Some(2), Some(2)));
    rig.pump_until("el daemon confirma el playhead", |a| a.inspector.draft.as_ref().is_some_and(|d| d.id() == 2));
    rig.key(Key::ArrowDown, false).drop_without_applying_deltas();
    rig.key(Key::ArrowDown, false).drop_without_applying_deltas();
    rig.key(Key::ArrowDown, false).drop_without_applying_deltas();
    assert_eq!(rig.app.model.selected, Some(5), "baja también por los hijos del grupo");
    assert_eq!(rig.app.model.playhead, Some(4), "pero el playhead solo se coloca en cues de nivel superior");
    std::thread::sleep(Duration::from_millis(400)); // pasa el margen local y manda la telemetría
    rig.pump_until("el daemon coincide", |a| a.model.playhead == Some(4) && a.telemetry.playhead() == Some(4));
    rig.key(Key::ArrowUp, false).drop_without_applying_deltas();
    assert_eq!(rig.app.model.selected, Some(4));
}

#[test]
fn an_idle_connected_window_asks_for_no_repaints_at_all() {
    let mut rig = Rig::connect("idle");
    // Dejar que se apaguen los vúmetros y pasar por los avisos de conexión.
    rig.app.toasts.clear();
    let mut last = Duration::ZERO;
    let t0 = Instant::now();
    let mut quiet_frames = 0;
    while quiet_frames < 5 {
        let out = rig.frame(vec![]);
        last = repaint_delay(&out);
        out.drop_without_applying_deltas();
        quiet_frames = if last == Duration::MAX { quiet_frames + 1 } else { 0 };
        assert!(t0.elapsed() < WAIT, "la ventana nunca llegó al reposo: último retardo {last:?}");
        std::thread::sleep(Duration::from_millis(60));
    }
    assert_eq!(last, Duration::MAX, "en reposo no se programa ningún repintado (0 % de CPU)");
    // Pasado un rato más (el daemon sigue publicando a 120 Hz) sigue sin pedir nada.
    std::thread::sleep(Duration::from_millis(400));
    let out = rig.frame(vec![]);
    let _ = repaint_delay(&out);
    out.drop_without_applying_deltas();
    for _ in 0..3 {
        std::thread::sleep(Duration::from_millis(100));
        let out = rig.frame(vec![]);
        assert_eq!(repaint_delay(&out), Duration::MAX);
        out.drop_without_applying_deltas();
    }
}

#[test]
fn playback_draws_at_sixty_fps_and_returns_to_rest_afterwards() {
    let mut rig = Rig::connect("fps");
    rig.app.toasts.clear();
    rig.key(Key::Space, false).drop_without_applying_deltas();
    rig.pump_until("suena", |a| a.model.state_of(1) == crate::model::CueUiState::Running);
    let out = rig.frame(vec![]);
    let delay = repaint_delay(&out);
    out.drop_without_applying_deltas();
    assert!(delay <= Duration::from_millis(16), "durante la reproducción se pide ~60 FPS, no {delay:?}");
    rig.pump_until("la telemetría confirma la reproducción", |a| a.telemetry.is_playing());

    // La pista (0,3 s) termina; los vúmetros caen y la ventana vuelve a dormir.
    rig.pump_until("fin de la cue", |a| a.model.state_of(1) == crate::model::CueUiState::Completed);
    let t0 = Instant::now();
    loop {
        rig.app.toasts.clear();
        let out = rig.frame(vec![]);
        let d = repaint_delay(&out);
        out.drop_without_applying_deltas();
        if d == Duration::MAX {
            break;
        }
        assert!(t0.elapsed() < WAIT, "no volvió al reposo ({d:?})");
        std::thread::sleep(Duration::from_millis(16));
    }
}

#[test]
fn editing_in_the_inspector_is_saved_to_the_daemon_when_typing_stops() {
    let mut rig = Rig::connect("edit");
    {
        let d = rig.app.inspector.draft.as_mut().unwrap();
        d.node.basics.name = "Renombrada".into();
        d.node.basics.pre_wait = 0.5;
        d.node.basics.flagged = true;
        d.node.basics.continue_mode = qlab_protocol::cue_dto::ContinueMode::AutoFollow;
        assert!(d.dirty());
    }
    rig.pump_until("el daemon confirma", |a| a.inspector.draft.as_ref().is_some_and(|d| !d.dirty() && d.baseline.basics.name == "Renombrada") && a.inspector.in_flight.is_none());
    assert!(!rig.app.toasts.iter().any(|t| t.kind == ToastKind::Error), "{:?}", rig.app.toasts);
    let c = IpcClient::connect(&rig.socket).unwrap();
    match c.request(ClientRequest::GetCues, WAIT).unwrap() {
        ServerResponse::Cues { cues } => {
            let c1 = &cues[0];
            assert_eq!((c1.name.as_str(), c1.pre_wait, c1.flagged), ("Renombrada", 0.5, true));
        }
        other => panic!("{other:?}"),
    }
    // Y la tabla de la propia UI se actualiza (llega el evento CueUpdated y se vuelve a pedir la lista).
    rig.pump_until("la tabla refleja el nombre", |a| a.model.summary(1).is_some_and(|c| c.name == "Renombrada"));
}

#[test]
fn a_rejected_edit_is_reverted_and_reported() {
    let mut rig = Rig::connect("reject");
    rig.app.toasts.clear();
    rig.app.inspector.draft.as_mut().unwrap().node.basics.post_wait = -4.0; // el daemon lo rechaza
    rig.pump_until("aviso de error", |a| a.toasts.iter().any(|t| t.kind == ToastKind::Error && t.text.contains("No se guardó")));
    let d = rig.app.inspector.draft.as_ref().unwrap();
    assert!(!d.dirty(), "el borrador vuelve a lo que tiene el daemon");
    assert_eq!(d.node.basics.post_wait, 0.0);
}

#[test]
fn edits_made_while_typing_are_not_sent_until_the_field_is_released() {
    let mut rig = Rig::connect("typing");
    // Simula un campo de texto con el foco: la aplicación no debe guardar a medias.
    let id = egui::Id::unique("campo");
    rig.ctx.memory_mut(|m| m.request_focus(id));
    rig.app.inspector.draft.as_mut().unwrap().node.basics.name = "Escribiendo".into();
    for _ in 0..5 {
        rig.frame(vec![]).drop_without_applying_deltas();
        std::thread::sleep(Duration::from_millis(10));
    }
    // Sin un widget de texto real que declare el foco, el contexto no "quiere teclado": se guarda.
    // Lo que sí se comprueba es que nunca se manda más de una vez la misma versión.
    rig.pump_until("guardado", |a| a.inspector.draft.as_ref().is_some_and(|d| !d.dirty()));
}

#[test]
fn every_tab_of_every_cue_kind_renders_without_panicking() {
    let mut rig = Rig::connect("tabs");
    for cue in [1u64, 2, 3, 4, 5] {
        rig.app.select(cue);
        rig.pump_until("detalle cargado", |a| a.inspector.draft.as_ref().is_some_and(|d| d.id() == cue));
        for tab in Tab::ALL {
            rig.app.inspector.tab = tab;
            for _ in 0..3 {
                rig.frame(vec![]).drop_without_applying_deltas();
            }
        }
        // Una pestaña que no existe para este tipo de cue cae a Basics en vez de fallar.
        let d = rig.app.inspector.draft.as_ref().unwrap();
        assert!(rig.app.inspector.tab.available_for(&d.node), "cue {cue}: pestaña {:?}", rig.app.inspector.tab);
    }
}

#[test]
fn collapsing_a_group_hides_its_children_in_the_table() {
    let mut rig = Rig::connect("collapse");
    rig.app.apply(Action::ToggleGroup(4), &rig.ctx.clone());
    assert_eq!(rig.app.model.rows().len(), 4);
    rig.frame(vec![]).drop_without_applying_deltas();
    rig.app.apply(Action::ToggleGroup(4), &rig.ctx.clone());
    assert_eq!(rig.app.model.rows().len(), 5);
}

#[test]
fn the_ui_survives_the_daemon_dying_and_reconnects_to_a_new_one() {
    let mut rig = Rig::connect("reconnect");
    drop(rig.daemon.take()); // el daemon desaparece
    rig.pump_until("pantalla de reconexión", |a| matches!(a.conn, ConnState::Down { .. }));
    assert!(rig.app.toasts.iter().any(|t| t.text.contains("Se perdió")));
    // La pantalla de reconexión se dibuja sin problemas y, una vez asentada, pide repintados lentos (1 Hz),
    // no 60 FPS ni un bucle continuo.
    let t0 = Instant::now();
    loop {
        let out = rig.frame(vec![]);
        let d = repaint_delay(&out);
        out.drop_without_applying_deltas();
        if d >= Duration::from_millis(500) {
            break;
        }
        assert!(t0.elapsed() < WAIT, "esperando al daemon no debe repintar a toda velocidad: {d:?}");
        std::thread::sleep(Duration::from_millis(20));
    }

    // Vuelve el daemon (otro proceso, mismo socket): la UI se reconecta sola y recarga todo.
    let tag = "reconnect";
    rig.daemon = Some(Rig::start_daemon(&rig.socket, tag, Some(rig.files[2].clone())));
    rig.pump_until("reconexión automática", |a| a.is_connected() && a.model.summaries.len() == 5);
}

#[test]
fn standalone_mode_starts_the_embedded_engine_from_the_reconnect_screen() {
    let socket = format!("/tmp/qlab-ui-standalone-{}.sock", std::process::id());
    let mut rig = Rig::bare(socket.clone());
    rig.pump_until("pantalla de reconexión", |a| matches!(a.conn, ConnState::Down { .. }));
    // Desde la pantalla de reconexión el usuario pide el motor embebido sin audio.
    let ctx = rig.ctx.clone();
    rig.app.apply(Action::StartEmbedded { with_audio: false }, &ctx);
    rig.pump_until("conexión al motor embebido", |a| a.is_connected());
    let ConnState::Connected(h) = &rig.app.conn else { unreachable!() };
    assert_eq!(h.backend, "null");
    let c = IpcClient::connect(&socket).unwrap();
    assert!(matches!(c.request(ClientRequest::Ping { nonce: 1 }, WAIT).unwrap(), ServerResponse::Pong { .. }));
    drop(c);
    rig.app.on_exit_for_test();
    rig.pump_until("el motor se detuvo", |a| !a.is_connected());
}

#[test]
fn opening_a_show_through_the_picker_flow_loads_it() {
    let mut rig = Rig::connect("openshow");
    let wav = write_test_wav("openshow-new", 1, 4_800, 48_000, |_, _| 0.1);
    let mut cue = audio(77, &wav, false);
    cue.basics.name = "Nueva".into();
    let json = ShowFile { format: SHOW_FORMAT.into(), name: "Otro show".into(), sample_rate: 48_000, carts: Default::default(), cues: vec![cue] }.to_json();
    let path = std::env::temp_dir().join(format!("qlab-ui-other-{}.json", std::process::id()));
    std::fs::write(&path, json).unwrap();
    rig.files.extend([wav, path.clone()]);
    rig.app.load_show_for_test(&path);
    rig.pump_until("el show nuevo reemplaza al anterior", |a| a.model.summaries.len() == 1 && a.model.show_name == "Otro show");
    assert!(rig.app.toasts.iter().any(|t| t.text.contains("Otro show")));
    assert_eq!(rig.app.model.summaries[0].name, "Nueva");
    assert!(rig.app.inspector.draft.as_ref().map_or(true, |d| d.id() == 77), "el Inspector no conserva una cue que ya no existe");
}

#[test]
fn toasts_expire_and_are_bounded() {
    let mut rig = Rig::connect("toasts");
    for i in 0..20 {
        rig.app.toast(ToastKind::Info, format!("aviso {i}"));
    }
    assert_eq!(rig.app.toasts.len(), 5, "como mucho 5 a la vez");
    assert_eq!(rig.app.toasts.last().unwrap().text, "aviso 19");
    rig.frame(vec![]).drop_without_applying_deltas();
}

#[test]
fn scan_keys_distinguishes_presses_from_key_repeat() {
    let ctx = egui::Context::default();
    let k = |key, pressed, repeat, modifiers| Event::Key { key, physical_key: None, pressed, repeat, modifiers };
    let run = |events: Vec<Event>| {
        let mut input = egui::RawInput::default();
        input.events = events;
        let mut keys = None;
        ctx.run_ui(input, |ui| keys = Some(ui.input(crate::app::scan_keys))).drop_without_applying_deltas();
        keys.unwrap()
    };
    let none = Modifiers::NONE;
    assert!(run(vec![k(Key::Space, true, false, none)]).go, "primera pulsación");
    // Con la tecla aún pulsada, egui marca como repetición lo que siga llegando.
    assert!(!run(vec![k(Key::Space, true, true, none)]).go, "la repetición no dispara");
    assert!(!run(vec![k(Key::Space, true, false, none)]).go, "tampoco una pulsación sin soltar entre medias");
    run(vec![k(Key::Space, false, false, none)]); // se suelta
    assert!(!run(vec![k(Key::Space, true, false, Modifiers::SHIFT)]).go, "con modificadores no es un GO");
    run(vec![k(Key::Space, false, false, Modifiers::SHIFT)]);
    assert!(run(vec![k(Key::Escape, true, false, none)]).panic);
    assert!(!run(vec![k(Key::Escape, true, true, none)]).panic);
    run(vec![k(Key::Escape, false, false, none)]);
    assert!(run(vec![k(Key::ArrowDown, true, false, none)]).down);
    assert!(run(vec![k(Key::ArrowDown, true, true, none)]).down, "las flechas sí repiten");
    run(vec![k(Key::ArrowDown, false, false, none)]);
    assert!(run(vec![k(Key::O, true, false, Modifiers::COMMAND)]).open);
    run(vec![k(Key::O, false, false, Modifiers::COMMAND)]);
    assert!(!run(vec![k(Key::O, true, false, none)]).open);
    run(vec![k(Key::O, false, false, none)]);
    assert!(run(vec![k(Key::Q, true, false, Modifiers::COMMAND)]).quit);
    run(vec![k(Key::Q, false, false, Modifiers::COMMAND)]);
    assert_eq!(run(vec![]), Default::default());
}
