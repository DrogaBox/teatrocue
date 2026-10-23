//! Enlace con el daemon. Un hilo propio mantiene la conexión IPC y la memoria compartida; la UI
//! solo manda órdenes y recoge resultados por canales, así que **nunca espera por el daemon**.
//!
//! * Reintenta la conexión cada segundo mientras no la tenga, y detecta si el daemon desaparece o
//!   deja de contestar.
//! * Reenvía a la UI los eventos del daemon y las respuestas (con la etiqueta de la petición).
//! * Vigila la memoria compartida con sondeos baratos (cada 50 ms) y despierta a la UI solo cuando
//!   algo visible cambia: así la ventana consume 0 % de CPU en reposo.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use qlab_daemon::shm::SharedMemory;
use qlab_protocol::ipc_client::IpcClient;
use qlab_protocol::ipc_msg::{ClientRequest, ServerEvent, ServerResponse, IPC_PROTOCOL_VERSION};

use crate::model::Telemetry;

/// Espera máxima por una respuesta del daemon.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(3);

/// Peticiones que pueden tardar (cargar o guardar shows, copiar medios al empaquetar).
fn timeout_for(request: &ClientRequest) -> Duration {
    match request {
        ClientRequest::BundleShow { .. } => Duration::from_secs(900),
        ClientRequest::LoadShow { .. } | ClientRequest::NewShow | ClientRequest::SaveShow { .. } => Duration::from_secs(30),
        _ => REQUEST_TIMEOUT,
    }
}
const RETRY_EVERY: Duration = Duration::from_secs(1);
const PING_EVERY: Duration = Duration::from_secs(2);
/// Cada cuánto se sondea la memoria compartida mientras la UI duerme.
const WATCH_EVERY: Duration = Duration::from_millis(50);
/// Si el latido del secuenciador (1 kHz) no cambia durante este tiempo, el daemon está muerto o colgado.
const HEARTBEAT_TIMEOUT: Duration = Duration::from_millis(1_500);
const LOOP_TICK: Duration = Duration::from_millis(25);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HelloInfo {
    pub shm_name: String,
    pub sample_rate: u32,
    pub channels: u16,
    pub backend: String,
}

pub enum LinkCommand {
    /// Petición cuya respuesta vuelve como `LinkEvent::Reply` con la misma etiqueta.
    Tagged { tag: u64, request: ClientRequest },
    /// Petición sin respuesta que seguir (los errores llegan como `LinkEvent::Failure`).
    Fire(ClientRequest),
    /// Reintentar ya (cancela la espera entre intentos).
    RetryNow,
}

pub enum LinkEvent {
    Connected { hello: HelloInfo, shm: Option<Arc<SharedMemory>> },
    Disconnected { reason: String },
    Reply { tag: u64, response: Result<ServerResponse, String> },
    /// Un `Fire` que el daemon rechazó o que no llegó.
    Failure(String),
    Server(ServerEvent),
}

pub struct DaemonLink {
    tx: Sender<LinkCommand>,
    rx: Receiver<LinkEvent>,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl DaemonLink {
    /// `wake` se invoca desde el hilo del enlace cuando hay algo nuevo que dibujar.
    pub fn spawn(socket: String, wake: impl Fn() + Send + Sync + 'static) -> Self {
        let (tx, cmd_rx) = channel();
        let (ev_tx, rx) = channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::clone(&stop);
        let join = std::thread::Builder::new()
            .name("qlab-ui-link".into())
            .spawn(move || Worker::new(socket, cmd_rx, ev_tx, Arc::new(wake), stop_flag).run())
            .expect("no se pudo crear el hilo del enlace");
        Self { tx, rx, stop, join: Some(join) }
    }

    pub fn request(&self, tag: u64, request: ClientRequest) {
        let _ = self.tx.send(LinkCommand::Tagged { tag, request });
    }

    pub fn fire(&self, request: ClientRequest) {
        let _ = self.tx.send(LinkCommand::Fire(request));
    }

    pub fn retry_now(&self) {
        let _ = self.tx.send(LinkCommand::RetryNow);
    }

    /// Todo lo que haya llegado, sin esperar.
    pub fn poll(&self) -> Vec<LinkEvent> {
        self.rx.try_iter().collect()
    }
}

impl Drop for DaemonLink {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

struct Connection {
    client: IpcClient,
    shm: Option<Arc<SharedMemory>>,
    channels: usize,
    last_ping: Instant,
    last_signature: (u64, u64, bool, bool),
    /// Último latido del secuenciador visto en la memoria compartida y desde cuándo no cambia.
    last_heartbeat: u64,
    heartbeat_changed: Instant,
}

struct Worker {
    socket: String,
    commands: Receiver<LinkCommand>,
    events: Sender<LinkEvent>,
    wake: Arc<dyn Fn() + Send + Sync>,
    stop: Arc<AtomicBool>,
    conn: Option<Connection>,
    last_attempt: Option<Instant>,
    last_watch: Instant,
    reported_down: bool,
}

impl Worker {
    fn new(socket: String, commands: Receiver<LinkCommand>, events: Sender<LinkEvent>, wake: Arc<dyn Fn() + Send + Sync>, stop: Arc<AtomicBool>) -> Self {
        Self { socket, commands, events, wake, stop, conn: None, last_attempt: None, last_watch: Instant::now(), reported_down: false }
    }

