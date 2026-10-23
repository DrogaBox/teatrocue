//! La aplicación: estado, reparto de eventos del daemon, atajos de teclado y diseño de la ventana.
//!
//! # Repintado puramente reactivo
//! `eframe` solo dibuja cuando algo lo pide. Esta aplicación pide un repintado en tres casos:
//! * hay actividad visible (algo suena o algún vúmetro sigue cayendo): cada 16 ms (~60 FPS);
//! * el hilo del enlace detectó un cambio en la memoria compartida o llegó un evento del daemon;
//! * vence un aviso temporal o se está esperando la reconexión (1 Hz).
//!
//! En reposo no se pide nada: la ventana no consume CPU. El daemon publica su telemetría a 120 Hz
//! aunque no pase nada; la huella de actividad (`Telemetry::activity_signature`) ignora ese ruido.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{self, Align2, Color32, RichText};
use qlab_daemon::shm::SharedMemory;
use qlab_protocol::ipc_msg::{ClientRequest, ServerEvent, ServerResponse};
use qlab_protocol::show_format::{CueBody, CueNodeDto, VideoFit};

use crate::media::{cue_name_of, media_kind};
use crate::file_picker::{home_dir, FilePicker, Filter, PickerOutcome};
use crate::link::{DaemonLink, HelloInfo, LinkEvent};
use crate::model::{CueUiState, Telemetry, UiModel};
use crate::native_dialog::{show_name_of, DialogKind, DialogOutcome, NativeDialogs};
use crate::standalone::{self, EmbeddedEngine};
use crate::theme;
use crate::views::inspector::{self, Draft, InspectorOutput, InspectorState, TabCtx};
use crate::views::{self, Action, ViewCtx};
use crate::video::{VideoHub, VideoMeta};
use crate::widgets;
use crate::waveform::WaveformCache;

/// Cadencia de dibujo mientras hay actividad.
/// Alto inicial y mínimo del panel del Inspector (px).
pub const INSPECTOR_DEFAULT_HEIGHT: f32 = 380.0;
pub const INSPECTOR_MIN_HEIGHT: f32 = 340.0;

pub const ACTIVE_FRAME: Duration = Duration::from_millis(16);
const TOAST_LIFETIME: Duration = Duration::from_secs(6);
const MAX_TOASTS: usize = 5;
/// Dos pulsaciones de Panic dentro de este margen son una parada dura.
const DOUBLE_PANIC: Duration = Duration::from_millis(700);

#[derive(Clone, Debug)]
pub struct AppConfig {
    pub socket: String,
    /// Arrancar el motor embebido al abrir (`Some(con_audio)`).
    pub embedded_on_start: Option<bool>,
    /// Cerrar la ventana tras este tiempo (para pruebas de humo).
    pub exit_after: Option<Duration>,
    /// Estado de la licencia de esta PC (bloquea la interfaz si es inválida).
    pub license: crate::license_gate::LicenseInfo,
    /// Entorno de licencia para la reactivación de emergencia (`None` en compilaciones que no la exigen).
    pub license_setup: Option<qlab_license::activation::Setup>,
    /// Ajustes de audio con los que arranca el motor embebido.
    pub audio: crate::settings::AudioSettings,
    /// Dónde se guardan los ajustes de audio (`None` = no se guardan: pruebas).
    pub settings_path: Option<std::path::PathBuf>,
    /// Cada cuántos segundos se autoguarda un show con cambios (`None` = sin autoguardado: pruebas).
    pub autosave_secs: Option<u64>,
}

impl AppConfig {
    pub fn new(socket: impl Into<String>) -> Self {
        Self { socket: socket.into(), embedded_on_start: None, exit_after: None, license: crate::license_gate::LicenseInfo::not_enforced(), license_setup: None, audio: Default::default(), settings_path: None, autosave_secs: None }
    }
}

#[derive(Clone, Debug)]
pub enum ConnState {
    Connecting,
    Connected(HelloInfo),
    Down { reason: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Warn,
    Error,
}

#[derive(Clone, Debug)]
pub struct Toast {
    pub text: String,
    pub kind: ToastKind,
    born: Instant,
}

enum Pending {
    Cues,
    /// Punto de arranque de «retomar desde donde se paró» (o su vuelta al original).
    StartPoint,
    Detail(u64),
    Update(u64, CueNodeDto),
    LoadShow(Option<std::path::PathBuf>),
    NewShow,
    Save { path: std::path::PathBuf, name: String },
    Bundle,
    CueSheet,
    GetCarts,
    CartLoad(usize),
    CartPlay(usize),
    /// Audición (PFL) pedida; al confirmarse se sabe cuánto dura.
    Pfl,
    /// Autoguardado periódico: no cambia el show abierto ni su marca de cambios.
    Autosave,
    /// Detalle de una cue de vídeo que suena y de la que aún no se conocen archivo y frecuencia.
    VideoMeta(u64),
    /// Alta de una cue: al llegar la lista se selecciona la nueva.
    AddCue,
    /// Importación por arrastre: alta de la cue (con el archivo), su detalle y su guardado con el archivo asignado.
    ImportAdd(std::path::PathBuf),
    ImportDetail(u64, std::path::PathBuf),
    ImportUpdate(u64),
    /// Baja o cambio de orden (el texto es lo que se hizo, para los avisos).
    Structure(&'static str),
    /// Clave de reactivación de emergencia enviada al daemon en marcha.
    Reactivate,
    /// Deshacer/Rehacer: reenvío del estado anterior o posterior de una cue (no se anota de nuevo en el historial).
    UpdateReplay(u64, CueNodeDto),
    /// Duplicar: se creó una cue vacía del mismo tipo; falta pedir su detalle y copiarle los datos del original.
    DupAdd(Box<CueNodeDto>),
    DupDetail(Box<CueNodeDto>),
    DupUpdate(u64),
    /// Se pidió el detalle de una cue para cambiarle el modo de continuación y reenviarla.
    CycleContinue(u64),
}

/// Cola de archivos soltados que se convierten en cues de uno en uno.
#[derive(Default)]
struct Import {
    queue: std::collections::VecDeque<std::path::PathBuf>,
    /// La siguiente cue entra detrás de esta (la anterior importada).
    after: Option<u64>,
    first: Option<u64>,
    active: bool,
    done: usize,
}

/// Operación que necesita confirmación porque descartaría cambios sin guardar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Confirm {
    NewShow,
    OpenShow,
}

enum ActivePicker {
    Audio(FilePicker),
    Show(FilePicker),
}

pub struct QlabApp {
    cfg: AppConfig,
    link: DaemonLink,
    pub conn: ConnState,
    pub model: UiModel,
    pub telemetry: Telemetry,
    shm: Option<Arc<SharedMemory>>,
    pub inspector: InspectorState,
    /// Altura (px) que ocupó el panel del Inspector en el último fotograma.
    pub inspector_height: f32,
    pub(crate) waveforms: WaveformCache,
    picker: Option<ActivePicker>,
    dialogs: NativeDialogs,
    /// Archivo del show abierto (`None` = sin guardar todavía).
    pub show_path: Option<std::path::PathBuf>,
    /// Hay cambios desde el último guardado, apertura o show nuevo.
    pub show_dirty: bool,
    pub confirm: Option<Confirm>,
    select_after_refresh: Option<u64>,
    import: Import,
    pub video: VideoHub,
    /// Archivo, frecuencia y ajuste de las cues de vídeo vistas (de `GetCueDetail`).
    pub video_meta: HashMap<u64, VideoMeta>,
    video_meta_requested: std::collections::HashSet<u64>,
    video_was_running: std::collections::HashSet<u64>,
    /// Ventana secundaria de proyección (abierta/cerrada, pantalla completa).
    pub projection: views::projection_viewport::Projection,
    /// Cue de vídeo que se proyecta ahora: la última en arrancar entre las que suenan.
    pub projector_cue: Option<u64>,
    /// Modo de continuación elegido en la tabla y aún sin confirmar por el daemon: gana a una lista de cues anterior.
    continue_overrides: HashMap<u64, qlab_protocol::cue_dto::ContinueMode>,
    /// Rectángulo del panel del Inspector en el último fotograma (para soltar archivos sobre él).
    pub inspector_rect: egui::Rect,
    last_title: String,
    pub toasts: Vec<Toast>,
    pub license: crate::license_gate::LicenseInfo,
    pub help: views::help_modal::HelpState,
    /// Dónde se anota qué novedades ya se vieron (`None` sin ajustes: pruebas) y si hay que mostrarlas al arrancar.
    news_seen: Option<std::path::PathBuf>,
    news_waiting: bool,
    /// Preferencias del operador (retomar al parar, silencio de Windows, atajos) y dónde se guardan (`None`: pruebas).
    pub prefs: crate::prefs::Prefs,
    prefs_path: Option<std::path::PathBuf>,
    pub prefs_ui: views::preferences::PrefsUi,
    /// Último envío del EQ mientras se arrastra un punto (para no pasar de ~16 por segundo).
    last_live_commit: Option<Instant>,
    /// Importar audio: convertidor y copia de CD.
    pub audio_tools: views::audio_tools::AudioToolsUi,
    /// Mantiene en silencio los sonidos de Windows (solo en el programa real, en Windows).
    system_sounds: Option<crate::system_sounds::Keeper>,
    /// Punto donde retomar tras una parada (cue, segundos del archivo) y desde cuándo espera: se anuncia en seguida y se
    /// guarda cuando la cue terminó de apagarse (el motor no deja cambiar el arranque de una cue que todavía suena).
    pending_resume: Option<(u64, f64)>,
    resume_since: Option<Instant>,
    /// Cues con un punto de «retomar» puesto: el arranque que tenían antes (se les devuelve) y si ya se dispararon desde ahí.
    resume_points: HashMap<u64, ResumePoint>,
    pub react: views::reactivation::ReactivationState,
    pub history: crate::history::History,
    /// Ventana global «Memorias».
    pub memories_open: bool,
    pub memories_panel: views::memories::PanelState,
    /// Memoria que se está nombrando (tecla M).
    pub(crate) quick_memory: Option<views::memories::Quick>,
    /// Volumen maestro (dB); empieza en 0 y se manda al daemon al conectar.
    pub master_db: f32,
    /// Memoria elegida mientras la cue sonaba: al confirmarse su nuevo punto de arranque la cue se relanza desde ahí.
    restart_after_save: Option<(u64, f64)>,
    /// Relanzar esta cue cuando llegue el instante (deja pasar el Stop antes del nuevo GO).
    restart_due: Option<(Instant, u64)>,
    /// Audición (PFL) en curso: cue, punto de arranque explícito (memoria) y cuándo termina por sí sola.
    pub(crate) pfl: Option<(u64, Option<f64>)>,
    hud: Option<(String, Instant)>,
    /// Copias reparadas de archivos dañados, por archivo original.
    pub(crate) repairs: std::collections::HashMap<std::path::PathBuf, crate::repair::RepairState>,
    repair_tx: std::sync::mpsc::Sender<(std::path::PathBuf, Result<std::path::PathBuf, String>)>,
    repair_rx: std::sync::mpsc::Receiver<(std::path::PathBuf, Result<std::path::PathBuf, String>)>,
    /// Cambio de tiempo de una memoria pedido desde el panel para una cue cuyo detalle todavía no está abierto.
    pending_memory_time: Option<(u64, f64, f64)>,
    /// «Bloqueo de función»: durante una función en vivo no se puede alterar el show por accidente.
    pub(crate) locked: bool,
    /// Navegación numérica de memorias («. N .» del teclado numérico): dígitos tecleados hasta ahora.
    num_nav: Option<(String, Instant)>,
    pub(crate) carts: views::carts::CartsState,
    /// Pad para el que se está eligiendo archivo.
    cart_pick: Option<usize>,
    pfl_until: Option<Instant>,
    /// Último archivo y volumen enviados al daemon por pad (para no recargar en cada letra que se teclea).
    carts_prepared: std::collections::HashMap<usize, (String, u32)>,
    autosave_due: Instant,
    autosave_inflight: bool,
    /// Autoguardado de una sesión que se cerró de golpe, pendiente de que el operador decida.
    pub(crate) recovery: Option<crate::autosave::Recoverable>,
    /// Al terminar de cargar un autoguardado: a qué show pertenece (`Some(None)` = show sin nombre).
    recover_as: Option<Option<std::path::PathBuf>>,
    /// Shows abiertos o guardados hace poco (el más reciente primero).
    pub recent: Vec<std::path::PathBuf>,
    pub audio_ui: views::audio_settings::AudioSettingsUi,
    /// Show a recargar cuando el motor reiniciado vuelva a conectar (tras cambiar el audio).
    reload_show_after_connect: Option<std::path::PathBuf>,
    embedded: Option<EmbeddedEngine>,
    engine_message: Option<String>,
    pending: HashMap<u64, Pending>,
    next_tag: u64,
    cues_requested: bool,
    refresh_cues: bool,
    follow_playhead: bool,
    scroll_to_selected: bool,
    pub safe_mute: bool,
    panic_at: Option<Instant>,
    /// Tras mover el playhead localmente, se ignora la telemetría un instante (aún trae el valor viejo).
    local_playhead_until: Option<Instant>,
    down_since: Option<Instant>,
    started: Instant,
    last_frame: Instant,
    pub frames: u64,
}

impl QlabApp {
    pub fn new(ctx: &egui::Context, cfg: AppConfig) -> Self {
        theme::apply(ctx);
        let wake_ctx = ctx.clone();
        let link = DaemonLink::spawn(cfg.socket.clone(), move || wake_ctx.request_repaint());
        let wave_ctx = ctx.clone();
        let repair_chan = std::sync::mpsc::channel();
        let mut app = Self {
            link,
            conn: ConnState::Connecting,
            model: UiModel::default(),
            telemetry: Telemetry::default(),
            shm: None,
            inspector: InspectorState::default(),
            inspector_height: 0.0,
            waveforms: WaveformCache::new(move || wave_ctx.request_repaint()),
            picker: None,
            dialogs: NativeDialogs::new({
                let c = ctx.clone();
                move || c.request_repaint()
            }),
            show_path: None,
            show_dirty: false,
            confirm: None,
            select_after_refresh: None,
            import: Import::default(),
            video: VideoHub::default(),
            video_meta: HashMap::new(),
            video_meta_requested: Default::default(),
            video_was_running: Default::default(),
            projection: Default::default(),
            projector_cue: None,
            continue_overrides: HashMap::new(),
            inspector_rect: egui::Rect::NOTHING,
            last_title: String::new(),
            toasts: Vec::new(),
            license: cfg.license.clone(),
            help: Default::default(),
            news_seen: cfg.settings_path.as_deref().map(crate::novedades::seen_path),
            prefs: cfg.settings_path.as_deref().map(|p| crate::prefs::load(&crate::prefs::path(p))).unwrap_or_default(),
            prefs_path: cfg.settings_path.as_deref().map(crate::prefs::path),
            prefs_ui: Default::default(),
            last_live_commit: None,
            audio_tools: views::audio_tools::AudioToolsUi::new(crate::file_picker::home_dir().join("Music")),
            system_sounds: None,
            pending_resume: None,
            resume_since: None,
            resume_points: HashMap::new(),
            news_waiting: cfg.settings_path.as_deref().map(crate::novedades::seen_path).is_some_and(|p| crate::novedades::should_show(&p)),
            react: Default::default(),
            history: Default::default(),
            memories_open: false,
            memories_panel: Default::default(),
            quick_memory: None,
            master_db: 0.0,
            restart_after_save: None,
            restart_due: None,
            pfl: None,
            hud: None,
            repairs: Default::default(),
            repair_tx: repair_chan.0,
            repair_rx: repair_chan.1,
            pending_memory_time: None,
            locked: false,
            num_nav: None,
            carts: Default::default(),
            cart_pick: None,
            pfl_until: None,
            carts_prepared: Default::default(),
            autosave_due: Instant::now() + Duration::from_secs(cfg.autosave_secs.unwrap_or(60)),
            autosave_inflight: false,
            recovery: None,
            recover_as: None,
            recent: cfg.settings_path.as_deref().map(|p| crate::settings::load_recent(&crate::settings::recent_path(p))).unwrap_or_default(),
            audio_ui: Default::default(),
            reload_show_after_connect: None,
            embedded: None,
            engine_message: None,
            pending: HashMap::new(),
            next_tag: 1,
            cues_requested: false,
            refresh_cues: false,
            follow_playhead: false,
            scroll_to_selected: false,
            safe_mute: false,
            panic_at: None,
            local_playhead_until: None,
            down_since: None,
            started: Instant::now(),
            last_frame: Instant::now(),
            frames: 0,
            cfg,
        };
        app.memories_panel.preroll = app.cfg.audio.preroll_secs;
        if crate::system_sounds::SUPPORTED && app.cfg.settings_path.is_some() {
            app.system_sounds = Some(crate::system_sounds::Keeper::system(app.cfg.settings_path.as_deref(), app.prefs.mute_system_sounds));
        }
        app.look_for_recovery();
        if let Some(w) = app.license.expiry_warning() {
            app.toast(ToastKind::Warn, w);
        }
        if let (Some(with_audio), false) = (app.cfg.embedded_on_start, app.license.is_locked()) {
            app.start_embedded(with_audio);
        }
        app
    }

