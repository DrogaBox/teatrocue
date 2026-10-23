//! Control remoto por OSC sobre UDP (el mismo protocolo que usan QLab, TouchOSC, Bitfocus Companion o un Stream Deck).
//!
//! Es opcional (`--osc-port`) porque cualquiera en la red que conozca el puerto puede disparar la función.
//! Direcciones: `/go`, `/panic`, `/stop`, `/pause`, `/resume`, `/hardstop`, `/next`, `/prev`, `/cue/<número o id>/start` y por cue `/cue/<n>/stop`, `pause`, `resume`, `devamp`, `load`, `arm`, `disarm`.

use std::io;
use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use qlab_protocol::ipc_msg::{ClientRequest, ServerResponse};

use crate::ipc_server::IpcContext;

#[derive(Clone, Debug, PartialEq)]
pub enum OscArg {
    Int(i32),
    Float(f32),
    Str(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct OscMessage {
    pub addr: String,
    pub args: Vec<OscArg>,
}

fn padded(len: usize) -> usize {
    (len + 4) & !3
}

/// Cadena OSC (terminada en cero y rellena a múltiplos de 4) al inicio de `b`: `(texto, bytes consumidos)`.
fn read_str(b: &[u8]) -> Option<(String, usize)> {
    let end = b.iter().position(|&c| c == 0)?;
    let used = padded(end);
    if used > b.len() {
        return None;
    }
    Some((std::str::from_utf8(&b[..end]).ok()?.to_string(), used))
}

fn parse_message(b: &[u8]) -> Option<OscMessage> {
    let (addr, mut at) = read_str(b)?;
    if !addr.starts_with('/') {
        return None;
    }
    // Un mensaje sin lista de tipos (OSC antiguo) vale como mensaje sin argumentos.
    if at >= b.len() {
        return Some(OscMessage { addr, args: vec![] });
    }
    let (tags, used) = read_str(&b[at..])?;
    at += used;
    let tags = tags.strip_prefix(',')?;
    let mut args = Vec::new();
    for t in tags.chars() {
        match t {
            'i' => {
                args.push(OscArg::Int(i32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?)));
                at += 4;
            }
            'f' => {
                args.push(OscArg::Float(f32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?)));
                at += 4;
            }
            's' => {
                let (s, used) = read_str(b.get(at..)?)?;
                args.push(OscArg::Str(s));
                at += used;
            }
            // Sin datos: verdadero, falso, nulo, impulso.
            'T' | 'F' | 'N' | 'I' => {}
            _ => return None,
        }
    }
    Some(OscMessage { addr, args })
}

/// Mensajes de un paquete UDP (uno suelto o un `#bundle`, que puede anidar otros).
pub fn parse_packet(b: &[u8]) -> Vec<OscMessage> {
    fn walk(b: &[u8], out: &mut Vec<OscMessage>, depth: u8) {
        if b.starts_with(b"#bundle\0") {
            if depth > 4 {
                return;
            }
            let mut at = 16; // etiqueta + tiempo
            while at + 4 <= b.len() {
                let size = u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]) as usize;
                at += 4;
                let Some(element) = b.get(at..at + size) else { return };
                walk(element, out, depth + 1);
                at += size;
            }
        } else if let Some(m) = parse_message(b) {
            out.push(m);
        }
    }
    let mut out = Vec::new();
    walk(b, &mut out, 0);
    out
}

/// Qué hay que hacer con un mensaje.
#[derive(Debug, PartialEq)]
pub enum Action {
    Request(ClientRequest),
    /// Disparar la cue con ese número (o id).
    FireCue(String),
    /// Mover el playhead `n` cues de nivel superior (negativo = hacia atrás) sin disparar.
    Step(i32),
    /// Acción sobre una cue concreta: `/cue/<n>/stop`, `pause`, `resume`, `devamp`, `load`, `arm`, `disarm`.
    CueAction(String, qlab_protocol::show_format::ControlActionDto),
}

