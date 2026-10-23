//! Mensajes del canal de control local (socket Unix / tubería con nombre), una línea JSON por mensaje.
//!
//! * Cliente → daemon: `{"id": 7, "cmd": "go"}`.
//! * Daemon → cliente: respuesta `{"id": 7, "result": "ok"}` y, tras `subscribe`, eventos
//!   `{"event": "playhead_moved", "cue": 3}` intercalados.
//!
//! Los gestos de latencia crítica (GO, Panic, arrastre de faders) también tienen camino directo por
//! el anillo caliente de memoria compartida (`ipc_shm`); este canal es el de control estructural y el
//! de los clientes que no mapean la memoria.

use serde::{Deserialize, Serialize};

use crate::cue_dto::{ContinueMode, CueColor};
use crate::show_format::CueNodeDto;

/// Versión del protocolo; sube cuando un cambio rompe la compatibilidad.
pub const IPC_PROTOCOL_VERSION: u32 = 1;
pub const DEFAULT_SOCKET_PATH_UNIX: &str = "/tmp/teatrocue.sock";
pub const DEFAULT_PIPE_NAME_WINDOWS: &str = r"\\.\pipe\teatrocue_ipc";

/// Ruta por defecto del canal en la plataforma actual.
pub fn default_socket_name() -> &'static str {
    if cfg!(windows) { DEFAULT_PIPE_NAME_WINDOWS } else { DEFAULT_SOCKET_PATH_UNIX }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum ClientRequest {
    Hello { client: String, protocol: u32 },
    Ping { nonce: u64 },
    /// Botón GO.
    Go,
    /// Parada suave de todo.
    Panic,
    /// Parada inmediata de todo.
    HardStopAll,
    StopAll,
    PauseAll,
    ResumeAll,
    /// Reactivación de emergencia: canjea una clave `RESP-…` y recarga la licencia sin reiniciar.
    ApplyEmergencyKey { key: String },
    /// Volumen maestro en dB (−60 o menos = silencio, máximo +6). No se guarda con el show.
    SetMasterDb { db: f32 },
    /// Acción sobre una cue concreta (Stop, Pause, Resume, Devamp, Load, Arm, Disarm, Go to).
    CueAction { cue_id: u64, action: crate::show_format::ControlActionDto },
    /// Devamp: la cue de audio sale de su slice infinito al final de la pasada en curso y sigue sin saltos.
    Devamp { cue_id: u64 },
    /// Arrastre en vivo de una celda de la matriz de la cue de audio `cue_id`.
    FaderDrag { cue_id: u64, row: u8, col: u8, db: f32 },
    /// Mueve el playhead (independiente de la selección de la UI).
    SetPlayhead { cue_id: Option<u64> },
    GetStatus,
    GetCues,
    /// Datos completos de una cue (los que edita el Inspector). Los grupos no incluyen a sus hijos.
    GetCueDetail { cue_id: u64 },
    /// Sustituye los datos editables de una cue existente (mismo id y mismo tipo). Un cambio de
    /// archivo, región o bucle de una cue que está sonando se rechaza; los niveles, la velocidad y
    /// el resto se aplican en vivo.
    UpdateCue { node: Box<CueNodeDto> },
    /// Pone (o quita, con `None`) el punto de arranque de una cue de audio o vídeo sin tocar nada más: «retomar desde donde
    /// se paró» en un ensayo. Como cualquier cambio de región, se rechaza mientras la cue suena.
    SetStartPoint { cue_id: u64, start_secs: Option<f64> },
    /// Carga un show nativo (`qlab-rs/v1`) desde un archivo del lado del daemon.
    LoadShow { path: String },
    /// Crea una cue nueva (`audio`, `fade` o `group`) en el grupo `parent`, o justo después de `after`
    /// (en su mismo contenedor), o al final de la lista.
    AddCue { kind: String, parent: Option<u64>, after: Option<u64> },
    /// Elimina la cue y sus hijas. Se rechaza si alguna está sonando.
    DeleteCue { cue_id: u64 },
    /// Sube (`delta < 0`) o baja (`delta > 0`) la cue una posición entre sus hermanas.
    MoveCue { cue_id: u64, delta: i32 },
    /// Descarta el show actual (detiene todo) y deja una lista vacía.
    NewShow,
    /// Empaqueta el show en la carpeta `dir`: copia sus archivos multimedia a `dir/audio/`, reescribe sus rutas
    /// como relativas y guarda `dir/<name>.json`.
    BundleShow { dir: String, name: String },
    /// Serializa la lista completa al archivo `path` (JSON nativo) con el nombre `name`.
    SaveShow { path: String, name: String },
    /// Exporta el guion técnico (HTML imprimible) al archivo `path`. `durations` trae la duración conocida de cada cue
    /// (id, segundos) y `generated` la fecha que se imprime en la cabecera.
    ExportCueSheet { path: String, name: String, durations: Vec<(u64, f64)>, generated: String },
    /// Prepara el reproductor independiente `slot` (botonera de Carts o audición) con un archivo: no suena todavía.
    /// `monitor`: sale solo por la salida de auriculares (PFL) y no por la sala. `start_secs`: desde dónde suena.
    AdhocLoad { slot: u32, path: String, gain_db: f32, monitor: bool, start_secs: f64 },
    /// Reemplaza la botonera de Carts del show abierto (se guarda con el show).
    SetCarts { bank: crate::cue_dto::CartBank },
    /// Pide la botonera de Carts del show abierto.
    GetCarts,
    /// Dispara el reproductor `slot` desde su inicio, sin tocar la lista de cues ni el playhead.
    AdhocPlay { slot: u32 },
    /// Detiene el reproductor `slot` con un fundido corto.
    AdhocStop { slot: u32 },
    /// Detiene todos los reproductores independientes.
    AdhocStopAll,
    /// Audición (PFL): reproduce el archivo de la cue `cue_id` solo por los auriculares, desde `start_secs` (o desde el
    /// punto de arranque de la cue). No toca el playhead ni lo que suena en la sala.
    Audition { cue_id: u64, start_secs: Option<f64> },
    /// Detiene la audición.
    AuditionStop,
    /// Pide (o deja de pedir) los eventos del daemon en esta conexión.
    Subscribe { events: bool },
    /// Apaga el daemon.
    Shutdown,
}