    /// ¿Hay un motor embebido en marcha?
    pub fn embedded_running(&self) -> bool {
        self.embedded.as_ref().is_some_and(EmbeddedEngine::is_running)
    }

    pub fn is_connected(&self) -> bool {
        matches!(self.conn, ConnState::Connected(_))
    }

    pub fn toast(&mut self, kind: ToastKind, text: impl Into<String>) {
        if self.toasts.len() >= MAX_TOASTS {
            self.toasts.remove(0);
        }
        self.toasts.push(Toast { text: text.into(), kind, born: Instant::now() });
    }

    fn hello(&self) -> Option<&HelloInfo> {
        match &self.conn {
            ConnState::Connected(h) => Some(h),
            _ => None,
        }
    }

    fn new_tag(&mut self, p: Pending) -> u64 {
        let tag = self.next_tag;
        self.next_tag += 1;
        self.pending.insert(tag, p);
        tag
    }

    fn request_cues(&mut self) {
        if self.cues_requested {
            self.refresh_cues = true;
            return;
        }
        self.cues_requested = true;
        let tag = self.new_tag(Pending::Cues);
        self.link.request(tag, ClientRequest::GetCues);
    }

    fn request_detail(&mut self, id: u64) {
        let tag = self.new_tag(Pending::Detail(id));
        self.link.request(tag, ClientRequest::GetCueDetail { cue_id: id });
    }

    // -- Reparto de lo que llega del daemon ----------------------------------------------------

    fn handle_link_events(&mut self) {
        for ev in self.link.poll() {
            match ev {
                LinkEvent::Connected { hello, shm } => {
                    self.toast(ToastKind::Info, format!("Conectado · {} · {} Hz · {} canales", hello.backend, hello.sample_rate, hello.channels));
                    self.conn = ConnState::Connected(hello);
                    self.shm = shm;
                    self.telemetry = Telemetry::default();
                    self.down_since = None;
                    self.pending.clear();
                    self.cues_requested = false;
                    self.request_cues();
                    self.request_carts();
                    self.link.fire(ClientRequest::SetMasterDb { db: self.master_db });
                    if let Some(path) = self.reload_show_after_connect.take() {
                        self.load_show_path(path);
                    }
                }
                LinkEvent::Disconnected { reason } => {
                    if self.is_connected() {
                        self.toast(ToastKind::Error, "Se perdió la conexión con el daemon");
                    }
                    self.conn = ConnState::Down { reason };
                    self.shm = None;
                    self.pending.clear();
                    self.cues_requested = false;
                    self.inspector.in_flight = None;
                    self.down_since.get_or_insert_with(Instant::now);
                }
                LinkEvent::Reply { tag, response } => self.on_reply(tag, response),
                LinkEvent::Failure(msg) => self.toast(ToastKind::Error, msg),
                LinkEvent::Server(e) => self.on_server_event(e),
            }
        }
    }

    fn on_reply(&mut self, tag: u64, response: Result<ServerResponse, String>) {
        let Some(pending) = self.pending.remove(&tag) else { return };
        match (pending, response) {
            (Pending::Cues, r) => {
                self.cues_requested = false;
                match r {
                    Ok(ServerResponse::Cues { cues }) => {
                        self.model.set_summaries(cues);
                        for (id, mode) in &self.continue_overrides {
                            if let Some(c) = self.model.summaries.iter_mut().find(|c| c.id == *id) {
                                c.continue_mode = *mode;
                            }
                        }
                        // Una cue borrada deja de estar seleccionada y de ser el playhead (la telemetría puede
                        // tardar un instante en enterarse): se descarta su borrador y se elige otra.
                        if self.inspector.draft.as_ref().is_some_and(|d| self.model.summary(d.id()).is_none()) {
                            self.inspector.draft = None;
                            self.inspector.in_flight = None;
                        }
                        if self.model.playhead.is_some_and(|id| self.model.summary(id).is_none()) {
                            self.model.playhead = None;
                        }
                        // Solo se consume cuando la cue ya figura en la lista: una respuesta anterior al alta (pedida
                        // antes) no debe llevarse la selección pendiente.
                        if let Some(id) = self.select_after_refresh.filter(|&id| self.model.summary(id).is_some()) {
                            self.select_after_refresh = None;
                            self.model.selected = None;
                            self.select(id);
                        }
                        if self.model.playhead.is_none() {
                            self.model.playhead = self.telemetry.playhead().filter(|&id| self.model.summary(id).is_some());
                        }
                        match (self.model.selected, &self.inspector.draft) {
                            (Some(sel), None) => self.request_detail(sel),
                            // Primera carga: como QLab, la selección empieza en la cue en espera.
                            (None, _) => {
                                if let Some(first) = self.model.playhead.or_else(|| self.model.summaries.first().map(|c| c.id)) {
                                    self.select(first);
                                }
                            }
                            _ => {}
                        }
                    }
                    Ok(ServerResponse::Error { message }) => self.toast(ToastKind::Error, message),
                    Ok(_) => {}
                    Err(e) => self.toast(ToastKind::Error, format!("No se pudo leer la lista de cues: {e}")),
                }
                if std::mem::take(&mut self.refresh_cues) {
                    self.request_cues();
                }
            }
            (Pending::Detail(id), Ok(ServerResponse::CueDetail { node, state, .. })) => {
                self.remember_video_meta(&node);
                if self.model.selected == Some(id) {
                    match &mut self.inspector.draft {
                        Some(d) if d.id() == id => {
                            // Se conserva lo que el usuario esté escribiendo; solo se actualiza la base.
                            let keep_edits = d.dirty();
                            d.baseline = (*node).clone();
                            d.state = state;
                            if !keep_edits {
                                d.node = *node;
                            }
                        }
                        _ => self.inspector.draft = Some(Draft::new(*node, state)),
                    }
                }
            }
            (Pending::VideoMeta(_), Ok(ServerResponse::CueDetail { node, .. })) => self.remember_video_meta(&node),
            (Pending::VideoMeta(id), _) => {
                self.video_meta_requested.remove(&id);
            }
            (Pending::Detail(_), Ok(ServerResponse::Error { message })) => self.toast(ToastKind::Warn, message),
            (Pending::CycleContinue(id), Ok(ServerResponse::CueDetail { node, .. })) => {
                let mut node = *node;
                node.basics.continue_mode = self.model.summary(id).map_or(node.basics.continue_mode, |c| c.continue_mode);
                let tag = self.new_tag(Pending::Update(id, node.clone()));
                self.link.request(tag, ClientRequest::UpdateCue { node: Box::new(node) });
            }
            (Pending::CycleContinue(_), Ok(ServerResponse::Error { message })) => self.toast(ToastKind::Warn, message),
            (Pending::StartPoint, Ok(ServerResponse::Error { message })) => self.toast(ToastKind::Warn, format!("Retomar: {message}")),
            (Pending::StartPoint, Err(e)) => self.toast(ToastKind::Warn, format!("Retomar: {e}")),
            (Pending::Update(id, sent), r) => {
                // El daemon ya tiene (o rechazó) lo último que mandamos: deja de hacer falta el valor provisional.
                if self.continue_overrides.get(&id) == Some(&sent.basics.continue_mode) || r.is_err() {
                    self.continue_overrides.remove(&id);
                }
                if self.inspector.in_flight.as_ref() == Some(&sent) {
                    self.inspector.in_flight = None;
                }
                match r {
                    Ok(ServerResponse::Ok) => {
                        self.show_dirty = true;
                        if let Some(d) = self.inspector.draft.as_mut().filter(|d| d.id() == id) {
                            if let Some((rid, secs)) = self.restart_after_save {
                                if rid == id && sent.body.audio().is_some_and(|a| a.time_loops.start_override_secs == Some(secs)) {
                                    self.restart_after_save = None;
                                    self.restart_due = Some((Instant::now() + Duration::from_millis(150), id));
                                }
                            }
                            let before = std::mem::replace(&mut d.baseline, sent.clone());
                            self.history.record(id, before, sent.clone(), self.started.elapsed());
                            // Si no hay ediciones más nuevas, se adopta lo que quedó en el daemon
                            // (p. ej. el final real del archivo tras recargarlo).
                            if d.node == sent {
                                self.request_detail(id);
                            }
                        }
                        self.refresh_cues = true;
                    }
                    Ok(ServerResponse::Error { message }) => {
                        self.toast(ToastKind::Error, format!("No se guardó: {message}"));
                        self.revert_draft(id);
                    }
                    Ok(_) => {}
                    Err(e) => {
                        self.toast(ToastKind::Error, format!("No se guardó: {e}"));
                        self.revert_draft(id);
                    }
                }
            }
            (Pending::LoadShow(path), Ok(ServerResponse::ShowLoaded { name, cues, warnings })) => {
                self.history.clear();
                self.request_carts();
                if let Some(original) = self.recover_as.take() {
                    // Era un autoguardado: el show vuelve a ser el original, con cambios sin guardar.
                    self.show_path = original;
                    self.show_dirty = true;
                    self.toast(ToastKind::Info, format!("Sesión recuperada «{name}»: {cues} cues. Guarde con Ctrl+S"));
                } else {
                    if let Some(p) = &path {
                        self.remember_recent(p.clone());
                    }
                    self.show_path = path;
                    self.show_dirty = false;
                    self.toast(ToastKind::Info, format!("Show «{name}» cargado: {cues} cues"));
                }
                for w in warnings {
                    self.toast(ToastKind::Warn, w);
                }
            }
            (Pending::LoadShow(_), Ok(ServerResponse::Error { message })) => self.toast(ToastKind::Error, format!("No se pudo cargar el show: {message}")),
            (Pending::LoadShow(_), Err(e)) => self.toast(ToastKind::Error, format!("No se pudo cargar el show: {e}")),
            (Pending::NewShow, Ok(ServerResponse::ShowLoaded { .. })) => {
                self.request_carts();
                self.history.clear();
                self.show_path = None;
                self.show_dirty = false;
                self.model.selected = None;
                self.toast(ToastKind::Info, "Show nuevo");
            }
            (Pending::Save { path, name }, Ok(ServerResponse::ShowSaved { cues, .. })) => {
                self.remember_recent(path.clone());
                // El guardado manual deja obsoleto el autoguardado del show anterior y el del show nuevo.
                crate::autosave::remove(self.show_path.as_deref(), &self.autosave_dir());
                crate::autosave::remove(None, &self.autosave_dir());
                crate::autosave::remove(Some(&path), &self.autosave_dir());
                self.show_path = Some(path.clone());
                self.model.show_name = name.clone();
                self.show_dirty = false;
                self.toast(ToastKind::Info, format!("Guardado «{name}» ({cues} cues) en {}", path.display()));
            }
            (Pending::CueSheet, Ok(ServerResponse::CueSheetExported { path, cues })) => self.toast(ToastKind::Info, format!("Guion técnico exportado ({cues} cues): {path}")),
            (Pending::CueSheet, Ok(ServerResponse::Error { message })) => self.toast(ToastKind::Error, format!("No se pudo exportar el guion: {message}")),
            (Pending::CueSheet, Err(e)) => self.toast(ToastKind::Error, format!("No se pudo exportar el guion: {e}")),
            (Pending::GetCarts, Ok(ServerResponse::Carts { bank })) => {
                self.carts.set_bank(bank);
                for i in 0..qlab_protocol::cue_dto::CART_PADS {
                    self.prepare_pad(i);
                }
            }
            (Pending::CartLoad(i), Ok(ServerResponse::AdhocLoaded { duration, .. })) => self.carts.duration[i] = duration,
            (Pending::CartLoad(i), Ok(ServerResponse::Error { message })) => self.toast(ToastKind::Warn, format!("Pad {}: {message}", i + 1)),
            (Pending::CartPlay(i), Ok(ServerResponse::Error { message })) => {
                self.carts.mark_stopped(i);
                self.toast(ToastKind::Warn, format!("Pad {}: {message}", i + 1));
            }
            (Pending::Pfl, Ok(ServerResponse::AdhocLoaded { duration, .. })) => self.pfl_until = duration.map(|d| Instant::now() + Duration::from_secs_f64(d + 0.2)),
            (Pending::Pfl, Ok(ServerResponse::Error { message })) => {
                self.pfl = None;
                self.toast(ToastKind::Error, format!("No se pudo escuchar en auriculares: {message}"));
            }
            (Pending::Pfl, Err(e)) => {
                self.pfl = None;
                self.toast(ToastKind::Error, format!("No se pudo escuchar en auriculares: {e}"));
            }
            (Pending::Autosave, _) => self.autosave_inflight = false,
            (Pending::Bundle, Ok(ServerResponse::ShowBundled { path, files, bytes, missing })) => {
                self.toast(ToastKind::Info, format!("Show empaquetado en {path}: {files} archivo(s), {:.1} MB", bytes as f64 / 1_048_576.0));
                for m in missing.iter().take(3) {
                    self.toast(ToastKind::Warn, format!("No se encontró {m}: se dejó su ruta original"));
                }
            }
            (Pending::Bundle, Ok(ServerResponse::Error { message })) => self.toast(ToastKind::Error, format!("No se pudo empaquetar: {message}")),
            (Pending::Bundle, Err(e)) => self.toast(ToastKind::Error, format!("No se pudo empaquetar: {e}")),
            (Pending::AddCue, Ok(ServerResponse::CueAdded { cue_id })) => {
                self.show_dirty = true;
                self.select_after_refresh = Some(cue_id);
                self.refresh_cues = true;
            }
            (Pending::DupAdd(src), Ok(ServerResponse::CueAdded { cue_id })) => {
                self.show_dirty = true;
                self.refresh_cues = true;
                let tag = self.new_tag(Pending::DupDetail(src));
                self.link.request(tag, ClientRequest::GetCueDetail { cue_id });
            }
            (Pending::DupDetail(src), Ok(ServerResponse::CueDetail { node, .. })) => {
                // La copia conserva el id y el número que le dio el daemon; todo lo demás viene del original.
                let mut copy = *src;
                copy.basics.unique_id = node.basics.unique_id;
                copy.basics.number = node.basics.number.clone();
                if !copy.basics.name.is_empty() {
                    copy.basics.name = format!("{} (copia)", copy.basics.name);
                }
                let id = copy.basics.unique_id.0;
                let tag = self.new_tag(Pending::DupUpdate(id));
                self.link.request(tag, ClientRequest::UpdateCue { node: Box::new(copy) });
            }
            (Pending::DupUpdate(id), Ok(ServerResponse::Ok)) => {
                self.show_dirty = true;
                self.refresh_cues = true;
                self.select_after_refresh = Some(id);
                self.toast(ToastKind::Info, "Cue duplicada");
            }
            (Pending::DupAdd(_) | Pending::DupDetail(_) | Pending::DupUpdate(_), r) => {
                let why = match r {
                    Ok(ServerResponse::Error { message }) => message,
                    Err(e) => e,
                    Ok(_) => "respuesta inesperada".into(),
                };
                self.toast(ToastKind::Error, format!("No se pudo duplicar la cue: {why}"));
            }
            (Pending::ImportAdd(path), Ok(ServerResponse::CueAdded { cue_id })) => {
                self.show_dirty = true;
                self.refresh_cues = true;
                let tag = self.new_tag(Pending::ImportDetail(cue_id, path));
                self.link.request(tag, ClientRequest::GetCueDetail { cue_id });
            }
            (Pending::ImportAdd(path), r) => {
                let why = match r {
                    Ok(ServerResponse::Error { message }) => message,
                    Err(e) => e,
                    Ok(_) => "respuesta inesperada".into(),
                };
                self.toast(ToastKind::Error, format!("No se pudo crear la cue de «{}»: {why}", cue_name_of(&path)));
                self.next_import();
            }
            (Pending::ImportDetail(id, path), Ok(ServerResponse::CueDetail { node, .. })) => {
                let mut node = *node;
                node.basics.file_target = Some(qlab_protocol::cue_dto::MediaTarget { absolute_path: path.to_string_lossy().into_owned(), ..Default::default() });
                node.basics.name = cue_name_of(&path);
                node.basics.armed = true;
                if let Some(t) = crate::views::inspector::time_loops_mut(&mut node) {
                    t.start_time_sec = 0.0;
                    t.end_time_sec = 0.0;
                    t.slices.clear();
                }
                let tag = self.new_tag(Pending::ImportUpdate(id));
                self.link.request(tag, ClientRequest::UpdateCue { node: Box::new(node) });
            }
            (Pending::ImportDetail(id, path), _) => {
                self.toast(ToastKind::Error, format!("No se pudo preparar la cue de «{}»", cue_name_of(&path)));
                self.import.after = Some(id);
                self.next_import();
            }
            (Pending::ImportUpdate(id), r) => {
                if let Ok(ServerResponse::Ok) = r {
                    self.import.done += 1;
                    if self.import.first.is_none() {
                        self.import.first = Some(id);
                        self.select_after_refresh = Some(id);
                    }
                } else {
                    let why = match r {
                        Ok(ServerResponse::Error { message }) => message,
                        Err(e) => e,
                        Ok(_) => "respuesta inesperada".into(),
                    };
                    self.toast(ToastKind::Error, format!("La cue {id} quedó sin archivo: {why}"));
                }
                self.import.after = Some(id);
                self.refresh_cues = true;
                self.next_import();
            }
            (Pending::Structure(_), Ok(ServerResponse::Ok)) => {
                self.show_dirty = true;
                self.refresh_cues = true;
            }
            (Pending::Structure(what), Ok(ServerResponse::Error { message })) => self.toast(ToastKind::Error, format!("No se pudo {what} la cue: {message}")),
            (Pending::Structure(what), Err(e)) => self.toast(ToastKind::Error, format!("No se pudo {what} la cue: {e}")),
            (Pending::UpdateReplay(id, node), Ok(ServerResponse::Ok)) => {
                self.show_dirty = true;
                if let Some(d) = self.inspector.draft.as_mut().filter(|d| d.id() == id) {
                    d.baseline = node;
                }
                self.request_detail(id);
                self.refresh_cues = true;
            }
            (Pending::UpdateReplay(..), Ok(ServerResponse::Error { message })) => self.toast(ToastKind::Error, format!("No se pudo deshacer/rehacer: {message}")),
            (Pending::UpdateReplay(..), Err(e)) => self.toast(ToastKind::Error, format!("No se pudo deshacer/rehacer: {e}")),
            (Pending::Reactivate, Ok(ServerResponse::LicenseExtended { valid_until, .. })) => self.on_license_extended(valid_until),
            (Pending::Reactivate, Ok(ServerResponse::Error { message })) => self.react.outcome = Some(views::reactivation::Outcome::Err(format!("No se pudo activar: {message}"))),
            (Pending::Reactivate, Err(e)) => self.react.outcome = Some(views::reactivation::Outcome::Err(format!("No se pudo activar: {e}"))),
            (Pending::NewShow | Pending::AddCue | Pending::Save { .. }, Ok(ServerResponse::Error { message })) => self.toast(ToastKind::Error, message),
            (Pending::NewShow | Pending::AddCue | Pending::Save { .. }, Err(e)) => self.toast(ToastKind::Error, e),
            _ => {}
        }
    }

