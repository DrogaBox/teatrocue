//! Pruebas de integración del daemon completo con el backend nulo: IPC, memoria compartida,
//! secuenciador, motor y watchdog funcionando juntos.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use qlab_protocol::cue_dto::{CueBasicsData, CueId, MediaTarget};
use qlab_protocol::ipc_client::IpcClient;
use qlab_protocol::ipc_msg::{ClientRequest, ServerEvent, ServerResponse, IPC_PROTOCOL_VERSION};
use qlab_protocol::show_format::{AudioBody, CueBody, CueNodeDto, ShowFile, SHOW_FORMAT};

use crate::backend_audio::BackendKind;
use crate::config::DaemonConfig;
use crate::ipc_server::RingProducer;
use crate::shm::SharedMemory;
use crate::test_support::{temp_path, write_wav, Cleanup};
use crate::Daemon;

const TIMEOUT: Duration = Duration::from_secs(3);

fn socket_for(tag: &str) -> String {
    if cfg!(windows) {
        format!(r"\\.\pipe\qlab_test_{}_{tag}", std::process::id())
    } else {
        temp_path(tag, "sock").to_string_lossy().into_owned()
    }
}

fn config(tag: &str) -> DaemonConfig {
    let mut cfg = DaemonConfig::default();
    cfg.socket = socket_for(tag);
    cfg.shm_name = format!("qlab_test_{}_{tag}", std::process::id());
    cfg.backend.kind = BackendKind::Null;
    cfg.lock_memory = false;
    cfg.disk_threads = 2;
    cfg
}

fn audio_node(id: u64, wav: Option<&std::path::Path>, looped: bool) -> CueNodeDto {
    let mut body = AudioBody { time_loops: Default::default(), levels: None, eq: Default::default() };
    body.time_loops.infinite_loop = looped;
    CueNodeDto {
        triggers: Default::default(),
        basics: CueBasicsData {
            unique_id: CueId(id),
            number: id.to_string(),
            name: format!("cue {id}"),
            file_target: wav.map(|p| MediaTarget { absolute_path: p.to_string_lossy().into(), ..Default::default() }),
            ..Default::default()
        },
        dead: false,
        body: CueBody::Audio(body),
    }
}

fn write_show(tag: &str, cues: Vec<CueNodeDto>) -> std::path::PathBuf {
    let show = ShowFile { format: SHOW_FORMAT.into(), name: format!("show {tag}"), sample_rate: 48_000, carts: Default::default(), cues };
    let path = temp_path(tag, "json");
    std::fs::write(&path, show.to_json()).unwrap();
    path
}

fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while !cond() {
        assert!(t0.elapsed() < TIMEOUT, "tiempo agotado esperando: {what}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn wait_until_for(what: &str, limit: Duration, mut cond: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while !cond() {
        assert!(t0.elapsed() < limit, "tiempo agotado esperando: {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn req(c: &IpcClient, r: ClientRequest) -> ServerResponse {
    c.request(r, TIMEOUT).expect("sin respuesta del daemon")
}

/// Pico más alto visto en `samples` lecturas consumidoras del medidor `ch`.
fn peak(shm: &SharedMemory, ch: usize, samples: usize) -> f32 {
    (0..samples)
        .map(|_| {
            std::thread::sleep(Duration::from_millis(5));
            shm.telemetry().meters.consume_output(ch)
        })
        .fold(0.0, f32::max)
}

struct Fixture {
    daemon: Daemon,
    socket: String,
    _cleanup: Cleanup,
}

fn fixture(tag: &str, ring: RingProducer, cues: impl FnOnce(&mut Vec<std::path::PathBuf>) -> Vec<CueNodeDto>) -> Fixture {
    let mut files = Vec::new();
    let nodes = cues(&mut files);
    let show = write_show(tag, nodes);
    files.push(show.clone());
    let mut cfg = config(tag);
    cfg.ring_producer = ring;
    cfg.show = Some(show);
    let socket = cfg.socket.clone();
    Fixture { daemon: Daemon::start(cfg).expect("el daemon debe arrancar"), socket, _cleanup: Cleanup(files) }
}

fn simple(tag: &str, ring: RingProducer) -> Fixture {
    fixture(tag, ring, |files| {
        let a = write_wav(&format!("{tag}-a"), 2, 14_400, 48_000, |_, _| 0.5);
        let b = write_wav(&format!("{tag}-b"), 2, 48_000, 48_000, |_, _| 0.25);
        files.extend([a.clone(), b.clone()]);
        vec![audio_node(1, Some(&a), false), audio_node(2, Some(&b), true)]
    })
}

#[test]
fn hello_ping_and_status() {
    let f = simple("hello", RingProducer::IpcServer);
    let c = IpcClient::connect(&f.socket).unwrap();
    match req(&c, ClientRequest::Hello { client: "test".into(), protocol: IPC_PROTOCOL_VERSION }) {
        ServerResponse::Hello { server_protocol, shm_name, sample_rate, channels, backend } => {
            assert_eq!((server_protocol, sample_rate, channels, backend.as_str()), (IPC_PROTOCOL_VERSION, 48_000, 2, "null"));
            assert_eq!(shm_name, f.daemon.shm().name());
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(req(&c, ClientRequest::Ping { nonce: 99 }), ServerResponse::Pong { nonce: 99 });
    assert!(matches!(req(&c, ClientRequest::Hello { client: "futuro".into(), protocol: 999 }), ServerResponse::Error { .. }));
    match req(&c, ClientRequest::GetStatus) {
        ServerResponse::Status(s) => {
            assert_eq!(s.playhead, Some(1));
            assert!(s.running.is_empty() && !s.audio_stalled && !s.safe_mute);
            assert!(s.sequencer_ticks > 0);
            assert_eq!(s.rt_alloc_violations, 0);
        }
        other => panic!("{other:?}"),
    }
    match req(&c, ClientRequest::GetCues) {
        ServerResponse::Cues { cues } => {
            assert_eq!(cues.iter().map(|c| (c.id, c.kind.as_str(), c.state.as_str(), c.armed)).collect::<Vec<_>>(), vec![(1, "audio", "Idle", true), (2, "audio", "Idle", true)]);
            assert_eq!(cues[0].name, "cue 1");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn malformed_input_gets_an_error_and_the_connection_survives() {
    let f = simple("garbage", RingProducer::IpcServer);
    let c = IpcClient::connect(&f.socket).unwrap();
    c.send_raw("esto no es json").unwrap();
    c.send_raw(r#"{"id":5,"cmd":"inventado"}"#).unwrap();
    c.send_raw("").unwrap();
    c.send_raw(r#"{"id":6,"cmd":"fader_drag","cue_id":1,"row":200,"col":1,"db":0.0}"#).unwrap();
    assert_eq!(req(&c, ClientRequest::Ping { nonce: 1 }), ServerResponse::Pong { nonce: 1 }, "sigue atendiendo");
    assert!(matches!(req(&c, ClientRequest::FaderDrag { cue_id: 1, row: 99, col: 1, db: 0.0 }), ServerResponse::Error { .. }));
    assert!(matches!(req(&c, ClientRequest::FaderDrag { cue_id: 1, row: 1, col: 1, db: f32::NAN }), ServerResponse::Error { .. }));
    // Línea gigante: se corta la conexión con un error, el daemon sigue vivo.
    let big = IpcClient::connect(&f.socket).unwrap();
    let _ = big.send_raw(&"x".repeat(2 << 20));
    let again = IpcClient::connect(&f.socket).unwrap();
    assert_eq!(req(&again, ClientRequest::Ping { nonce: 2 }), ServerResponse::Pong { nonce: 2 });
}

#[test]
fn go_over_ipc_plays_audio_and_is_visible_in_shared_telemetry() {
    let f = simple("go", RingProducer::IpcServer);
    let c = IpcClient::connect(&f.socket).unwrap();
    assert_eq!(req(&c, ClientRequest::Subscribe { events: true }), ServerResponse::Ok);
    let ui = SharedMemory::open(f.daemon.shm().name()).expect("la UI mapea el mismo segmento");

    assert_eq!(req(&c, ClientRequest::Go), ServerResponse::Ok);
    c.wait_event(TIMEOUT, |e| matches!(e, ServerEvent::PlayheadMoved { cue: Some(2) })).expect("el playhead avanzó");
    c.wait_event(TIMEOUT, |e| matches!(e, ServerEvent::CueStateChanged { cue: 1, state } if state == "Running")).expect("la cue 1 corre");

    // El audio sale: pico 0,5 en ambos canales, medidos desde el segmento compartido.
    wait_until("el medidor refleja la pista", || peak(&ui, 0, 1) > 0.49);
    assert!((peak(&ui, 1, 3) - 0.5).abs() < 1e-3);

    // La UI lee playhead y progreso por el seqlock, sin pasar por el socket.
    wait_until("telemetría con la cue en marcha", || {
        ui.telemetry().status.snapshot().is_some_and(|s| s.is_playing && s.running_cues[0].cue_id == 1 && s.active_playhead_cue_id == 2)
    });

    // La pista dura 0,3 s: termina sola y el audio queda en silencio.
    c.wait_event(TIMEOUT, |e| matches!(e, ServerEvent::CueStateChanged { cue: 1, state } if state == "Completed")).expect("terminó");
    wait_until("silencio", || peak(&ui, 0, 4) == 0.0);
    assert_eq!(f.daemon.health().rt_alloc_violations.load(Ordering::Relaxed), 0, "el callback no debe asignar memoria");
    assert_eq!(f.daemon.health().xruns.load(Ordering::Relaxed), 0);
}

#[test]
fn the_ui_can_write_the_hot_ring_directly() {
    let f = simple("ring", RingProducer::ExternalUi);
    let c = IpcClient::connect(&f.socket).unwrap();
    req(&c, ClientRequest::Subscribe { events: true });
    let ui = SharedMemory::open(f.daemon.shm().name()).unwrap();
    // La UI es la única productora del anillo: GO sin pasar por el socket.
    ui.telemetry().hot_ring.push(qlab_protocol::ipc_shm::HotControlEvent::Go).unwrap();
    c.wait_event(TIMEOUT, |e| matches!(e, ServerEvent::CueStateChanged { cue: 1, state } if state == "Running")).expect("GO por el anillo");
    wait_until("audio", || peak(&ui, 0, 1) > 0.49);
    // Los gestos del socket siguen funcionando, por el canal del secuenciador.
    assert_eq!(req(&c, ClientRequest::Go), ServerResponse::Ok);
    c.wait_event(TIMEOUT, |e| matches!(e, ServerEvent::CueStateChanged { cue: 2, state } if state == "Running")).expect("GO por el socket");
    assert!(ui.telemetry().hot_ring.is_empty());
}

#[test]
fn live_fader_and_panic() {
    let f = fixture("fader", RingProducer::IpcServer, |files| {
        let b = write_wav("fader-b", 2, 48_000, 48_000, |_, _| 0.25);
        files.push(b.clone());
        vec![audio_node(1, Some(&b), true)]
    });
    let c = IpcClient::connect(&f.socket).unwrap();
    req(&c, ClientRequest::Subscribe { events: true });
    let ui = SharedMemory::open(f.daemon.shm().name()).unwrap();
    req(&c, ClientRequest::Go);
    wait_until("suena en bucle", || peak(&ui, 0, 1) > 0.24 && peak(&ui, 1, 1) > 0.24);

    // Silenciar el crosspoint 1→1 en vivo: la salida izquierda se apaga con rampa, la derecha sigue.
    assert_eq!(req(&c, ClientRequest::FaderDrag { cue_id: 1, row: 1, col: 1, db: -120.0 }), ServerResponse::Ok);
    wait_until("izquierda en silencio", || peak(&ui, 0, 6) == 0.0);
    assert!(peak(&ui, 1, 3) > 0.24, "la derecha no se toca");
    req(&c, ClientRequest::FaderDrag { cue_id: 1, row: 1, col: 1, db: 0.0 });
    wait_until("izquierda de vuelta", || peak(&ui, 0, 1) > 0.24);

    // Panic: fade suave y la cue termina cuando el motor confirma el fin de la voz.
    assert_eq!(req(&c, ClientRequest::Panic), ServerResponse::Ok);
    wait_until("el audio decae", || peak(&ui, 0, 2) < 0.1);
    c.wait_event(Duration::from_secs(4), |e| matches!(e, ServerEvent::CueStateChanged { cue: 1, state } if state == "Completed")).expect("fin tras el panic");
    wait_until("silencio total", || peak(&ui, 0, 4) == 0.0 && peak(&ui, 1, 4) == 0.0);
}

#[test]
fn killing_a_client_does_not_disturb_the_audio_or_the_clock() {
    let f = fixture("kill", RingProducer::IpcServer, |files| {
        let b = write_wav("kill-b", 2, 48_000, 48_000, |_, _| 0.25);
        files.push(b.clone());
        vec![audio_node(1, Some(&b), true)]
    });
    let ui = SharedMemory::open(f.daemon.shm().name()).unwrap();
    {
        let victim = IpcClient::connect(&f.socket).unwrap();
        req(&victim, ClientRequest::Subscribe { events: true });
        req(&victim, ClientRequest::Go);
        wait_until("suena", || peak(&ui, 0, 1) > 0.24);
        // El cliente desaparece de golpe, sin despedirse (equivalente a `kill -9`).
    }
    let ticks = f.daemon.health().seq_ticks.load(Ordering::Relaxed);
    let callbacks = f.daemon.health().audio_callbacks.load(Ordering::Relaxed);
    std::thread::sleep(Duration::from_millis(400));
    assert!(peak(&ui, 0, 10) > 0.24, "el audio sigue sonando");
    assert!(peak(&ui, 1, 10) > 0.24);
    assert!(f.daemon.health().seq_ticks.load(Ordering::Relaxed) > ticks + 300, "el reloj del show sigue");
    assert!(f.daemon.health().audio_callbacks.load(Ordering::Relaxed) > callbacks + 100);
    assert!(!f.daemon.health().audio_stalled.load(Ordering::Relaxed));
    // Y otro cliente puede reconectarse y ver el estado en vivo.
    let again = IpcClient::connect(&f.socket).unwrap();
    match req(&again, ClientRequest::GetStatus) {
        ServerResponse::Status(s) => assert_eq!(s.running.iter().map(|r| r.id).collect::<Vec<_>>(), vec![1]),
        other => panic!("{other:?}"),
    }
}

#[test]
fn several_clients_share_events_and_commands() {
    let f = simple("multi", RingProducer::IpcServer);
    let clients: Vec<_> = (0..4).map(|_| IpcClient::connect(&f.socket).unwrap()).collect();
    for c in &clients {
        req(c, ClientRequest::Subscribe { events: true });
    }
    assert_eq!(req(&clients[0], ClientRequest::Go), ServerResponse::Ok);
    for (i, c) in clients.iter().enumerate() {
        c.wait_event(TIMEOUT, |e| matches!(e, ServerEvent::PlayheadMoved { cue: Some(2) })).unwrap_or_else(|| panic!("el cliente {i} no recibió el evento"));
    }
    // Peticiones simultáneas desde hilos distintos: todas reciben su propia respuesta.
    std::thread::scope(|s| {
        for (i, c) in clients.iter().enumerate() {
            s.spawn(move || {
                for n in 0..50u64 {
                    let nonce = i as u64 * 1_000 + n;
                    assert_eq!(c.request(ClientRequest::Ping { nonce }, TIMEOUT).unwrap(), ServerResponse::Pong { nonce });
                }
            });
        }
    });
    // Dejar de suscribirse corta el flujo de eventos pero no las respuestas.
    req(&clients[1], ClientRequest::Subscribe { events: false });
    assert_eq!(req(&clients[1], ClientRequest::Ping { nonce: 7 }), ServerResponse::Pong { nonce: 7 });
}

#[test]
fn only_one_daemon_per_socket_and_stale_sockets_are_replaced() {
    let f = simple("single", RingProducer::IpcServer);
    let mut second = config("single2");
    second.socket = f.socket.clone();
    let err = Daemon::start(second).err().expect("el segundo daemon debe fallar");
    assert!(matches!(err, crate::DaemonError::Ipc(ref e) if e.kind() == std::io::ErrorKind::AddrInUse), "{err}");
    // El fallido no debe haber tocado al primero.
    let c = IpcClient::connect(&f.socket).unwrap();
    assert_eq!(req(&c, ClientRequest::Ping { nonce: 3 }), ServerResponse::Pong { nonce: 3 });

    #[cfg(unix)]
    {
        // Un archivo huérfano (de un daemon que murió) no impide arrancar.
        let stale = socket_for("stale");
        std::fs::write(&stale, b"basura").unwrap();
        let mut cfg = config("stale");
        cfg.socket = stale.clone();
        let d = Daemon::start(cfg).expect("reemplaza el socket huérfano");
        let c = IpcClient::connect(&stale).unwrap();
        assert!(matches!(req(&c, ClientRequest::Ping { nonce: 1 }), ServerResponse::Pong { .. }));
        drop(d);
    }
}

#[test]
fn load_show_command_replaces_the_show_and_reports_problems() {
    let f = simple("load", RingProducer::IpcServer);
    let c = IpcClient::connect(&f.socket).unwrap();
    req(&c, ClientRequest::Subscribe { events: true });
    req(&c, ClientRequest::Go);
    wait_until("suena", || f.daemon.health().commands_sent.load(Ordering::Relaxed) >= 1);

    let wav = write_wav("load-new", 1, 4_800, 48_000, |_, _| 0.1);
    let missing = std::path::Path::new("/no/existe/audio.wav");
    let show = write_show("load-new", vec![audio_node(10, Some(&wav), false), audio_node(11, Some(missing), false), audio_node(12, None, false)]);
    let _c = Cleanup(vec![wav, show.clone()]);
    match req(&c, ClientRequest::LoadShow { path: show.to_string_lossy().into() }) {
        ServerResponse::ShowLoaded { name, cues, warnings } => {
            assert_eq!((name.as_str(), cues, warnings.len()), ("show load-new", 3, 2), "{warnings:?}");
        }
        other => panic!("{other:?}"),
    }
    // Los avisos de la carga se publican antes que el evento de show cargado.
    assert!(c.wait_event(TIMEOUT, |e| matches!(e, ServerEvent::Warning { source, .. } if source == "show")).is_some());
    c.wait_event(TIMEOUT, |e| matches!(e, ServerEvent::ShowLoaded { .. })).expect("evento de show cargado");
    match req(&c, ClientRequest::GetCues) {
        ServerResponse::Cues { cues } => {
            assert_eq!(cues.iter().map(|c| (c.id, c.armed)).collect::<Vec<_>>(), vec![(10, true), (11, false), (12, false)]);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(f.daemon.loader().lock().unwrap().loaded_count(), 1, "solo la cue sana tiene voz");

    // GO dispara la sana y luego se detiene ante la desarmada (sin saltarla).
    req(&c, ClientRequest::Go);
    c.wait_event(TIMEOUT, |e| matches!(e, ServerEvent::CueStateChanged { cue: 10, state } if state == "Running")).unwrap();
    req(&c, ClientRequest::Go);
    std::thread::sleep(Duration::from_millis(50));
    match req(&c, ClientRequest::GetStatus) {
        ServerResponse::Status(s) => assert_eq!(s.playhead, Some(11), "el playhead espera en la cue desarmada"),
        other => panic!("{other:?}"),
    }

    // Errores de carga: archivo inexistente, JSON inválido, formato ajeno.
    assert!(matches!(req(&c, ClientRequest::LoadShow { path: "/no/hay/show.json".into() }), ServerResponse::Error { .. }));
    let junk = temp_path("junk", "json");
    std::fs::write(&junk, "{ no es json").unwrap();
    let _j = Cleanup(vec![junk.clone()]);
    assert!(matches!(req(&c, ClientRequest::LoadShow { path: junk.to_string_lossy().into() }), ServerResponse::Error { .. }));
    assert_eq!(f.daemon.loader().lock().unwrap().loaded_count(), 1, "un show roto no descarga el actual");
}

#[test]
fn control_commands_pause_resume_stop_and_playhead() {
    let f = simple("ctl", RingProducer::IpcServer);
    let c = IpcClient::connect(&f.socket).unwrap();
    req(&c, ClientRequest::Subscribe { events: true });
    let ui = SharedMemory::open(f.daemon.shm().name()).unwrap();
    req(&c, ClientRequest::SetPlayhead { cue_id: Some(2) });
    c.wait_event(TIMEOUT, |e| matches!(e, ServerEvent::PlayheadMoved { cue: Some(2) })).unwrap();
    req(&c, ClientRequest::Go);
    wait_until("suena el bucle", || peak(&ui, 0, 1) > 0.24);
    req(&c, ClientRequest::PauseAll);
    wait_until("pausa", || peak(&ui, 0, 4) == 0.0);
    req(&c, ClientRequest::ResumeAll);
    wait_until("reanuda", || peak(&ui, 0, 1) > 0.24);
    req(&c, ClientRequest::HardStopAll);
    wait_until("parada dura", || peak(&ui, 0, 4) == 0.0);
    c.wait_event(TIMEOUT, |e| matches!(e, ServerEvent::CueStateChanged { cue: 2, state } if state == "Completed")).unwrap();
}

#[test]
fn watchdog_restart_request_reopens_the_audio_stream() {
    let mut f = simple("restart", RingProducer::IpcServer);
    let health = std::sync::Arc::clone(f.daemon.health());
    wait_until("callbacks", || health.audio_callbacks.load(Ordering::Relaxed) > 10);
    health.restart_audio.store(true, Ordering::Release);
    f.daemon.poll();
    assert!(!health.restart_audio.load(Ordering::Acquire), "la petición se consume");
    let n = health.audio_callbacks.load(Ordering::Relaxed);
    wait_until("el stream reabierto sigue latiendo", || health.audio_callbacks.load(Ordering::Relaxed) > n + 10);
    assert!(health.audio_running.load(Ordering::Acquire));
}

#[test]
fn a_frozen_sequencer_triggers_safe_mute_through_the_whole_stack() {
    let mut cfg = config("freeze");
    cfg.watchdog.timeout = Duration::from_millis(60);
    cfg.watchdog.check_period = Duration::from_millis(10);
    cfg.watchdog.healthy_checks_to_unmute = 2;
    let wav = write_wav("freeze", 2, 48_000, 48_000, |_, _| 0.25);
    let show = write_show("freeze", vec![audio_node(1, Some(&wav), true)]);
    let _c = Cleanup(vec![wav, show.clone()]);
    cfg.show = Some(show);
    let socket = cfg.socket.clone();
    let daemon = Daemon::start(cfg).unwrap();
    let ui = SharedMemory::open(daemon.shm().name()).unwrap();
    let c = IpcClient::connect(&socket).unwrap();
    req(&c, ClientRequest::Subscribe { events: true });
    req(&c, ClientRequest::Go);
    wait_until("suena", || peak(&ui, 0, 1) > 0.24);

    // Se simula el congelamiento: el latido del secuenciador deja de avanzar.
    let health = std::sync::Arc::clone(daemon.health());
    health.seq_running.store(true, Ordering::Release);
    let frozen_at = health.seq_heartbeat_ns.load(Ordering::Acquire);
    // El hilo real sigue latiendo, así que se detiene el reloj del watchdog "congelando" su vista:
    // lo más fiel es dejar de dar latidos reales parando el hilo.
    let mut daemon = daemon;
    daemon.pause_sequencer_for_test();
    wait_until("safe mute activado", || health.safe_mute.load(Ordering::Acquire));
    c.wait_event(TIMEOUT, |e| matches!(e, ServerEvent::SafeMute { engaged: true })).expect("evento de safe mute");
    wait_until("el audio se apaga", || peak(&ui, 0, 4) == 0.0);
    assert!(health.seq_heartbeat_ns.load(Ordering::Acquire) >= frozen_at);
    assert!(health.sequencer_stalled.load(Ordering::Acquire));
    drop(daemon);
}

#[test]
fn shutdown_command_cleans_up_socket_and_shared_memory() {
    let Fixture { daemon: mut daemon_, socket, _cleanup } = simple("shutdown", RingProducer::IpcServer);
    let shm_name = daemon_.shm().name().to_string();
    let c = IpcClient::connect(&socket).unwrap();
    req(&c, ClientRequest::Subscribe { events: true });
    assert!(!daemon_.shutdown_requested());
    assert_eq!(req(&c, ClientRequest::Shutdown), ServerResponse::Ok);
    c.wait_event(TIMEOUT, |e| matches!(e, ServerEvent::ShuttingDown)).expect("aviso de apagado");
    daemon_.run(); // vuelve en cuanto se pidió el apagado
    daemon_.stop();
    daemon_.stop(); // idempotente
    drop(daemon_);
    #[cfg(unix)]
    {
        assert!(!std::path::Path::new(&socket).exists(), "el socket se elimina");
        assert!(SharedMemory::open(&shm_name).is_err(), "la memoria compartida se elimina");
    }
    assert!(IpcClient::connect(&socket).is_err());
    let _ = shm_name;
}

#[test]
fn unknown_backend_failures_are_reported_not_hidden() {
    let mut cfg = config("badaudio");
    cfg.backend.kind = BackendKind::Cpal;
    cfg.backend.host = Some("no-existe".into());
    let err = Daemon::start(cfg).err().expect("host inexistente");
    assert!(matches!(err, crate::DaemonError::Audio(_)), "{err}");
    assert!(err.to_string().contains("no-existe"));
}

fn detail(c: &IpcClient, id: u64) -> (Box<CueNodeDto>, String) {
    match req(c, ClientRequest::GetCueDetail { cue_id: id }) {
        ServerResponse::CueDetail { node, state, .. } => (node, state),
        other => panic!("{other:?}"),
    }
}

#[test]
fn inspector_edits_over_ipc() {
    let f = simple("edit", RingProducer::IpcServer);
    let c = IpcClient::connect(&f.socket).unwrap();
    req(&c, ClientRequest::Subscribe { events: true });
    let ui = SharedMemory::open(f.daemon.shm().name()).unwrap();

    // El detalle trae todo lo que edita el Inspector, con la región ya explícita.
    let (mut node, state) = detail(&c, 1);
    assert_eq!(state, "Idle");
    let CueBody::Audio(a) = &node.body else { panic!() };
    assert_eq!((a.time_loops.file_sample_rate, a.time_loops.end_time_sec), (48_000, 0.3));
    let levels = a.levels.as_ref().expect("el detalle incluye la matriz");
    assert_eq!((levels.inputs, levels.outputs), (2, 2));
    assert!(matches!(req(&c, ClientRequest::GetCueDetail { cue_id: 999 }), ServerResponse::Error { .. }));

    // Metadatos: se ven en la lista y se difunde el evento.
    node.basics.name = "Entrada".into();
    node.basics.pre_wait = 0.25;
    node.basics.continue_mode = qlab_protocol::cue_dto::ContinueMode::AutoFollow;
    node.basics.flagged = true;
    assert_eq!(req(&c, ClientRequest::UpdateCue { node: node.clone() }), ServerResponse::Ok);
    c.wait_event(TIMEOUT, |e| matches!(e, ServerEvent::CueUpdated { cue: 1 })).expect("evento de edición");
    match req(&c, ClientRequest::GetCues) {
        ServerResponse::Cues { cues } => {
            let c1 = &cues[0];
            assert_eq!((c1.name.as_str(), c1.pre_wait, c1.flagged, c1.target.as_str()), ("Entrada", 0.25, true, c1.target.as_str()));
            assert_eq!(c1.continue_mode, qlab_protocol::cue_dto::ContinueMode::AutoFollow);
            assert!(c1.target.ends_with(".wav") && c1.duration == Some(0.3));
        }
        other => panic!("{other:?}"),
    }

    // Cambiar la región de una cue parada recarga su voz: ahora dura la mitad.
    if let CueBody::Audio(a) = &mut node.body {
        a.time_loops.end_time_sec = 0.15;
    }
    assert_eq!(req(&c, ClientRequest::UpdateCue { node: node.clone() }), ServerResponse::Ok);
    match req(&c, ClientRequest::GetCues) {
        ServerResponse::Cues { cues } => assert_eq!(cues[0].duration, Some(0.15)),
        other => panic!("{other:?}"),
    }
    req(&c, ClientRequest::Go);
    wait_until("suena el tramo recortado", || peak(&ui, 0, 1) > 0.49);
    c.wait_event(TIMEOUT, |e| matches!(e, ServerEvent::CueStateChanged { cue: 1, state } if state == "Completed")).expect("termina al recortar");

    // Una cue que suena: archivo y región bloqueados; niveles y velocidad, en vivo.
    req(&c, ClientRequest::SetPlayhead { cue_id: Some(2) });
    req(&c, ClientRequest::Go);
    wait_until("suena el bucle", || peak(&ui, 1, 1) > 0.24 && peak(&ui, 0, 1) > 0.24);
    let (mut live, state) = detail(&c, 2);
    assert_eq!(state, "Running");
    let mut blocked = live.clone();
    if let CueBody::Audio(a) = &mut blocked.body {
        a.time_loops.start_time_sec = 0.1;
    }
    match req(&c, ClientRequest::UpdateCue { node: blocked }) {
        ServerResponse::Error { message } => assert!(message.contains("detén"), "{message}"),
        other => panic!("{other:?}"),
    }
    if let CueBody::Audio(a) = &mut live.body {
        a.levels.as_mut().unwrap().set_mute(2, true).unwrap();
    }
    assert_eq!(req(&c, ClientRequest::UpdateCue { node: live.clone() }), ServerResponse::Ok);
    wait_until("la salida 2 se silencia en vivo", || peak(&ui, 1, 6) == 0.0);
    assert!(peak(&ui, 0, 3) > 0.24, "la salida 1 sigue");
    if let CueBody::Audio(a) = &mut live.body {
        a.levels.as_mut().unwrap().set_mute(2, false).unwrap();
        a.time_loops.rate = 2.0;
    }
    assert_eq!(req(&c, ClientRequest::UpdateCue { node: live }), ServerResponse::Ok);
    wait_until("vuelve la salida 2", || peak(&ui, 1, 1) > 0.24);

    // Rechazos.
    let (mut bad, _) = detail(&c, 1);
    bad.basics.unique_id = CueId(777);
    assert!(matches!(req(&c, ClientRequest::UpdateCue { node: bad }), ServerResponse::Error { .. }));
    let (mut bad, _) = detail(&c, 1);
    bad.basics.post_wait = -3.0;
    assert!(matches!(req(&c, ClientRequest::UpdateCue { node: bad }), ServerResponse::Error { .. }));
    let (mut bad, _) = detail(&c, 1);
    if let CueBody::Audio(a) = &mut bad.body {
        a.time_loops.end_time_sec = 0.1;
        a.time_loops.start_time_sec = 9.0; // región vacía: la recarga falla y la voz anterior sigue intacta
    }
    assert!(matches!(req(&c, ClientRequest::UpdateCue { node: bad }), ServerResponse::Error { .. }));
    assert_eq!(f.daemon.loader().lock().unwrap().loaded_count(), 2);
}

#[test]
fn devamp_releases_a_cue_trapped_in_an_infinite_slice() {
    use qlab_protocol::cue_dto::{AudioSliceMarker, INFINITE_SLICE_LOOP};
    let f = fixture("devamp", RingProducer::IpcServer, |files| {
        let a = write_wav("devamp-a", 2, 48_000, 48_000, |_, _| 0.1);
        files.push(a.clone());
        let mut n = audio_node(1, Some(&a), false);
        if let CueBody::Audio(b) = &mut n.body {
            b.time_loops.end_time_sec = 1.0;
            b.time_loops.slices = vec![AudioSliceMarker::new(12_000, INFINITE_SLICE_LOOP)]; // [0,0,25 s) y [0,25 s,1 s) en bucle
        }
        vec![n]
    });
    let c = IpcClient::connect(&f.socket).unwrap();
    let status = |c: &IpcClient| match req(c, ClientRequest::GetStatus) {
        ServerResponse::Status(s) => s,
        other => panic!("{other:?}"),
    };
    req(&c, ClientRequest::Go);
    // Con la duración de la pista (1 s) ya superada tres veces, sigue sonando y atrapada en el tramo 1.
    std::thread::sleep(Duration::from_millis(3200));
    let s = status(&c);
    let cue = s.running.iter().find(|r| r.id == 1).expect("la cue sigue sonando: el bucle la retiene");
    assert_eq!((cue.slice, cue.vamping), (Some(1), true));

    assert_eq!(req(&c, ClientRequest::Devamp { cue_id: 1 }), ServerResponse::Ok);
    // Al final de la pasada en curso (< 0,75 s) sale del bucle y la pista termina de forma natural.
    wait_until_for("la cue termina tras el Devamp", Duration::from_secs(3), || !status(&c).running.iter().any(|r| r.id == 1));
}

fn cue_ids(c: &IpcClient) -> Vec<u64> {
    match req(c, ClientRequest::GetCues) {
        ServerResponse::Cues { cues } => cues.iter().map(|c| c.id).collect(),
        other => panic!("{other:?}"),
    }
}

fn add(c: &IpcClient, kind: &str, parent: Option<u64>, after: Option<u64>) -> u64 {
    match req(c, ClientRequest::AddCue { kind: kind.into(), parent, after }) {
        ServerResponse::CueAdded { cue_id } => cue_id,
        other => panic!("AddCue: {other:?}"),
    }
}

#[test]
fn authoring_adds_moves_and_deletes_cues() {
    let f = simple("author", RingProducer::IpcServer);
    let c = IpcClient::connect(&f.socket).unwrap();
    req(&c, ClientRequest::Subscribe { events: true });
    assert_eq!(cue_ids(&c), [1, 2]);

    // Alta: al final, justo después de una cue, y en un grupo.
    let fade = add(&c, "fade", None, None);
    let audio = add(&c, "audio", None, Some(1));
    let group = add(&c, "group", None, None);
    let child = add(&c, "audio", Some(group), None);
    assert_eq!((fade, audio, group, child), (3, 4, 5, 6), "ids correlativos");
    assert_eq!(cue_ids(&c), [1, 4, 2, 3, 5, 6], "el audio nuevo va tras la 1; el hijo sigue a su grupo");
    c.wait_event(TIMEOUT, |e| matches!(e, ServerEvent::StructureChanged)).expect("evento de estructura");
    match req(&c, ClientRequest::GetCueDetail { cue_id: audio }) {
        ServerResponse::CueDetail { node, .. } => {
            assert_eq!(node.basics.number, "4", "número = mayor numérico + 1 (había 1, 2 y el fade 3)");
            assert!(!node.basics.armed, "un audio sin archivo queda desarmado");
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(req(&c, ClientRequest::AddCue { kind: "hologram".into(), parent: None, after: None }), ServerResponse::Error { .. }));

    // Orden.
    assert_eq!(req(&c, ClientRequest::MoveCue { cue_id: 3, delta: -1 }), ServerResponse::Ok);
    assert_eq!(cue_ids(&c), [1, 4, 3, 2, 5, 6]);
    assert_eq!(req(&c, ClientRequest::MoveCue { cue_id: 1, delta: -1 }), ServerResponse::Ok, "ya es la primera: no es un error");
    assert_eq!(cue_ids(&c), [1, 4, 3, 2, 5, 6]);

    // Baja: una cue sonando se rechaza; un grupo se lleva a sus hijos.
    req(&c, ClientRequest::Go);
    wait_until("la cue 1 suena", || matches!(req(&c, ClientRequest::GetCues), ServerResponse::Cues { cues } if cues.iter().any(|q| q.id == 1 && q.state == "Running")));
    assert!(matches!(req(&c, ClientRequest::DeleteCue { cue_id: 1 }), ServerResponse::Error { .. }));
    assert_eq!(cue_ids(&c), [1, 4, 3, 2, 5, 6], "no se eliminó nada");
    assert_eq!(req(&c, ClientRequest::DeleteCue { cue_id: 5 }), ServerResponse::Ok);
    assert_eq!(cue_ids(&c), [1, 4, 3, 2]);
    assert!(matches!(req(&c, ClientRequest::DeleteCue { cue_id: 99 }), ServerResponse::Error { .. }));
}

#[test]
fn a_new_cue_gets_its_file_through_update_cue_and_is_armed() {
    let f = simple("author_file", RingProducer::IpcServer);
    let c = IpcClient::connect(&f.socket).unwrap();
    let id = add(&c, "audio", None, None);
    let wav = write_wav("author-file", 2, 24_000, 48_000, |_, _| 0.2);
    let _guard = Cleanup(vec![wav.clone()]);
    let mut node = match req(&c, ClientRequest::GetCueDetail { cue_id: id }) {
        ServerResponse::CueDetail { node, .. } => *node,
        other => panic!("{other:?}"),
    };
    node.basics.file_target = Some(MediaTarget { absolute_path: wav.to_string_lossy().into(), ..Default::default() });
    node.basics.armed = true;
    assert_eq!(req(&c, ClientRequest::UpdateCue { node: Box::new(node) }), ServerResponse::Ok);
    match req(&c, ClientRequest::GetCueDetail { cue_id: id }) {
        ServerResponse::CueDetail { node, .. } => match &node.body {
            // El daemon calculó la duración real del archivo (0,5 s) y fijó el final de la región.
            CueBody::Audio(a) => assert!((a.time_loops.end_time_sec - 0.5).abs() < 1e-6, "{}", a.time_loops.end_time_sec),
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

#[test]
fn save_new_and_open_roundtrip_the_whole_show() {
    let f = simple("author_save", RingProducer::IpcServer);
    let c = IpcClient::connect(&f.socket).unwrap();
    let group = add(&c, "group", None, None);
    add(&c, "fade", Some(group), None);
    let path = temp_path("author-save", "json");
    let _guard = Cleanup(vec![path.clone()]);
    let before = cue_ids(&c);
    match req(&c, ClientRequest::SaveShow { path: path.to_string_lossy().into(), name: "Mi show".into() }) {
        ServerResponse::ShowSaved { cues, .. } => assert_eq!(cues, 4),
        other => panic!("{other:?}"),
    }
    assert!(!path.with_extension("json.tmp").exists(), "no queda el temporal");
    let saved = ShowFile::from_json(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!((saved.name.as_str(), saved.cue_count()), ("Mi show", 4));

    // Nuevo show: lista vacía.
    assert!(matches!(req(&c, ClientRequest::NewShow), ServerResponse::ShowLoaded { cues: 0, .. }));
    assert!(cue_ids(&c).is_empty());
    // Abrir: vuelve todo, en el mismo orden y con el grupo y su hijo.
    match req(&c, ClientRequest::LoadShow { path: path.to_string_lossy().into() }) {
        ServerResponse::ShowLoaded { name, cues, .. } => assert_eq!((name.as_str(), cues), ("Mi show", 4)),
        other => panic!("{other:?}"),
    }
    assert_eq!(cue_ids(&c), before);
    // Guardar de nuevo da el mismo contenido (ida y vuelta sin pérdidas).
    let path2 = temp_path("author-save2", "json");
    let _guard2 = Cleanup(vec![path2.clone()]);
    req(&c, ClientRequest::SaveShow { path: path2.to_string_lossy().into(), name: "Mi show".into() });
    assert_eq!(std::fs::read_to_string(&path).unwrap(), std::fs::read_to_string(&path2).unwrap());
    // Una ruta imposible es un error, no un pánico.
    assert!(matches!(req(&c, ClientRequest::SaveShow { path: "/no/existe/x.json".into(), name: "x".into() }), ServerResponse::Error { .. }));
}

#[test]
fn bundling_copies_media_rewrites_relative_paths_and_the_bundle_opens_from_anywhere() {
    let f = simple("bundle", RingProducer::IpcServer);
    let c = IpcClient::connect(&f.socket).unwrap();
    // Dos archivos distintos con el mismo nombre y un archivo que ya no existe.
    let dir_a = std::env::temp_dir().join(format!("qlab-bundle-a-{}", std::process::id()));
    let dir_b = std::env::temp_dir().join(format!("qlab-bundle-b-{}", std::process::id()));
    for d in [&dir_a, &dir_b] {
        std::fs::create_dir_all(d).unwrap();
    }
    let same_name_1 = dir_a.join("pista.wav");
    let same_name_2 = dir_b.join("pista.wav");
    std::fs::copy(write_wav("bundle-1", 2, 24_000, 48_000, |_, _| 0.2), &same_name_1).unwrap();
    std::fs::copy(write_wav("bundle-2", 2, 48_000, 48_000, |_, _| 0.2), &same_name_2).unwrap();
    let out = std::env::temp_dir().join(format!("qlab-bundle-out-{}", std::process::id()));
    let moved = std::env::temp_dir().join(format!("qlab-bundle-moved-{}", std::process::id()));
    let _clean = Cleanup(vec![same_name_1.clone(), same_name_2.clone()]);

    // Las cues 1 y 2 del show de prueba pasan a usar esos archivos (la 2 también la repite la 3, un tercer alta).
    for (id, file) in [(1u64, &same_name_1), (2, &same_name_2)] {
        let mut node = match req(&c, ClientRequest::GetCueDetail { cue_id: id }) {
            ServerResponse::CueDetail { node, .. } => *node,
            other => panic!("{other:?}"),
        };
        node.basics.file_target = Some(MediaTarget { absolute_path: file.to_string_lossy().into(), ..Default::default() });
        assert_eq!(req(&c, ClientRequest::UpdateCue { node: Box::new(node) }), ServerResponse::Ok);
    }
    let id3 = add(&c, "audio", None, None);
    let mut node = match req(&c, ClientRequest::GetCueDetail { cue_id: id3 }) {
        ServerResponse::CueDetail { node, .. } => *node,
        other => panic!("{other:?}"),
    };
    node.basics.file_target = Some(MediaTarget { absolute_path: same_name_1.to_string_lossy().into(), ..Default::default() });
    node.basics.armed = true;
    req(&c, ClientRequest::UpdateCue { node: Box::new(node) });

    match req(&c, ClientRequest::BundleShow { dir: out.to_string_lossy().into(), name: "Gira: acto 1".into() }) {
        ServerResponse::ShowBundled { path, files, bytes, missing } => {
            assert_eq!(files, 2, "el archivo repetido se copia una sola vez");
            assert!(bytes > 0 && missing.is_empty(), "{missing:?}");
            assert!(path.ends_with("Gira_ acto 1.teatroshow"), "{path}");
        }
        other => panic!("{other:?}"),
    }
    let names: std::collections::BTreeSet<String> = std::fs::read_dir(out.join("audio")).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    assert_eq!(names, ["pista-2.wav".to_string(), "pista.wav".to_string()].into_iter().collect(), "los nombres iguales no se pisan");
    let json = std::fs::read_to_string(out.join("Gira_ acto 1.teatroshow")).unwrap();
    assert!(!json.contains(&out.to_string_lossy().to_string()) && !json.contains(&dir_a.to_string_lossy().to_string()), "no quedan rutas absolutas");
    assert!(json.contains("audio/pista.wav") && json.contains("audio/pista-2.wav"));

    // La carpeta se puede mover y el show sigue abriendo sus archivos desde su nueva ubicación.
    std::fs::rename(&out, &moved).unwrap();
    let _clean2 = Cleanup(vec![]);
    match req(&c, ClientRequest::LoadShow { path: moved.join("Gira_ acto 1.teatroshow").to_string_lossy().into() }) {
        ServerResponse::ShowLoaded { cues, warnings, .. } => assert_eq!((cues, warnings.len()), (3, 0), "{warnings:?}"),
        other => panic!("{other:?}"),
    }
    match req(&c, ClientRequest::GetCueDetail { cue_id: 1 }) {
        ServerResponse::CueDetail { node, .. } => {
            let t = node.basics.file_target.unwrap();
            assert_eq!(t.absolute_path, moved.join("audio/pista.wav").to_string_lossy());
        }
        other => panic!("{other:?}"),
    }
    for d in [dir_a, dir_b, moved] {
        let _ = std::fs::remove_dir_all(d);
    }
}

#[test]
fn a_video_cue_plays_the_audio_track_of_its_video_file() {
    let video = std::env::temp_dir().join(format!("qlab-daemon-video-{}.mp4", std::process::id()));
    let made = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc=duration=1:size=160x90:rate=25", "-f", "lavfi", "-i", "sine=frequency=440:duration=1:sample_rate=44100", "-c:v", "mpeg4", "-c:a", "aac", "-shortest"])
        .arg(&video)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !made {
        eprintln!("ffmpeg no está instalado: se omite la prueba de vídeo");
        return;
    }
    let _clean = Cleanup(vec![video.clone()]);
    let f = simple("video_cue", RingProducer::IpcServer);
    let c = IpcClient::connect(&f.socket).unwrap();
    req(&c, ClientRequest::Subscribe { events: true });
    let id = add(&c, "video", None, None);
    let mut node = match req(&c, ClientRequest::GetCueDetail { cue_id: id }) {
        ServerResponse::CueDetail { node, .. } => *node,
        other => panic!("{other:?}"),
    };
    assert!(node.body.is_video());
    node.basics.file_target = Some(MediaTarget { absolute_path: video.to_string_lossy().into(), ..Default::default() });
    node.basics.armed = true;
    assert_eq!(req(&c, ClientRequest::UpdateCue { node: Box::new(node) }), ServerResponse::Ok);
    match req(&c, ClientRequest::GetCues) {
        ServerResponse::Cues { cues } => {
            let v = cues.iter().find(|q| q.id == id).unwrap();
            assert_eq!(v.kind, "video");
            assert!(v.target.ends_with(".mp4"), "{}", v.target);
            assert!(v.duration.is_some_and(|d| (0.9..1.1).contains(&d)), "duración de la pista de audio: {:?}", v.duration);
        }
        other => panic!("{other:?}"),
    }
    // Suena de verdad: el playhead va a la cue de vídeo, GO la arranca y termina sola tras ~1 s.
    req(&c, ClientRequest::SetPlayhead { cue_id: Some(id) });
    req(&c, ClientRequest::Go);
    c.wait_event(TIMEOUT, |e| matches!(e, ServerEvent::CueStateChanged { cue, state } if *cue == id && state == "Running")).expect("empieza a sonar");
    c.wait_event(Duration::from_secs(4), |e| matches!(e, ServerEvent::CueStateChanged { cue, state } if *cue == id && state == "Completed")).expect("termina con su audio");
}

#[test]
fn emergency_key_over_ipc_extends_the_license_without_restarting() {
    use ed25519_dalek::SigningKey;
    use qlab_license::activation::{sign_challenge, Setup};
    use qlab_license::clock::{self, FileStore, MarkStore};
    use qlab_license::MachineId;

    const DAY: u64 = 86_400;
    let tag = "lic-emergency";
    let dir = temp_path(tag, "d");
    std::fs::create_dir_all(&dir).unwrap();
    let sk = SigningKey::from_bytes(&[5u8; 32]);
    let machine = MachineId::from_guid("pc-del-teatro");
    let t0 = clock::BUILD_UNIX + 100 * DAY;
    let license_path = dir.join("license.key");
    std::fs::write(&license_path, qlab_license::issue("Ballet", &machine, t0, t0 + 30 * DAY, &sk)).unwrap();
    let stores: Vec<Box<dyn MarkStore>> = (0..3).map(|i| Box::new(FileStore(dir.join(format!("mark{i}.dat")))) as Box<dyn MarkStore>).collect();
    let license = qlab_license::verify_with(&std::fs::read_to_string(&license_path).unwrap(), &sk.verifying_key(), &machine).unwrap();
    clock::reset(t0, &license, &stores);
    let now = t0 + 40 * DAY; // vencida hace 10 días
    let setup = Setup { license_path: license_path.clone(), stores: std::sync::Arc::new(stores), anchors: vec![], public: Some(sk.verifying_key()), machine: Ok(machine), now: Some(now) };
    assert!(matches!(setup.status().result, Err(qlab_license::LicenseError::Expired { .. })));

    let mut cfg = config(tag);
    cfg.license = Some(setup.clone());
    let socket = cfg.socket.clone();
    let _daemon = Daemon::start(cfg).expect("el daemon debe arrancar");
    let c = IpcClient::connect(&socket).unwrap();

    // Claves malas: error descriptivo y el daemon sigue en pie.
    match req(&c, ClientRequest::ApplyEmergencyKey { key: "RESP-NOESUNACLAVE".into() }) {
        ServerResponse::Error { message } => assert!(!message.is_empty()),
        other => panic!("{other:?}"),
    }
    let (_, key) = sign_challenge(&setup.challenge().unwrap(), 15, now, &sk);
    match req(&c, ClientRequest::ApplyEmergencyKey { key: key.clone() }) {
        ServerResponse::LicenseExtended { valid_until, epoch } => assert_eq!((valid_until, epoch), (now + 15 * DAY, 1)),
        other => panic!("{other:?}"),
    }
    assert!(setup.status().result.is_ok(), "la licencia queda vigente en disco");
    // Canjearla otra vez falla (anti-replay) y el daemon sigue respondiendo.
    match req(&c, ClientRequest::ApplyEmergencyKey { key }) {
        ServerResponse::Error { message } => assert!(message.contains("ya fue usada"), "{message}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(req(&c, ClientRequest::Ping { nonce: 1 }), ServerResponse::Pong { nonce: 1 });
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn osc_over_udp_fires_go_and_a_cue_by_number_and_ignores_garbage() {
    let tag = "osc";
    let a = write_wav(&format!("{tag}-a"), 2, 96_000, 48_000, |_, _| 0.5);
    let b = write_wav(&format!("{tag}-b"), 2, 96_000, 48_000, |_, _| 0.25);
    let show = write_show(tag, vec![audio_node(1, Some(&a), false), audio_node(2, Some(&b), false)]);
    let _cleanup = Cleanup(vec![a, b, show.clone()]);
    let mut cfg = config(tag);
    cfg.show = Some(show);
    cfg.osc_port = Some(0);
    let socket = cfg.socket.clone();
    let daemon = Daemon::start(cfg).expect("el daemon debe arrancar");
    let port = daemon.osc_port().expect("OSC activo");
    let c = IpcClient::connect(&socket).unwrap();
    let udp = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let send = |addr: &str| {
        let mut v = addr.as_bytes().to_vec();
        v.push(0);
        while v.len() % 4 != 0 {
            v.push(0);
        }
        v.extend(b",\0\0\0");
        udp.send_to(&v, ("127.0.0.1", port)).unwrap();
    };
    let running = |c: &IpcClient| match req(c, ClientRequest::GetStatus) {
        ServerResponse::Status(s) => s.running.iter().map(|r| r.id).collect::<Vec<_>>(),
        other => panic!("{other:?}"),
    };
    udp.send_to(b"basura", ("127.0.0.1", port)).unwrap();
    send("/nope");
    send("/go");
    wait_until("la cue 1 suena por OSC", || running(&c).contains(&1));
    send("/cue/2/start");
    wait_until("la cue 2 suena por su número", || running(&c).contains(&2));
    // Tras disparar la última cue el playhead queda vacío: /prev va a la última, otro /prev a la anterior, /next vuelve.
    let playhead = |c: &IpcClient| match req(c, ClientRequest::GetStatus) {
        ServerResponse::Status(s) => s.playhead,
        other => panic!("{other:?}"),
    };
    send("/prev");
    wait_until("/prev desde el final va a la última cue", || playhead(&c) == Some(2));
    send("/prev");
    wait_until("otro /prev va a la primera", || playhead(&c) == Some(1));
    send("/next");
    wait_until("/next vuelve a la segunda", || playhead(&c) == Some(2));
    // Acciones por cue: arrancar la 1 y pararla, pausarla y reanudarla, todo por su número.
    send("/cue/1/start");
    wait_until("la cue 1 vuelve a sonar", || running(&c).contains(&1));
    send("/cue/1/pause");
    wait_until("la cue 1 queda en pausa", || matches!(req(&c, ClientRequest::GetCues), ServerResponse::Cues { cues } if cues.iter().any(|q| q.id == 1 && q.state == "Paused")));
    send("/cue/1/resume");
    wait_until("la cue 1 se reanuda", || matches!(req(&c, ClientRequest::GetCues), ServerResponse::Cues { cues } if cues.iter().any(|q| q.id == 1 && q.state == "Running")));
    send("/cue/1/stop");
    wait_until("la cue 1 se detiene", || !running(&c).contains(&1));
    send("/cue/2/disarm");
    wait_until("la cue 2 se desarma", || matches!(req(&c, ClientRequest::GetCues), ServerResponse::Cues { cues } if cues.iter().any(|q| q.id == 2 && !q.armed)));
    send("/cue/2/arm");
    wait_until("la cue 2 se arma", || matches!(req(&c, ClientRequest::GetCues), ServerResponse::Cues { cues } if cues.iter().any(|q| q.id == 2 && q.armed)));
    send("/panic");
    wait_until("el panic por OSC detiene todo", || running(&c).is_empty());
    assert_eq!(req(&c, ClientRequest::Ping { nonce: 5 }), ServerResponse::Pong { nonce: 5 }, "la basura no tumbó nada");
}

#[test]
fn an_osc_cue_sends_its_message_over_udp_when_fired() {
    use qlab_protocol::show_format::OscBody;
    let tag = "osc-out";
    let receiver = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    receiver.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    let port = receiver.local_addr().unwrap().port();
    let mut node = audio_node(1, None, false);
    node.basics.name = "Luces".into();
    node.basics.armed = true;
    node.body = CueBody::Osc(OscBody { destination: format!("127.0.0.1:{port}"), message: "/luces/escena 3 0.5 \"acto uno\"".into() });
    let show = write_show(tag, vec![node]);
    let _cleanup = Cleanup(vec![show.clone()]);
    let mut cfg = config(tag);
    cfg.show = Some(show);
    let socket = cfg.socket.clone();
    let _daemon = Daemon::start(cfg).expect("el daemon debe arrancar");
    let c = IpcClient::connect(&socket).unwrap();
    assert_eq!(req(&c, ClientRequest::Go), ServerResponse::Ok);
    let mut buf = [0u8; 512];
    let n = receiver.recv(&mut buf).expect("el mensaje OSC debe llegar");
    let m = crate::osc::parse_packet(&buf[..n]);
    assert_eq!(m[0].addr, "/luces/escena");
    assert_eq!(m[0].args, vec![crate::osc::OscArg::Int(3), crate::osc::OscArg::Float(0.5), crate::osc::OscArg::Str("acto uno".into())]);
    // La cue queda registrada como de tipo osc en la lista.
    match req(&c, ClientRequest::GetCues) {
        ServerResponse::Cues { cues } => assert_eq!((cues[0].kind.as_str(), cues[0].target.as_str()), ("osc", format!("127.0.0.1:{port}").as_str())),
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_master_volume_scales_everything_that_comes_out_and_returns_to_unity() {
    let f = simple("master", RingProducer::IpcServer);
    let c = IpcClient::connect(&f.socket).unwrap();
    let ui = SharedMemory::open(f.daemon.shm().name()).unwrap();
    // La cue 2 es un bucle continuo con pico 0,25.
    assert_eq!(req(&c, ClientRequest::SetPlayhead { cue_id: Some(2) }), ServerResponse::Ok);
    assert_eq!(req(&c, ClientRequest::Go), ServerResponse::Ok);
    wait_until("suena a nivel completo", || (peak(&ui, 0, 2) - 0.25).abs() < 1e-3);
    // −6 dB (≈ 0,501): el pico baja a ~0,125.
    assert_eq!(req(&c, ClientRequest::SetMasterDb { db: -6.0 }), ServerResponse::Ok);
    wait_until("−6 dB", || (peak(&ui, 0, 3) - 0.25 * 0.501_187).abs() < 2e-3);
    // −60 dB o menos: silencio total, aunque la cue siga corriendo.
    assert_eq!(req(&c, ClientRequest::SetMasterDb { db: -80.0 }), ServerResponse::Ok);
    wait_until("silencio", || peak(&ui, 0, 3) == 0.0);
    match req(&c, ClientRequest::GetStatus) {
        ServerResponse::Status(s) => assert!(s.running.iter().any(|r| r.id == 2), "la cue sigue corriendo en silencio"),
        other => panic!("{other:?}"),
    }
    // Un NaN no es JSON válido: se rechaza con un error y el maestro no cambia (sigue en silencio).
    assert!(matches!(req(&c, ClientRequest::SetMasterDb { db: f32::NAN }), ServerResponse::Error { .. }));
    assert_eq!(peak(&ui, 0, 3), 0.0);
    assert_eq!(req(&c, ClientRequest::SetMasterDb { db: 0.0 }), ServerResponse::Ok);
    wait_until("de vuelta a 0 dB", || (peak(&ui, 0, 3) - 0.25).abs() < 1e-3);
    // Por encima del máximo se recorta a +6 dB (≈ 1,995).
    assert_eq!(req(&c, ClientRequest::SetMasterDb { db: 40.0 }), ServerResponse::Ok);
    wait_until("+6 dB", || (peak(&ui, 0, 3) - 0.25 * 1.995_262).abs() < 3e-3);
}

#[test]
fn a_cue_eq_set_by_editing_filters_the_live_output_and_turning_it_off_restores_it() {
    use qlab_protocol::cue_dto::{AudioEqData, EqBandData, EqFilterType};
    let f = simple("eq", RingProducer::IpcServer);
    let c = IpcClient::connect(&f.socket).unwrap();
    let ui = SharedMemory::open(f.daemon.shm().name()).unwrap();
    // La cue 2 es un bucle continuo con valor constante 0,25 (corriente continua).
    assert_eq!(req(&c, ClientRequest::SetPlayhead { cue_id: Some(2) }), ServerResponse::Ok);
    assert_eq!(req(&c, ClientRequest::Go), ServerResponse::Ok);
    wait_until("suena a nivel completo", || (peak(&ui, 0, 2) - 0.25).abs() < 1e-3);
    let (mut node, _) = detail(&c, 2);
    // Paso alto en 1 kHz: elimina la señal continua.
    let mut eq = AudioEqData::default();
    eq.enabled = true;
    eq.bands[0] = EqBandData { filter_type: EqFilterType::LowCut, frequency_hz: 1_000.0, gain_db: 0.0, q_factor: 0.707, enabled: true };
    if let CueBody::Audio(a) = &mut node.body {
        a.eq = eq;
    }
    assert_eq!(req(&c, ClientRequest::UpdateCue { node: node.clone() }), ServerResponse::Ok);
    wait_until("el EQ la silencia", || peak(&ui, 0, 3) < 0.02);
    // El show guardado conserva el EQ.
    let (saved, _) = detail(&c, 2);
    assert!(matches!(&saved.body, CueBody::Audio(a) if a.eq == eq));
    // Apagado: la señal vuelve entera, sin recargar la voz ni cortar la cue.
    eq.enabled = false;
    if let CueBody::Audio(a) = &mut node.body {
        a.eq = eq;
    }
    assert_eq!(req(&c, ClientRequest::UpdateCue { node }), ServerResponse::Ok);
    wait_until("de vuelta al nivel original", || (peak(&ui, 0, 3) - 0.25).abs() < 1e-3);
    assert_eq!(f.daemon.health().rt_alloc_violations.load(Ordering::Relaxed), 0, "el EQ no asigna memoria en el callback");
}

#[test]
fn independent_players_reach_the_room_or_only_the_headphones_without_moving_the_playhead() {
    let tag = "adhoc";
    let wav = write_wav(&format!("{tag}-w"), 2, 96_000, 48_000, |_, _| 0.25);
    let show = write_show(tag, vec![audio_node(1, Some(&wav), false), audio_node(2, Some(&wav), false)]);
    let mut cfg = config(tag);
    cfg.backend.channels = 4;
    cfg.monitor_first = Some(2); // auriculares en los canales 3-4
    cfg.show = Some(show.clone());
    let socket = cfg.socket.clone();
    let daemon = Daemon::start(cfg).expect("el daemon debe arrancar");
    let _cleanup = Cleanup(vec![wav.clone(), show]);
    let c = IpcClient::connect(&socket).unwrap();
    let ui = SharedMemory::open(daemon.shm().name()).unwrap();
    let path = wav.to_string_lossy().into_owned();
    let playhead = |c: &IpcClient| match req(c, ClientRequest::GetStatus) {
        ServerResponse::Status(s) => s.playhead,
        other => panic!("{other:?}"),
    };
    assert_eq!(playhead(&c), Some(1));

    // Audición (PFL): solo los canales 3-4.
    match req(&c, ClientRequest::AdhocLoad { slot: 20, path: path.clone(), gain_db: 0.0, monitor: true, start_secs: 0.0 }) {
        ServerResponse::AdhocLoaded { slot: 20, duration: Some(d) } => assert!((d - 2.0).abs() < 0.01, "{d}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(req(&c, ClientRequest::AdhocPlay { slot: 20 }), ServerResponse::Ok);
    wait_until("suena en los auriculares", || peak(&ui, 2, 2) > 0.2 && peak(&ui, 3, 2) > 0.2);
    assert!(peak(&ui, 0, 4) < 1e-4 && peak(&ui, 1, 4) < 1e-4, "la audición no sale por la sala");
    // El maestro de la sala no toca los auriculares.
    assert_eq!(req(&c, ClientRequest::SetMasterDb { db: -60.0 }), ServerResponse::Ok);
    std::thread::sleep(Duration::from_millis(150));
    assert!(peak(&ui, 2, 4) > 0.2, "el maestro no baja los auriculares");
    assert_eq!(req(&c, ClientRequest::SetMasterDb { db: 0.0 }), ServerResponse::Ok);
    assert_eq!(req(&c, ClientRequest::AdhocStop { slot: 20 }), ServerResponse::Ok);
    wait_until("silencio tras detener", || peak(&ui, 2, 2) < 1e-4);

    // Pad de Carts: sala (canales 1-2), con su propio volumen (−6 dB) y sin sonar en los auriculares.
    assert!(matches!(req(&c, ClientRequest::AdhocLoad { slot: 0, path: path.clone(), gain_db: -6.0, monitor: false, start_secs: 0.0 }), ServerResponse::AdhocLoaded { slot: 0, .. }));
    assert_eq!(req(&c, ClientRequest::AdhocPlay { slot: 0 }), ServerResponse::Ok);
    wait_until("suena en la sala a −6 dB", || (peak(&ui, 0, 3) - 0.25 * 10f32.powf(-6.0 / 20.0)).abs() < 0.01);
    assert!(peak(&ui, 2, 4) < 1e-4, "la sala no entra en los auriculares");
    // Disparar de nuevo mientras suena lo reinicia sin error; y se puede parar todo.
    assert_eq!(req(&c, ClientRequest::AdhocPlay { slot: 0 }), ServerResponse::Ok);
    wait_until("sigue sonando tras reiniciar", || peak(&ui, 0, 3) > 0.1);
    assert_eq!(req(&c, ClientRequest::AdhocStopAll), ServerResponse::Ok);
    wait_until("silencio", || peak(&ui, 0, 2) < 1e-4);

    // Nada de esto movió la lista de cues.
    assert_eq!(playhead(&c), Some(1));
    // Sin salida de auriculares configurada la audición se rechaza con un mensaje útil.
    assert!(matches!(req(&c, ClientRequest::AdhocLoad { slot: 99, path: path.clone(), gain_db: 0.0, monitor: false, start_secs: 0.0 }), ServerResponse::Error { .. }), "ranura fuera de rango");
    assert_eq!(daemon.health().rt_alloc_violations.load(Ordering::Relaxed), 0);
}

#[test]
fn headphone_audition_is_refused_when_no_headphone_output_is_configured() {
    let f = simple("noph", RingProducer::IpcServer);
    let c = IpcClient::connect(&f.socket).unwrap();
    let wav = write_wav("noph-w", 2, 4_800, 48_000, |_, _| 0.1);
    let _cleanup = Cleanup(vec![wav.clone()]);
    match req(&c, ClientRequest::AdhocLoad { slot: 20, path: wav.to_string_lossy().into(), gain_db: 0.0, monitor: true, start_secs: 0.0 }) {
        ServerResponse::Error { message } => assert!(message.contains("auriculares"), "{message}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn auditioning_a_cue_plays_it_in_the_headphones_from_a_chosen_point_and_leaves_the_playhead() {
    let tag = "pfl";
    let wav = write_wav(&format!("{tag}-w"), 2, 96_000, 48_000, |_, _| 0.25);
    let show = write_show(tag, vec![audio_node(1, Some(&wav), false), audio_node(2, Some(&wav), false)]);
    let mut cfg = config(tag);
    cfg.backend.channels = 4;
    cfg.monitor_first = Some(2);
    cfg.show = Some(show.clone());
    let socket = cfg.socket.clone();
    let daemon = Daemon::start(cfg).expect("el daemon debe arrancar");
    let _cleanup = Cleanup(vec![wav, show]);
    let c = IpcClient::connect(&socket).unwrap();
    let ui = SharedMemory::open(daemon.shm().name()).unwrap();
    // Desde el segundo 1,5 quedan 0,5 s de la pista de 2 s.
    match req(&c, ClientRequest::Audition { cue_id: 2, start_secs: Some(1.5) }) {
        ServerResponse::AdhocLoaded { duration: Some(d), .. } => assert!((d - 0.5).abs() < 0.01, "{d}"),
        other => panic!("{other:?}"),
    }
    wait_until("suena en los auriculares", || peak(&ui, 2, 2) > 0.2);
    assert!(peak(&ui, 0, 4) < 1e-4, "la sala no oye la audición");
    wait_until("termina sola al acabar el archivo", || peak(&ui, 2, 4) < 1e-4);
    assert_eq!(req(&c, ClientRequest::AuditionStop), ServerResponse::Ok);
    assert!(matches!(req(&c, ClientRequest::Audition { cue_id: 999, start_secs: None }), ServerResponse::Error { .. }));
    match req(&c, ClientRequest::GetStatus) {
        ServerResponse::Status(s) => assert!(s.running.is_empty() && s.playhead == Some(1), "{s:?}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_cart_bank_is_saved_with_the_show_and_comes_back_when_it_is_loaded() {
    use qlab_protocol::cue_dto::{CartBank, CartMode, CartPad};
    let f = simple("carts", RingProducer::IpcServer);
    let c = IpcClient::connect(&f.socket).unwrap();
    let mut bank = CartBank::default().clamped();
    bank.pads[3] = CartPad { path: "/fx/aplauso.wav".into(), name: "Aplauso".into(), key: "F5".into(), mode: CartMode::Toggle, gain_db: -4.5, ..Default::default() };
    assert_eq!(req(&c, ClientRequest::SetCarts { bank: bank.clone() }), ServerResponse::Ok);
    let out = temp_path("carts-out", "json");
    let _cleanup = Cleanup(vec![out.clone()]);
    assert!(matches!(req(&c, ClientRequest::SaveShow { path: out.to_string_lossy().into(), name: "Con carts".into() }), ServerResponse::ShowSaved { .. }));
    assert!(std::fs::read_to_string(&out).unwrap().contains("Aplauso"));
    // Un show nuevo la vacía; abrir el guardado la recupera.
    assert!(matches!(req(&c, ClientRequest::NewShow), ServerResponse::ShowLoaded { .. }));
    match req(&c, ClientRequest::GetCarts) {
        ServerResponse::Carts { bank } => assert!(bank.is_empty()),
        other => panic!("{other:?}"),
    }
    assert!(matches!(req(&c, ClientRequest::LoadShow { path: out.to_string_lossy().into() }), ServerResponse::ShowLoaded { .. }));
    match req(&c, ClientRequest::GetCarts) {
        ServerResponse::Carts { bank: got } => assert_eq!(got, bank),
        other => panic!("{other:?}"),
    }
}

#[test]
fn headphones_on_another_card_keep_the_room_silent_and_the_audition_works_without_four_channels() {
    let tag = "monsep";
    let wav = write_wav(&format!("{tag}-w"), 2, 96_000, 48_000, |_, _| 0.25);
    let show = write_show(tag, vec![audio_node(1, Some(&wav), false)]);
    let mut cfg = config(tag);
    cfg.backend.channels = 2; // la sala es una placa estéreo
    cfg.backend.monitor_device = Some("Auriculares USB".into()); // otra placa
    cfg.show = Some(show.clone());
    let socket = cfg.socket.clone();
    let daemon = Daemon::start(cfg).expect("el daemon debe arrancar");
    let _cleanup = Cleanup(vec![wav, show]);
    let c = IpcClient::connect(&socket).unwrap();
    let ui = SharedMemory::open(daemon.shm().name()).unwrap();
    // Con la placa principal estéreo y los auriculares aparte, la audición funciona igual (sin canales 3-4 en la sala).
    match req(&c, ClientRequest::Audition { cue_id: 1, start_secs: None }) {
        ServerResponse::AdhocLoaded { duration: Some(d), .. } => assert!((d - 2.0).abs() < 0.01, "{d}"),
        other => panic!("{other:?}"),
    }
    std::thread::sleep(Duration::from_millis(200));
    assert!(peak(&ui, 0, 6) < 1e-4 && peak(&ui, 1, 6) < 1e-4, "la sala no oye la audición");
    assert_eq!(req(&c, ClientRequest::AuditionStop), ServerResponse::Ok);
    // Un pad de Carts sigue saliendo por la sala.
    assert!(matches!(req(&c, ClientRequest::AdhocLoad { slot: 0, path: daemon_wav_path(&c), gain_db: 0.0, monitor: false, start_secs: 0.0 }), ServerResponse::AdhocLoaded { .. }));
    assert_eq!(req(&c, ClientRequest::AdhocPlay { slot: 0 }), ServerResponse::Ok);
    wait_until("el pad suena en la sala", || peak(&ui, 0, 3) > 0.2);
    assert_eq!(daemon.health().rt_alloc_violations.load(Ordering::Relaxed), 0);
}

/// Ruta del audio de la cue 1 del show cargado.
fn daemon_wav_path(c: &IpcClient) -> String {
    let (node, _) = detail(c, 1);
    node.basics.file_target.expect("la cue 1 tiene archivo").absolute_path
}

#[test]
fn the_equalizer_changes_the_sound_of_a_cue_that_is_already_playing_and_of_one_started_afterwards() {
    use qlab_protocol::cue_dto::{EqBandData, EqFilterType};
    let f = fixture("eqlive", RingProducer::IpcServer, |files| {
        let b = write_wav("eqlive-b", 2, 48_000, 48_000, |_, _| 0.25);
        files.push(b.clone());
        vec![audio_node(1, Some(&b), true)]
    });
    let c = IpcClient::connect(&f.socket).unwrap();
    req(&c, ClientRequest::Subscribe { events: true });
    let ui = SharedMemory::open(f.daemon.shm().name()).unwrap();
    req(&c, ClientRequest::Go);
    wait_until("suena sin EQ", || peak(&ui, 0, 1) > 0.24);

    // Una señal constante no pasa por un corte de graves a 1 kHz: con el EQ activo en vivo la salida se apaga.
    let (mut node, _) = detail(&c, 1);
    let cut = EqBandData { filter_type: EqFilterType::LowCut, frequency_hz: 1_000.0, gain_db: 0.0, q_factor: 0.707, enabled: true };
    if let CueBody::Audio(a) = &mut node.body {
        a.eq.enabled = true;
        a.eq.bands[0] = cut;
    }
    assert_eq!(req(&c, ClientRequest::UpdateCue { node: node.clone() }), ServerResponse::Ok);
    wait_until("el EQ en vivo corta la señal", || peak(&ui, 0, 6) < 0.05);

    // Bypass: vuelve a sonar entera.
    if let CueBody::Audio(a) = &mut node.body {
        a.eq.enabled = false;
    }
    assert_eq!(req(&c, ClientRequest::UpdateCue { node }), ServerResponse::Ok);
    wait_until("sin EQ vuelve", || peak(&ui, 0, 3) > 0.24);

    // Y el EQ guardado en la cue se aplica al disparar de nuevo (parar y volver a empezar).
    assert_eq!(req(&c, ClientRequest::HardStopAll), ServerResponse::Ok);
    wait_until("silencio", || peak(&ui, 0, 4) == 0.0);
    let (mut node, _) = detail(&c, 1);
    if let CueBody::Audio(a) = &mut node.body {
        a.eq.enabled = true;
        a.eq.bands[0] = cut;
    }
    assert_eq!(req(&c, ClientRequest::UpdateCue { node }), ServerResponse::Ok);
    req(&c, ClientRequest::SetPlayhead { cue_id: Some(1) });
    req(&c, ClientRequest::Go);
    c.wait_event(TIMEOUT, |e| matches!(e, ServerEvent::CueStateChanged { cue: 1, state } if state == "Running")).expect("la cue suena");
    std::thread::sleep(Duration::from_millis(300));
    let _ = peak(&ui, 0, 3); // el medidor guarda el pico desde la última lectura: se vacía el golpe inicial del arranque
    assert_eq!(detail(&c, 1).1, "Running", "sigue sonando (con el EQ puesto no hay señal, pero la cue corre)");
    assert!(peak(&ui, 0, 10) < 0.05, "arranca con el EQ puesto: la señal constante no pasa");
    // Y sin EQ, la misma cue disparada igual sí suena: el silencio de arriba es el EQ, no que no haya nada.
    let (mut node, _) = detail(&c, 1);
    if let CueBody::Audio(a) = &mut node.body {
        a.eq.enabled = false;
    }
    assert_eq!(req(&c, ClientRequest::UpdateCue { node }), ServerResponse::Ok);
    wait_until("al apagar el EQ suena", || peak(&ui, 0, 3) > 0.24);
}

#[test]
fn an_equalizer_saved_before_the_first_go_is_already_in_effect_when_the_cue_starts() {
    use qlab_protocol::cue_dto::{EqBandData, EqFilterType};
    let f = fixture("eqfirst", RingProducer::IpcServer, |files| {
        let b = write_wav("eqfirst-b", 2, 48_000, 48_000, |_, _| 0.25);
        files.push(b.clone());
        vec![audio_node(1, Some(&b), true)]
    });
    let c = IpcClient::connect(&f.socket).unwrap();
    req(&c, ClientRequest::Subscribe { events: true });
    let ui = SharedMemory::open(f.daemon.shm().name()).unwrap();
    let (mut node, _) = detail(&c, 1);
    if let CueBody::Audio(a) = &mut node.body {
        a.eq.enabled = true;
        a.eq.bands[0] = EqBandData { filter_type: EqFilterType::LowCut, frequency_hz: 1_000.0, gain_db: 0.0, q_factor: 0.707, enabled: true };
    }
    assert_eq!(req(&c, ClientRequest::UpdateCue { node }), ServerResponse::Ok);
    req(&c, ClientRequest::Go);
    c.wait_event(TIMEOUT, |e| matches!(e, ServerEvent::CueStateChanged { cue: 1, state } if state == "Running")).expect("la cue suena");
    std::thread::sleep(Duration::from_millis(300));
    let _ = peak(&ui, 0, 3); // vaciar el golpe inicial del arranque
    let p = peak(&ui, 0, 10);
    assert!(p < 0.05, "con el EQ guardado antes del GO, la señal constante no pasa (pico {p})");
}

/// Un mp4 de 3 s con imagen negra y un tono de `hz` en el audio (hace falta `ffmpeg`).
fn make_mp4_with_tone(tag: &str, hz: u32) -> Option<std::path::PathBuf> {
    let out = temp_path(tag, "mp4");
    let ok = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-f", "lavfi", "-i", "color=c=black:s=64x64:d=3:r=10", "-f", "lavfi", "-i"])
        .arg(format!("sine=f={hz}:d=3:sample_rate=48000"))
        .args(["-c:v", "libx264", "-pix_fmt", "yuv420p", "-c:a", "aac", "-b:a", "192k", "-shortest"])
        .arg(&out)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    ok.then_some(out)
}

#[test]
fn the_equalizer_also_works_on_a_video_cue() {
    use qlab_protocol::cue_dto::{EqBandData, EqFilterType};
    let Some(mp4) = make_mp4_with_tone("eqvideo", 100) else {
        eprintln!("sin ffmpeg: se omite");
        return;
    };
    let f = fixture("eqvideo", RingProducer::IpcServer, |files| {
        files.push(mp4.clone());
        let mut node = audio_node(1, Some(&mp4), true);
        let CueBody::Audio(a) = node.body.clone() else { unreachable!() };
        node.body = CueBody::Video(qlab_protocol::show_format::VideoBody { audio: a, fit: Default::default() });
        vec![node]
    });
    let c = IpcClient::connect(&f.socket).unwrap();
    req(&c, ClientRequest::Subscribe { events: true });
    let ui = SharedMemory::open(f.daemon.shm().name()).unwrap();
    req(&c, ClientRequest::Go);
    wait_until("suena el video sin EQ", || peak(&ui, 0, 1) > 0.08);
    let (mut node, _) = detail(&c, 1);
    let CueBody::Video(v) = &mut node.body else { panic!("{:?}", node.body) };
    v.audio.eq.enabled = true;
    v.audio.eq.bands[0] = EqBandData { filter_type: EqFilterType::LowCut, frequency_hz: 2_000.0, gain_db: 0.0, q_factor: 0.707, enabled: true };
    assert_eq!(req(&c, ClientRequest::UpdateCue { node: node.clone() }), ServerResponse::Ok);
    std::thread::sleep(Duration::from_millis(300));
    let _ = peak(&ui, 0, 3);
    let p = peak(&ui, 0, 10);
    assert!(p < 0.02, "el EQ de una cue de video corta los graves (pico {p}; sin EQ ~0,125)");
}

#[test]
fn the_headphone_audition_plays_with_the_cue_equalizer_and_follows_it_while_it_is_edited() {
    use qlab_protocol::cue_dto::{EqBandData, EqFilterType};
    let tag = "pflq";
    let wav = write_wav(&format!("{tag}-w"), 2, 480_000, 48_000, |_, _| 0.25); // 10 s de señal constante
    let show = write_show(tag, vec![audio_node(1, Some(&wav), false)]);
    let mut cfg = config(tag);
    cfg.backend.channels = 4;
    cfg.monitor_first = Some(2);
    cfg.show = Some(show.clone());
    let socket = cfg.socket.clone();
    let daemon = Daemon::start(cfg).expect("el daemon debe arrancar");
    let _cleanup = Cleanup(vec![wav, show]);
    let c = IpcClient::connect(&socket).unwrap();
    let ui = SharedMemory::open(daemon.shm().name()).unwrap();
    let cut = EqBandData { filter_type: EqFilterType::LowCut, frequency_hz: 1_000.0, gain_db: 0.0, q_factor: 0.707, enabled: true };

    // Sin EQ la audición suena entera.
    assert!(matches!(req(&c, ClientRequest::Audition { cue_id: 1, start_secs: None }), ServerResponse::AdhocLoaded { .. }));
    wait_until("suena en los auriculares", || peak(&ui, 2, 2) > 0.2);

    // Se edita el EQ de la cue mientras se la escucha: la audición lo sigue en vivo.
    let (mut node, _) = detail(&c, 1);
    if let CueBody::Audio(a) = &mut node.body {
        a.eq.enabled = true;
        a.eq.bands[0] = cut;
    }
    assert_eq!(req(&c, ClientRequest::UpdateCue { node: node.clone() }), ServerResponse::Ok);
    std::thread::sleep(Duration::from_millis(300));
    let _ = peak(&ui, 2, 3);
    let p = peak(&ui, 2, 10);
    assert!(p < 0.05, "el PFL sigue el EQ editado en vivo (pico {p})");

    // Y una audición nueva arranca ya con el EQ de la cue puesto.
    assert_eq!(req(&c, ClientRequest::AuditionStop), ServerResponse::Ok);
    wait_until("silencio", || peak(&ui, 2, 4) < 1e-4);
    assert!(matches!(req(&c, ClientRequest::Audition { cue_id: 1, start_secs: None }), ServerResponse::AdhocLoaded { .. }));
    std::thread::sleep(Duration::from_millis(300));
    let _ = peak(&ui, 2, 3);
    let p = peak(&ui, 2, 10);
    assert!(p < 0.05, "la audición nueva arranca con el EQ puesto (pico {p})");

    // Con el EQ apagado vuelve a sonar entera.
    if let CueBody::Audio(a) = &mut node.body {
        a.eq.enabled = false;
    }
    assert_eq!(req(&c, ClientRequest::UpdateCue { node }), ServerResponse::Ok);
    wait_until("sin EQ suena", || peak(&ui, 2, 3) > 0.2);
}

/// El EQ en vivo con distintas placas: frecuencia, tamaño de buffer y canales como los de una PC de cabina con Windows
/// (44,1 o 48 kHz, buffers de 480 o 1056 frames, 2 u 8 salidas) y un archivo de 48 kHz (a 44,1 kHz se remuestrea).
#[test]
fn the_equalizer_is_heard_with_the_sample_rates_buffers_and_channel_counts_of_real_sound_cards() {
    use qlab_protocol::cue_dto::{EqBandData, EqFilterType};
    for (rate, buffer, channels) in [(44_100u32, 480u32, 2u16), (48_000, 1_056, 2), (44_100, 256, 8), (96_000, 128, 2), (48_000, 480, 6)] {
        let tag = format!("eqcard{rate}-{buffer}-{channels}");
        let wav = write_wav(&format!("{tag}-w"), 2, 480_000, 48_000, |_, _| 0.25);
        let show = write_show(&tag, vec![audio_node(1, Some(&wav), true)]);
        let mut cfg = config(&tag);
        cfg.backend.sample_rate = rate;
        cfg.backend.buffer_frames = buffer;
        cfg.backend.channels = channels;
        cfg.show = Some(show.clone());
        let socket = cfg.socket.clone();
        let daemon = Daemon::start(cfg).expect("el daemon debe arrancar");
        let _cleanup = Cleanup(vec![wav, show]);
        let c = IpcClient::connect(&socket).unwrap();
        req(&c, ClientRequest::Subscribe { events: true });
        let ui = SharedMemory::open(daemon.shm().name()).unwrap();
        req(&c, ClientRequest::Go);
        wait_until(&format!("{tag}: suena sin EQ"), || peak(&ui, 0, 1) > 0.2);
        let (mut node, _) = detail(&c, 1);
        if let CueBody::Audio(a) = &mut node.body {
            a.eq.enabled = true;
            a.eq.bands[0] = EqBandData { filter_type: EqFilterType::LowCut, frequency_hz: 1_000.0, gain_db: 0.0, q_factor: 0.707, enabled: true };
        }
        assert_eq!(req(&c, ClientRequest::UpdateCue { node }), ServerResponse::Ok);
        std::thread::sleep(Duration::from_millis(400));
        let _ = peak(&ui, 0, 3);
        let p = peak(&ui, 0, 10);
        assert!(p < 0.05, "{tag}: el EQ en vivo corta la señal (pico {p})");
    }
}