pub fn route(m: &OscMessage) -> Option<Action> {
    let parts: Vec<&str> = m.addr.trim_matches('/').split('/').collect();
    match parts.as_slice() {
        ["go"] => Some(Action::Request(ClientRequest::Go)),
        ["panic"] => Some(Action::Request(ClientRequest::Panic)),
        ["stop"] => Some(Action::Request(ClientRequest::StopAll)),
        ["pause"] => Some(Action::Request(ClientRequest::PauseAll)),
        ["resume"] => Some(Action::Request(ClientRequest::ResumeAll)),
        ["hardstop"] => Some(Action::Request(ClientRequest::HardStopAll)),
        ["master"] => match m.args.first() {
            Some(OscArg::Float(db)) => Some(Action::Request(ClientRequest::SetMasterDb { db: *db })),
            Some(OscArg::Int(db)) => Some(Action::Request(ClientRequest::SetMasterDb { db: *db as f32 })),
            _ => None,
        },
        ["next"] => Some(Action::Step(1)),
        ["prev"] | ["previous"] => Some(Action::Step(-1)),
        ["cue", ident, "start"] if !ident.is_empty() => Some(Action::FireCue((*ident).to_string())),
        ["cue", ident, verb] if !ident.is_empty() => {
            use qlab_protocol::show_format::ControlActionDto as A;
            let action = match *verb {
                "stop" => A::Stop,
                "pause" => A::Pause,
                "resume" => A::Resume,
                "devamp" => A::Devamp,
                "load" => A::Load,
                "arm" => A::Arm,
                "disarm" => A::Disarm,
                _ => return None,
            };
            Some(Action::CueAction((*ident).to_string(), action))
        }
        _ => None,
    }
}

/// Ejecuta un mensaje contra el daemon. Devuelve `false` si no se entendió.
pub fn apply(ctx: &IpcContext, m: &OscMessage) -> bool {
    match route(m) {
        Some(Action::Request(r)) => {
            ctx.handle(r);
            true
        }
        Some(Action::FireCue(ident)) => {
            let ServerResponse::Cues { cues } = ctx.handle(ClientRequest::GetCues) else { return false };
            let Some(id) = cues.iter().find(|c| c.number == ident || c.id.to_string() == ident).map(|c| c.id) else { return false };
            ctx.handle(ClientRequest::SetPlayhead { cue_id: Some(id) });
            ctx.handle(ClientRequest::Go);
            true
        }
        Some(Action::CueAction(ident, action)) => {
            let ServerResponse::Cues { cues } = ctx.handle(ClientRequest::GetCues) else { return false };
            let Some(id) = cues.iter().find(|c| c.number == ident || c.id.to_string() == ident).map(|c| c.id) else { return false };
            ctx.handle(ClientRequest::CueAction { cue_id: id, action });
            true
        }
        Some(Action::Step(n)) => {
            let ServerResponse::Cues { cues } = ctx.handle(ClientRequest::GetCues) else { return false };
            let ServerResponse::Status(status) = ctx.handle(ClientRequest::GetStatus) else { return false };
            let top: Vec<u64> = cues.iter().filter(|c| c.parent.is_none()).map(|c| c.id).collect();
            match step_target(&top, status.playhead, n) {
                Some(id) => {
                    ctx.handle(ClientRequest::SetPlayhead { cue_id: Some(id) });
                    true
                }
                None => false,
            }
        }
        None => false,
    }
}

/// Cue a la que lleva mover el playhead `n` posiciones desde `current` (sin pasarse de los extremos).
pub fn step_target(top: &[u64], current: Option<u64>, n: i32) -> Option<u64> {
    if top.is_empty() {
        return None;
    }
    let at = current.and_then(|c| top.iter().position(|&x| x == c));
    let idx = match at {
        Some(i) => (i as i64 + n as i64).clamp(0, top.len() as i64 - 1),
        // Sin playhead: «siguiente» empieza por la primera; «anterior», por la última.
        None => if n >= 0 { 0 } else { top.len() as i64 - 1 },
    };
    Some(top[idx as usize])
}

// ---------------------------------------------------------------------------------------------
// Salida: cue de red
// ---------------------------------------------------------------------------------------------

/// Parte una línea en palabras respetando comillas dobles: `/x 1 "hola mundo"` → `["/x", "1", "hola mundo"]`.
fn tokens(line: &str) -> Vec<String> {
    let (mut out, mut cur, mut quoted, mut any) = (Vec::new(), String::new(), false, false);
    for ch in line.chars() {
        match ch {
            '"' => {
                quoted = !quoted;
                any = true;
            }
            c if c.is_whitespace() && !quoted => {
                if any {
                    out.push(std::mem::take(&mut cur));
                    any = false;
                }
            }
            c => {
                cur.push(c);
                any = true;
            }
        }
    }
    if any {
        out.push(cur);
    }
    out
}

fn push_padded(v: &mut Vec<u8>, s: &str) {
    v.extend(s.as_bytes());
    v.push(0);
    while v.len() % 4 != 0 {
        v.push(0);
    }
}