    fn revert_draft(&mut self, id: u64) {
        if let Some(d) = self.inspector.draft.as_mut().filter(|d| d.id() == id) {
            d.node = d.baseline.clone();
        }
    }

    fn on_server_event(&mut self, e: ServerEvent) {
        match e {
            ServerEvent::PlayheadMoved { cue } => {
                // Los eventos de una ráfaga de movimientos locales llegan con retraso y en orden: durante
                // el margen se ignoran para que el playhead no parpadee hacia atrás; la telemetría corrige.
                if !self.local_playhead_grace() {
                    self.set_playhead(cue);
                }
            }
            ServerEvent::CueStateChanged { cue, state } => self.model.set_state(cue, &state),
            ServerEvent::CueUpdated { cue } => {
                self.refresh_cues = true;
                self.video_meta.remove(&cue);
                self.video_meta_requested.remove(&cue);
                let editing = self.inspector.draft.as_ref().is_some_and(|d| d.id() == cue && (d.dirty() || self.inspector.in_flight.is_some()));
                if self.model.selected == Some(cue) && !editing {
                    self.request_detail(cue);
                }
            }
            // Alta, baja o cambio de orden de cues: se vuelve a pedir la lista (asíncrona, sin bloquear la UI).
            ServerEvent::StructureChanged => self.refresh_cues = true,
            ServerEvent::ShowLoaded { name } => {
                self.video_meta.clear();
                self.video_meta_requested.clear();
                self.video.players.clear();
                self.model.show_name = name;
                self.inspector.draft = None;
                self.inspector.in_flight = None;
                self.refresh_cues = true;
            }
            ServerEvent::Warning { source, message } => self.toast(ToastKind::Warn, format!("[{source}] {message}")),
            ServerEvent::SafeMute { engaged } => {
                self.safe_mute = engaged;
                self.toast(if engaged { ToastKind::Error } else { ToastKind::Info }, if engaged { "SAFE MUTE: el secuenciador se congeló" } else { "Safe mute liberado" });
            }
            ServerEvent::ShuttingDown => self.toast(ToastKind::Warn, "El daemon se está apagando"),
        }
    }

    fn local_playhead_grace(&self) -> bool {
        self.local_playhead_until.is_some_and(|t| Instant::now() < t)
    }

    fn set_playhead(&mut self, cue: Option<u64>) {
        if self.model.playhead == cue {
            return;
        }
        self.model.playhead = cue;
        if std::mem::take(&mut self.follow_playhead) {
            if let Some(c) = cue {
                self.select(c);
            }
        }
    }

    // -- Acciones del usuario --------------------------------------------------------------------

    pub fn select(&mut self, id: u64) {
        if self.model.selected == Some(id) && self.inspector.draft.as_ref().is_some_and(|d| d.id() == id) {
            return;
        }
        self.model.selected = Some(id);
        self.scroll_to_selected = true;
        if self.inspector.draft.as_ref().is_some_and(|d| d.id() != id) {
            self.inspector.draft = None;
            self.inspector.in_flight = None;
        }
        if self.is_connected() {
            self.request_detail(id);
        }
    }

    /// GO: manda el comando y adelanta la selección de inmediato a la cue que va a quedar en espera.
    pub fn go(&mut self) {
        if !self.is_connected() || self.model.playhead.is_none() {
            return;
        }
        self.link.fire(ClientRequest::Go);
        self.follow_playhead = true;
        if let Some(next) = self.model.playhead.and_then(|p| self.model.next_top_level_after(p)) {
            self.select(next);
        }
    }

    #[cfg(test)]
    pub(crate) fn panic_fired_for_test(&self) -> bool {
        self.panic_at.is_some()
    }

    pub fn panic_press(&mut self) {
        self.note_stop_for_resume();
        let hard = self.panic_at.is_some_and(|t| t.elapsed() < DOUBLE_PANIC);
        self.panic_at = Some(Instant::now());
        self.link.fire(if hard { ClientRequest::HardStopAll } else { ClientRequest::Panic });
        if hard {
            self.toast(ToastKind::Warn, "Parada dura");
        }
    }

    /// Acciones que alteran el show: el Bloqueo de función las rechaza.
    fn blocked_by_lock(a: &Action) -> bool {
        matches!(
            a,
            Action::OpenShowPicker | Action::PickAudioFile | Action::NewShow | Action::OpenRecent(_) | Action::Duplicate | Action::Undo | Action::Redo | Action::AddCue(_) | Action::DeleteSelected | Action::CycleContinue(_) | Action::MoveSelected(_)
        )
    }

    pub fn toggle_lock(&mut self) {
        self.locked = !self.locked;
        self.quick_memory = None;
        if self.locked {
            // Entrar en función: los puntos de ensayo se quitan y cada cue vuelve a su arranque.
            self.pending_resume = None;
            self.restore_resume_points();
        }
        self.hud(if self.locked { "Bloqueo de función: ACTIVADO" } else { "Bloqueo de función: desactivado" });
    }

    fn lock_refuses(&mut self) -> bool {
        if self.locked {
            let key = self.prefs.keys.primary(crate::prefs::Command::Lock);
            self.toast(ToastKind::Info, format!("Bloqueo de función activo: {key} para desbloquear"));
        }
        self.locked
    }

    pub fn apply(&mut self, a: Action, ctx: &egui::Context) {
        if self.locked && Self::blocked_by_lock(&a) {
            self.lock_refuses();
            return;
        }
        match a {
            Action::Go => self.go(),
            Action::PrefsChanged => self.prefs_changed(),
            Action::Panic => self.panic_press(),
            Action::HardStopAll => {
                self.note_stop_for_resume();
                self.link.fire(ClientRequest::HardStopAll);
            }
            Action::StopAll => {
                self.note_stop_for_resume();
                self.link.fire(ClientRequest::StopAll);
            }
            Action::PauseAll => self.link.fire(ClientRequest::PauseAll),
            Action::ResumeAll => self.link.fire(ClientRequest::ResumeAll),
            Action::SelectCue(id) => self.select(id),
            Action::SetPlayhead(id) => self.move_playhead_locally(id),
            Action::FireCue(id) => {
                if self.model.summary(id).is_some_and(|c| c.parent.is_none()) {
                    self.move_playhead_locally(id);
                    self.link.fire(ClientRequest::Go);
                    self.select(id);
                    self.follow_playhead = true;
                } else {
                    self.toast(ToastKind::Info, "Solo se pueden disparar cues de nivel superior; usa el grupo");
                }
            }
            Action::ToggleGroup(id) => self.model.toggle_collapsed(id),
            Action::OpenShowPicker => self.begin_open_show(),
            Action::PickAudioFile => self.begin_pick_audio(),
            Action::NewShow => self.begin_new_show(),
            Action::SaveShow => self.save_show(),
            Action::OpenRecent(path) => self.load_show_path(path),
            Action::Duplicate => self.duplicate_selected(),
            Action::SetMasterDb(db) => {
                self.master_db = db.clamp(-60.0, 6.0);
                self.link.fire(ClientRequest::SetMasterDb { db: self.master_db });
            }
            Action::Undo => self.replay_history(true),
            Action::Redo => self.replay_history(false),
            Action::SaveShowAs => self.save_show_as(),
            Action::BundleShow => self.begin_bundle(),
            Action::OpenAudioTools => self.audio_tools.open = true,
            Action::ExportCueSheet => self.begin_export_cue_sheet(),
            Action::ToggleLock => self.toggle_lock(),
            Action::OpenProjector => {
                self.open_projector();
                // Windows y wgpu crean la superficie de la ventana nueva en el siguiente repintado: se pide ya, en las dos
                // ventanas, para que aparezca al instante aunque no haya nada sonando.
                ctx.request_repaint();
                ctx.request_repaint_of(views::projection_viewport::Projection::id());
            }
            Action::AddCue(kind) => self.add_cue(kind),
            Action::DeleteSelected => self.delete_selected(),
            Action::CycleContinue(id) => self.cycle_continue(id),
            Action::MoveSelected(delta) => self.move_selected(delta),
            Action::RetryConnection => self.link.retry_now(),
            Action::StartEmbedded { with_audio } => self.start_embedded(with_audio),
            Action::LaunchDaemonProcess => match standalone::launch_daemon_process(&self.cfg.socket, &self.cfg.audio) {
                Ok(()) => {
                    self.engine_message = Some("Daemon lanzado; conectando…".into());
                    self.link.retry_now();
                }
                Err(e) => self.engine_message = Some(format!("No se pudo lanzar el daemon: {e}")),
            },
            Action::Quit => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
        }
    }

    // -- Vídeo ------------------------------------------------------------------------------------

    /// Guarda archivo, frecuencia, inicio y ajuste de una cue de vídeo para poder sincronizar su imagen.
    fn remember_video_meta(&mut self, node: &CueNodeDto) {
        let CueBody::Video(v) = &node.body else { return };
        let Some(path) = node.basics.file_target.as_ref().map(|t| t.absolute_path.clone()).filter(|p| !p.is_empty()) else { return };
        let id = node.basics.unique_id.0;
        self.video_meta_requested.remove(&id);
        self.video_meta.insert(id, VideoMeta { path: path.into(), sample_rate: v.audio.time_loops.file_sample_rate.max(1), start_time_sec: v.audio.time_loops.start_time_sec, fit: v.fit });
    }

    /// Un fotograma de vídeo: las cues que suenan siguen el reloj de audio (`source_frame`); la seleccionada, si no
    /// suena, muestra su primer cuadro como miniatura; el resto de reproductores se liberan.
    fn video_tick(&mut self, ctx: &egui::Context) {
        if !self.is_connected() {
            return;
        }
        let running: Vec<(u64, crate::model::RunningInfo)> = self
            .telemetry
            .running
            .iter()
            .filter(|(id, i)| i.phase == Some(crate::model::CueUiPhase::Action) && self.model.summary(**id).is_some_and(|c| c.kind == "video"))
            .map(|(id, i)| (*id, *i))
            .collect();
        let mut keep: Vec<u64> = Vec::new();
        let mut latest: Option<(u64, f32)> = None;
        for (id, info) in &running {
            let Some(meta) = self.video_meta.get(id).cloned() else {
                if self.video_meta_requested.insert(*id) {
                    let tag = self.new_tag(Pending::VideoMeta(*id));
                    self.link.request(tag, ClientRequest::GetCueDetail { cue_id: *id });
                }
                continue;
            };
            let clock = info.cursor_secs(meta.sample_rate, meta.start_time_sec, 1.0);
            self.video.player(ctx, *id, &meta).sync(ctx, clock);
            keep.push(*id);
            if latest.is_none_or(|(_, e)| info.action_elapsed < e) {
                latest = Some((*id, info.action_elapsed));
            }
        }
        self.projector_cue = latest.map(|(id, _)| id);
        // Las que dejaron de sonar vuelven a su miniatura.
        let now_running: std::collections::HashSet<u64> = running.iter().map(|(id, _)| *id).collect();
        for id in self.video_was_running.difference(&now_running) {
            if let Some(p) = self.video.players.get_mut(id) {
                p.reset_to_still();
            }
        }
        self.video_was_running = now_running.clone();
        // Miniatura de la cue de vídeo del Inspector.
        if let Some(d) = self.inspector.draft.as_ref().filter(|d| d.node.body.is_video() && !now_running.contains(&d.id())) {
            let id = d.id();
            if let (CueBody::Video(v), Some(t)) = (&d.node.body, d.node.basics.file_target.as_ref().filter(|t| !t.absolute_path.is_empty())) {
                let meta = VideoMeta { path: t.absolute_path.clone().into(), sample_rate: v.audio.time_loops.file_sample_rate.max(1), start_time_sec: v.audio.time_loops.start_time_sec, fit: v.fit };
                self.video.player(ctx, id, &meta).show_still(ctx, meta.start_time_sec);
                keep.push(id);
            }
        } else if let Some(d) = self.inspector.draft.as_ref().filter(|d| d.node.body.is_video()) {
            keep.push(d.id());
        }
        self.video.keep_only(&keep);
    }

    pub fn open_projector(&mut self) {
        self.projection.toggle_open();
    }

    /// Ventana secundaria para el proyector: muestra el cuadro de la cue de vídeo que suena (negro si no hay
    /// ninguna), sincronizado con el reloj de audio por `video_tick`. Ver `views::projection_viewport`.
    fn show_projector(&mut self, ctx: &egui::Context) {
        let content = match self.projector_cue {
            Some(cue) => views::projection_viewport::Content { texture: self.video.texture_of(cue).cloned(), fit: self.video_meta.get(&cue).map_or(VideoFit::Fit, |m| m.fit) },
            None => views::projection_viewport::Content { texture: None, fit: VideoFit::Fit },
        };
        self.projection.show(ctx, &content);
    }

    // -- Autoría y archivos --------------------------------------------------------------------------

    /// Título de la ventana: nombre del show (o «Sin título»), con `*` si hay cambios sin guardar.
    pub fn window_title(&self) -> String {
        let name = if self.model.show_name.is_empty() { "Sin título" } else { self.model.show_name.as_str() };
        format!("{name}{}{} — TeatroCue", if self.show_dirty { " *" } else { "" }, if self.locked { " [FUNCIÓN BLOQUEADA]" } else { "" })
    }

    fn start_dir(&self) -> Option<std::path::PathBuf> {
        self.show_path.as_ref().and_then(|p| p.parent().map(std::path::Path::to_path_buf)).or_else(|| std::env::current_dir().ok())
    }

    pub fn begin_new_show(&mut self) {
        if !self.is_connected() {
            return;
        }
        if self.show_dirty {
            self.confirm = Some(Confirm::NewShow);
        } else {
            self.do_new_show();
        }
    }

    fn do_new_show(&mut self) {
        let tag = self.new_tag(Pending::NewShow);
        self.link.request(tag, ClientRequest::NewShow);
    }

    pub fn begin_open_show(&mut self) {
        if !self.is_connected() {
            return;
        }
        if self.show_dirty {
            self.confirm = Some(Confirm::OpenShow);
        } else {
            self.start_open_show();
        }
    }

    fn start_open_show(&mut self) {
        if self.dialogs.enabled() {
            if !self.dialogs.open(DialogKind::OpenShow, self.start_dir(), None) {
                self.toast(ToastKind::Info, "Ya hay un diálogo abierto");
            }
        } else {
            let start = self.start_dir().unwrap_or_else(home_dir);
            self.picker = Some(ActivePicker::Show(FilePicker::new("Abrir show", start, Filter::Show)));
        }
    }

    /// Archivo → Empaquetar show: pide una carpeta y la daemon copia allí medios y show.
    pub fn begin_bundle(&mut self) {
        if !self.is_connected() {
            return;
        }
        if self.dialogs.enabled() {
            if !self.dialogs.open(DialogKind::PickBundleFolder, self.start_dir(), None) {
                self.toast(ToastKind::Info, "Ya hay un diálogo abierto");
            }
        } else {
            // Sin diálogo nativo: carpeta «<nombre>-paquete» junto al directorio actual.
            let name = if self.model.show_name.is_empty() { "show".to_string() } else { self.model.show_name.clone() };
            let dir = self.start_dir().unwrap_or_else(home_dir).join(format!("{name}-paquete"));
            self.bundle_to(dir);
        }
    }

