//! Servidor IPC local (spec §2.1): socket Unix (`/tmp/teatrocue.sock`) en Linux y tubería con nombre
//! (`\\.\pipe\teatrocue_ipc`) en Windows, ambos mediante `interprocess`.
//!
//! El transporte es una línea JSON por mensaje (ver `qlab_protocol::ipc_msg`). Cada conexión tiene su
//! hilo, así que un cliente lento o caído no afecta al secuenciador ni al audio.
//!
//! **Anillo caliente**: GO, Panic, StopAll y los arrastres de fader se vuelcan en el `HotControlRing`
//! de la memoria compartida con el IPC como único productor (el anillo es SPSC; las conexiones se
//! serializan con un cerrojo propio, fuera del hilo RT). Una UI que escriba directamente en el
//! anillo debe elegir [`RingProducer::ExternalUi`]; entonces estos gestos viajan por el canal del
//! secuenciador en vez de por el anillo, para no tener dos productores.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use interprocess::local_socket::traits::Stream as StreamTrait;
use interprocess::local_socket::{prelude::*, ListenerNonblockingMode, ListenerOptions, Stream};
use qlab_core::clock::Timebase;
use qlab_protocol::cue_dto::CueId;
use qlab_protocol::ipc_msg::{ClientMessage, ClientRequest, ResponseMessage, ServerEvent, ServerResponse, IPC_PROTOCOL_VERSION};
use qlab_protocol::ipc_client::socket_name;
use qlab_protocol::ipc_shm::HotControlEvent;
use qlab_protocol::show_format::{validate_node, CueNodeDto, NewCueKind, ShowFile, SHOW_FORMAT};

use crate::cue_edit::{file_path, needs_voice_reload, node_to_kind};
use crate::events::EventHub;
use crate::health::Health;
use crate::sequencer_thread::{Control, SeqCommand};
use crate::shm::SharedMemory;
use crate::show_loader::{build_show, normalize_time};
use crate::voice_loader::VoiceLoader;

type SendHalf = <Stream as StreamTrait>::SendHalf;

/// Línea más larga que se acepta de un cliente.
const MAX_LINE_BYTES: u64 = 1 << 20;
/// Tamaño máximo de un archivo de show.
const MAX_SHOW_BYTES: u64 = 64 << 20;
/// Espera por la respuesta del secuenciador.
const SEQUENCER_TIMEOUT: Duration = Duration::from_millis(1_500);
/// Instalar un show para antes el audio y puede tardar más.
const INSTALL_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RingProducer {
    /// El IPC escribe en el anillo caliente (por defecto).
    IpcServer,
    /// Una UI escribe directamente en el anillo; el IPC usa el canal del secuenciador.
    ExternalUi,
}

#[derive(Clone, Debug)]
pub struct ServerInfo {
    pub shm_name: String,
    pub sample_rate: u32,
    pub channels: u16,
    pub backend: String,
}

#[derive(Clone, Debug)]
struct AdhocParams {
    path: String,
    gain_db: f32,
    monitor: bool,
    start_secs: f64,
}

pub struct IpcContext {
    pub seq_tx: Mutex<Sender<SeqCommand>>,
    pub shm: Arc<SharedMemory>,
    pub events: Arc<EventHub>,
    pub health: Arc<Health>,
    pub loader: Arc<Mutex<VoiceLoader>>,
    pub timebase: Timebase,
    pub info: ServerInfo,
    pub ring_producer: RingProducer,
    pub shutdown: Arc<AtomicBool>,
    license: Option<qlab_license::activation::Setup>,
    ring_lock: Mutex<()>,
    /// Lo último que se preparó en cada reproductor independiente, para reponerlo si hay que reiniciarlo.
    adhoc: Mutex<std::collections::HashMap<u32, AdhocParams>>,
    /// Cue que se está escuchando por PFL: si se edita su EQ, la audición lo sigue en vivo.
    audition_cue: Mutex<Option<u64>>,
    /// Botonera de Carts del show abierto (se guarda con él).
    carts: Mutex<qlab_protocol::cue_dto::CartBank>,
    next_list_id: std::sync::atomic::AtomicU64,
}

