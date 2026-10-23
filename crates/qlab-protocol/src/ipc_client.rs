//! Cliente del canal de control local: lo usan la UI, las herramientas y las pruebas.
//!
//! Un hilo lector separa las respuestas (por `id`) de los eventos, de modo que `request` puede
//! esperar su respuesta mientras los eventos se acumulan en [`IpcClient::events`].

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Write};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use interprocess::local_socket::{prelude::*, GenericFilePath, Stream};

use crate::ipc_msg::{ClientMessage, ClientRequest, ServerEvent, ServerLine, ServerResponse};

/// Nombre local (ruta de socket Unix o `\\.\pipe\…` en Windows) → `Name` de `interprocess`.
pub fn socket_name(path: &str) -> io::Result<interprocess::local_socket::Name<'static>> {
    path.to_string().to_fs_name::<GenericFilePath>()
}

#[derive(Debug)]
pub enum ClientError {
    Io(io::Error),
    Json(serde_json::Error),
    Timeout,
    Disconnected,
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "error de E/S: {e}"),
            Self::Json(e) => write!(f, "JSON inválido: {e}"),
            Self::Timeout => write!(f, "tiempo de espera agotado"),
            Self::Disconnected => write!(f, "conexión cerrada"),
        }
    }
}

impl std::error::Error for ClientError {}

impl From<io::Error> for ClientError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

type Pending = Arc<Mutex<HashMap<u64, Sender<ServerResponse>>>>;

pub struct IpcClient {
    writer: Mutex<<Stream as interprocess::local_socket::traits::Stream>::SendHalf>,
    pending: Pending,
    events: Mutex<Receiver<ServerEvent>>,
    next_id: Mutex<u64>,
    closed: Arc<std::sync::atomic::AtomicBool>,
}

impl IpcClient {
    pub fn connect(path: &str) -> Result<Self, ClientError> {
        Self::connect_with_notifier(path, || {})
    }

    /// Como [`IpcClient::connect`], y `notify` se invoca desde el hilo lector cada vez que llega un
    /// evento o se cierra la conexión (la UI lo usa para pedir un repintado).
    pub fn connect_with_notifier(path: &str, notify: impl Fn() + Send + 'static) -> Result<Self, ClientError> {
        let stream = Stream::connect(socket_name(path)?)?;
        let (recv, send) = stream.split();
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (ev_tx, events) = mpsc::channel();
        let pending_reader = Arc::clone(&pending);
        let closed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let closed_reader = Arc::clone(&closed);
        std::thread::Builder::new().name("qlab-ipc-client".into()).spawn(move || {
            let mut lines = BufReader::new(recv).lines();
            while let Some(Ok(line)) = lines.next() {
                match serde_json::from_str::<ServerLine>(&line) {
                    Ok(ServerLine::Response(r)) => {
                        if let Some(tx) = pending_reader.lock().expect("envenenado").remove(&r.id) {
                            let _ = tx.send(r.response);
                        }
                    }
                    Ok(ServerLine::Event(e)) => {
                        let _ = ev_tx.send(e);
                        notify();
                    }
                    Err(_) => {}
                }
            }
            // Al cerrarse la conexión se sueltan los remitentes y los `request` pendientes fallan.
            pending_reader.lock().expect("envenenado").clear();
            closed_reader.store(true, std::sync::atomic::Ordering::Release);
            notify();
        })?;
        Ok(Self { writer: Mutex::new(send), pending, events: Mutex::new(events), next_id: Mutex::new(1), closed })
    }

    /// Envía `request` y espera su respuesta.
    pub fn request(&self, request: ClientRequest, timeout: Duration) -> Result<ServerResponse, ClientError> {
        let id = {
            let mut n = self.next_id.lock().expect("envenenado");
            let id = *n;
            *n += 1;
            id
        };
        let (tx, rx) = mpsc::channel();
        self.pending.lock().expect("envenenado").insert(id, tx);
        let line = serde_json::to_string(&ClientMessage { id, request }).map_err(ClientError::Json)?;
        {
            let mut w = self.writer.lock().expect("envenenado");
            let sent = w.write_all(line.as_bytes()).and_then(|_| w.write_all(b"\n")).and_then(|_| w.flush());
            if let Err(e) = sent {
                self.pending.lock().expect("envenenado").remove(&id);
                return Err(e.into());
            }
        }
        match rx.recv_timeout(timeout) {
            Ok(r) => Ok(r),
            Err(RecvTimeoutError::Timeout) => {
                self.pending.lock().expect("envenenado").remove(&id);
                Err(ClientError::Timeout)
            }
            Err(RecvTimeoutError::Disconnected) => Err(ClientError::Disconnected),
        }
    }

    /// Envía una línea cruda (para probar la robustez del servidor ante basura).
    /// ¿Sigue abierta la conexión? (falso en cuanto el hilo lector la ve cerrada)
    pub fn is_closed(&self) -> bool {
        self.closed.load(std::sync::atomic::Ordering::Acquire)
    }

    pub fn send_raw(&self, line: &str) -> io::Result<()> {
        let mut w = self.writer.lock().expect("envenenado");
        w.write_all(line.as_bytes())?;
        w.write_all(b"\n")?;
        w.flush()
    }

    /// Eventos ya recibidos (tras `subscribe`), sin esperar.
    pub fn drain_events(&self) -> Vec<ServerEvent> {
        self.events.lock().expect("envenenado").try_iter().collect()
    }

    /// Espera un evento que cumpla `pred` hasta `timeout`, descartando los anteriores.
    pub fn wait_event(&self, timeout: Duration, mut pred: impl FnMut(&ServerEvent) -> bool) -> Option<ServerEvent> {
        let end = Instant::now() + timeout;
        let events = self.events.lock().expect("envenenado");
        loop {
            let left = end.checked_duration_since(Instant::now())?;
            match events.recv_timeout(left) {
                Ok(e) if pred(&e) => return Some(e),
                Ok(_) => {}
                Err(_) => return None,
            }
        }
    }
}