/// Arma el paquete OSC de una línea `/direccion arg arg…`: enteros, decimales o texto (entre comillas si lleva espacios).
pub fn encode_line(line: &str) -> Result<Vec<u8>, String> {
    let t = tokens(line);
    let addr = t.first().ok_or("el mensaje está vacío")?;
    if !addr.starts_with('/') {
        return Err(format!("la dirección OSC debe empezar con `/`: `{addr}`"));
    }
    let (mut tags, mut data) = (String::from(","), Vec::new());
    for a in &t[1..] {
        if let Ok(i) = a.parse::<i32>() {
            tags.push('i');
            data.extend(i.to_be_bytes());
        } else if let Ok(f) = a.parse::<f32>() {
            tags.push('f');
            data.extend(f.to_be_bytes());
        } else {
            tags.push('s');
            push_padded(&mut data, a);
        }
    }
    let mut v = Vec::new();
    push_padded(&mut v, addr);
    push_padded(&mut v, &tags);
    v.extend(data);
    Ok(v)
}

/// Envía en un hilo aparte (resolver un nombre puede tardar y no debe frenar al secuenciador). Los fallos salen como aviso.
pub fn send_async(destination: String, message: String, events: Arc<crate::events::EventHub>) {
    use std::net::ToSocketAddrs;
    let _ = std::thread::Builder::new().name("qlab-osc-out".into()).spawn(move || {
        let result = (|| -> Result<(), String> {
            let packet = encode_line(&message)?;
            let addr = destination.to_socket_addrs().map_err(|e| format!("destino `{destination}` inválido: {e}"))?.next().ok_or_else(|| format!("destino `{destination}` sin dirección"))?;
            let socket = UdpSocket::bind(if addr.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" }).map_err(|e| e.to_string())?;
            socket.send_to(&packet, addr).map(|_| ()).map_err(|e| format!("no se pudo enviar a {destination}: {e}"))
        })();
        if let Err(e) = result {
            events.publish(qlab_protocol::ipc_msg::ServerEvent::Warning { source: "osc".into(), message: format!("cue OSC: {e}") });
        }
    });
}

pub struct OscServer {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
    port: u16,
}

impl OscServer {
    /// Escucha en todas las interfaces (el control remoto es justamente desde otro equipo). `port` 0 = el sistema elige.
    pub fn start(port: u16, ctx: Arc<IpcContext>) -> io::Result<Self> {
        let socket = UdpSocket::bind(("0.0.0.0", port))?;
        socket.set_read_timeout(Some(Duration::from_millis(100)))?;
        let port = socket.local_addr()?.port();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let join = std::thread::Builder::new().name("qlab-osc".into()).spawn(move || {
            let mut buf = [0u8; 2048];
            while !flag.load(Ordering::Acquire) {
                if let Ok(n) = socket.recv(&mut buf) {
                    for m in parse_packet(&buf[..n]) {
                        apply(&ctx, &m);
                    }
                }
            }
        })?;
        Ok(Self { stop, join: Some(join), port })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

impl Drop for OscServer {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn osc(addr: &str, tags: &str, args: &[u8]) -> Vec<u8> {
        let pad = |s: &str| {
            let mut v = s.as_bytes().to_vec();
            v.push(0);
            while v.len() % 4 != 0 {
                v.push(0);
            }
            v
        };
        [pad(addr), pad(tags), args.to_vec()].concat()
    }

    #[test]
    fn parses_messages_with_and_without_arguments() {
        let m = parse_packet(&osc("/go", ",", &[]));
        assert_eq!(m, vec![OscMessage { addr: "/go".into(), args: vec![] }]);
        let mut args = 42i32.to_be_bytes().to_vec();
        args.extend(1.5f32.to_be_bytes());
        args.extend(b"hola\0\0\0\0");
        let m = parse_packet(&osc("/x", ",ifs", &args));
        assert_eq!(m[0].args, vec![OscArg::Int(42), OscArg::Float(1.5), OscArg::Str("hola".into())]);
        // Mensaje antiguo sin lista de tipos.
        let mut old = b"/go\0".to_vec();
        assert_eq!(parse_packet(&old).len(), 1);
        old.clear();
        assert!(parse_packet(&old).is_empty());
    }

    #[test]
    fn bundles_and_garbage_are_handled() {
        let a = osc("/go", ",", &[]);
        let b = osc("/panic", ",", &[]);
        let mut bundle = b"#bundle\0".to_vec();
        bundle.extend([0u8; 8]);
        for e in [&a, &b] {
            bundle.extend((e.len() as u32).to_be_bytes());
            bundle.extend(e.iter());
        }
        let m = parse_packet(&bundle);
        assert_eq!(m.iter().map(|m| m.addr.as_str()).collect::<Vec<_>>(), ["/go", "/panic"]);
        for junk in [&b"hola"[..], b"/go", b"\xff\xfe\x00", b"/x\0\0,q\0\0", b"#bundle\0", b"/x\0\0,i\0\0\x00\x00"] {
            let _ = parse_packet(junk); // nunca entra en pánico
        }
        assert!(parse_packet(b"/x\0\0,q\0\0").is_empty(), "tipo desconocido");
        assert!(parse_packet(b"/x\0\0,i\0\0\x00\x00").is_empty(), "argumento truncado");
    }

    #[test]
    fn addresses_map_to_commands() {
        let r = |a: &str| route(&OscMessage { addr: a.into(), args: vec![] });
        assert_eq!(r("/go"), Some(Action::Request(ClientRequest::Go)));
        assert_eq!(r("/panic"), Some(Action::Request(ClientRequest::Panic)));
        assert_eq!(r("/stop"), Some(Action::Request(ClientRequest::StopAll)));
        assert_eq!(r("/pause"), Some(Action::Request(ClientRequest::PauseAll)));
        assert_eq!(r("/resume"), Some(Action::Request(ClientRequest::ResumeAll)));
        assert_eq!(r("/hardstop"), Some(Action::Request(ClientRequest::HardStopAll)));
        assert_eq!(r("/cue/12.5/start"), Some(Action::FireCue("12.5".into())));
        assert_eq!(route(&OscMessage { addr: "/master".into(), args: vec![OscArg::Float(-6.0)] }), Some(Action::Request(ClientRequest::SetMasterDb { db: -6.0 })));
        assert_eq!(route(&OscMessage { addr: "/master".into(), args: vec![OscArg::Int(-12)] }), Some(Action::Request(ClientRequest::SetMasterDb { db: -12.0 })));
        assert_eq!(r("/master"), None, "sin argumento no hace nada");
        assert_eq!(r("/next"), Some(Action::Step(1)));
        assert_eq!(r("/prev"), Some(Action::Step(-1)));
        assert_eq!(r("/cue/3/stop"), Some(Action::CueAction("3".into(), qlab_protocol::show_format::ControlActionDto::Stop)));
        for bad in ["/nope", "/cue/start", "/cue//start", "/cue/1/bailar", "/"] {
            assert_eq!(r(bad), None, "{bad}");
        }
    }
    #[test]
    fn stepping_the_playhead_stays_inside_the_list() {
        let top = [10, 20, 30];
        assert_eq!(step_target(&top, Some(10), 1), Some(20));
        assert_eq!(step_target(&top, Some(30), 1), Some(30), "no pasa del final");
        assert_eq!(step_target(&top, Some(10), -1), Some(10), "no pasa del principio");
        assert_eq!(step_target(&top, None, 1), Some(10));
        assert_eq!(step_target(&top, None, -1), Some(30));
        assert_eq!(step_target(&top, Some(99), 1), Some(10), "playhead desconocido");
        assert_eq!(step_target(&[], Some(1), 1), None);
    }

    #[test]
    fn lines_become_osc_packets_with_typed_arguments() {
        assert_eq!(encode_line("/go").unwrap(), b"/go\0,\0\0\0");
        let p = encode_line("/mix 7 0.5 \"hola mundo\"").unwrap();
        let m = parse_packet(&p);
        assert_eq!(m[0].addr, "/mix");
        assert_eq!(m[0].args, vec![OscArg::Int(7), OscArg::Float(0.5), OscArg::Str("hola mundo".into())]);
        assert!(encode_line("").is_err() && encode_line("sin barra").is_err());
        assert_eq!(tokens(r#"/a "b c" d"#), vec!["/a", "b c", "d"]);
    }

    #[test]
    fn random_and_mutated_packets_never_panic_the_parser() {
        // Generador congruencial: reproducible sin dependencias.
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = move || {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (x >> 33) as u32
        };
        let valid = [
            osc_bytes("/go", ",", &[]),
            osc_bytes("/cue/3/start", ",is", &[1, 2, 3, 4, b'h', b'i', 0, 0]),
            osc_bytes("/x", ",ff", &[0, 0, 0, 1, 0, 0, 0, 2]),
        ];
        for round in 0..20_000 {
            let len = (next() % 80) as usize;
            let mut buf: Vec<u8> = (0..len).map(|_| next() as u8).collect();
            // Una de cada tres veces se parte de un mensaje válido y se le rompe un byte.
            if round % 3 == 0 {
                buf = valid[(next() % 3) as usize].clone();
                let at = (next() as usize) % buf.len();
                buf[at] = next() as u8;
                if next() % 2 == 0 {
                    buf.truncate((next() as usize) % (buf.len() + 1));
                }
            }
            let _ = parse_packet(&buf);
            let _ = encode_line(&String::from_utf8_lossy(&buf));
        }
    }

    fn osc_bytes(addr: &str, tags: &str, args: &[u8]) -> Vec<u8> {
        let pad = |s: &str| {
            let mut v = s.as_bytes().to_vec();
            v.push(0);
            while v.len() % 4 != 0 {
                v.push(0);
            }
            v
        };
        [pad(addr), pad(tags), args.to_vec()].concat()
    }

}