impl IpcContext {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        seq_tx: Sender<SeqCommand>,
        shm: Arc<SharedMemory>,
        events: Arc<EventHub>,
        health: Arc<Health>,
        loader: Arc<Mutex<VoiceLoader>>,
        timebase: Timebase,
        info: ServerInfo,
        ring_producer: RingProducer,
        shutdown: Arc<AtomicBool>,
    ) -> Self {
        Self {
            seq_tx: Mutex::new(seq_tx),
            shm,
            events,
            health,
            loader,
            timebase,
            info,
            ring_producer,
            shutdown,
            license: None,
            adhoc: Mutex::new(Default::default()),
            audition_cue: Mutex::new(None),
            carts: Mutex::new(Default::default()),
            ring_lock: Mutex::new(()),
            next_list_id: std::sync::atomic::AtomicU64::new(1),
        }
    }

    /// Entorno de licencia con el que se canjean claves de reactivación de emergencia.
    pub fn with_license(mut self, setup: Option<qlab_license::activation::Setup>) -> Self {
        self.license = setup;
        self
    }

    /// Reactivación de emergencia en caliente: valida y aplica la clave; la licencia en memoria y en disco se actualiza
    /// sin parar el audio ni el show.
    pub fn apply_emergency_key(&self, key: &str) -> Result<ServerResponse, String> {
        let setup = self.license.as_ref().ok_or("esta compilación no exige licencia: no hay nada que reactivar")?;
        let applied = setup.apply(key).map_err(|e| e.to_string())?;
        Ok(ServerResponse::LicenseExtended { valid_until: applied.valid_until, epoch: applied.epoch })
    }

    fn send_seq(&self, cmd: SeqCommand) -> Result<(), String> {
        self.seq_tx.lock().expect("envenenado").send(cmd).map_err(|_| "el secuenciador no está en marcha".to_string())
    }

    /// Pregunta al secuenciador y espera la respuesta por un canal de un solo uso.
    fn ask<T>(&self, make: impl FnOnce(Sender<T>) -> SeqCommand, timeout: Duration) -> Result<T, String> {
        let (tx, rx) = channel();
        self.send_seq(make(tx))?;
        rx.recv_timeout(timeout).map_err(|e| match e {
            RecvTimeoutError::Timeout => "el secuenciador no respondió a tiempo".to_string(),
            RecvTimeoutError::Disconnected => "el secuenciador se detuvo".to_string(),
        })
    }

    /// Gesto de latencia crítica: anillo compartido o, si lo usa una UI, canal del secuenciador.
    fn hot(&self, event: HotControlEvent) -> Result<(), String> {
        match self.ring_producer {
            RingProducer::IpcServer => {
                let _one_producer = self.ring_lock.lock().expect("envenenado");
                self.shm.telemetry().hot_ring.push(event).map_err(|_| "el anillo de control está lleno".to_string())
            }
            RingProducer::ExternalUi => self.send_seq(SeqCommand::Hot(event)),
        }
    }

    /// Lee, valida y carga un show y lo instala en el secuenciador.
    pub fn load_show_file(&self, path: &Path) -> Result<ServerResponse, String> {
        let meta = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
        if meta.len() > MAX_SHOW_BYTES {
            return Err(format!("el show pesa {} bytes (máximo {MAX_SHOW_BYTES})", meta.len()));
        }
        let json = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut show = ShowFile::from_json(&json).map_err(|e| e.to_string())?;
        // Un show empaquetado trae rutas relativas a su propia carpeta.
        if let Some(base) = path.parent() {
            show.resolve_media_paths(base);
        }
        let list_id = self.next_list_id.fetch_add(1, Ordering::Relaxed);
        *self.carts.lock().expect("envenenado") = show.carts.clone().clamped();
        let built = {
            let mut loader = self.loader.lock().expect("envenenado");
            build_show(&show, &mut loader, self.timebase, list_id)
        };
        let (name, cues, warnings) = (built.name.clone(), built.cue_count, built.warnings.clone());
        let list = built.list;
        self.ask(|reply| SeqCommand::InstallShow { list, reply }, INSTALL_TIMEOUT)?;
        for w in &warnings {
            self.events.publish(ServerEvent::Warning { source: "show".into(), message: w.clone() });
        }
        self.events.publish(ServerEvent::ShowLoaded { name: name.clone() });
        Ok(ServerResponse::ShowLoaded { name, cues, warnings })
    }

    /// Edición del Inspector. Si cambia el archivo, la región o los bucles se recarga antes la voz (en
    /// este hilo, sin tocar el secuenciador); una cue que está sonando no admite esos cambios.
    pub fn update_cue(&self, mut node: CueNodeDto) -> Result<(), String> {
        validate_node(&node).map_err(|e| e.to_string())?;
        let id = node.basics.unique_id;
        let (old, _parent, state) = self
            .ask(|reply| SeqCommand::GetCueDetail(id, reply), SEQUENCER_TIMEOUT)?
            .ok_or_else(|| format!("no existe la cue {}", id.0))?;
        if needs_voice_reload(&old, &node) {
            let active = !matches!(state.as_str(), "Idle" | "Armed" | "Completed");
            if active {
                return Err("detén la cue antes de cambiar su archivo, región o bucles".into());
            }
            let path = file_path(&node).map(str::to_string);
            if let Some(a) = node.body.audio_mut() {
                let path = path.ok_or("la cue necesita un archivo")?;
                let info = {
                    let mut loader = self.loader.lock().expect("envenenado");
                    loader.load_path(id, Path::new(&path), &a.time_loops, a.levels.as_ref()).map_err(|e| e.to_string())?
                };
                normalize_time(&mut a.time_loops, &info);
            }
        }
        let eq = node.body.audio().map(|a| qlab_core::cue::eq_params(&a.eq));
        self.ask(|reply| SeqCommand::UpdateCue { node: Box::new(node), reply }, SEQUENCER_TIMEOUT)??;
        // Si esta cue se está escuchando por PFL, la audición sigue su EQ en vivo (es otra voz, no la de la sala).
        if let (Some(eq), true) = (eq, *self.audition_cue.lock().expect("envenenado") == Some(id.0)) {
            let _ = self.send_seq(SeqCommand::Audio(qlab_engine::rt_command::AudioCommand::SetEq { voice_id: crate::adhoc::voice_id(crate::adhoc::AUDITION_SLOT), eq }));
        }
        self.events.publish(ServerEvent::CueUpdated { cue: id.0 });
        Ok(())
    }

    /// Cambia solo el punto de arranque de una cue de audio o vídeo (retomar desde donde se paró).
    pub fn set_start_point(&self, cue_id: u64, start_secs: Option<f64>) -> Result<(), String> {
        let (mut node, _, _) = self.ask(|reply| SeqCommand::GetCueDetail(CueId(cue_id), reply), SEQUENCER_TIMEOUT)?.ok_or_else(|| format!("no existe la cue {cue_id}"))?;
        let a = node.body.audio_mut().ok_or("la cue no tiene audio")?;
        if a.time_loops.start_override_secs == start_secs {
            return Ok(());
        }
        a.time_loops.start_override_secs = start_secs;
        self.update_cue(node)
    }

    /// Crea una cue nueva con valores por defecto; el id es el siguiente libre y el número, el siguiente
    /// entero tras el mayor numérico de la lista.
    pub fn add_cue(&self, kind: &str, parent: Option<u64>, after: Option<u64>) -> Result<u64, String> {
        let kind = NewCueKind::parse(kind).ok_or_else(|| format!("tipo de cue desconocido: `{kind}` (audio, video, fade, control o group)"))?;
        let cues = self.ask(SeqCommand::GetCues, SEQUENCER_TIMEOUT)?;
        let id = cues.iter().map(|c| c.id).max().unwrap_or(0) + 1;
        let number = cues.iter().filter_map(|c| c.number.trim().parse::<u64>().ok()).max().unwrap_or(0) + 1;
        let node = CueNodeDto::new_default(kind, id, number.to_string());
        let device_channels = self.loader.lock().expect("envenenado").device_channels();
        let levels = qlab_protocol::matrix::AudioLevelMatrixData::new_one_to_one(2, device_channels.clamp(1, qlab_protocol::matrix::MAX_OUTPUT_CHANNELS)).map_err(|e| e.to_string())?;
        let mut cue = qlab_core::cue::Cue::new(node.basics.clone(), node_to_kind(&node, None, levels));
        cue.triggers = node.triggers.clone();
        self.ask(|reply| SeqCommand::AddCue { cue: Box::new(cue), parent: parent.map(CueId), after: after.map(CueId), reply }, SEQUENCER_TIMEOUT)??;
        Ok(id)
    }

    /// Elimina una cue (y sus hijas) y descarga sus voces.
    pub fn delete_cue(&self, id: u64) -> Result<(), String> {
        let removed = self.ask(|reply| SeqCommand::RemoveCue { id: CueId(id), reply }, SEQUENCER_TIMEOUT)??;
        let mut loader = self.loader.lock().expect("envenenado");
        for r in removed {
            loader.unload(r);
        }
        Ok(())
    }

    /// Descarta el show actual y deja una lista vacía.
    pub fn new_show(&self) -> Result<ServerResponse, String> {
        let list_id = self.next_list_id.fetch_add(1, Ordering::Relaxed);
        self.loader.lock().expect("envenenado").unload_all();
        *self.carts.lock().expect("envenenado") = Default::default();
        self.adhoc.lock().expect("envenenado").clear();
        let list = qlab_core::cue_list::CueList::new(list_id, self.timebase);
        self.ask(|reply| SeqCommand::InstallShow { list, reply }, INSTALL_TIMEOUT)?;
        self.events.publish(ServerEvent::ShowLoaded { name: String::new() });
        Ok(ServerResponse::ShowLoaded { name: String::new(), cues: 0, warnings: vec![] })
    }

    /// Guarda la lista completa en `path` (JSON nativo). Escribe en un temporal y lo renombra para que un
    /// fallo a medias no deje un archivo truncado.
    pub fn save_show(&self, path: &Path, name: &str) -> Result<ServerResponse, String> {
        let cues = self.ask(SeqCommand::ExportShow, SEQUENCER_TIMEOUT)?;
        let mut show = ShowFile { format: SHOW_FORMAT.into(), name: name.to_string(), sample_rate: self.timebase.sample_rate(), carts: self.carts.lock().expect("envenenado").clone(), cues };
        if let Some(base) = path.parent() {
            show.relativize_media_paths(base);
        }
        show.validate().map_err(|e| format!("el show no es válido: {e}"))?;
        let json = show.to_json();
        let tmp = path.with_extension(format!("{}.tmp", path.extension().and_then(|e| e.to_str()).unwrap_or("json")));
        std::fs::write(&tmp, json).map_err(|e| format!("{}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, path).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            format!("{}: {e}", path.display())
        })?;
        Ok(ServerResponse::ShowSaved { path: path.to_string_lossy().into_owned(), cues: show.cue_count() })
    }

    /// Carga el reproductor independiente `slot` (voz nueva, en reposo).
    fn adhoc_load_voice(&self, slot: u32, p: &AdhocParams) -> Result<Option<f64>, String> {
        use qlab_protocol::cue_dto::AudioTimeAndLoopsData;
        if slot >= crate::adhoc::MAX_SLOTS {
            return Err(format!("ranura {slot} fuera de rango"));
        }
        let first_out = if p.monitor {
            match self.health.monitor_first.load(Ordering::Relaxed) {
                0 => return Err("No hay salida de auriculares: elíjala en el menú Ver, Configuración de audio".to_string()),
                n => n as usize - 1,
            }
        } else {
            0
        };
        let probe = VoiceLoader::probe(Path::new(&p.path)).map_err(|e| e.to_string())?;
        let mut loader = self.loader.lock().expect("envenenado");
        let levels = crate::adhoc::levels(probe.channels, loader.device_channels(), first_out, p.gain_db);
        let time = AudioTimeAndLoopsData { start_time_sec: p.start_secs.max(0.0), file_sample_rate: probe.sample_rate, ..Default::default() };
        let id = CueId(crate::adhoc::voice_id(slot));
        loader.load_path(id, Path::new(&p.path), &time, Some(&levels)).map_err(|e| e.to_string())?;
        Ok(probe.total_frames.map(|f| (f as f64 / f64::from(probe.sample_rate) - p.start_secs.max(0.0)).max(0.0)))
    }

    fn adhoc_load(&self, slot: u32, p: AdhocParams) -> Result<ServerResponse, String> {
        let duration = self.adhoc_load_voice(slot, &p)?;
        self.adhoc.lock().expect("envenenado").insert(slot, p);
        Ok(ServerResponse::AdhocLoaded { slot, duration })
    }

    fn adhoc_play(&self, slot: u32, eq: Option<qlab_engine::biquad::EqParams>) -> Result<ServerResponse, String> {
        use qlab_engine::voice::VoiceState;
        let params = self.adhoc.lock().expect("envenenado").get(&slot).cloned().ok_or("el reproductor no tiene archivo")?;
        let id = CueId(crate::adhoc::voice_id(slot));
        let idle = self.loader.lock().expect("envenenado").voice(id).is_some_and(|v| matches!(v.state(), VoiceState::Idle | VoiceState::Stopped));
        // Si todavía suena (o está en pausa) se arma una voz nueva: «disparar y reiniciar» sin esperar al fundido.
        if !idle {
            self.adhoc_load_voice(slot, &params)?;
        }
        // El EQ de la cue (PFL) se pone en la voz ya armada y antes de Play: se escucha igual que saldrá por la sala.
        if let Some(eq) = eq {
            self.send_seq(SeqCommand::Audio(qlab_engine::rt_command::AudioCommand::SetEq { voice_id: id.0, eq }))?;
        }
        let cmd = qlab_engine::rt_command::AudioCommand::Play { voice_id: id.0, start_host_time_ns: 0 };
        self.send_seq(SeqCommand::Audio(cmd))?;
        Ok(ServerResponse::Ok)
    }

    fn audition(&self, cue_id: u64, start_secs: Option<f64>) -> Result<ServerResponse, String> {
        let (node, _, _) = self.ask(|reply| SeqCommand::GetCueDetail(CueId(cue_id), reply), SEQUENCER_TIMEOUT)?.ok_or_else(|| format!("no existe la cue {cue_id}"))?;
        let audio = node.body.audio().ok_or("la audición solo sirve en cues de audio o vídeo")?;
        let path = node.basics.file_target.as_ref().map(|t| t.absolute_path.clone()).filter(|p| !p.is_empty()).ok_or("la cue no tiene archivo")?;
        let start = start_secs.unwrap_or_else(|| audio.time_loops.start_override_secs.unwrap_or(audio.time_loops.start_time_sec));
        let slot = crate::adhoc::AUDITION_SLOT;
        let loaded = self.adhoc_load(slot, AdhocParams { path, gain_db: 0.0, monitor: true, start_secs: start })?;
        *self.audition_cue.lock().expect("envenenado") = Some(cue_id);
        self.adhoc_play(slot, Some(qlab_core::cue::eq_params(&audio.eq)))?;
        Ok(loaded)
    }

    fn adhoc_stop(&self, slot: u32) -> Result<ServerResponse, String> {
        let id = crate::adhoc::voice_id(slot);
        if slot == crate::adhoc::AUDITION_SLOT {
            *self.audition_cue.lock().expect("envenenado") = None;
        }
        let cmd = qlab_engine::rt_command::AudioCommand::Stop { voice_id: id, fade_frames: 480, curve: qlab_engine::rt_command::CurveTable::LINEAR };
        self.send_seq(SeqCommand::Audio(cmd))?;
        Ok(ServerResponse::Ok)
    }

    /// Escribe el guion técnico (HTML imprimible) del show actual en `path`.
    pub fn export_cue_sheet(&self, path: &Path, name: &str, durations: &[(u64, f64)], generated: &str) -> Result<ServerResponse, String> {
        let cues = self.ask(SeqCommand::ExportShow, SEQUENCER_TIMEOUT)?;
        let show = ShowFile { format: SHOW_FORMAT.into(), name: name.to_string(), sample_rate: self.timebase.sample_rate(), carts: self.carts.lock().expect("envenenado").clone(), cues };
        let map: std::collections::HashMap<u64, f64> = durations.iter().copied().collect();
        let html = qlab_protocol::cue_sheet::render_html(&show, &map, generated);
        let tmp = path.with_extension("html.tmp");
        std::fs::write(&tmp, html).map_err(|e| format!("{}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, path).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            format!("{}: {e}", path.display())
        })?;
        Ok(ServerResponse::CueSheetExported { path: path.to_string_lossy().into_owned(), cues: show.cue_count() })
    }

    /// Empaqueta el show en `dir`: copia cada archivo multimedia vinculado a `dir/audio/` (sin duplicar el que
    /// usan varias cues, y renombrando si dos archivos distintos se llaman igual), reescribe sus rutas como
    /// relativas y guarda `dir/<name>.json`. Los archivos que no existen se anotan y su ruta se deja como estaba.
    pub fn bundle_show(&self, dir: &Path, name: &str) -> Result<ServerResponse, String> {
        let cues = self.ask(SeqCommand::ExportShow, SEQUENCER_TIMEOUT)?;
        let mut show = ShowFile { format: SHOW_FORMAT.into(), name: name.to_string(), sample_rate: self.timebase.sample_rate(), carts: self.carts.lock().expect("envenenado").clone(), cues };
        let audio_dir = dir.join("audio");
        std::fs::create_dir_all(&audio_dir).map_err(|e| format!("{}: {e}", audio_dir.display()))?;

        let mut copied: std::collections::HashMap<std::path::PathBuf, String> = std::collections::HashMap::new();
        let mut used: std::collections::HashSet<String> = std::collections::HashSet::new();
        let (mut files, mut bytes) = (0usize, 0u64);
        let (mut missing, mut errors) = (Vec::<String>::new(), Vec::<String>::new());
        show.walk_mut(|n| {
            let Some(t) = n.basics.file_target.as_mut() else { return };
            if t.absolute_path.is_empty() {
                return;
            }
            let src = std::path::PathBuf::from(&t.absolute_path);
            if !src.is_file() {
                missing.push(t.absolute_path.clone());
                return;
            }
            let rel = copied.entry(src.clone()).or_insert_with(|| {
                let file = src.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_else(|| "media".into());
                let (stem, ext) = match file.rsplit_once('.') {
                    Some((s, e)) => (s.to_string(), format!(".{e}")),
                    None => (file.clone(), String::new()),
                };
                let mut candidate = file.clone();
                let mut k = 2;
                while !used.insert(candidate.to_lowercase()) {
                    candidate = format!("{stem}-{k}{ext}");
                    k += 1;
                }
                let dest = audio_dir.join(&candidate);
                let same = dest.canonicalize().ok().zip(src.canonicalize().ok()).is_some_and(|(a, b)| a == b);
                if !same {
                    match std::fs::copy(&src, &dest) {
                        Ok(n) => {
                            files += 1;
                            bytes += n;
                        }
                        Err(e) => errors.push(format!("{}: {e}", src.display())),
                    }
                }
                format!("audio/{candidate}")
            });
            t.absolute_path = rel.clone();
            t.relative_path = Some(rel.clone());
        });
        if let Some(e) = errors.first() {
            return Err(format!("no se pudo copiar {e}"));
        }
        show.validate().map_err(|e| format!("el show no es válido: {e}"))?;
        let stem: String = name.chars().map(|c| if c.is_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.') { c } else { '_' }).collect();
        let file = dir.join(format!("{}.teatroshow", if stem.trim().is_empty() { "show" } else { stem.trim() }));
        let tmp = file.with_extension("teatroshow.tmp");
        std::fs::write(&tmp, show.to_json()).map_err(|e| format!("{}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, &file).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            format!("{}: {e}", file.display())
        })?;
        Ok(ServerResponse::ShowBundled { path: file.to_string_lossy().into_owned(), files, bytes, missing })
    }

    pub(crate) fn handle(&self, request: ClientRequest) -> ServerResponse {
        let ok_or = |r: Result<(), String>| match r {
            Ok(()) => ServerResponse::Ok,
            Err(message) => ServerResponse::Error { message },
        };
        match request {
            ClientRequest::Hello { protocol, .. } => {
                if protocol > IPC_PROTOCOL_VERSION {
                    return ServerResponse::Error { message: format!("el cliente habla el protocolo {protocol}; el daemon, el {IPC_PROTOCOL_VERSION}") };
                }
                ServerResponse::Hello {
                    server_protocol: IPC_PROTOCOL_VERSION,
                    shm_name: self.info.shm_name.clone(),
                    sample_rate: self.info.sample_rate,
                    channels: self.info.channels,
                    backend: self.info.backend.clone(),
                }
            }
            ClientRequest::Ping { nonce } => ServerResponse::Pong { nonce },
            ClientRequest::Go => ok_or(self.hot(HotControlEvent::Go)),
            ClientRequest::Panic => ok_or(self.hot(HotControlEvent::Panic)),
            ClientRequest::StopAll => ok_or(self.hot(HotControlEvent::StopAll)),
            ClientRequest::HardStopAll => ok_or(self.send_seq(SeqCommand::Control(Control::HardStopAll))),
            ClientRequest::PauseAll => ok_or(self.send_seq(SeqCommand::Control(Control::PauseAll))),
            ClientRequest::ResumeAll => ok_or(self.send_seq(SeqCommand::Control(Control::ResumeAll))),
            ClientRequest::FaderDrag { cue_id, row, col, db } => {
                if row as usize > qlab_protocol::matrix::MAX_INPUT_CHANNELS || col as usize > qlab_protocol::matrix::MAX_OUTPUT_CHANNELS || db.is_nan() {
                    return ServerResponse::Error { message: format!("celda ({row}, {col}) o nivel {db} inválidos") };
                }
                ok_or(self.hot(HotControlEvent::LiveCrosspointDrag { cue_num_id: cue_id, row, col, db }))
            }
            ClientRequest::Devamp { cue_id } => ok_or(self.send_seq(SeqCommand::Devamp(CueId(cue_id)))),
            ClientRequest::SetPlayhead { cue_id } => ok_or(self.send_seq(SeqCommand::SetPlayhead(cue_id.map(CueId)))),
            ClientRequest::GetStatus => match self.ask(SeqCommand::GetStatus, SEQUENCER_TIMEOUT) {
                Ok(s) => ServerResponse::Status(s),
                Err(message) => ServerResponse::Error { message },
            },
            ClientRequest::GetCues => match self.ask(SeqCommand::GetCues, SEQUENCER_TIMEOUT) {
                Ok(cues) => ServerResponse::Cues { cues },
                Err(message) => ServerResponse::Error { message },
            },
            ClientRequest::GetCueDetail { cue_id } => match self.ask(|reply| SeqCommand::GetCueDetail(CueId(cue_id), reply), SEQUENCER_TIMEOUT) {
                Ok(Some((node, parent, state))) => ServerResponse::CueDetail { node: Box::new(node), parent, state },
                Ok(None) => ServerResponse::Error { message: format!("no existe la cue {cue_id}") },
                Err(message) => ServerResponse::Error { message },
            },
            ClientRequest::AddCue { kind, parent, after } => match self.add_cue(&kind, parent, after) {
                Ok(cue_id) => ServerResponse::CueAdded { cue_id },
                Err(message) => ServerResponse::Error { message },
            },
            ClientRequest::DeleteCue { cue_id } => ok_or(self.delete_cue(cue_id)),
            ClientRequest::MoveCue { cue_id, delta } => ok_or(self.ask(|reply| SeqCommand::MoveCue { id: CueId(cue_id), delta, reply }, SEQUENCER_TIMEOUT).and_then(|r| r).map(|_| ())),
            ClientRequest::SetMasterDb { db } => {
                let x10 = if db.is_finite() { (db * 10.0).round().clamp(crate::health::MASTER_MUTE_X10 as f32, crate::health::MASTER_MAX_X10 as f32) as i32 } else { 0 };
                self.health.master_db_x10.store(x10, Ordering::Relaxed);
                ServerResponse::Ok
            }
            ClientRequest::CueAction { cue_id, action } => {
                use crate::cue_edit::control_action_from_dto;
                ok_or(self.send_seq(SeqCommand::CueAction(CueId(cue_id), control_action_from_dto(action))))
            }
            ClientRequest::ApplyEmergencyKey { key } => self.apply_emergency_key(&key).unwrap_or_else(|message| ServerResponse::Error { message }),
            ClientRequest::NewShow => self.new_show().unwrap_or_else(|message| ServerResponse::Error { message }),
            ClientRequest::AdhocLoad { slot, path, gain_db, monitor, start_secs } => self.adhoc_load(slot, AdhocParams { path, gain_db, monitor, start_secs }).unwrap_or_else(|message| ServerResponse::Error { message }),
            ClientRequest::SetCarts { bank } => {
                *self.carts.lock().expect("envenenado") = bank.clamped();
                ServerResponse::Ok
            }
            ClientRequest::GetCarts => ServerResponse::Carts { bank: self.carts.lock().expect("envenenado").clone() },
            ClientRequest::AdhocPlay { slot } => self.adhoc_play(slot, None).unwrap_or_else(|message| ServerResponse::Error { message }),
            ClientRequest::AdhocStop { slot } => self.adhoc_stop(slot).unwrap_or_else(|message| ServerResponse::Error { message }),
            ClientRequest::Audition { cue_id, start_secs } => self.audition(cue_id, start_secs).unwrap_or_else(|message| ServerResponse::Error { message }),
            ClientRequest::AuditionStop => self.adhoc_stop(crate::adhoc::AUDITION_SLOT).unwrap_or_else(|message| ServerResponse::Error { message }),
            ClientRequest::AdhocStopAll => {
                let slots: Vec<u32> = self.adhoc.lock().expect("envenenado").keys().copied().collect();
                for s in slots {
                    if let Err(message) = self.adhoc_stop(s) {
                        return ServerResponse::Error { message };
                    }
                }
                ServerResponse::Ok
            }
            ClientRequest::ExportCueSheet { path, name, durations, generated } => self.export_cue_sheet(Path::new(&path), &name, &durations, &generated).unwrap_or_else(|message| ServerResponse::Error { message }),
            ClientRequest::BundleShow { dir, name } => self.bundle_show(Path::new(&dir), &name).unwrap_or_else(|message| ServerResponse::Error { message }),
            ClientRequest::SaveShow { path, name } => self.save_show(Path::new(&path), &name).unwrap_or_else(|message| ServerResponse::Error { message }),
            ClientRequest::UpdateCue { node } => ok_or(self.update_cue(*node)),
            ClientRequest::SetStartPoint { cue_id, start_secs } => ok_or(self.set_start_point(cue_id, start_secs)),
            ClientRequest::LoadShow { path } => match self.load_show_file(Path::new(&path)) {
                Ok(r) => r,
                Err(message) => ServerResponse::Error { message },
            },
            // `Subscribe` lo trata la conexión, que es la dueña del flujo de eventos.
            ClientRequest::Subscribe { .. } => ServerResponse::Ok,
            ClientRequest::Shutdown => {
                self.events.publish(ServerEvent::ShuttingDown);
                self.shutdown.store(true, Ordering::Release);
                ServerResponse::Ok
            }
        }
    }
}