    pub fn begin_export_cue_sheet(&mut self) {
        if !self.is_connected() {
            return;
        }
        let name = if self.model.show_name.is_empty() { "cue_sheet".to_string() } else { format!("{} - guion tecnico", self.model.show_name) };
        let file_name = format!("{name}.html");
        if self.dialogs.enabled() {
            if !self.dialogs.open(DialogKind::ExportCueSheet, self.start_dir(), Some(file_name)) {
                self.toast(ToastKind::Info, "Ya hay un diálogo abierto");
            }
        } else {
            let dir = self.start_dir().unwrap_or_else(home_dir);
            self.export_cue_sheet_to(dir.join("cue_sheet.html"));
        }
    }

    pub fn export_cue_sheet_to(&mut self, path: std::path::PathBuf) {
        let name = if self.model.show_name.is_empty() { "Sin título".to_string() } else { self.model.show_name.clone() };
        let durations = self.model.summaries.iter().filter_map(|c| c.duration.map(|d| (c.id, d))).collect();
        let generated = qlab_license::fmt_date(qlab_license::now_unix());
        let tag = self.new_tag(Pending::CueSheet);
        self.link.request(tag, ClientRequest::ExportCueSheet { path: path.to_string_lossy().into_owned(), name, durations, generated });
    }

    pub fn bundle_to(&mut self, dir: std::path::PathBuf) {
        let name = if self.model.show_name.is_empty() { "Sin título".to_string() } else { self.model.show_name.clone() };
        self.toast(ToastKind::Info, "Empaquetando show…");
        let tag = self.new_tag(Pending::Bundle);
        self.link.request(tag, ClientRequest::BundleShow { dir: dir.to_string_lossy().into_owned(), name });
    }

    pub fn load_show_path(&mut self, path: std::path::PathBuf) {
        let tag = self.new_tag(Pending::LoadShow(Some(path.clone())));
        self.link.request(tag, ClientRequest::LoadShow { path: path.to_string_lossy().into_owned() });
    }

    pub fn save_show(&mut self) {
        match self.show_path.clone() {
            Some(p) => self.save_to(p),
            None => self.save_show_as(),
        }
    }

    pub fn save_show_as(&mut self) {
        if !self.is_connected() {
            return;
        }
        let file_name = format!("{}.{}", if self.model.show_name.is_empty() { "show" } else { self.model.show_name.as_str() }, crate::native_dialog::SHOW_EXTENSION);
        if self.dialogs.enabled() {
            if !self.dialogs.open(DialogKind::SaveShowAs, self.start_dir(), Some(file_name)) {
                self.toast(ToastKind::Info, "Ya hay un diálogo abierto");
            }
        } else {
            // Sin diálogo nativo: se guarda junto al directorio actual con el nombre del show.
            let dir = self.start_dir().unwrap_or_else(home_dir);
            self.save_to(dir.join(file_name));
        }
    }

    fn save_to(&mut self, path: std::path::PathBuf) {
        // Un guardado pendiente de edición: primero se confirma lo que haya en el Inspector.
        let name = show_name_of(&path);
        // Los puntos de «retomar» del ensayo no se guardan en el show: se quitan para guardar y se vuelven a poner.
        let rehearsal: Vec<(u64, Option<f64>, f64)> = self.resume_points.iter().filter(|(id, _)| !self.sounding(**id)).map(|(&id, r)| (id, r.prev, r.start)).collect();
        for &(id, prev, _) in &rehearsal {
            self.send_start_point(id, prev);
        }
        let tag = self.new_tag(Pending::Save { path: path.clone(), name: name.clone() });
        self.link.request(tag, ClientRequest::SaveShow { path: path.to_string_lossy().into_owned(), name });
        for &(id, _, start) in &rehearsal {
            self.send_start_point(id, Some(start));
        }
    }

    fn begin_pick_audio(&mut self) {
        let start = self
            .inspector
            .draft
            .as_ref()
            .and_then(|d| d.node.basics.file_target.as_ref())
            .and_then(|t| std::path::Path::new(&t.absolute_path).parent().map(std::path::Path::to_path_buf))
            .filter(|p| p.is_dir())
            .unwrap_or_else(home_dir);
        if self.dialogs.enabled() {
            if !self.dialogs.open(DialogKind::PickAudio, Some(start), None) {
                self.toast(ToastKind::Info, "Ya hay un diálogo abierto");
            }
        } else {
            self.picker = Some(ActivePicker::Audio(FilePicker::new("Elegir archivo de audio", start, Filter::Audio)));
        }
    }

    /// Recoge lo que eligió el usuario en un diálogo nativo.
    pub fn on_dialog_outcome(&mut self, o: DialogOutcome) {
        let Some(path) = o.path else { return };
        match o.kind {
            DialogKind::OpenShow => self.load_show_path(path),
            DialogKind::SaveShowAs => self.save_to(path),
            DialogKind::PickAudio => self.set_audio_file(path),
            DialogKind::PickBundleFolder => self.bundle_to(path),
            DialogKind::ExportCueSheet => self.export_cue_sheet_to(path),
            DialogKind::PickCartAudio => self.on_cart_file_picked(path),
            DialogKind::PickConvertFiles => {
                let mut files = vec![path];
                files.extend(o.more);
                self.audio_tools.add_files(files);
            }
            DialogKind::PickToolFolder => self.audio_tools.set_folder(path),
        }
    }

    /// Crea una cue (`audio`, `fade` o `group`) justo después de la seleccionada.
    pub fn add_cue(&mut self, kind: &'static str) {
        if !self.is_connected() {
            return;
        }
        let tag = self.new_tag(Pending::AddCue);
        // Sin selección (o con una que ya no existe, p. ej. lista vacía) la cue va al final de la raíz.
        let after = self.model.selected.filter(|&id| self.model.summary(id).is_some());
        self.link.request(tag, ClientRequest::AddCue { kind: kind.into(), parent: None, after });
    }

    /// Cicla el modo de continuación de una cue desde la tabla. Si es la que está en el Inspector se edita
    /// su borrador (se guarda sola); si no, se pide su detalle y se reenvía modificada.
    pub fn cycle_continue(&mut self, id: u64) {
        let Some(current) = self.model.summary(id).map(|c| c.continue_mode) else { return };
        let next = views::cue_table::next_continue(current);
        // La celda cambia al instante; el daemon la confirma al guardar.
        if let Some(c) = self.model.summaries.iter_mut().find(|c| c.id == id) {
            c.continue_mode = next;
        }
        self.continue_overrides.insert(id, next);
        if let Some(d) = self.inspector.draft.as_mut().filter(|d| d.id() == id) {
            d.node.basics.continue_mode = next;
        } else if self.is_connected() {
            let tag = self.new_tag(Pending::CycleContinue(id));
            self.link.request(tag, ClientRequest::GetCueDetail { cue_id: id });
        }
    }

    pub fn delete_selected(&mut self) {
        let (true, Some(id)) = (self.is_connected(), self.model.selected) else { return };
        let tag = self.new_tag(Pending::Structure("eliminar"));
        self.link.request(tag, ClientRequest::DeleteCue { cue_id: id });
    }

    pub fn move_selected(&mut self, delta: i32) {
        let (true, Some(id)) = (self.is_connected(), self.model.selected) else { return };
        let tag = self.new_tag(Pending::Structure("mover"));
        self.link.request(tag, ClientRequest::MoveCue { cue_id: id, delta });
    }

    // -- Arrastrar y soltar --------------------------------------------------------------------------

    /// ¿La cue en el Inspector puede recibir un archivo directamente (audio o vídeo)?
    fn selected_takes_media(&self) -> bool {
        self.inspector.draft.as_ref().is_some_and(|d| crate::views::inspector::time_loops_ref(&d.node).is_some())
    }