    fn emit(&self, e: LinkEvent) {
        let _ = self.events.send(e);
        (self.wake)();
    }

    fn run(mut self) {
        while !self.stop.load(Ordering::Acquire) {
            if self.conn.is_none() {
                self.try_connect();
            }
            // Órdenes de la UI: se vacía la cola entera y los arrastres de fader se fusionan.
            match self.commands.recv_timeout(LOOP_TICK) {
                Ok(first) => {
                    let mut batch = vec![first];
                    batch.extend(self.commands.try_iter());
                    for cmd in coalesce(batch) {
                        self.handle(cmd);
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            self.pump_server_events();
            self.watch();
        }
    }

    fn handle(&mut self, cmd: LinkCommand) {
        match cmd {
            LinkCommand::RetryNow => {
                if self.conn.is_none() {
                    self.last_attempt = None;
                    self.try_connect();
                }
            }
            LinkCommand::Tagged { tag, request } => {
                let result = self.send(request);
                self.emit(LinkEvent::Reply { tag, response: result });
            }
            LinkCommand::Fire(request) => match self.send(request) {
                Ok(ServerResponse::Error { message }) => self.emit(LinkEvent::Failure(message)),
                Ok(_) => {}
                Err(e) => self.emit(LinkEvent::Failure(e)),
            },
        }
    }

    /// Una petición. Si la conexión se pierde, lo notifica y la descarta.
    fn send(&mut self, request: ClientRequest) -> Result<ServerResponse, String> {
        let Some(conn) = &self.conn else { return Err("sin conexión con el daemon".into()) };
        let timeout = timeout_for(&request);
        match conn.client.request(request, timeout) {
            Ok(r) => Ok(r),
            Err(e) => {
                let lost = matches!(e, qlab_protocol::ipc_client::ClientError::Disconnected | qlab_protocol::ipc_client::ClientError::Io(_));
                if lost || conn.client.is_closed() {
                    self.lose(e.to_string());
                }
                Err(e.to_string())
            }
        }
    }

    fn lose(&mut self, reason: String) {
        if self.conn.take().is_some() {
            self.reported_down = true;
            self.last_attempt = Some(Instant::now());
            self.emit(LinkEvent::Disconnected { reason });
        }
    }

    fn try_connect(&mut self) {
        if self.last_attempt.is_some_and(|t| t.elapsed() < RETRY_EVERY) {
            return;
        }
        self.last_attempt = Some(Instant::now());
        let wake = Arc::clone(&self.wake);
        let client = match IpcClient::connect_with_notifier(&self.socket, move || wake()) {
            Ok(c) => c,
            Err(e) => {
                if !self.reported_down {
                    self.reported_down = true;
                    self.emit(LinkEvent::Disconnected { reason: format!("no se pudo conectar a {}: {e}", self.socket) });
                }
                return;
            }
        };
        let hello = match client.request(ClientRequest::Hello { client: "qlab-ui".into(), protocol: IPC_PROTOCOL_VERSION }, REQUEST_TIMEOUT) {
            Ok(ServerResponse::Hello { shm_name, sample_rate, channels, backend, .. }) => HelloInfo { shm_name, sample_rate, channels, backend },
            Ok(ServerResponse::Error { message }) => {
                self.emit(LinkEvent::Disconnected { reason: format!("el daemon rechazó la conexión: {message}") });
                self.reported_down = true;
                return;
            }
            Ok(other) => {
                self.emit(LinkEvent::Disconnected { reason: format!("respuesta inesperada del daemon: {other:?}") });
                self.reported_down = true;
                return;
            }
            Err(e) => {
                self.emit(LinkEvent::Disconnected { reason: format!("el daemon no contesta: {e}") });
                self.reported_down = true;
                return;
            }
        };
        let _ = client.request(ClientRequest::Subscribe { events: true }, REQUEST_TIMEOUT);
        let shm = SharedMemory::open(&hello.shm_name).ok().map(Arc::new);
        self.reported_down = false;
        self.conn = Some(Connection { client, shm: shm.clone(), channels: hello.channels as usize, last_ping: Instant::now(), last_signature: (0, 0, false, false), last_heartbeat: 0, heartbeat_changed: Instant::now() });
        self.emit(LinkEvent::Connected { hello, shm });
    }

    fn pump_server_events(&mut self) {
        let Some(conn) = &self.conn else { return };
        let events = conn.client.drain_events();
        let closed = conn.client.is_closed();
        for e in events {
            self.emit(LinkEvent::Server(e));
        }
        if closed {
            self.lose("el daemon cerró la conexión".into());
        }
    }

    /// Sondeo periódico: latido hacia el daemon y huella de actividad de la memoria compartida.
    fn watch(&mut self) {
        if self.last_watch.elapsed() < WATCH_EVERY {
            return;
        }
        self.last_watch = Instant::now();
        let mut wake_ui = false;
        let mut ping = false;
        let mut dead = None;
        if let Some(conn) = &mut self.conn {
            if let Some(shm) = &conn.shm {
                let hb = shm.telemetry().last_heartbeat_ns();
                if hb != conn.last_heartbeat {
                    conn.last_heartbeat = hb;
                    conn.heartbeat_changed = Instant::now();
                } else if conn.heartbeat_changed.elapsed() >= HEARTBEAT_TIMEOUT {
                    dead = Some("el daemon dejó de latir".to_string());
                }
                let sig = Telemetry::activity_signature(shm.telemetry(), conn.channels);
                if sig != conn.last_signature {
                    conn.last_signature = sig;
                    wake_ui = true;
                }
            }
            if conn.last_ping.elapsed() >= PING_EVERY {
                conn.last_ping = Instant::now();
                ping = true;
            }
        }
        if let Some(reason) = dead {
            // Al soltar la conexión se cierra el socket aunque el daemon no lo haya hecho.
            self.lose(reason);
            return;
        }
        if wake_ui {
            (self.wake)();
        }
        if ping {
            // Detecta un daemon colgado (no contesta) o desaparecido.
            if let Err(e) = self.send(ClientRequest::Ping { nonce: 0 }) {
                self.lose(format!("el daemon dejó de responder: {e}"));
            }
        }
    }
}

/// Fusiona los arrastres de fader repetidos sobre la misma celda (solo importa el último valor),
/// conservando el orden del resto.
fn coalesce(batch: Vec<LinkCommand>) -> Vec<LinkCommand> {
    let key = |c: &LinkCommand| match c {
        LinkCommand::Fire(ClientRequest::FaderDrag { cue_id, row, col, .. }) => Some((*cue_id, *row, *col)),
        _ => None,
    };
    let mut out: Vec<LinkCommand> = Vec::with_capacity(batch.len());
    for cmd in batch {
        if let Some(k) = key(&cmd) {
            if let Some(slot) = out.iter_mut().rev().take_while(|c| key(c).is_some()).find(|c| key(c) == Some(k)) {
                *slot = cmd;
                continue;
            }
        }
        out.push(cmd);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drag(cue: u64, row: u8, db: f32) -> LinkCommand {
        LinkCommand::Fire(ClientRequest::FaderDrag { cue_id: cue, row, col: 1, db })
    }

    #[test]
    fn fader_drags_are_coalesced_per_cell_but_ordering_is_kept() {
        let batch = vec![
            drag(1, 1, -1.0),
            drag(1, 1, -2.0),
            drag(1, 2, -5.0),
            drag(1, 1, -3.0),
            LinkCommand::Fire(ClientRequest::Go),
            drag(1, 1, -4.0), // tras un GO no se fusiona hacia atrás
        ];
        let out = coalesce(batch);
        let dbs: Vec<_> = out
            .iter()
            .map(|c| match c {
                LinkCommand::Fire(ClientRequest::FaderDrag { row, db, .. }) => format!("drag{row}:{db}"),
                LinkCommand::Fire(ClientRequest::Go) => "go".into(),
                _ => "otro".into(),
            })
            .collect();
        assert_eq!(dbs, vec!["drag1:-3", "drag2:-5", "go", "drag1:-4"]);
    }

    #[test]
    fn a_dead_socket_is_reported_once_and_retried() {
        let woke = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let w = Arc::clone(&woke);
        let link = DaemonLink::spawn(format!("/tmp/qlab-ui-test-missing-{}.sock", std::process::id()), move || {
            w.fetch_add(1, Ordering::Relaxed);
        });
        let t0 = Instant::now();
        let mut downs = 0;
        while t0.elapsed() < Duration::from_millis(2_500) {
            for e in link.poll() {
                if let LinkEvent::Disconnected { reason } = e {
                    assert!(reason.contains("no se pudo conectar"), "{reason}");
                    downs += 1;
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(downs, 1, "se avisa una sola vez aunque reintente cada segundo");
        assert!(woke.load(Ordering::Relaxed) >= 1);
        link.request(7, ClientRequest::GetCues);
        std::thread::sleep(Duration::from_millis(150));
        let replies: Vec<_> = link.poll();
        assert!(replies.iter().any(|e| matches!(e, LinkEvent::Reply { tag: 7, response: Err(_) })), "sin conexión, la petición falla al instante");
    }

    #[test]
    fn dropping_the_link_stops_the_thread_promptly() {
        let t0 = Instant::now();
        drop(DaemonLink::spawn("/tmp/qlab-ui-test-drop.sock".into(), || {}));
        assert!(t0.elapsed() < Duration::from_millis(500), "{:?}", t0.elapsed());
    }
}