/// `id` de una petición que no se pudo interpretar entera (0 si ni eso se puede leer).
fn recover_id(text: &str) -> u64 {
    serde_json::from_str::<serde_json::Value>(text).ok().and_then(|v| v.get("id")?.as_u64()).unwrap_or(0)
}

fn write_line(writer: &Mutex<SendHalf>, json: &str) -> io::Result<()> {
    let mut w = writer.lock().expect("envenenado");
    w.write_all(json.as_bytes())?;
    w.write_all(b"\n")?;
    w.flush()
}

/// Una conexión: lee peticiones, responde, y si se suscribió reenvía los eventos del daemon.
fn serve(stream: Stream, ctx: Arc<IpcContext>) {
    let (recv, send) = stream.split();
    let writer = Arc::new(Mutex::new(send));
    let mut reader = BufReader::new(recv);
    let alive = Arc::new(AtomicBool::new(true));
    let mut pump_stop: Option<Arc<AtomicBool>> = None;
    let mut line = String::new();

    loop {
        line.clear();
        let n = match (&mut reader).take(MAX_LINE_BYTES).read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        if n as u64 >= MAX_LINE_BYTES && !line.ends_with('\n') {
            let r = ResponseMessage { id: 0, response: ServerResponse::Error { message: "línea demasiado larga".into() } };
            let _ = write_line(&writer, &serde_json::to_string(&r).expect("serializable"));
            break;
        }
        let text = line.trim();
        if text.is_empty() {
            continue;
        }
        let (id, response) = match serde_json::from_str::<ClientMessage>(text) {
            Ok(msg) => {
                if let ClientRequest::Subscribe { events } = &msg.request {
                    match (events, pump_stop.is_some()) {
                        (true, false) => pump_stop = Some(spawn_event_pump(&ctx, &writer, &alive)),
                        (false, true) => {
                            if let Some(stop) = pump_stop.take() {
                                stop.store(true, Ordering::Release);
                            }
                        }
                        _ => {}
                    }
                }
                (msg.id, ctx.handle(msg.request))
            }
            // Si al menos el `id` se puede leer, el error llega con él y el cliente lo asocia a su petición.
            Err(e) => (recover_id(text), ServerResponse::Error { message: format!("mensaje inválido: {e}") }),
        };
        let json = serde_json::to_string(&ResponseMessage { id, response }).expect("serializable");
        if write_line(&writer, &json).is_err() {
            break;
        }
    }
    alive.store(false, Ordering::Release);
    if let Some(stop) = pump_stop {
        stop.store(true, Ordering::Release);
    }
}