    /// Archivos soltados sobre la ventana: sobre el Inspector con una cue de audio/vídeo cambian su archivo;
    /// en cualquier otro sitio se crean cues nuevas (una por archivo compatible, en orden).
    fn handle_drops(&mut self, ctx: &egui::Context) {
        let dropped: Vec<std::path::PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).collect());
        if dropped.is_empty() {
            return;
        }
        if !self.is_connected() {
            self.toast(ToastKind::Warn, "Conéctate al daemon antes de soltar archivos");
            return;
        }
        if self.lock_refuses() {
            return;
        }
        let total = dropped.len();
        let media: Vec<std::path::PathBuf> = dropped.into_iter().filter(|p| media_kind(p).is_some()).collect();
        if media.is_empty() {
            self.toast(ToastKind::Warn, "Ningún archivo compatible (.wav, .flac, .mp3, .ogg, .mp4, .mov, .mkv)");
            return;
        }
        if media.len() < total {
            self.toast(ToastKind::Info, format!("{} archivo(s) ignorados por no ser multimedia", total - media.len()));
        }
        let over_inspector = ctx.input(|i| i.pointer.latest_pos()).is_some_and(|p| self.inspector_rect.contains(p));
        if over_inspector && self.selected_takes_media() {
            if media.len() > 1 {
                self.toast(ToastKind::Info, "Solo se usa el primer archivo para la cue seleccionada");
            }
            self.set_audio_file(media[0].clone());
            return;
        }
        self.begin_import(media);
    }

    pub fn begin_import(&mut self, files: Vec<std::path::PathBuf>) {
        self.import.queue.extend(files);
        if self.import.active {
            return; // se encadenan a la importación en curso
        }
        self.import = Import { queue: std::mem::take(&mut self.import.queue), after: self.model.selected.filter(|&id| self.model.summary(id).is_some()), first: None, active: true, done: 0 };
        self.next_import();
    }

    fn next_import(&mut self) {
        let Some(path) = self.import.queue.pop_front() else {
            if self.import.active && self.import.done > 0 {
                self.toast(ToastKind::Info, format!("{} cue(s) importadas", self.import.done));
            }
            self.import.active = false;
            return;
        };
        let kind = media_kind(&path).map_or("audio", |k| k.cue_kind());
        let tag = self.new_tag(Pending::ImportAdd(path));
        self.link.request(tag, ClientRequest::AddCue { kind: kind.into(), parent: None, after: self.import.after });
    }

    /// Capa semitransparente con recuadro punteado mientras se arrastran archivos sobre la ventana.
    fn show_drop_overlay(&self, ctx: &egui::Context) {
        let hovered = ctx.input(|i| i.raw.hovered_files.clone());
        if hovered.is_empty() {
            return;
        }
        let screen = ctx.content_rect();
        let over_inspector = ctx.input(|i| i.pointer.latest_pos()).is_some_and(|p| self.inspector_rect.contains(p));
        let replaces = over_inspector && self.selected_takes_media();
        let area = if replaces { self.inspector_rect } else { screen };
        let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::NULL.with("drop_overlay")));
        painter.rect_filled(screen, 0.0, Color32::from_black_alpha(150));
        let frame = area.shrink(14.0);
        let corners = [frame.left_top(), frame.right_top(), frame.right_bottom(), frame.left_bottom(), frame.left_top()];
        painter.extend(egui::Shape::dashed_line(&corners, egui::Stroke::new(2.5, theme::AMBER), 12.0, 8.0));
        let text = if replaces { "Soltar para cambiar el archivo de la cue" } else { "Soltar archivos para crear Cues" };
        painter.text(frame.center(), Align2::CENTER_CENTER, text, egui::FontId::proportional(24.0), theme::AMBER);
        painter.text(frame.center() + egui::vec2(0.0, 28.0), Align2::CENTER_CENTER, "wav · flac · mp3 · ogg · mp4 · mov · mkv", egui::FontId::proportional(13.0), theme::TEXT_DIM);
    }

    #[cfg(test)]
    pub(crate) fn enable_autosave_for_test(&mut self, secs: u64, dir: &std::path::Path) {
        self.cfg.autosave_secs = Some(secs);
        self.cfg.settings_path = Some(dir.join("audio.json"));
        self.autosave_due = Instant::now();
        self.look_for_recovery();
    }

    fn request_carts(&mut self) {
        let tag = self.new_tag(Pending::GetCarts);
        self.link.request(tag, ClientRequest::GetCarts);
    }

    /// Deja listo el reproductor del pad `i` (carga el archivo para que suene sin demora al pulsar).
    fn prepare_pad(&mut self, i: usize) {
        let Some(pad) = self.carts.bank.pads.get(i).filter(|p| !p.is_empty()).cloned() else { return };
        self.carts.duration[i] = None;
        let tag = self.new_tag(Pending::CartLoad(i));
        self.link.request(tag, ClientRequest::AdhocLoad { slot: i as u32, path: pad.path, gain_db: pad.gain_db, monitor: false, start_secs: 0.0 });
    }

    fn cart_play(&mut self, i: usize) {
        self.carts.mark_started(i, Instant::now());
        let tag = self.new_tag(Pending::CartPlay(i));
        self.link.request(tag, ClientRequest::AdhocPlay { slot: i as u32 });
    }

    fn cart_stop(&mut self, i: usize) {
        self.carts.mark_stopped(i);
        self.link.fire(ClientRequest::AdhocStop { slot: i as u32 });
    }

    /// Un pad se pulsó (clic o tecla): suena sin tocar el playhead ni el Standby.
    pub fn cart_press(&mut self, i: usize) {
        let Some(pad) = self.carts.bank.pads.get(i).filter(|p| !p.is_empty()) else { return };
        let mode = pad.mode;
        match mode {
            qlab_protocol::cue_dto::CartMode::Toggle if self.carts.playing(i, Instant::now()) => self.cart_stop(i),
            _ => self.cart_play(i),
        }
    }

    pub fn cart_release(&mut self, i: usize) {
        if self.carts.bank.pads.get(i).is_some_and(|p| p.mode == qlab_protocol::cue_dto::CartMode::Hold) {
            self.cart_stop(i);
        }
    }

    fn carts_ui(&mut self, ctx: &egui::Context) {
        for ev in views::carts::show(ctx, &mut self.carts, self.locked) {
            match ev {
                views::carts::CartEvent::Press(i) => self.cart_press(i),
                views::carts::CartEvent::Release(i) => self.cart_release(i),
                views::carts::CartEvent::StopAll => {
                    for i in 0..qlab_protocol::cue_dto::CART_PADS {
                        self.carts.mark_stopped(i);
                    }
                    self.link.fire(ClientRequest::AdhocStopAll);
                }
                views::carts::CartEvent::Browse(i) => {
                    self.cart_pick = Some(i);
                    if !self.dialogs.open(DialogKind::PickCartAudio, self.start_dir(), None) {
                        self.toast(ToastKind::Info, "Ya hay un diálogo abierto");
                    }
                }
                views::carts::CartEvent::Edited(i) => self.cart_edited(i),
            }
        }
    }

    /// Cambió un pad: se guarda en el daemon (y con el show) y, si cambió el archivo o el volumen, se vuelve a preparar.
    fn cart_edited(&mut self, i: usize) {
        self.link.fire(ClientRequest::SetCarts { bank: self.carts.bank.clone() });
        self.show_dirty = true;
        let pad = self.carts.bank.pads[i].clone();
        let key = (pad.path.clone(), pad.gain_db.to_bits());
        if self.carts_prepared.get(&i) != Some(&key) {
            self.carts_prepared.insert(i, key);
            if pad.is_empty() {
                self.cart_stop(i);
            } else if std::path::Path::new(&pad.path).is_file() {
                self.prepare_pad(i);
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn cart_edited_for_test(&mut self, i: usize) {
        self.cart_edited(i);
    }

    fn on_cart_file_picked(&mut self, path: std::path::PathBuf) {
        let Some(i) = self.cart_pick.take() else { return };
        self.carts.open = true;
        self.carts.editing = Some(i);
        let pad = &mut self.carts.bank.pads[i];
        pad.path = path.to_string_lossy().into_owned();
        if pad.name.trim().is_empty() {
            pad.name = show_name_of(&path);
        }
        self.cart_edited(i);
    }

    /// Atajos de la botonera: una tecla asignada dispara su pad; soltarla detiene los pads en modo «mantener».
    fn handle_pad_keys(&mut self, ctx: &egui::Context, typing: bool) {
        let events: Vec<(usize, bool)> = ctx.input(|inp| {
            inp.events
                .iter()
                .filter_map(|e| match e {
                    egui::Event::Key { key, pressed, repeat: false, modifiers, .. } if modifiers.is_none() => {
                        let i = views::carts::KEYS.iter().find(|n| views::carts::key_of(n) == Some(*key)).and_then(|n| self.carts.pad_for_key(n))?;
                        Some((i, *pressed))
                    }
                    _ => None,
                })
                .collect()
        });
        for (i, pressed) in events {
            if pressed && !typing {
                self.carts.held_by_key[i] = true;
                self.cart_press(i);
            } else if !pressed && self.carts.held_by_key[i] {
                self.carts.held_by_key[i] = false;
                self.cart_release(i);
            }
        }
    }

    /// Todas las memorias del show numeradas como en el panel (con las del borrador abierto, que son las más nuevas).
    fn memory_rows(&self) -> Vec<views::memories::Row> {
        let live = self.inspector.draft.as_ref().and_then(|d| d.node.body.audio().map(|a| (d.id(), a.time_loops.memories.clone())));
        views::memories::rows(&self.model.summaries, live.as_ref().map(|(id, m)| (*id, m.as_slice())), "")
    }

    /// Secuencia «. N .» (o «. N Enter») del teclado numérico: salta a la memoria número N. `true` si consumió la tecla.
    fn handle_num_nav(&mut self, ctx: &egui::Context) -> bool {
        const TIMEOUT: Duration = Duration::from_secs(6);
        if self.num_nav.as_ref().is_some_and(|(_, t)| t.elapsed() > TIMEOUT) {
            self.num_nav = None;
        }
        let keys: Vec<egui::Key> = ctx.input(|i| {
            i.events.iter().filter_map(|e| if let egui::Event::Key { key, pressed: true, repeat: false, modifiers, .. } = e { modifiers.is_none().then_some(*key) } else { None }).collect()
        });
        let digit = |k: egui::Key| {
            use egui::Key::*;
            [Num0, Num1, Num2, Num3, Num4, Num5, Num6, Num7, Num8, Num9].iter().position(|d| *d == k).map(|d| char::from(b'0' + d as u8))
        };
        let mut consumed = false;
        for k in keys {
            let active = self.num_nav.is_some();
            match k {
                egui::Key::Period if !active => {
                    self.num_nav = Some((String::new(), Instant::now()));
                    self.hud("Memoria #…");
                    consumed = true;
                }
                egui::Key::Period | egui::Key::Enter if active => {
                    let digits = self.num_nav.take().map(|(d, _)| d).unwrap_or_default();
                    if let Ok(n) = digits.parse::<usize>() {
                        self.goto_memory_number(ctx, n);
                    } else {
                        self.hud.take();
                    }
                    consumed = true;
                }
                k if active && digit(k).is_some() => {
                    if let Some((d, t)) = self.num_nav.as_mut() {
                        if d.len() < 4 {
                            d.push(digit(k).unwrap_or('0'));
                        }
                        *t = Instant::now();
                        let text = format!("Memoria #{d}…");
                        self.hud(text);
                    }
                    consumed = true;
                }
                // Cualquier otra tecla cancela la secuencia y sigue su camino (Esc sigue siendo PANIC).
                _ if active => {
                    self.num_nav = None;
                    self.hud.take();
                }
                _ => {}
            }
        }
        consumed
    }

    /// Salta a la memoria número `n` de la lista (la numeración del panel de Memorias).
    pub fn goto_memory_number(&mut self, ctx: &egui::Context, n: usize) {
        match self.memory_rows().into_iter().find(|r| r.number == n) {
            Some(r) => {
                self.hud = None;
                self.activate_memory(ctx, r.cue_id, r.secs, false);
                match self.hud.as_mut() {
                    Some((t, _)) => *t = format!("Memoria #{n} · {t}"),
                    None => self.hud(format!("Memoria #{n}")),
                }
            }
            None => self.hud(format!("No existe la memoria #{n}")),
        }
    }

    /// Duración del archivo de la cue abierta, si ya se conoce (forma de onda lista).
    fn file_duration_of_draft(&mut self) -> Option<f64> {
        let path = self.inspector.draft.as_ref().and_then(crate::views::inspector::tab_time_loops::file_path_of)?;
        match self.waveforms.get(&path) {
            crate::waveform::WaveState::Ready(p) => Some(p.duration_secs()),
            _ => None,
        }
    }

    /// Escribe a mano el tiempo de una memoria (desde el panel): se valida contra el archivo, se ajusta a un frame y la lista
    /// de la cue se reordena. Si la cue no está abierta se abre y el cambio se aplica en cuanto cargue.
    fn set_memory_time(&mut self, cue_id: u64, from: f64, to: f64) {
        if self.locked {
            self.lock_refuses();
            return;
        }
        if self.inspector.draft.as_ref().is_some_and(|d| d.id() == cue_id) {
            self.apply_memory_time(from, to);
        } else {
            self.pending_memory_time = Some((cue_id, from, to));
            self.select(cue_id);
        }
    }

    fn apply_memory_time(&mut self, from: f64, to: f64) {
        let max = self.file_duration_of_draft().unwrap_or(f64::INFINITY);
        let Some(d) = self.inspector.draft.as_mut() else { return };
        let Some(t) = d.node.body.audio_mut().map(|a| &mut a.time_loops) else { return };
        let Some(m) = t.memories.iter_mut().find(|m| (m.secs - from).abs() < 1e-6) else { return };
        if !to.is_finite() {
            return;
        }
        m.secs = views::memories::snap_to_frame(if max.is_finite() { to.min(max) } else { to }, t.file_sample_rate);
        crate::views::inspector::tab_time_loops::normalize_memories(t, if max.is_finite() { max } else { f64::MAX });
    }

    /// Crea en segundo plano la copia reparada de `original` (hasta `until` segundos) junto a él.
    pub fn start_repair(&mut self, ctx: &egui::Context, original: std::path::PathBuf, until: f64) {
        if matches!(self.repairs.get(&original), Some(crate::repair::RepairState::Running)) {
            return;
        }
        self.repairs.insert(original.clone(), crate::repair::RepairState::Running);
        let (tx, ctx) = (self.repair_tx.clone(), ctx.clone());
        let _ = std::thread::Builder::new().name("qlab-ui-repair".into()).spawn(move || {
            let out = crate::repair::repaired_path(&original, until);
            let r = crate::repair::write_repaired_copy(&original, &out, until).map(|()| out);
            let _ = tx.send((original, r));
            ctx.request_repaint();
        });
    }

    fn poll_repairs(&mut self) {
        while let Ok((original, r)) = self.repair_rx.try_recv() {
            let state = match r {
                Ok(copy) => {
                    let name = copy.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
                    self.toast(ToastKind::Info, format!("Copia reparada creada: «{name}». En Time & Loops: «Usar la copia en esta cue»"));
                    crate::repair::RepairState::Done(copy)
                }
                Err(e) => {
                    self.toast(ToastKind::Error, format!("No se pudo crear la copia reparada: {e}"));
                    crate::repair::RepairState::Failed(e)
                }
            };
            self.repairs.insert(original, state);
        }
    }

    /// Cambia el pre-roll de los saltos a memoria y lo recuerda entre sesiones.
    fn set_preroll(&mut self, v: u8) {
        self.memories_panel.preroll = v;
        self.cfg.audio.preroll_secs = v;
        if let Some(p) = &self.cfg.settings_path {
            let _ = crate::settings::save(p, &self.cfg.audio);
        }
    }

    #[cfg(test)]
    pub(crate) fn activate_memory_for_test(&mut self, ctx: &egui::Context, id: u64, secs: f64) {
        self.activate_memory(ctx, id, secs, false);
    }

    #[cfg(test)]
    pub(crate) fn news_open_for_test(&self) -> bool {
        self.help.news_open
    }

    #[cfg(test)]
    pub(crate) fn apply_for_test(&mut self, a: Action) {
        let ctx = egui::Context::default();
        self.apply(a, &ctx);
    }

    #[cfg(test)]
    pub(crate) fn hud_text(&self) -> Option<&str> {
        self.hud.as_ref().map(|(t, _)| t.as_str())
    }

    /// Aviso grande y temporal sobre la ventana (pre-roll, número de memoria…).
    pub fn hud(&mut self, text: impl Into<String>) {
        self.hud = Some((text.into(), Instant::now()));
    }

    fn show_hud(&mut self, ctx: &egui::Context) {
        const LIFETIME: Duration = Duration::from_millis(2500);
        let Some((text, born)) = self.hud.clone() else { return };
        if born.elapsed() >= LIFETIME {
            self.hud = None;
            return;
        }
        egui::Area::new(egui::Id::NULL.with("hud")).anchor(egui::Align2::CENTER_TOP, [0.0, 96.0]).order(egui::Order::Tooltip).interactable(false).show(ctx, |ui| {
            egui::Frame::new().fill(Color32::from_black_alpha(215)).stroke(egui::Stroke::new(1.5, theme::AMBER)).corner_radius(8.0).inner_margin(egui::Margin::symmetric(18, 10)).show(ui, |ui| {
                ui.label(RichText::new(text).size(22.0).strong().color(theme::AMBER));
            });
        });
        ctx.request_repaint_after(Duration::from_millis(200));
    }

    /// «Retomar desde donde se paró» (ensayo): al parar, anota dónde estaba la música que sonaba. Si suenan varias, la
    /// última que arrancó (la que se estaba ensayando). Solo cues de audio o vídeo de nivel superior: son las que el GO
    /// vuelve a disparar. Con el Bloqueo de función no hace nada: en función cada cue arranca donde corresponde.
    fn note_stop_for_resume(&mut self) {
        if !self.prefs.resume_after_stop || self.locked {
            return;
        }
        let pick = self
            .telemetry
            .running
            .iter()
            .filter(|(_, i)| matches!(i.phase, Some(crate::model::CueUiPhase::Action)) && i.source_frame > 0)
            .filter_map(|(&id, i)| {
                let c = self.model.summary(id)?;
                (c.parent.is_none() && matches!(c.kind.as_str(), "audio" | "video") && c.file_sample_rate > 0).then(|| (id, i.action_elapsed, i.source_frame as f64 / f64::from(c.file_sample_rate)))
            })
            .min_by(|a, b| a.1.total_cmp(&b.1));
        if let Some((id, _, secs)) = pick {
            self.pending_resume = Some((id, secs));
            self.resume_since = None;
        }
    }

    /// Retomar tras una parada: primero se anuncia (playhead en la cue y aviso), y cuando la cue terminó de apagarse se le
    /// pone el punto de arranque donde se paró (menos el pre-roll). Si antes se la vuelve a disparar, o pasan 10 s, se olvida.
    fn drive_resume(&mut self, ctx: &egui::Context) {
        const GIVE_UP: Duration = Duration::from_secs(10);
        let Some((id, stopped_at)) = self.pending_resume else { return };
        let start = views::memories::preroll_start(stopped_at, self.memories_panel.preroll);
        let Some(since) = self.resume_since else {
            self.resume_since = Some(Instant::now());
            self.inspector.tab = crate::views::inspector::Tab::TimeLoops;
            self.select(id);
            self.move_playhead_locally(id);
            let go = self.prefs.keys.primary(crate::prefs::Command::Go);
            let pre = if start < stopped_at { format!(" (pre-roll -{:.0} s)", stopped_at - start) } else { String::new() };
            self.hud(format!("Retoma en {}{pre}: {go}", crate::format::fmt_time(start)));
            return;
        };
        let sounding = self.telemetry.running.get(&id).is_some_and(|i| i.phase.is_some()) || matches!(self.model.state_of(id), CueUiState::Running | CueUiState::PreWaiting | CueUiState::Paused);
        if since.elapsed() > GIVE_UP {
            self.pending_resume = None;
            self.resume_since = None;
        } else if !sounding {
            let prev = self.resume_points.get(&id).map_or_else(|| self.model.summary(id).and_then(|c| c.start_override_secs), |r| r.prev);
            self.resume_points.insert(id, ResumePoint { prev, start, fired: false });
            self.send_start_point(id, Some(start));
            self.pending_resume = None;
            self.resume_since = None;
        } else {
            ctx.request_repaint_after(Duration::from_millis(50));
        }
    }

    fn send_start_point(&mut self, id: u64, start: Option<f64>) {
        let tag = self.new_tag(Pending::StartPoint);
        self.link.request(tag, ClientRequest::SetStartPoint { cue_id: id, start_secs: start });
    }

    fn sounding(&self, id: u64) -> bool {
        self.telemetry.running.get(&id).is_some_and(|i| i.phase.is_some()) || matches!(self.model.state_of(id), CueUiState::Running | CueUiState::PreWaiting | CueUiState::Paused)
    }

    /// Un punto de «retomar» sirve una vez: cuando la música que arrancó desde ahí termina (o se para sin dejar otro punto),
    /// la cue vuelve a su arranque de antes. Así no queda nada de ensayo guardado para la función.
    fn tick_resume_points(&mut self) {
        let ids: Vec<u64> = self.resume_points.keys().copied().collect();
        for id in ids {
            let sounding = self.sounding(id);
            let waiting_new = self.pending_resume.is_some_and(|(p, _)| p == id);
            let Some(rp) = self.resume_points.get_mut(&id) else { continue };
            if sounding {
                rp.fired = true;
            } else if rp.fired && !waiting_new {
                let prev = rp.prev;
                self.resume_points.remove(&id);
                self.send_start_point(id, prev);
            }
        }
    }

    /// Devuelve a su arranque original todas las cues con un punto de «retomar» (las que suenan, al terminar).
    fn restore_resume_points(&mut self) {
        let ids: Vec<u64> = self.resume_points.keys().copied().collect();
        for id in ids {
            if self.sounding(id) {
                if let Some(rp) = self.resume_points.get_mut(&id) {
                    rp.fired = true;
                }
            } else if let Some(rp) = self.resume_points.remove(&id) {
                self.send_start_point(id, rp.prev);
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn save_show_as_for_test(&mut self, path: &std::path::Path) {
        self.save_to(path.to_path_buf());
    }

    #[cfg(test)]
    pub(crate) fn resume_points_for_test(&self) -> usize {
        self.resume_points.len()
    }

    #[cfg(test)]
    pub(crate) fn pending_resume_for_test(&self) -> Option<(u64, f64)> {
        self.pending_resume
    }

    /// Guarda las preferencias y aplica lo que cambió (silencio de Windows).
    fn prefs_changed(&mut self) {
        if !self.prefs.resume_after_stop {
            self.pending_resume = None;
            self.restore_resume_points();
        }
        if let Some(k) = &self.system_sounds {
            k.set(self.prefs.mute_system_sounds);
        }
        if let Some(p) = &self.prefs_path {
            if let Err(e) = crate::prefs::save(p, &self.prefs) {
                self.toast(ToastKind::Warn, format!("No se pudieron guardar las preferencias: {e}"));
            }
        }
    }

    /// P: si hay algo en pausa lo reanuda desde el mismo frame; si no, pausa lo que suena. La pausa baja el nivel con una rampa de
    /// 5 ms (sin clic) y deja el cabezal quieto en ese frame: sigue visible en la onda y M puede marcar una memoria justo ahí.
    pub fn toggle_pause(&mut self) {
        if !self.is_connected() {
            return;
        }
        let states: Vec<CueUiState> = self.model.summaries.iter().map(|c| self.model.state_of(c.id)).collect();
        match pause_decision(&states) {
            Some(PauseToggle::Resume) => {
                self.link.fire(ClientRequest::ResumeAll);
                self.hud("Reanudado");
            }
            Some(PauseToggle::Pause) => {
                self.link.fire(ClientRequest::PauseAll);
                let key = self.prefs.keys.primary(crate::prefs::Command::Pause);
                self.hud(format!("En pausa ({key} para reanudar)"));
            }
            None => {}
        }
    }

    /// PFL: escucha `cue_id` (desde `secs`, o desde su punto de arranque) solo en los auriculares; si ya suena lo mismo, la detiene.
    pub fn toggle_pfl(&mut self, cue_id: u64, secs: Option<f64>) {
        if !self.is_connected() {
            return;
        }
        if self.pfl == Some((cue_id, secs)) {
            self.stop_pfl();
            return;
        }
        self.pfl = Some((cue_id, secs));
        self.pfl_until = None;
        let tag = self.new_tag(Pending::Pfl);
        self.link.request(tag, ClientRequest::Audition { cue_id, start_secs: secs });
    }

    pub fn stop_pfl(&mut self) {
        if self.pfl.take().is_some() {
            self.pfl_until = None;
            self.link.fire(ClientRequest::AuditionStop);
        }
    }

    /// Carpeta del autoguardado de un show sin nombre: la de ajustes, o la temporal si no hay.
    fn autosave_dir(&self) -> std::path::PathBuf {
        self.cfg.settings_path.as_deref().and_then(std::path::Path::parent).map(std::path::Path::to_path_buf).unwrap_or_else(std::env::temp_dir)
    }

    /// Al arrancar: ¿quedó un autoguardado más nuevo que el último guardado manual?
    fn look_for_recovery(&mut self) {
        if self.cfg.autosave_secs.is_some() {
            self.recovery = crate::autosave::find_recoverable(&self.recent, &self.autosave_dir());
        }
    }

    /// Cada `autosave_secs`: si hay cambios sin guardar pide al daemon que escriba `~<nombre>.autosave`.
    fn autosave_tick(&mut self, now: Instant) {
        let Some(secs) = self.cfg.autosave_secs else { return };
        if now < self.autosave_due {
            return;
        }
        self.autosave_due = now + Duration::from_secs(secs.max(1));
        if !self.show_dirty || self.autosave_inflight || !self.is_connected() || self.recovery.is_some() {
            return;
        }
        let path = crate::autosave::path_for(self.show_path.as_deref(), &self.autosave_dir());
        let name = self.show_path.as_deref().map_or_else(|| self.model.show_name.clone(), show_name_of);
        self.autosave_inflight = true;
        let tag = self.new_tag(Pending::Autosave);
        self.link.request(tag, ClientRequest::SaveShow { path: path.to_string_lossy().into_owned(), name });
    }

    /// Cierre limpio: el autoguardado ya no hace falta.
    fn drop_autosaves(&self) {
        if self.cfg.autosave_secs.is_some() {
            crate::autosave::remove(self.show_path.as_deref(), &self.autosave_dir());
            crate::autosave::remove(None, &self.autosave_dir());
        }
    }

    fn show_recovery(&mut self, ctx: &egui::Context) {
        let Some(r) = self.recovery.clone() else { return };
        let mut decision = None;
        egui::Modal::new(egui::Id::NULL.with("recover_autosave")).show(ctx, |ui| {
            ui.set_width(420.0);
            ui.heading("Recuperar sesión");
            ui.add_space(6.0);
            ui.label("Se detectó una sesión previa cerrada abruptamente. ¿Desea recuperar los cambios autoguardados?");
            let what = r.original.as_deref().map_or_else(|| "show sin nombre".to_string(), |p| show_name_of(p));
            ui.label(RichText::new(format!("{what} · autoguardado hace {}", crate::autosave::fmt_ago(r.saved_at))).color(theme::TEXT_DIM));
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button("Recuperar cambios").clicked() {
                    decision = Some(true);
                }
                if ui.button("Descartar").clicked() {
                    decision = Some(false);
                }
            });
        });
        match decision {
            Some(true) => {
                self.recovery = None;
                self.recover_as = Some(r.original.clone());
                self.load_show_path(r.autosave);
            }
            Some(false) => {
                self.recovery = None;
                let _ = std::fs::remove_file(&r.autosave);
            }
            None => {}
        }
    }

    /// Diálogo modal que pide confirmar una operación que descarta cambios sin guardar.
    fn show_confirm(&mut self, ctx: &egui::Context) {
        let Some(which) = self.confirm else { return };
        let (title, text) = match which {
            Confirm::NewShow => ("Show nuevo", "Hay cambios sin guardar. ¿Descartarlos y crear un show nuevo?"),
            Confirm::OpenShow => ("Abrir show", "Hay cambios sin guardar. ¿Descartarlos y abrir otro show?"),
        };
        let mut decision = None;
        let modal = egui::Modal::new(egui::Id::NULL.with("confirm_discard")).show(ctx, |ui| {
            ui.set_width(340.0);
            ui.heading(title);
            ui.add_space(6.0);
            ui.label(text);
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button("Descartar cambios").clicked() {
                    decision = Some(true);
                }
                if ui.button("Cancelar").clicked() {
                    decision = Some(false);
                }
            });
        });
        if modal.should_close() && decision.is_none() {
            decision = Some(false);
        }
        match decision {
            Some(true) => {
                self.confirm = None;
                match which {
                    Confirm::NewShow => self.do_new_show(),
                    Confirm::OpenShow => self.start_open_show(),
                }
            }
            Some(false) => self.confirm = None,
            None => {}
        }
    }

    fn start_embedded(&mut self, with_audio: bool) {
        if self.embedded.as_ref().is_some_and(EmbeddedEngine::is_running) {
            return;
        }
        match EmbeddedEngine::start(standalone::embedded_config(&self.cfg.socket, with_audio, &self.cfg.audio)) {
            Ok(engine) => {
                self.embedded = Some(engine);
                self.engine_message = Some("Motor embebido en marcha; conectando…".into());
                self.link.retry_now();
            }
            Err(e) => self.engine_message = Some(format!("No se pudo iniciar el motor embebido: {e}")),
        }
    }

    fn remember_recent(&mut self, path: std::path::PathBuf) {
        crate::settings::push_recent(&mut self.recent, path);
        if let Some(p) = &self.cfg.settings_path {
            let _ = crate::settings::save_recent(&crate::settings::recent_path(p), &self.recent);
        }
    }

    /// Ctrl+D: crea una cue nueva del mismo tipo justo después de la seleccionada y le copia todos los datos.
    /// Los grupos no se duplican (sus hijos no se copiarían).
    pub fn duplicate_selected(&mut self) {
        let Some(id) = self.model.selected else { return };
        let Some(draft) = self.inspector.draft.as_ref().filter(|d| d.id() == id) else {
            self.toast(ToastKind::Warn, "Espere a que cargue la cue para duplicarla");
            return;
        };
        let kind = match draft.baseline.body {
            CueBody::Audio(_) => "audio",
            CueBody::Video(_) => "video",
            CueBody::Fade(_) => "fade",
            CueBody::Control(_) => "control",
            CueBody::Osc(_) => "osc",
            CueBody::Memo => "memo",
            CueBody::Group(_) => {
                self.toast(ToastKind::Warn, "Los grupos no se pueden duplicar todavía");
                return;
            }
        };
        let parent = self.model.summary(id).and_then(|c| c.parent);
        let tag = self.new_tag(Pending::DupAdd(Box::new(draft.baseline.clone())));
        self.link.request(tag, ClientRequest::AddCue { kind: kind.into(), parent, after: Some(id) });
    }

    /// Ir a una memoria. Si la cue suena, se detiene y se relanza desde ese punto en cuanto se guarde el nuevo punto de
    /// arranque; si no, queda como siguiente en disparar (Espacio) y con el cursor de edición ahí.
    fn activate_memory(&mut self, ctx: &egui::Context, id: u64, marker: f64, shift: bool) {
        // Pre-roll: arranca unos segundos antes del marcador (Shift+clic aplica 2 s aunque esté apagado).
        let pre = match (self.memories_panel.preroll, shift) {
            (0, true) => 2,
            (p, _) => p,
        };
        let secs = views::memories::preroll_start(marker, pre);
        if pre > 0 {
            let name = self.model.summary(id).and_then(|c| c.memories.iter().find(|m| (m.secs - marker).abs() < 1e-6)).map(|m| m.name.clone()).filter(|n| !n.is_empty()).unwrap_or_else(|| format!("{marker:.1} s"));
            self.hud(format!("Pre-roll: -{:.1}s -> Memoria [{name}]", marker - secs));
        }
        if self.telemetry.running.get(&id).is_some_and(|i| i.phase.is_some()) {
            self.link.fire(ClientRequest::CueAction { cue_id: id, action: qlab_protocol::show_format::ControlActionDto::Stop });
            let same = self.inspector.draft.as_ref().is_some_and(|d| d.id() == id && d.baseline.body.audio().is_some_and(|a| a.time_loops.start_override_secs == Some(secs)));
            if same {
                self.restart_due = Some((Instant::now() + Duration::from_millis(150), id));
            } else {
                self.restart_after_save = Some((id, secs));
            }
        }
        ctx.data_mut(|d| d.insert_temp(egui::Id::NULL.with("memory_goto"), (id, secs)));
        self.inspector.tab = crate::views::inspector::Tab::TimeLoops;
        self.select(id);
        if self.model.summary(id).is_some_and(|c| c.parent.is_none()) {
            self.move_playhead_locally(id);
        }
    }

    /// Panel «Memorias» (estilo Ubicaciones de memoria de Pro Tools), acoplado a un lado y sin tapar la tabla de cues.
    /// No toca `self` entero (la tabla de cues sigue prestada); devuelve lo que pidió el operador.
    fn memories_panel_ui(ui: &mut egui::Ui, state: &mut views::memories::PanelState, summaries: &[qlab_protocol::ipc_msg::CueSummary], draft: Option<&Draft>, pfl: Option<(u64, f64)>) -> Option<views::memories::PanelEvent> {
        let live = draft.and_then(|d| d.node.body.audio().map(|a| (d.id(), a.time_loops.memories.clone())));
        let live_ref = live.as_ref().map(|(id, m)| (*id, m.as_slice()));
        let total: usize = summaries.iter().map(|c| live_ref.filter(|(id, _)| *id == c.id).map_or(c.memories.len(), |(_, m)| m.len())).sum();
        let rows = views::memories::rows(summaries, live_ref, &state.filter);
        let start = draft.and_then(|d| d.node.body.audio().and_then(|a| a.time_loops.start_override_secs.map(|s| (d.id(), s))));
        let right = state.right;
        let mut event = None;
        let body = |ui: &mut egui::Ui| {
            event = views::memories::show_panel(ui, state, &rows, total, start, pfl);
        };
        if right {
            egui::Panel::right("memories_panel").resizable(true).default_size(340.0).min_size(240.0).show(ui, body);
        } else {
            egui::Panel::left("memories_panel").resizable(true).default_size(340.0).min_size(240.0).show(ui, body);
        }
        event
    }

    /// M: marca una memoria donde suena la cue (o donde está el cursor de edición) y pide su nombre.
    fn begin_quick_memory(&mut self, ctx: &egui::Context, at: Option<(u64, f64)>, quiet: bool) {
        if self.quick_memory.is_some() {
            return;
        }
        let is_audio = |a: &Self, id: u64| a.model.summary(id).is_some_and(|c| matches!(c.kind.as_str(), "audio" | "video"));
        let (cue_id, secs, frame) = if let Some((id, secs)) = at {
            (id, Some(secs), None)
        } else {
            // La cue que suena (la seleccionada si es una de ellas); captura el frame de lectura de forma atómica.
            let running: Vec<u64> = self.telemetry.running.iter().filter(|(_, i)| i.phase.is_some()).map(|(id, _)| *id).filter(|id| is_audio(self, *id)).collect();
            let pick = self.model.selected.filter(|s| running.contains(s)).or_else(|| running.iter().copied().min());
            if let Some(id) = pick {
                let frame = self.telemetry.running.get(&id).map_or(0, |i| i.source_frame);
                if frame == 0 {
                    if !quiet {
                        self.toast(ToastKind::Warn, "Todavía no hay posición de lectura: pulse M otra vez en un instante");
                    }
                    return;
                }
                (id, None, Some(frame))
            } else {
                let published: Option<(u64, Option<f64>)> = ctx.data(|d| d.get_temp(egui::Id::NULL.with("edit_cursor_pub")));
                match (self.model.selected, published) {
                    (Some(sel), Some((id, Some(secs)))) if id == sel && is_audio(self, sel) => (sel, Some(secs), None),
                    _ => {
                        if !quiet {
                            self.toast(ToastKind::Info, "Para marcar una memoria: deje sonar la cue y pulse M, o seleccione una cue de audio y haga clic sobre la onda");
                        }
                        return;
                    }
                }
            }
        };
        if self.model.selected != Some(cue_id) {
            self.select(cue_id);
        }
        let count = self.model.summary(cue_id).map_or(0, |c| c.memories.len());
        self.quick_memory = Some(views::memories::Quick { cue_id, secs, frame, name: String::new(), default_name: format!("Memoria {}", count + 1), first: true });
    }

    /// Muestra el cuadro de nombre y, al confirmar, añade la memoria al borrador de la cue (se guarda sola).
    fn drive_quick_memory(&mut self, ctx: &egui::Context) {
        let Some(mut q) = self.quick_memory.take() else { return };
        let sr = self.inspector.draft.as_ref().filter(|d| d.id() == q.cue_id).and_then(|d| d.node.body.audio().map(|a| a.time_loops.file_sample_rate.max(1)));
        let at = q.secs.or_else(|| q.frame.zip(sr).map(|(f, sr)| f as f64 / sr as f64));
        let label = at.map_or_else(|| "(cargando…)".to_string(), crate::format::fmt_time);
        match views::memories::show_quick(ctx, &mut q, &label) {
            views::memories::QuickOutcome::Pending => self.quick_memory = Some(q),
            views::memories::QuickOutcome::Cancel => {}
            views::memories::QuickOutcome::Save(name) => {
                let Some(secs) = at else {
                    self.toast(ToastKind::Warn, "La cue todavía se está cargando: pulse M otra vez");
                    return;
                };
                if let Some(d) = self.inspector.draft.as_mut().filter(|d| d.id() == q.cue_id) {
                    if let Some(a) = d.node.body.audio_mut() {
                        a.time_loops.memories.push(qlab_protocol::cue_dto::MemoryMark { secs, name, note: String::new() });
                        views::inspector::tab_time_loops::normalize_memories(&mut a.time_loops, f64::MAX);
                        self.memories_open = true;
                    }
                } else {
                    self.toast(ToastKind::Warn, "La cue todavía se está cargando: pulse M otra vez");
                }
            }
        }
    }

    /// Pedidos que dejan las pestañas (clic en una bandera de la onda, botón «Marcar memoria»).
    fn consume_memory_requests(&mut self, ctx: &egui::Context) {
        let activate: Option<(u64, f64)> = ctx.data(|d| d.get_temp(egui::Id::NULL.with("memory_activate")));
        if let Some((id, secs)) = activate {
            ctx.data_mut(|d| d.remove::<(u64, f64)>(egui::Id::NULL.with("memory_activate")));
            self.activate_memory(ctx, id, secs, false);
        }
        let quick: Option<(u64, f64)> = ctx.data(|d| d.get_temp(egui::Id::NULL.with("quick_memory_request")));
        if let Some(at) = quick {
            ctx.data_mut(|d| d.remove::<(u64, f64)>(egui::Id::NULL.with("quick_memory_request")));
            self.begin_quick_memory(ctx, Some(at), false);
        }
    }

    // -- Deshacer / Rehacer -------------------------------------------------------------------------

    /// Vuelve al estado anterior (`undo`) o al posterior (`redo`) de la última cue editada.
    fn replay_history(&mut self, undo: bool) {
        let step = if undo { self.history.undo() } else { self.history.redo() };
        let Some((id, node)) = step else {
            self.toast(ToastKind::Info, if undo { "No hay nada que deshacer" } else { "No hay nada que rehacer" });
            return;
        };
        if self.model.summary(id).is_none() {
            self.toast(ToastKind::Warn, "La cue de ese cambio ya no existe");
            return;
        }
        if self.model.selected != Some(id) {
            self.select(id);
        }
        if let Some(d) = self.inspector.draft.as_mut().filter(|d| d.id() == id) {
            d.node = node.clone();
        }
        let tag = self.new_tag(Pending::UpdateReplay(id, node.clone()));
        self.link.request(tag, ClientRequest::UpdateCue { node: Box::new(node) });
        self.toast(ToastKind::Info, if undo { "Deshecho" } else { "Rehecho" });
    }

    // -- Configuración de audio ---------------------------------------------------------------------

    /// Guarda los ajustes y, si el motor es el embebido, lo reinicia con ellos (recargando el show abierto).
    fn apply_audio_settings(&mut self, s: crate::settings::AudioSettings) {
        if let Some(path) = &self.cfg.settings_path {
            if let Err(e) = crate::settings::save(path, &s) {
                self.toast(ToastKind::Warn, format!("No se pudieron guardar los ajustes de audio: {e}"));
            }
        }
        self.cfg.audio = s;
        if self.embedded_running() {
            self.reload_show_after_connect = self.show_path.clone();
            if let Some(mut e) = self.embedded.take() {
                e.stop();
            }
            self.start_embedded(true);
            self.toast(ToastKind::Info, format!("Audio: {}", self.cfg.audio.describe()));
        } else {
            self.toast(ToastKind::Info, "Ajustes de audio guardados: se usarán al iniciar el motor (o al relanzar el daemon).");
        }
    }

    // -- Reactivación de emergencia -------------------------------------------------------------------

    /// Calcula (una vez) el código de solicitud que se muestra en el bloque de reactivación.
    fn ensure_challenge(&mut self) {
        if self.react.challenge.is_none() {
            self.react.challenge = Some(match &self.cfg.license_setup {
                Some(setup) => setup.challenge().map(|c| c.to_string()).map_err(|e| e.to_string()),
                None => Err("esta compilación no exige licencia".into()),
            });
        }
    }

    /// Relee la licencia (como al arrancar) y, si quedó vigente, enciende lo que el bloqueo había impedido.
    fn refresh_license(&mut self) {
        let Some(setup) = self.cfg.license_setup.clone() else { return };
        let was_locked = self.license.is_locked();
        self.license = crate::license_gate::LicenseInfo::from_status(&setup.status());
        self.react.invalidate();
        if was_locked && !self.license.is_locked() {
            if let Some(with_audio) = self.cfg.embedded_on_start {
                self.start_embedded(with_audio);
            }
        }
    }

    fn on_license_extended(&mut self, valid_until: u64) {
        let msg = format!("Licencia extendida hasta: {}", crate::license_gate::fmt_dmy(valid_until));
        self.refresh_license();
        self.react.input.clear();
        self.react.outcome = Some(views::reactivation::Outcome::Ok(msg.clone()));
        self.toast(ToastKind::Info, msg);
    }

    /// El operador pegó una clave y pulsó «Activar». Con el daemon en marcha se la mandamos a él (recarga en
    /// caliente); sin él (pantalla de bloqueo) se canjea aquí mismo.
    fn apply_emergency_key(&mut self, key: String) {
        if self.is_connected() {
            let tag = self.new_tag(Pending::Reactivate);
            self.link.request(tag, ClientRequest::ApplyEmergencyKey { key });
            return;
        }
        let Some(setup) = self.cfg.license_setup.clone() else {
            self.react.outcome = Some(views::reactivation::Outcome::Err("Esta compilación no exige licencia: no hay nada que reactivar.".into()));
            return;
        };
        match setup.apply(&key) {
            Ok(applied) => self.on_license_extended(applied.valid_until),
            Err(e) => {
                self.react.outcome = Some(views::reactivation::Outcome::Err(format!("No se pudo activar: {e}")));
                self.react.invalidate();
            }
        }
    }

    // -- Fotograma ----------------------------------------------------------------------------------

    /// Todo lo previo al dibujo: eventos, telemetría, atajos, guardado del borrador y reloj de repintado.
    pub fn logic_step(&mut self, ctx: &egui::Context) {
        self.frames += 1;
        if self.license.is_locked() {
            return; // sin licencia no se conecta, no se atienden atajos ni se importa nada
        }
        let now = Instant::now();
        if let Some((due, id)) = self.restart_due {
            if now >= due {
                self.restart_due = None;
                self.link.fire(ClientRequest::SetPlayhead { cue_id: Some(id) });
                self.link.fire(ClientRequest::Go);
            }
        }
        let dt = now.duration_since(self.last_frame).as_secs_f32().min(0.1);
        self.last_frame = now;

        self.handle_link_events();
        if let Some((id, from, to)) = self.pending_memory_time {
            if self.inspector.draft.as_ref().is_some_and(|d| d.id() == id) {
                self.pending_memory_time = None;
                self.apply_memory_time(from, to);
            }
        }
        self.autosave_tick(now);
        if self.pfl_until.is_some_and(|t| now >= t) {
            self.pfl = None;
            self.pfl_until = None;
        }
        self.waveforms.poll();
        self.poll_repairs();
        for (path, from) in self.waveforms.take_damaged() {
            let name = path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned());
            self.toast(ToastKind::Warn, format!("Audio dañado en «{name}»: desde {} no se puede leer y suena en silencio (la onda lo marca en rojo)", crate::format::fmt_time(from)));
        }
        if let Some(outcome) = self.dialogs.poll() {
            self.on_dialog_outcome(outcome);
        }
        let title = self.window_title();
        if title != self.last_title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.last_title = title;
        }

        let channels = self.hello().map_or(2, |h| h.channels as usize);
        let mut active = false;
        if let Some(shm) = &self.shm {
            active = self.telemetry.update(shm.telemetry(), dt, channels);
            // El playhead de la memoria compartida llega antes que el evento del socket.
            if self.telemetry.status.is_some() && !self.local_playhead_grace() {
                self.set_playhead(self.telemetry.playhead());
            }
        }

        // El evento de «empezó a sonar» llega por el socket unos milisegundos antes que la telemetría.
        active |= self.model.any_running();

        self.video_tick(ctx);
        self.handle_drops(ctx);
        self.handle_keys(ctx);
        let ready = self.audio_tools.tick();
        if !ready.is_empty() {
            if self.is_connected() && !self.locked {
                self.begin_import(ready);
            } else {
                self.toast(ToastKind::Warn, "Los archivos están listos pero no se agregaron al show (sin conexión o con el Bloqueo de función)");
            }
        }
        self.drive_resume(ctx);
        self.tick_resume_points();
        self.maybe_commit(ctx);
        if std::mem::take(&mut self.refresh_cues) && self.is_connected() {
            self.request_cues();
        }

        self.toasts.retain(|t| t.born.elapsed() < TOAST_LIFETIME);
        self.schedule_repaints(ctx, active);
    }

    fn schedule_repaints(&mut self, ctx: &egui::Context, active: bool) {
        if active {
            ctx.request_repaint_after(ACTIVE_FRAME);
        }
        if let Some(oldest) = self.toasts.iter().map(|t| t.born).min() {
            ctx.request_repaint_after(TOAST_LIFETIME.saturating_sub(oldest.elapsed()) + Duration::from_millis(20));
        }
        if !self.is_connected() {
            ctx.request_repaint_after(Duration::from_secs(1));
        }
        if let Some(limit) = self.cfg.exit_after {
            let left = limit.saturating_sub(self.started.elapsed());
            if left.is_zero() {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            } else {
                ctx.request_repaint_after(left);
            }
        }
    }

    fn handle_keys(&mut self, ctx: &egui::Context) {
        // Con Preferencias abierta las teclas son de la ventana (se está eligiendo un atajo).
        if self.prefs_ui.open {
            return;
        }
        let keys = ctx.input(|i| scan_keys_with(i, &self.prefs.keys));
        if keys.help && self.picker.is_none() && self.confirm.is_none() {
            self.help.toggle_manual();
            return;
        }
        if keys.lock {
            self.toggle_lock();
        }
        // Con un cuadro modal abierto (nombre de memoria, recuperación de sesión, configuración de audio) las teclas son
        // suyas: Esc lo cierra y NO debe disparar un Panic, ni Espacio un GO, mientras se está en medio de otra cosa.
        if self.picker.is_some() || self.confirm.is_some() || self.help.any_open() || self.quick_memory.is_some() || self.recovery.is_some() || self.audio_ui.open {
            return;
        }
        // «. N .»: salto a una memoria por número. Mientras se teclea consume dígitos, Enter y el punto.
        if ctx.egui_wants_keyboard_input() {
            self.num_nav = None;
        } else if self.handle_num_nav(ctx) {
            return;
        }
        // Los pads responden aunque la ventana de Carts tenga el foco, pero no mientras se escribe en un campo.
        self.handle_pad_keys(ctx, ctx.egui_wants_keyboard_input());
        if ctx.egui_wants_keyboard_input() {
            return;
        }
        if keys.carts {
            self.carts.open = !self.carts.open;
        }
        if keys.go {
            self.go();
        }
        if keys.pause {
            self.toggle_pause();
        }
        if keys.panic {
            self.panic_press();
        }
        if keys.down {
            if let Some(id) = self.model.select_relative(1) {
                self.after_key_selection(id);
            }
        }
        if keys.up {
            if let Some(id) = self.model.select_relative(-1) {
                self.after_key_selection(id);
            }
        }
        if keys.open {
            self.apply(Action::OpenShowPicker, ctx);
        }
        if keys.quit {
            self.apply(Action::Quit, ctx);
        }
        if keys.new_show && !self.lock_refuses() {
            self.begin_new_show();
        }
        if keys.save {
            self.save_show();
        }
        if keys.save_as {
            self.save_show_as();
        }
        if keys.delete && !self.lock_refuses() {
            self.delete_selected();
        }
        if keys.duplicate && !self.lock_refuses() {
            self.duplicate_selected();
        }
        if keys.memory && !self.lock_refuses() {
            self.begin_quick_memory(ctx, None, false);
        }
        if keys.memory_enter && !self.locked {
            // Enter (también el del teclado numérico): igual que M, pero sin avisos si no hay nada que marcar.
            self.begin_quick_memory(ctx, None, true);
        }
        if keys.undo && !self.lock_refuses() {
            self.replay_history(true);
        }
        if keys.redo && !self.lock_refuses() {
            self.replay_history(false);
        }
    }

    fn after_key_selection(&mut self, id: u64) {
        let is_top = self.model.summary(id).is_some_and(|c| c.parent.is_none());
        self.model.selected = None; // fuerza la carga del detalle en `select`
        self.select(id);
        if is_top {
            self.move_playhead_locally(id);
        }
    }

    /// Pide mover el playhead y lo refleja ya en la interfaz.
    fn move_playhead_locally(&mut self, id: u64) {
        self.link.fire(ClientRequest::SetPlayhead { cue_id: Some(id) });
        self.model.playhead = Some(id);
        self.local_playhead_until = Some(Instant::now() + Duration::from_millis(250));
    }

    /// Envía el borrador al daemon cuando hay cambios y el usuario ya no está tecleando ni arrastrando.
    fn maybe_commit(&mut self, ctx: &egui::Context) {
        if !self.is_connected() || ctx.egui_wants_keyboard_input() {
            return;
        }
        // Mientras se arrastra se espera a soltar, salvo en el EQ: ahí se envía cada ~60 ms para oír el cambio en vivo
        // (como en Pro Tools); el motor suaviza la transición y no hay saltos.
        if ctx.input(|i| i.pointer.any_down()) {
            let live_eq = self.inspector.tab == crate::views::inspector::Tab::Eq && self.last_live_commit.is_none_or(|t| t.elapsed() >= Duration::from_millis(60));
            if !live_eq {
                return;
            }
            self.last_live_commit = Some(Instant::now());
        }
        let Some(d) = &self.inspector.draft else { return };
        if !d.dirty() || self.inspector.in_flight.as_ref() == Some(&d.node) {
            return;
        }
        let (id, node) = (d.id(), d.node.clone());
        self.inspector.in_flight = Some(node.clone());
        let tag = self.new_tag(Pending::Update(id, node.clone()));
        self.link.request(tag, ClientRequest::UpdateCue { node: Box::new(node) });
    }

    #[cfg(test)]
    pub(crate) fn on_exit_for_test(&mut self) {
        eframe::App::on_exit(self);
    }

    #[cfg(test)]
    pub(crate) fn load_show_for_test(&mut self, path: &std::path::Path) {
        let tag = self.new_tag(Pending::LoadShow(Some(path.to_path_buf())));
        self.link.request(tag, ClientRequest::LoadShow { path: path.to_string_lossy().into_owned() });
    }

    // -- Dibujo -------------------------------------------------------------------------------------

    pub fn show(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        if self.license.is_locked() {
            self.ensure_challenge();
            if let Some(key) = views::license_lock::show(ui, &self.license, self.cfg.license_setup.is_some(), &mut self.react) {
                self.apply_emergency_key(key);
            }
            return;
        }
        let mut actions: Vec<Action> = Vec::new();

        if !self.is_connected() {
            self.show_reconnect(ui, &mut actions);
        } else {
            self.show_main(ui, &mut actions, &ctx);
        }
        self.consume_memory_requests(&ctx);
        self.drive_quick_memory(&ctx);
        self.show_picker(&ctx);
        self.show_confirm(&ctx);
        self.show_recovery(&ctx);
        self.carts_ui(&ctx);
        let note = if self.embedded_running() { "Motor embebido: los cambios se aplican al instante." } else { "El motor es un proceso aparte (o está apagado): los ajustes se guardan y valen al relanzarlo desde esta aplicación." };
        if let Some(views::audio_settings::Outcome::Apply(s)) = views::audio_settings::show(&ctx, &mut self.audio_ui, note) {
            self.apply_audio_settings(s);
        }
        if self.help.open && self.cfg.license_setup.is_some() {
            self.ensure_challenge();
        }
        // Novedades de una versión nueva: después de la recuperación de cambios, si la hubo, y nunca encima de otro cuadro.
        if self.news_waiting && self.recovery.is_none() && !self.help.any_open() && self.confirm.is_none() && self.picker.is_none() {
            self.news_waiting = false;
            self.help.open_news();
            if let Some(p) = &self.news_seen {
                // Se anota al mostrarlas: si el programa se cerrara con el cuadro abierto, no vuelven a salir en plena función.
                if let Err(e) = crate::novedades::mark_seen(p) {
                    eprintln!("[TeatroCue] no se pudo anotar que las novedades se vieron: {e}");
                }
            }
        }
        let req = views::audio_tools::show(&ctx, &mut self.audio_tools);
        if req.pick_files || req.pick_folder {
            let kind = if req.pick_files { DialogKind::PickConvertFiles } else { DialogKind::PickToolFolder };
            if !self.dialogs.enabled() {
                self.toast(ToastKind::Info, "Sin diálogo del sistema: arrastre los archivos a esta ventana de TeatroCue");
            } else if !self.dialogs.open(kind, self.audio_tools.convert_dest().parent().map(std::path::Path::to_path_buf), None) {
                self.toast(ToastKind::Info, "Ya hay un diálogo abierto");
            }
        }
        let sys_status = self.system_sounds.as_ref().map(crate::system_sounds::Keeper::status);
        if views::preferences::show(&ctx, &mut self.prefs_ui, &mut self.prefs, sys_status.as_ref()) {
            self.prefs_changed();
        }
        if let Some(key) = views::help_modal::show(&ctx, &mut self.help, &self.license, self.cfg.license_setup.is_some(), &mut self.react, &self.prefs.keys) {
            self.apply_emergency_key(key);
        }
        self.show_projector(&ctx);
        self.show_drop_overlay(&ctx);
        self.show_toasts(&ctx);
        self.show_hud(&ctx);
        if self.locked {
            // Borde ámbar sutil: de un vistazo se ve que la función está bloqueada.
            let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::NULL.with("lock_border")));
            painter.rect_stroke(ctx.content_rect().shrink(1.5), 0.0, egui::Stroke::new(3.0, theme::AMBER_DIM), egui::StrokeKind::Inside);
        }
        for a in actions {
            self.apply(a, &ctx);
        }
    }

    fn show_reconnect(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        let (reason, connecting) = match &self.conn {
            ConnState::Down { reason } => (reason.clone(), false),
            _ => (String::new(), true),
        };
        let down = self.down_since.map_or(0.0, |t| t.elapsed().as_secs_f32());
        // El primer segundo se muestra "conectando" para no parpadear la pantalla de error al arrancar.
        let connecting = connecting || (down < 1.0 && self.started.elapsed() < Duration::from_secs(2));
        egui::CentralPanel::default().show(ui, |ui| {
            views::reconnect::show(
                ui,
                &views::reconnect::ReconnectInfo {
                    socket: &self.cfg.socket,
                    reason: &reason,
                    connecting,
                    seconds_down: down,
                    engine_message: self.engine_message.as_deref(),
                    embedded_running: self.embedded.as_ref().is_some_and(EmbeddedEngine::is_running),
                    daemon_binary_found: standalone::find_daemon_binary().is_some(),
                },
                actions,
            );
        });
    }

    fn show_main(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>, _ctx: &egui::Context) {
        let hello = match &self.conn {
            ConnState::Connected(h) => Some(h.clone()),
            _ => None,
        };
        let view = ViewCtx { model: &self.model, telemetry: &self.telemetry, hello: hello.as_ref(), safe_mute: self.safe_mute, master_db: self.master_db, scroll_to_selected: std::mem::take(&mut self.scroll_to_selected) };

        egui::Panel::top("menu").show(ui, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                ui.menu_button("Archivo", |ui| {
                    for (label, action) in [
                        ("Nuevo show   Ctrl+N", Action::NewShow),
                        ("Abrir show…   Ctrl+O", Action::OpenShowPicker),
                        ("Guardar   Ctrl+S", Action::SaveShow),
                        ("Guardar como…   Ctrl+Mayús+S", Action::SaveShowAs),
                        ("Importar audio: convertir / copiar de CD…", Action::OpenAudioTools),
                        ("Empaquetar show…", Action::BundleShow),
                        ("Exportar Guion Técnico…", Action::ExportCueSheet),
                    ] {
                        if ui.button(label).clicked() {
                            actions.push(action);
                            ui.close();
                        }
                    }
                    ui.menu_button("Abrir reciente", |ui| {
                        let existing: Vec<_> = self.recent.iter().filter(|p| p.is_file()).cloned().collect();
                        if existing.is_empty() {
                            ui.label(RichText::new("(ninguno)").color(theme::TEXT_DIM));
                        }
                        for p in existing {
                            let name = p.file_name().map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into_owned());
                            if ui.button(name).on_hover_text(p.display().to_string()).clicked() {
                                actions.push(Action::OpenRecent(p));
                                ui.close();
                            }
                        }
                    });
                    ui.separator();
                    if ui.button("Salir   Ctrl+Q").clicked() {
                        actions.push(Action::Quit);
                        ui.close();
                    }
                });
                ui.menu_button("Edición", |ui| {
                    if ui.button("Duplicar cue   Ctrl+D").clicked() {
                        actions.push(Action::Duplicate);
                        ui.close();
                    }
                    ui.separator();
                    if ui.add_enabled(self.history.can_undo(), egui::Button::new("Deshacer   Ctrl+Z")).clicked() {
                        actions.push(Action::Undo);
                        ui.close();
                    }
                    if ui.add_enabled(self.history.can_redo(), egui::Button::new("Rehacer   Ctrl+Mayús+Z")).clicked() {
                        actions.push(Action::Redo);
                        ui.close();
                    }
                });
                ui.menu_button("Ver", |ui| {
                    let carts_key = self.prefs.keys.primary(crate::prefs::Command::Carts);
                    if ui.button(if self.carts.open { format!("Ocultar Carts ({carts_key})") } else { format!("Carts ({carts_key})") }).clicked() {
                        self.carts.open = !self.carts.open;
                        ui.close();
                    }
                    if ui.button(if self.memories_open { "Ocultar panel de memorias" } else { "Memorias…" }).clicked() {
                        self.memories_open = !self.memories_open;
                        ui.close();
                    }
                    if ui.button("Configuración de audio…").clicked() {
                        self.audio_ui.open_with(&self.cfg.audio);
                        ui.close();
                    }
                    if ui.button(if self.projection.open { "Cerrar pantalla de proyección" } else { "Abrir pantalla de proyección" }).clicked() {
                        actions.push(Action::OpenProjector);
                        ui.close();
                    }
                });
                ui.menu_button("Opciones", |ui| {
                    let mut changed = false;
                    if ui.checkbox(&mut self.prefs.resume_after_stop, "Retomar desde donde se paró (ensayo)").on_hover_text("Al parar con PANIC o «Parar todo», el próximo GO retoma la música desde ese punto (con el pre-roll). No actúa con el Bloqueo de función").changed() {
                        changed = true;
                    }
                    if crate::system_sounds::SUPPORTED && ui.checkbox(&mut self.prefs.mute_system_sounds, "Silenciar los sonidos de Windows").on_hover_text("Notificaciones y avisos de Windows no salen por los parlantes mientras TeatroCue está abierto").changed() {
                        changed = true;
                    }
                    ui.separator();
                    if ui.button("Preferencias y atajos de teclado…").clicked() {
                        self.prefs_ui.open = true;
                        self.prefs_ui.message = None;
                        ui.close();
                    }
                    if changed {
                        actions.push(Action::PrefsChanged);
                    }
                });
                ui.menu_button("Ayuda", |ui| {
                    if ui.button(format!("Manual de Uso ({})", self.prefs.keys.primary(crate::prefs::Command::Help))).clicked() {
                        self.help.toggle_manual();
                        ui.close();
                    }
                    if ui.button("Novedades de esta versión").clicked() {
                        self.help.open_news();
                        ui.close();
                    }
                    if ui.button("Acerca de TeatroCue").clicked() {
                        self.help.open_about();
                        ui.close();
                    }
                });
                let name = if self.model.show_name.is_empty() { "Sin título" } else { self.model.show_name.as_str() };
                ui.label(RichText::new(format!("{name}{}", if self.show_dirty { " *" } else { "" })).color(theme::TEXT_DIM));
            });
        });
        egui::Panel::top("toolbar").show(ui, |ui| {
            // Relleno justo y separación corta: en una pantalla chica (1366x768 de cabina) la barra baja de línea de forma
            // prolija en lugar de cortar los botones de Proyección o Papelera.
            ui.spacing_mut().button_padding = egui::vec2(6.0, 3.0);
            ui.spacing_mut().item_spacing = egui::vec2(4.0, 4.0);
            ui.horizontal_wrapped(|ui| {
                ui.add_space(2.0);
                for (label, kind, tip) in [("+ Audio Cue", "audio", "Cue de audio nueva después de la seleccionada"), ("+ Fade Cue", "fade", "Cue de fade nueva"), ("+ Memo/Espera", "memo", "Nota o espera: no suena; dura lo que indiques (usa Auto-Continue para encadenar)"), ("+ OSC Cue", "osc", "Cue de red: envía un mensaje OSC a otro equipo (luces, otro TeatroCue…)"), ("+ Control Cue", "control", "Cue de control: detiene, pausa, reanuda o hace Devamp de otra cue"), ("+ Video Cue", "video", "Cue de vídeo nueva: su audio suena por el motor y la imagen va a la pantalla de proyección"), ("+ Group Cue", "group", "Grupo nuevo")] {
                    if ui.add_enabled(!self.locked, egui::Button::new(label)).on_hover_text(tip).clicked() {
                        actions.push(Action::AddCue(kind));
                    }
                }
                // Proyección y las acciones de la cue seleccionada forman un solo grupo: bajan de línea juntos.
                ui.horizontal(|ui| {
                    ui.separator();
                    // Interruptor con estado claro: verde y «Proyección activa» mientras la ventana está abierta; otro clic la cierra.
                    let on = self.projection.open;
                    let label = if on { RichText::new("Proyección activa").strong().color(Color32::BLACK) } else { RichText::new("Proyección") };
                    let btn = egui::Button::new(label).fill(if on { theme::GREEN } else { ui.visuals().widgets.inactive.weak_bg_fill });
                    if ui.add(btn).on_hover_text(if on { "Cierra la ventana de proyección" } else { "Abre la ventana secundaria para el proyector" }).clicked() {
                        actions.push(Action::OpenProjector);
                    }
                    ui.separator();
                    let locked_now = self.locked;
                    let lock_label = RichText::new(if self.locked { "Función BLOQUEADA" } else { "Bloqueo de función" }).strong().color(if self.locked { theme::AMBER } else { theme::TEXT });
                    if ui.selectable_label(locked_now, lock_label).on_hover_text("Ctrl+L. Bloqueado: no se puede soltar archivos, editar, borrar ni mover cues. GO, Panic, MASTER, saltos de memoria y Proyección siguen funcionando").clicked() {
                        actions.push(Action::ToggleLock);
                    }
                    let resume = self.prefs.resume_after_stop;
                    let resume_label = RichText::new("Retomar al parar").strong().color(if resume { theme::AMBER } else { theme::TEXT });
                    if ui.selectable_label(resume, resume_label).on_hover_text("Ensayo: al parar con PANIC o «Parar todo», el próximo GO retoma la música desde ese punto (con el pre-roll). Con el Bloqueo de función no actúa").clicked() {
                        self.prefs.resume_after_stop = !resume;
                        actions.push(Action::PrefsChanged);
                    }
                    if crate::system_sounds::SUPPORTED {
                        let quiet = self.prefs.mute_system_sounds;
                        let label = RichText::new(if quiet { "Windows en silencio" } else { "Silenciar Windows" }).strong().color(if quiet { theme::GREEN } else { theme::TEXT });
                        if ui.selectable_label(quiet, label).on_hover_text("Silencia las notificaciones y avisos de Windows (no el audio del show). Al cerrar TeatroCue vuelven a sonar").clicked() {
                            self.prefs.mute_system_sounds = !quiet;
                            actions.push(Action::PrefsChanged);
                        }
                    }
                    ui.separator();
                    let sel = self.model.selected;
                    let sibling_pos = sel.and_then(|id| self.model.sibling_position(id));
                    if widgets::icon_button(ui, widgets::Icon::Up, "Subir la cue seleccionada", sibling_pos.is_some_and(|(i, _)| i > 0) && !self.locked).clicked() {
                        actions.push(Action::MoveSelected(-1));
                    }
                    if widgets::icon_button(ui, widgets::Icon::Down, "Bajar la cue seleccionada", sibling_pos.is_some_and(|(i, n)| i + 1 < n) && !self.locked).clicked() {
                        actions.push(Action::MoveSelected(1));
                    }
                    if widgets::icon_button(ui, widgets::Icon::Trash, "Eliminar la cue seleccionada (Supr)", sel.is_some() && !self.locked).clicked() {
                        actions.push(Action::DeleteSelected);
                    }
                });
            });
        });
        egui::Panel::top("transport").show(ui, |ui| {
            ui.add_space(4.0);
            views::top_bar::show(ui, &view, actions);
            ui.add_space(4.0);
        });
        egui::Panel::bottom("status").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("{} cues", self.model.summaries.len())).small().color(theme::TEXT_DIM));
                ui.separator();
                let k = &self.prefs.keys;
                use crate::prefs::Command as C;
                ui.label(RichText::new(format!("{}: GO · {}: Panic (dos veces = parada dura) · {}: pausa · {}: memoria · {}: borrar · {}/{}/{}: nuevo/abrir/guardar", k.primary(C::Go), k.primary(C::Panic), k.primary(C::Pause), k.primary(C::Memory), k.primary(C::Delete), k.primary(C::New), k.primary(C::Open), k.primary(C::Save))).small().color(theme::TEXT_DIM));
            });
        });
        let memory_event = if self.memories_open { Self::memories_panel_ui(ui, &mut self.memories_panel, &self.model.summaries, self.inspector.draft.as_ref(), self.pfl.and_then(|(id, s)| s.map(|s| (id, s)))) } else { None };
        self.inspector.locked = self.locked;
        self.inspector.pfl_active = self.inspector.draft.as_ref().is_some_and(|d| self.pfl.is_some_and(|(id, s)| id == d.id() && s.is_none()));
        let mut out = InspectorOutput::default();
        // Id estático (nunca depende de la cue). Un panel redimensionable se ajusta al contenido si este mide
        // menos que su tamaño guardado (p. ej. mientras el Inspector recarga el borrador tras un GO), y se
        // quedaba en `min_size`. Llenar el alto disponible fija la altura que haya arrastrado el usuario.
        let panel = egui::Panel::bottom("global_inspector_panel").resizable(true).default_size(INSPECTOR_DEFAULT_HEIGHT).min_size(INSPECTOR_MIN_HEIGHT).show(ui, |ui| {
            ui.set_min_height(ui.available_height());
            let repair_state = self.inspector.draft.as_ref().and_then(crate::views::inspector::tab_time_loops::file_path_of).and_then(|p| self.repairs.get(&p).cloned());
            let mut cx = TabCtx { model: &self.model, telemetry: &self.telemetry, waveforms: &mut self.waveforms, device_channels: hello.as_ref().map_or(2, |h| h.channels as usize), preroll: self.memories_panel.preroll, video: &mut self.video, repair: repair_state, memory_key: self.prefs.keys.primary(crate::prefs::Command::Memory) };
            inspector::show(ui, &mut self.inspector, &mut cx, &mut out);
        });
        self.inspector_height = panel.response.rect.height();
        self.inspector_rect = panel.response.rect;
        egui::CentralPanel::default().show(ui, |ui| {
            views::cue_table::show(ui, &view, actions);
        });

        match memory_event {
            Some(views::memories::PanelEvent::Close) => self.memories_open = false,
            Some(views::memories::PanelEvent::Pfl { cue_id, secs }) => self.toggle_pfl(cue_id, Some(secs)),
            Some(views::memories::PanelEvent::Activate { cue_id, secs, shift }) => {
                let ctx = ui.ctx().clone();
                self.activate_memory(&ctx, cue_id, secs, shift);
            }
            Some(views::memories::PanelEvent::Preroll(v)) => self.set_preroll(v),
            Some(views::memories::PanelEvent::SetTime { cue_id, from, to }) => self.set_memory_time(cue_id, from, to),
            None => {}
        }

        for (cue, row, col, db) in out.drags {
            self.link.fire(ClientRequest::FaderDrag { cue_id: cue, row, col, db });
        }
        if out.pick_audio {
            actions.push(Action::PickAudioFile);
        }
        if let Some((original, until)) = out.repair {
            let ctx = ui.ctx().clone();
            self.start_repair(&ctx, original, until);
        }
        if let Some(copy) = out.use_repaired {
            if self.lock_refuses() {
                // con el bloqueo puesto no se cambia el archivo de una cue
            } else {
                let name = copy.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
                self.set_audio_file(copy);
                self.toast(ToastKind::Info, format!("La cue usa ahora la copia reparada «{name}». Guarde el show (Ctrl+S)"));
            }
        }
        if let Some(v) = out.preroll {
            self.set_preroll(v);
        }
        if out.pfl {
            if let Some(id) = self.inspector.draft.as_ref().map(Draft::id) {
                self.toggle_pfl(id, None);
            }
        }
        if out.open_projector {
            actions.push(Action::OpenProjector);
        }
        if let Some(cue_id) = out.devamp {
            self.link.fire(ClientRequest::Devamp { cue_id });
        }
    }

    fn show_picker(&mut self, ctx: &egui::Context) {
        let Some(picker) = self.picker.take() else { return };
        let (mut picker, is_audio) = match picker {
            ActivePicker::Audio(p) => (p, true),
            ActivePicker::Show(p) => (p, false),
        };
        match picker.show(ctx) {
            None => self.picker = Some(if is_audio { ActivePicker::Audio(picker) } else { ActivePicker::Show(picker) }),
            Some(PickerOutcome::Cancelled) => {}
            Some(PickerOutcome::Picked(path)) => {
                if is_audio {
                    self.set_audio_file(path);
                } else {
                    self.load_show_path(path);
                }
            }
        }
    }

    /// Asigna un archivo a la cue en edición. La región vuelve a ser "todo el archivo".
    fn set_audio_file(&mut self, path: std::path::PathBuf) {
        let Some(d) = self.inspector.draft.as_mut() else { return };
        let Some(a) = d.node.body.audio_mut() else { return };
        // El nombre se pone solo (título de las etiquetas o nombre del archivo) si la cue no tiene uno propio: vacío o el que
        // se había puesto solo para el archivo anterior. Un nombre escrito a mano no se pisa.
        let old = d.node.basics.file_target.as_ref().map(|t| std::path::PathBuf::from(&t.absolute_path)).filter(|p| !p.as_os_str().is_empty());
        let current = d.node.basics.name.trim().to_string();
        let auto = current.is_empty()
            || old.as_ref().is_some_and(|o| current == crate::media::cue_name_of(o) || current == crate::media::name_from_file_name(o) || o.file_stem().is_some_and(|s| s.to_string_lossy() == current));
        if auto {
            d.node.basics.name = crate::media::cue_name_of(&path);
        }
        let target = d.node.basics.file_target.get_or_insert_with(Default::default);
        target.absolute_path = path.to_string_lossy().into_owned();
        target.relative_path = None;
        a.time_loops.start_time_sec = 0.0;
        a.time_loops.end_time_sec = 0.0;
        a.time_loops.slices.clear();
        // Con archivo, la cue queda lista para sonar (una cue nueva nace desarmada).
        d.node.basics.armed = true;
        self.waveforms.forget(&path);
    }

    fn show_toasts(&mut self, ctx: &egui::Context) {
        if self.toasts.is_empty() {
            return;
        }
        egui::Area::new(egui::Id::unique("toasts")).anchor(Align2::RIGHT_BOTTOM, [-12.0, -40.0]).interactable(false).show(ctx, |ui| {
            for t in &self.toasts {
                let (fill, fg) = match t.kind {
                    ToastKind::Info => (theme::PANEL_RAISED, theme::TEXT),
                    ToastKind::Warn => (Color32::from_rgb(0x3a, 0x2c, 0x0a), theme::AMBER),
                    ToastKind::Error => (theme::RED_DARK, Color32::WHITE),
                };
                egui::Frame::new().fill(fill).stroke(egui::Stroke::new(1.0, theme::GRID)).inner_margin(egui::Margin::symmetric(10, 6)).corner_radius(4).show(ui, |ui| {
                    ui.set_max_width(420.0);
                    ui.label(RichText::new(&t.text).color(fg));
                });
                ui.add_space(4.0);
            }
        });
    }
}