/// Petición con identificador, para casar la respuesta.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ClientMessage {
    pub id: u64,
    #[serde(flatten)]
    pub request: ClientRequest,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CueSummary {
    pub id: u64,
    pub number: String,
    pub name: String,
    /// `audio`, `fade` o `group`.
    pub kind: String,
    /// Estado de la máquina: `Idle`, `Armed`, `PreWaiting`, `Running`, `PostWaiting`, `Paused`, `Completed`.
    pub state: String,
    pub armed: bool,
    pub parent: Option<u64>,
    /// Archivo de audio (solo el nombre) o, en un fade, `→ número` de la cue destino.
    #[serde(default)]
    pub target: String,
    #[serde(default)]
    pub pre_wait: f64,
    #[serde(default)]
    pub post_wait: f64,
    /// Duración de la acción en segundos; `None` si no se conoce (bucle infinito, grupo).
    #[serde(default)]
    pub duration: Option<f64>,
    #[serde(default)]
    pub continue_mode: ContinueMode,
    #[serde(default)]
    pub flagged: bool,
    #[serde(default)]
    pub color: CueColor,
    #[serde(default)]
    pub dead: bool,
    /// Memorias (puntos con nombre y nota) de una cue de audio o vídeo.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub memories: Vec<crate::cue_dto::MemoryMark>,
    /// Frecuencia del archivo de una cue de audio o vídeo (0 si no tiene): convierte la posición de lectura del motor
    /// (frames del archivo) en segundos, p. ej. para retomar desde donde se paró.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub file_sample_rate: u32,
    /// Punto de arranque fijado (memoria elegida o «retomar»); `None` si arranca en su Inicio.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_override_secs: Option<f64>,
}