fn spawn_event_pump(ctx: &Arc<IpcContext>, writer: &Arc<Mutex<SendHalf>>, alive: &Arc<AtomicBool>) -> Arc<AtomicBool> {
    let stop = Arc::new(AtomicBool::new(false));
    let rx = ctx.events.subscribe();
    let (writer, alive, stop_flag) = (Arc::clone(writer), Arc::clone(alive), Arc::clone(&stop));
    let _ = std::thread::Builder::new().name("qlab-ipc-events".into()).spawn(move || {
        while alive.load(Ordering::Acquire) && !stop_flag.load(Ordering::Acquire) {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(event) => {
                    if write_line(&writer, &serde_json::to_string(&event).expect("serializable")).is_err() {
                        break;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
    });
    stop
}

pub struct IpcServer {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
    path: String,
}

impl IpcServer {
    /// Abre el socket. Falla con `AddrInUse` si ya hay un daemon atendiendo en `path`; un archivo de
    /// socket huérfano de un daemon anterior se reemplaza.
    pub fn start(path: &str, ctx: Arc<IpcContext>) -> io::Result<Self> {
        let name = socket_name(path)?;
        if Stream::connect(name.clone()).is_ok() {
            return Err(io::Error::new(io::ErrorKind::AddrInUse, format!("ya hay un daemon escuchando en {path}")));
        }
        #[cfg(unix)]
        if Path::new(path).exists() {
            let _ = std::fs::remove_file(path);
        }
        let listener = ListenerOptions::new().name(name).create_sync()?;
        listener.set_nonblocking(ListenerNonblockingMode::Accept)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::clone(&stop);
        let join = std::thread::Builder::new().name("qlab-ipc-accept".into()).spawn(move || {
            while !stop_flag.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok(stream) => {
                        let ctx = Arc::clone(&ctx);
                        let _ = std::thread::Builder::new().name("qlab-ipc-conn".into()).spawn(move || serve(stream, ctx));
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(5)),
                    Err(e) => {
                        eprintln!("[ipc] error al aceptar: {e}");
                        std::thread::sleep(Duration::from_millis(50));
                    }
                }
            }
        })?;
        Ok(Self { stop, join: Some(join), path: path.to_string() })
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
        #[cfg(unix)]
        let _ = std::fs::remove_file(&self.path);
    }
}

impl Drop for IpcServer {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::recover_id;

    #[test]
    fn ids_are_recovered_from_broken_requests() {
        assert_eq!(recover_id(r#"{"id":42,"cmd":"inventado"}"#), 42);
        assert_eq!(recover_id(r#"{"id":7,"cmd":"fader_drag","db":null}"#), 7);
        assert_eq!(recover_id("esto no es json"), 0);
        assert_eq!(recover_id(r#"{"cmd":"go"}"#), 0);
        assert_eq!(recover_id(r#"{"id":"x"}"#), 0);
    }
}