/// Qué hace la tecla P según el estado de las cues.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PauseToggle {
    Pause,
    Resume,
}

/// Con alguna cue en pausa, P reanuda; si no, con alguna sonando (o esperando), pausa; sin nada activo no hace nada.
pub fn pause_decision(states: &[CueUiState]) -> Option<PauseToggle> {
    if states.contains(&CueUiState::Paused) {
        Some(PauseToggle::Resume)
    } else if states.iter().any(|s| matches!(s, CueUiState::Running | CueUiState::PreWaiting | CueUiState::PostWaiting)) {
        Some(PauseToggle::Pause)
    } else {
        None
    }
}

/// Punto de «retomar desde donde se paró» puesto en una cue.
#[derive(Clone, Copy, Debug)]
struct ResumePoint {
    /// El arranque que tenía la cue antes (memoria elegida o ninguno).
    prev: Option<f64>,
    start: f64,
    /// Ya se disparó desde ahí: al terminar, la cue vuelve a `prev`.
    fired: bool,
}

#[derive(Default, Debug, PartialEq, Eq)]
pub struct Keys {
    pub go: bool,
    pub panic: bool,
    pub up: bool,
    pub down: bool,
    pub open: bool,
    pub quit: bool,
    pub new_show: bool,
    pub save: bool,
    pub save_as: bool,
    pub delete: bool,
    pub help: bool,
    pub undo: bool,
    pub redo: bool,
    pub duplicate: bool,
    pub memory: bool,
    pub memory_enter: bool,
    pub carts: bool,
    pub lock: bool,
    /// P: pausar / reanudar.
    pub pause: bool,
}