fn is_zero(v: &u32) -> bool {
    *v == 0
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunningCueInfo {
    pub id: u64,
    pub phase: String,
    pub pre_wait_elapsed_sec: f32,
    pub action_elapsed_sec: f32,
    pub post_wait_elapsed_sec: f32,
    /// Tramo (slice) en el que suena una cue de audio con marcadores; `None` si no los tiene.
    #[serde(default)]
    pub slice: Option<u16>,
    /// Atrapada en un tramo infinito: sigue en bucle hasta recibir `Devamp`.
    #[serde(default)]
    pub vamping: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct StatusSnapshot {
    pub playhead: Option<u64>,
    pub running: Vec<RunningCueInfo>,
    pub xruns: u64,
    pub cpu_permille: u32,
    pub audio_stalled: bool,
    pub sequencer_stalled: bool,
    pub safe_mute: bool,
    pub sequencer_ticks: u64,
    pub commands_sent: u64,
    /// Asignaciones o liberaciones detectadas dentro del callback de audio (debe ser 0).
    #[serde(default)]
    pub rt_alloc_violations: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum ServerResponse {
    Ok,
    Error { message: String },
    Hello { server_protocol: u32, shm_name: String, sample_rate: u32, channels: u16, backend: String },
    Pong { nonce: u64 },
    Status(StatusSnapshot),
    Cues { cues: Vec<CueSummary> },
    CueDetail { node: Box<CueNodeDto>, parent: Option<u64>, state: String },
    ShowLoaded { name: String, cues: usize, warnings: Vec<String> },
    CueAdded { cue_id: u64 },
    ShowSaved { path: String, cues: usize },
    /// Show empaquetado: archivo del show, medios copiados, bytes copiados y rutas que no se encontraron.
    ShowBundled { path: String, files: usize, bytes: u64, missing: Vec<String> },
    Carts { bank: crate::cue_dto::CartBank },
    /// Reproductor independiente preparado; `duration` en segundos si el archivo la declara.
    AdhocLoaded { slot: u32, duration: Option<f64> },
    /// Guion técnico escrito en `path` con `cues` filas.
    CueSheetExported { path: String, cues: usize },
    /// Clave de reactivación aceptada: la licencia vale hasta `valid_until` (segundos Unix) y la época subió a `epoch`.
    LicenseExtended { valid_until: u64, epoch: u32 },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ResponseMessage {
    pub id: u64,
    #[serde(flatten)]
    pub response: ServerResponse,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ServerEvent {
    PlayheadMoved { cue: Option<u64> },
    CueStateChanged { cue: u64, state: String },
    /// Aviso de salud: `source` = `audio`, `sequencer`, `engine`, `show`…
    Warning { source: String, message: String },
    SafeMute { engaged: bool },
    ShowLoaded { name: String },
    /// Cambió algún dato editable de la cue (nombre, tiempos, niveles…).
    CueUpdated { cue: u64 },
    /// Se añadió, eliminó o movió una cue: hay que volver a pedir la lista.
    StructureChanged,
    ShuttingDown,
}

/// Cualquier línea que envía el daemon.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ServerLine {
    Response(ResponseMessage),
    Event(ServerEvent),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_wire_format() {
        let m = ClientMessage { id: 7, request: ClientRequest::Go };
        assert_eq!(serde_json::to_string(&m).unwrap(), r#"{"id":7,"cmd":"go"}"#);
        let m: ClientMessage = serde_json::from_str(r#"{"id":9,"cmd":"fader_drag","cue_id":3,"row":1,"col":2,"db":-6.5}"#).unwrap();
        assert_eq!(m, ClientMessage { id: 9, request: ClientRequest::FaderDrag { cue_id: 3, row: 1, col: 2, db: -6.5 } });
        let m: ClientMessage = serde_json::from_str(r#"{"id":1,"cmd":"set_playhead","cue_id":null}"#).unwrap();
        assert_eq!(m.request, ClientRequest::SetPlayhead { cue_id: None });
        assert!(serde_json::from_str::<ClientMessage>(r#"{"id":1,"cmd":"nope"}"#).is_err());
        assert!(serde_json::from_str::<ClientMessage>(r#"{"cmd":"go"}"#).is_err(), "falta el id");
    }

    #[test]
    fn lines_are_told_apart() {
        let resp = ResponseMessage { id: 4, response: ServerResponse::Pong { nonce: 11 } };
        let json = serde_json::to_string(&resp).unwrap();
        assert_eq!(json, r#"{"id":4,"result":"pong","nonce":11}"#);
        assert_eq!(serde_json::from_str::<ServerLine>(&json).unwrap(), ServerLine::Response(resp));
        let ev = ServerEvent::PlayheadMoved { cue: Some(2) };
        let json = serde_json::to_string(&ev).unwrap();
        assert_eq!(json, r#"{"event":"playhead_moved","cue":2}"#);
        assert_eq!(serde_json::from_str::<ServerLine>(&json).unwrap(), ServerLine::Event(ev));
    }

    #[test]
    fn every_response_roundtrips() {
        let all = vec![
            ServerResponse::Ok,
            ServerResponse::Error { message: "x".into() },
            ServerResponse::Hello { server_protocol: 1, shm_name: "s".into(), sample_rate: 48_000, channels: 2, backend: "null".into() },
            ServerResponse::Status(StatusSnapshot { playhead: Some(1), xruns: 2, ..Default::default() }),
            ServerResponse::Cues {
                cues: vec![CueSummary {
                    id: 1,
                    number: "1".into(),
                    name: "a".into(),
                    kind: "audio".into(),
                    state: "Idle".into(),
                    armed: true,
                    parent: None,
                    target: "x.wav".into(),
                    pre_wait: 0.5,
                    post_wait: 1.0,
                    duration: Some(3.0),
                    continue_mode: ContinueMode::AutoContinue,
                    flagged: true,
                    color: CueColor::Red,
                    dead: false,
                    memories: vec![crate::cue_dto::MemoryMark { secs: 1.5, name: "m".into(), note: "n".into() }],
                    file_sample_rate: 48_000,
                    start_override_secs: None,
                }],
            },
            ServerResponse::ShowLoaded { name: "n".into(), cues: 3, warnings: vec!["w".into()] },
        ];
        for (i, r) in all.into_iter().enumerate() {
            let m = ResponseMessage { id: i as u64, response: r };
            let back: ResponseMessage = serde_json::from_str(&serde_json::to_string(&m).unwrap()).unwrap();
            assert_eq!(back, m);
        }
    }

    #[test]
    fn old_summaries_without_the_new_fields_still_parse() {
        let json = r#"{"id":1,"result":"cues","cues":[{"id":3,"number":"3","name":"n","kind":"audio","state":"Idle","armed":true,"parent":null}]}"#;
        let m: ResponseMessage = serde_json::from_str(json).unwrap();
        let ServerResponse::Cues { cues } = m.response else { panic!() };
        assert_eq!((cues[0].pre_wait, cues[0].continue_mode, cues[0].target.as_str()), (0.0, ContinueMode::DoNotContinue, ""));
    }

    #[test]
    fn update_and_detail_wire_format() {
        use crate::cue_dto::CueId;
        use crate::show_format::{AudioBody, CueBody};
        let node = CueNodeDto {
            basics: crate::cue_dto::CueBasicsData { unique_id: CueId(4), name: "x".into(), ..Default::default() },
            triggers: Default::default(),
            dead: false,
            body: CueBody::Audio(AudioBody { time_loops: Default::default(), levels: None, eq: Default::default() }),
        };
        let m = ClientMessage { id: 2, request: ClientRequest::UpdateCue { node: Box::new(node.clone()) } };
        let back: ClientMessage = serde_json::from_str(&serde_json::to_string(&m).unwrap()).unwrap();
        assert_eq!(back, m);
        let r = ResponseMessage { id: 2, response: ServerResponse::CueDetail { node: Box::new(node), parent: Some(1), state: "Idle".into() } };
        let back: ResponseMessage = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(back, r);
        let g: ClientMessage = serde_json::from_str(r#"{"id":3,"cmd":"get_cue_detail","cue_id":9}"#).unwrap();
        assert_eq!(g.request, ClientRequest::GetCueDetail { cue_id: 9 });
    }

    #[test]
    fn default_name_matches_platform() {
        assert!(default_socket_name().contains("teatrocue"));
    }
}