/// Atajos de una pasada de entrada con las teclas de fábrica.
pub fn scan_keys(i: &egui::InputState) -> Keys {
    scan_keys_with(i, &crate::prefs::Keymap::default())
}

/// Atajos de una pasada de entrada con las teclas del operador. GO y Panic ignoran la repetición automática de tecla:
/// mantener pulsada la barra espaciadora no debe disparar una cue tras otra.
pub fn scan_keys_with(i: &egui::InputState, map: &crate::prefs::Keymap) -> Keys {
    use crate::prefs::Command as C;
    let mut k = Keys::default();
    for e in &i.events {
        if let egui::Event::Key { key, pressed: true, repeat, modifiers, .. } = e {
            let (key, m, r) = (*key, *modifiers, *repeat);
            let on = |c| map.pressed(c, key, m, r);
            k.go |= on(C::Go);
            k.panic |= on(C::Panic);
            k.down |= on(C::Down);
            k.up |= on(C::Up);
            k.open |= on(C::Open);
            k.quit |= on(C::Quit);
            k.new_show |= on(C::New);
            k.save |= on(C::Save);
            k.save_as |= on(C::SaveAs);
            k.undo |= on(C::Undo);
            k.redo |= on(C::Redo);
            k.duplicate |= on(C::Duplicate);
            k.help |= on(C::Help);
            k.carts |= on(C::Carts);
            k.lock |= on(C::Lock);
            k.pause |= on(C::Pause);
            k.delete |= on(C::Delete);
            // Marcar memoria: la primera tecla avisa si no hay nada que marcar; la segunda (Enter de fábrica) no.
            let [first, second] = map.get(C::Memory);
            if !r {
                let hit = |b: Option<crate::prefs::Binding>| b.is_some_and(|b| b.key == key && b.ctrl == (m.command || m.ctrl) && b.shift == m.shift && b.alt == m.alt);
                k.memory |= hit(first);
                k.memory_enter |= hit(second);
            }
        }
    }
    k
}

impl eframe::App for QlabApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.logic_step(ctx);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.show(ui);
    }

    fn on_exit(&mut self) {
        // Devuelve los sonidos de Windows (espera a que el hilo termine).
        self.system_sounds = None;
        self.drop_autosaves();
        if let Some(mut e) = self.embedded.take() {
            e.stop();
        }
    }
}
