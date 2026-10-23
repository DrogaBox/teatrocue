//! Formato de show nativo `qlab-rs/v1` (JSON). El importador del formato `qlab-show/v1` queda pendiente.
//!
//! Un show es una lista de cues de nivel superior; los grupos llevan a sus hijos anidados. El
//! destino de un `FadeCue` es `basics.cue_target_id`, y el archivo de un audio `basics.file_target`.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::cue_dto::{AudioEqData, CartBank, AudioTimeAndLoopsData, CueBasicsData, CueId, CueTriggersData, DtoError, RATE_MAX, RATE_MIN};
use crate::matrix::AudioLevelMatrixData;

/// Identificador del formato nativo de TeatroCue (lo que se escribe).
pub const SHOW_FORMAT: &str = "teatrocue/v1";
/// Identificador del formato anterior (`qlab-rs/v1`): se sigue leyendo, con el mismo contenido.
pub const SHOW_FORMAT_LEGACY: &str = "qlab-rs/v1";
const MAX_GROUP_DEPTH: usize = 8;

fn default_sample_rate() -> u32 {
    48_000
}

fn yes() -> bool {
    true
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ShowFile {
    pub format: String,
    #[serde(default)]
    pub name: String,
    #[serde(default = "default_sample_rate")]
    pub sample_rate: u32,
    /// Botonera de Carts. Los shows sin pads no llevan este campo.
    #[serde(default, skip_serializing_if = "CartBank::is_empty")]
    pub carts: CartBank,
    pub cues: Vec<CueNodeDto>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CueNodeDto {
    pub basics: CueBasicsData,
    #[serde(default)]
    pub triggers: CueTriggersData,
    #[serde(default)]
    pub dead: bool,
    pub body: CueBody,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CueBody {
    Audio(AudioBody),
    /// Vídeo: lleva su pista de audio (que suena por el motor, como una cue de audio) y la imagen que se proyecta.
    Video(VideoBody),
    Fade(FadeBody),
    /// Cue de control: detiene, pausa, reanuda o hace Devamp de otra cue (`cue_target_id`).
    Control(ControlBody),
    /// Cue de red: envía un mensaje OSC por UDP.
    Osc(OscBody),
    /// Nota o espera: no suena ni actúa; dura `action_duration`.
    Memo,
    Group(GroupBody),
}

impl CueBody {
    /// Tiempos, bucles y niveles de una cue que suena por el motor (audio o la pista de audio de un vídeo).
    pub fn audio(&self) -> Option<&AudioBody> {
        match self {
            Self::Audio(a) => Some(a),
            Self::Video(v) => Some(&v.audio),
            _ => None,
        }
    }

    pub fn audio_mut(&mut self) -> Option<&mut AudioBody> {
        match self {
            Self::Audio(a) => Some(a),
            Self::Video(v) => Some(&mut v.audio),
            _ => None,
        }
    }

    pub fn is_video(&self) -> bool {
        matches!(self, Self::Video(_))
    }
}

/// Cómo se ajusta la imagen a la pantalla de proyección.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VideoFit {
    /// Cabe entera, con barras si la proporción no coincide.
    #[default]
    Fit,
    /// Llena la pantalla recortando lo que sobre.
    Fill,
    /// Se estira a la pantalla.
    Stretch,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VideoBody {
    #[serde(flatten)]
    pub audio: AudioBody,
    #[serde(default)]
    pub fit: VideoFit,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AudioBody {
    #[serde(default)]
    pub time_loops: AudioTimeAndLoopsData,
    /// Si falta, se usa una matriz uno-a-uno entre los canales del archivo y los de salida.
    #[serde(default)]
    pub levels: Option<AudioLevelMatrixData>,
    /// Ecualizador paramétrico de 4 bandas. Los shows sin EQ no llevan este campo.
    #[serde(default, skip_serializing_if = "AudioEqData::is_default")]
    pub eq: AudioEqData,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FadeModeDto {
    #[default]
    Absolute,
    Relative,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "shape", rename_all = "snake_case")]
pub enum FadeCurveDto {
    Linear,
    #[default]
    SCurve,
    Exponential { steepness: f32 },
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct FadeCellDto {
    pub row: u8,
    pub col: u8,
    pub target_db: f32,
    #[serde(default = "yes")]
    pub active: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FadeBody {
    #[serde(default)]
    pub mode: FadeModeDto,
    pub cells: Vec<FadeCellDto>,
    #[serde(default)]
    pub curve: FadeCurveDto,
    #[serde(default)]
    pub stop_target_when_done: bool,
    #[serde(default = "yes")]
    pub honor_gangs: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlActionDto {
    #[default]
    Stop,
    Pause,
    Resume,
    Devamp,
    GoTo,
    Load,
    Arm,
    Disarm,
}

impl ControlActionDto {
    pub const ALL: [Self; 8] = [Self::Stop, Self::Pause, Self::Resume, Self::Devamp, Self::GoTo, Self::Load, Self::Arm, Self::Disarm];

    pub fn label(self) -> &'static str {
        match self {
            Self::Stop => "Stop",
            Self::Pause => "Pause",
            Self::Resume => "Resume",
            Self::Devamp => "Devamp",
            Self::GoTo => "Go to",
            Self::Load => "Load",
            Self::Arm => "Arm",
            Self::Disarm => "Disarm",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ControlBody {
    #[serde(default)]
    pub action: ControlActionDto,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OscBody {
    /// `host:puerto` de destino.
    #[serde(default)]
    pub destination: String,
    /// Dirección OSC y argumentos en una línea.
    #[serde(default)]
    pub message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupModeDto {
    Timeline,
    FireFirstGoToNext,
    FireAllSimultaneously,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GroupBody {
    pub mode: GroupModeDto,
    #[serde(default)]
    pub children: Vec<CueNodeDto>,
}

#[derive(Debug)]
pub enum ShowError {
    Json(serde_json::Error),
    WrongFormat(String),
    DuplicateId(CueId),
    ZeroId,
    TooDeep,
    BadCue { cue: CueId, error: DtoError },
    BadFadeTarget { cue: CueId, target: CueId },
    BadSampleRate(u32),
}

impl std::fmt::Display for ShowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Json(e) => write!(f, "JSON inválido: {e}"),
            Self::WrongFormat(s) => write!(f, "formato `{s}` no soportado (se esperaba `{SHOW_FORMAT}`; también se lee `{SHOW_FORMAT_LEGACY}`)"),
            Self::DuplicateId(id) => write!(f, "id de cue repetido: {}", id.0),
            Self::ZeroId => write!(f, "el id de cue 0 está reservado"),
            Self::TooDeep => write!(f, "grupos anidados a más de {MAX_GROUP_DEPTH} niveles"),
            Self::BadCue { cue, error } => write!(f, "cue {}: {error}", cue.0),
            Self::BadFadeTarget { cue, target } => write!(f, "el fade {} apunta a la cue inexistente {}", cue.0, target.0),
            Self::BadSampleRate(sr) => write!(f, "frecuencia de muestreo {sr} fuera de 8000..=384000"),
        }
    }
}

impl std::error::Error for ShowError {}

impl From<serde_json::Error> for ShowError {
    fn from(e: serde_json::Error) -> Self {
        Self::Json(e)
    }
}

/// Comprueba una cue por sí sola (sin mirar a las demás): id distinto de 0, tiempos y velocidad válidos.
pub fn validate_node(n: &CueNodeDto) -> Result<(), ShowError> {
    let id = n.basics.unique_id;
    if id.0 == 0 {
        return Err(ShowError::ZeroId);
    }
    n.basics.validate().map_err(|error| ShowError::BadCue { cue: id, error })?;
    if let Some(a) = n.body.audio() {
        let t = &a.time_loops;
        let bad = if !(RATE_MIN..=RATE_MAX).contains(&t.rate) {
            Some(DtoError::RateOutOfRange(t.rate))
        } else if t.play_count == 0 {
            Some(DtoError::PlayCountZero)
        } else {
            None
        };
        if let Some(error) = bad {
            return Err(ShowError::BadCue { cue: id, error });
        }
    }
    Ok(())
}

/// Tipo de cue que se puede crear desde la UI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NewCueKind {
    Audio,
    Video,
    Fade,
    Control,
    Osc,
    Memo,
    Group,
}

impl NewCueKind {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "audio" => Some(Self::Audio),
            "video" => Some(Self::Video),
            "fade" => Some(Self::Fade),
            "control" => Some(Self::Control),
            "osc" => Some(Self::Osc),
            "memo" => Some(Self::Memo),
            "group" => Some(Self::Group),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Audio => "audio",
            Self::Video => "video",
            Self::Fade => "fade",
            Self::Control => "control",
            Self::Osc => "osc",
            Self::Memo => "memo",
            Self::Group => "group",
        }
    }
}

impl CueNodeDto {
    /// Cue nueva con valores por defecto: audio sin archivo (queda desarmada hasta que se le asigna uno),
    /// fade absoluto con curva S y sin celdas, o grupo Timeline vacío.
    pub fn new_default(kind: NewCueKind, id: u64, number: String) -> Self {
        let (body, default_name) = match kind {
            NewCueKind::Audio => (CueBody::Audio(AudioBody { time_loops: Default::default(), levels: None, eq: Default::default() }), "Audio"),
            NewCueKind::Video => (CueBody::Video(VideoBody { audio: AudioBody { time_loops: Default::default(), levels: None, eq: Default::default() }, fit: VideoFit::Fit }), "Vídeo"),
            NewCueKind::Fade => (
                CueBody::Fade(FadeBody { mode: FadeModeDto::Absolute, cells: vec![], curve: FadeCurveDto::SCurve, stop_target_when_done: false, honor_gangs: true }),
                "Fade",
            ),
            NewCueKind::Memo => (CueBody::Memo, "Memo"),
            NewCueKind::Osc => (CueBody::Osc(OscBody::default()), "OSC"),
            NewCueKind::Control => (CueBody::Control(ControlBody::default()), "Control"),
            NewCueKind::Group => (CueBody::Group(GroupBody { mode: GroupModeDto::Timeline, children: vec![] }), "Grupo"),
        };
        let mut basics = CueBasicsData { unique_id: CueId(id), number, default_name: default_name.into(), ..Default::default() };
        if kind == NewCueKind::Fade {
            basics.action_duration = 3.0;
        }
        if kind == NewCueKind::Memo {
            basics.action_duration = 5.0;
        }
        if matches!(kind, NewCueKind::Audio | NewCueKind::Video) {
            basics.armed = false;
        }
        CueNodeDto { basics, triggers: Default::default(), dead: false, body }
    }
}

impl ShowFile {
    pub fn from_json(json: &str) -> Result<Self, ShowError> {
        let show: ShowFile = serde_json::from_str(json)?;
        show.validate()?;
        Ok(show)
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("un ShowFile siempre se serializa")
    }

    /// Recorre todas las cues (grupos incluidos) en profundidad: `(cue, id del grupo padre)`.
    pub fn walk<'a>(&'a self, mut f: impl FnMut(&'a CueNodeDto, Option<CueId>)) {
        fn go<'a>(nodes: &'a [CueNodeDto], parent: Option<CueId>, f: &mut impl FnMut(&'a CueNodeDto, Option<CueId>)) {
            for n in nodes {
                f(n, parent);
                if let CueBody::Group(g) = &n.body {
                    go(&g.children, Some(n.basics.unique_id), f);
                }
            }
        }
        go(&self.cues, None, &mut f);
    }

    /// Recorre las cues (grupos incluidos, en profundidad) con acceso mutable.
    pub fn walk_mut(&mut self, mut f: impl FnMut(&mut CueNodeDto)) {
        fn go(nodes: &mut [CueNodeDto], f: &mut impl FnMut(&mut CueNodeDto)) {
            for n in nodes {
                f(n);
                if let CueBody::Group(g) = &mut n.body {
                    go(&mut g.children, f);
                }
            }
        }
        go(&mut self.cues, &mut f);
    }

    /// Resuelve los archivos relativos respecto a la carpeta del show `base`: una ruta no absoluta se cuelga
    /// de ella, y si la ruta absoluta ya no existe se prueba `relative_path`. Así un show empaquetado funciona
    /// donde se copie la carpeta.
    pub fn resolve_media_paths(&mut self, base: &std::path::Path) {
        self.walk_mut(|n| {
            let Some(t) = n.basics.file_target.as_mut() else { return };
            if t.absolute_path.is_empty() && t.relative_path.is_none() {
                return;
            }
            let abs = std::path::Path::new(&t.absolute_path);
            if !t.absolute_path.is_empty() && !abs.is_absolute() {
                t.absolute_path = base.join(abs).to_string_lossy().into_owned();
            } else if !abs.is_file() {
                if let Some(rel) = t.relative_path.as_deref().filter(|r| !r.is_empty()) {
                    let cand = base.join(rel);
                    if cand.is_file() {
                        t.absolute_path = cand.to_string_lossy().into_owned();
                    }
                }
            }
        });
    }

    /// Escribe como relativos (con `/`) los archivos que cuelgan de la carpeta del show `base`.
    pub fn relativize_media_paths(&mut self, base: &std::path::Path) {
        self.walk_mut(|n| {
            let Some(t) = n.basics.file_target.as_mut() else { return };
            if let Ok(rel) = std::path::Path::new(&t.absolute_path).strip_prefix(base) {
                let rel = rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect::<Vec<_>>().join("/");
                if !rel.is_empty() {
                    t.absolute_path = rel.clone();
                    t.relative_path = Some(rel);
                }
            }
        });
    }

    pub fn cue_count(&self) -> usize {
        let mut n = 0;
        self.walk(|_, _| n += 1);
        n
    }

    pub fn validate(&self) -> Result<(), ShowError> {
        if self.format != SHOW_FORMAT && self.format != SHOW_FORMAT_LEGACY {
            return Err(ShowError::WrongFormat(self.format.clone()));
        }
        if !(8_000..=384_000).contains(&self.sample_rate) {
            return Err(ShowError::BadSampleRate(self.sample_rate));
        }
        fn depth(nodes: &[CueNodeDto], level: usize) -> bool {
            level <= MAX_GROUP_DEPTH
                && nodes.iter().all(|n| match &n.body {
                    CueBody::Group(g) => depth(&g.children, level + 1),
                    _ => true,
                })
        }
        if !depth(&self.cues, 1) {
            return Err(ShowError::TooDeep);
        }
        let mut ids = HashSet::new();
        let mut first_error = None;
        self.walk(|n, _| {
            let id = n.basics.unique_id;
            if first_error.is_some() {
                return;
            }
            if id.0 != 0 && !ids.insert(id) {
                first_error = Some(ShowError::DuplicateId(id));
            } else if let Err(e) = validate_node(n) {
                first_error = Some(e);
            }
        });
        if let Some(e) = first_error {
            return Err(e);
        }
        let mut bad_target = None;
        self.walk(|n, _| {
            if let (CueBody::Fade(_) | CueBody::Control(_), Some(target)) = (&n.body, n.basics.cue_target_id) {
                if !ids.contains(&target) && bad_target.is_none() {
                    bad_target = Some(ShowError::BadFadeTarget { cue: n.basics.unique_id, target });
                }
            }
        });
        bad_target.map_or(Ok(()), Err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn audio(id: u64) -> CueNodeDto {
        CueNodeDto {
            basics: CueBasicsData { unique_id: CueId(id), ..Default::default() },
            triggers: Default::default(),
            dead: false,
            body: CueBody::Audio(AudioBody { time_loops: Default::default(), levels: None, eq: Default::default() }),
        }
    }

    fn show(cues: Vec<CueNodeDto>) -> ShowFile {
        ShowFile { format: SHOW_FORMAT.into(), name: "test".into(), sample_rate: 48_000, carts: Default::default(), cues }
    }

    #[test]
    fn minimal_json_uses_defaults() {
        let json = r#"{"format":"qlab-rs/v1","cues":[
            {"basics":{"unique_id":1,"name":"a"},"body":{"kind":"audio"}},
            {"basics":{"unique_id":2,"cue_target_id":1,"action_duration":2.0},
             "body":{"kind":"fade","cells":[{"row":0,"col":0,"target_db":-20.0}],"curve":{"shape":"exponential","steepness":2.5}}},
            {"basics":{"unique_id":3},"body":{"kind":"group","mode":"fire_first_go_to_next","children":[
                {"basics":{"unique_id":4},"body":{"kind":"audio"}}]}}
        ]}"#;
        let s = ShowFile::from_json(json).unwrap();
        assert_eq!((s.sample_rate, s.cues.len(), s.cue_count()), (48_000, 3, 4));
        let CueBody::Fade(f) = &s.cues[1].body else { panic!() };
        assert!(f.honor_gangs && f.cells[0].active);
        assert_eq!(f.curve, FadeCurveDto::Exponential { steepness: 2.5 });
        assert!(s.cues[0].basics.armed, "armed por defecto");
        let mut order = Vec::new();
        s.walk(|n, p| order.push((n.basics.unique_id.0, p.map(|c| c.0))));
        assert_eq!(order, vec![(1, None), (2, None), (3, None), (4, Some(3))]);
    }

    #[test]
    fn roundtrip() {
        let s = show(vec![audio(1), audio(2)]);
        assert_eq!(ShowFile::from_json(&s.to_json()).unwrap(), s);
    }

    #[test]
    fn validation_errors() {
        let mut s = show(vec![audio(1)]);
        s.format = "otro".into();
        assert!(matches!(s.validate(), Err(ShowError::WrongFormat(_))));
        assert!(matches!(show(vec![audio(1), audio(1)]).validate(), Err(ShowError::DuplicateId(CueId(1)))));
        assert!(matches!(show(vec![audio(0)]).validate(), Err(ShowError::ZeroId)));
        let mut s = show(vec![audio(1)]);
        s.sample_rate = 5;
        assert!(matches!(s.validate(), Err(ShowError::BadSampleRate(5))));

        let mut bad = audio(3);
        bad.basics.pre_wait = -1.0;
        assert!(matches!(show(vec![bad]).validate(), Err(ShowError::BadCue { .. })));
        let mut bad = audio(3);
        if let CueBody::Audio(a) = &mut bad.body {
            a.time_loops.rate = 100.0;
        }
        assert!(matches!(show(vec![bad]).validate(), Err(ShowError::BadCue { error: DtoError::RateOutOfRange(_), .. })));

        let mut fade = audio(5);
        fade.basics.cue_target_id = Some(CueId(99));
        fade.body = CueBody::Fade(FadeBody { mode: FadeModeDto::Absolute, cells: vec![], curve: FadeCurveDto::Linear, stop_target_when_done: false, honor_gangs: true });
        assert!(matches!(show(vec![fade]).validate(), Err(ShowError::BadFadeTarget { target: CueId(99), .. })));

        let mut deep = audio(100);
        for i in 0..10 {
            deep = CueNodeDto {
                basics: CueBasicsData { unique_id: CueId(i + 1), ..Default::default() },
                triggers: Default::default(),
                dead: false,
                body: CueBody::Group(GroupBody { mode: GroupModeDto::Timeline, children: vec![deep] }),
            };
        }
        assert!(matches!(show(vec![deep]).validate(), Err(ShowError::TooDeep)));
        assert!(matches!(ShowFile::from_json("{"), Err(ShowError::Json(_))));
        assert!(!ShowError::TooDeep.to_string().is_empty());
    }

    #[test]
    fn new_default_nodes_are_valid_and_typed() {
        for (kind, name) in [(NewCueKind::Audio, "audio"), (NewCueKind::Video, "video"), (NewCueKind::Fade, "fade"), (NewCueKind::Group, "group")] {
            assert_eq!(NewCueKind::parse(name), Some(kind));
            assert_eq!(kind.as_str(), name);
            let n = CueNodeDto::new_default(kind, 9, "9".into());
            validate_node(&n).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!((n.basics.unique_id, n.basics.number.as_str()), (CueId(9), "9"));
            assert!(!n.basics.default_name.is_empty());
        }
        assert_eq!(NewCueKind::parse("hologram"), None);
        assert!(!CueNodeDto::new_default(NewCueKind::Audio, 1, "1".into()).basics.armed, "audio sin archivo queda desarmado");
        assert_eq!(CueNodeDto::new_default(NewCueKind::Fade, 1, "1".into()).basics.action_duration, 3.0);
    }

    fn show_with_file(path: &str) -> ShowFile {
        let mut n = CueNodeDto::new_default(NewCueKind::Audio, 1, "1".into());
        n.basics.file_target = Some(crate::cue_dto::MediaTarget { absolute_path: path.into(), ..Default::default() });
        ShowFile { format: SHOW_FORMAT.into(), name: "t".into(), sample_rate: 48_000, carts: Default::default(), cues: vec![n] }
    }

    fn target_of(s: &ShowFile) -> (String, Option<String>) {
        let t = s.cues[0].basics.file_target.as_ref().unwrap();
        (t.absolute_path.clone(), t.relative_path.clone())
    }

    #[test]
    fn media_paths_inside_the_show_folder_become_relative_and_resolve_back() {
        let base = std::path::Path::new("/gira/acto1");
        let mut s = show_with_file("/gira/acto1/audio/Entrada coro.wav");
        s.relativize_media_paths(base);
        assert_eq!(target_of(&s), ("audio/Entrada coro.wav".to_string(), Some("audio/Entrada coro.wav".to_string())));
        s.resolve_media_paths(std::path::Path::new("/otra/maquina/acto1"));
        assert_eq!(target_of(&s).0, "/otra/maquina/acto1/audio/Entrada coro.wav", "una ruta relativa cuelga de la carpeta del show");
        // Lo que está fuera de la carpeta no se toca.
        let mut out = show_with_file("/musica/otro.wav");
        out.relativize_media_paths(base);
        assert_eq!(target_of(&out), ("/musica/otro.wav".to_string(), None));
    }

    #[test]
    fn a_missing_absolute_path_falls_back_to_the_relative_one_when_it_exists() {
        let dir = std::env::temp_dir().join(format!("qlab-resolve-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("audio")).unwrap();
        std::fs::write(dir.join("audio/a.wav"), b"x").unwrap();
        let mut s = show_with_file("/no/existe/a.wav");
        s.cues[0].basics.file_target.as_mut().unwrap().relative_path = Some("audio/a.wav".into());
        s.resolve_media_paths(&dir);
        assert_eq!(target_of(&s).0, dir.join("audio/a.wav").to_string_lossy());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn walk_mut_reaches_children_of_groups() {
        let mut g = CueNodeDto::new_default(NewCueKind::Group, 5, "5".into());
        if let CueBody::Group(b) = &mut g.body {
            b.children.push(CueNodeDto::new_default(NewCueKind::Audio, 6, "6".into()));
        }
        let mut s = ShowFile { format: SHOW_FORMAT.into(), name: "g".into(), sample_rate: 48_000, carts: Default::default(), cues: vec![g] };
        let mut n = 0;
        s.walk_mut(|c| {
            n += 1;
            c.basics.name = "x".into();
        });
        assert_eq!(n, 2);
    }

    #[test]
    fn video_cues_roundtrip_in_json_and_share_the_audio_helpers() {
        let mut v = CueNodeDto::new_default(NewCueKind::Video, 3, "3".into());
        assert!(v.body.is_video() && v.body.audio().is_some());
        if let CueBody::Video(b) = &mut v.body {
            b.fit = VideoFit::Fill;
            b.audio.time_loops.slices = vec![crate::cue_dto::AudioSliceMarker::new(100, 2)];
        }
        let json = serde_json::to_string(&v).unwrap();
        assert!(json.contains(r#""kind":"video""#) && json.contains(r#""fit":"fill""#) && json.contains("time_loops"), "{json}");
        assert_eq!(serde_json::from_str::<CueNodeDto>(&json).unwrap(), v);
        // Un vídeo sin `fit` (show más antiguo o escrito a mano) usa el valor por defecto.
        let old: CueNodeDto = serde_json::from_str(&json.replace(r#","fit":"fill""#, "")).unwrap();
        assert!(matches!(&old.body, CueBody::Video(b) if b.fit == VideoFit::Fit));
        // La velocidad inválida se rechaza igual que en una cue de audio.
        if let Some(a) = v.body.audio_mut() {
            a.time_loops.rate = 1000.0;
        }
        assert!(validate_node(&v).is_err());
        assert!(CueNodeDto::new_default(NewCueKind::Fade, 4, "4".into()).body.audio().is_none());
    }

    #[test]
    fn shows_are_written_as_teatrocue_and_the_legacy_format_still_loads() {
        let s = ShowFile { format: SHOW_FORMAT.into(), name: "x".into(), sample_rate: 48_000, carts: Default::default(), cues: vec![] };
        assert!(s.to_json().contains(r#""format": "teatrocue/v1""#));
        let legacy = s.to_json().replace("teatrocue/v1", "qlab-rs/v1");
        assert!(ShowFile::from_json(&legacy).is_ok(), "los shows `.json` de antes siguen abriendo");
        let other = s.to_json().replace("teatrocue/v1", "otro/v9");
        assert!(matches!(ShowFile::from_json(&other), Err(ShowError::WrongFormat(_))));
    }
    #[test]
    fn control_cues_roundtrip_and_their_target_is_validated() {
        let mut audio = CueNodeDto::new_default(NewCueKind::Audio, 1, "1".into());
        audio.basics.armed = true;
        let mut ctl = CueNodeDto::new_default(NewCueKind::Control, 2, "2".into());
        ctl.basics.cue_target_id = Some(CueId(1));
        ctl.body = CueBody::Control(ControlBody { action: ControlActionDto::Pause });
        let show = ShowFile { format: SHOW_FORMAT.into(), name: "c".into(), sample_rate: 48_000, carts: Default::default(), cues: vec![audio.clone(), ctl.clone()] };
        let back = ShowFile::from_json(&show.to_json()).unwrap();
        assert_eq!(back.cues[1].body, CueBody::Control(ControlBody { action: ControlActionDto::Pause }));
        assert!(NewCueKind::parse("control") == Some(NewCueKind::Control) && NewCueKind::Control.as_str() == "control");
        ctl.basics.cue_target_id = Some(CueId(77));
        let bad = ShowFile { cues: vec![audio, ctl], ..show };
        assert!(matches!(ShowFile::from_json(&bad.to_json()), Err(ShowError::BadFadeTarget { target, .. }) if target == CueId(77)));
    }

    /// Un show guardado con una versión anterior (sin memorias, sin cues de control/OSC/memo, sin «arrancar desde»)
    /// tiene que seguir abriendo igual tras actualizar el programa, y volver a guardarse sin ruido nuevo.
    #[test]
    fn a_show_saved_by_an_older_version_still_loads_and_does_not_grow_new_fields() {
        let old = r#"{
          "format": "qlab-rs/v1", "name": "Giselle acto 1", "sample_rate": 48000,
          "cues": [
            {"basics": {"unique_id": 1, "number": "1", "name": "Obertura"},
             "body": {"kind": "audio", "time_loops": {"file_sample_rate": 44100, "start_time_sec": 1.5, "end_time_sec": 20.0,
                      "slices": [{"frame": 88200, "play_count": 2}]}}},
            {"basics": {"unique_id": 2, "number": "2", "name": "Fade", "cue_target_id": 1},
             "body": {"kind": "fade", "cells": [{"row": 0, "col": 0, "target_db": -20.0}]}},
            {"basics": {"unique_id": 3, "number": "3", "name": "Grupo"},
             "body": {"kind": "group", "mode": "timeline", "children": []}}
          ]}"#;
        let show = match ShowFile::from_json(old) {
            Ok(s) => s,
            Err(e) => panic!("el show viejo debe abrir: {e}"),
        };
        assert_eq!(show.cues.len(), 3);
        let CueBody::Audio(a) = &show.cues[0].body else { panic!("la primera es de audio") };
        assert_eq!((a.time_loops.start_time_sec, a.time_loops.slices.len()), (1.5, 1));
        assert!(a.time_loops.memories.is_empty() && a.time_loops.start_override_secs.is_none(), "lo nuevo arranca vacío");
        // Al guardarlo de nuevo no aparecen campos nuevos vacíos (ni `memories` ni `start_override_secs`).
        let again = show.to_json();
        assert!(!again.contains("memories") && !again.contains("start_override_secs"), "{again}");
        assert_eq!(ShowFile::from_json(&again).unwrap(), show);
    }

    #[test]
    fn the_eq_configuration_survives_saving_and_loading_a_show() {
        use crate::cue_dto::{AudioEqData, EqBandData, EqFilterType};
        let mut audio = CueNodeDto::new_default(NewCueKind::Audio, 1, "1".into());
        let mut eq = AudioEqData::default();
        eq.enabled = true;
        eq.bands[0] = EqBandData { filter_type: EqFilterType::LowCut, frequency_hz: 80.0, gain_db: 0.0, q_factor: 0.707, enabled: true };
        eq.bands[1] = EqBandData { filter_type: EqFilterType::Peaking, frequency_hz: 440.5, gain_db: -7.5, q_factor: 3.2, enabled: true };
        eq.bands[2].enabled = false;
        eq.bands[3] = EqBandData { filter_type: EqFilterType::HighShelf, frequency_hz: 9_000.0, gain_db: 4.0, q_factor: 0.9, enabled: true };
        if let CueBody::Audio(a) = &mut audio.body {
            a.eq = eq;
        }
        let show = ShowFile { format: SHOW_FORMAT.into(), name: "mi show".into(), sample_rate: 48_000, carts: Default::default(), cues: vec![audio] };
        let json = show.to_json();
        assert!(json.contains("\"filter_type\": \"low_cut\"") && json.contains("440.5"), "{json}");
        let back = ShowFile::from_json(&json).unwrap();
        let CueBody::Audio(a) = &back.cues[0].body else { panic!() };
        assert_eq!(a.eq, eq, "el EQ vuelve idéntico");
        // Un show sin EQ no lleva el campo, y uno viejo (sin `eq`) abre con el EQ apagado y plano.
        let plain = ShowFile { cues: vec![CueNodeDto::new_default(NewCueKind::Audio, 1, "1".into())], ..show.clone() };
        assert!(!plain.to_json().contains("\"eq\""));
        let CueBody::Audio(p) = &ShowFile::from_json(&plain.to_json()).unwrap().cues[0].body else { panic!() };
        assert!(!p.eq.enabled && p.eq.is_default());
        // Valores fuera de rango o no numéricos se corrigen al ordenarlos.
        let wild = EqBandData { filter_type: EqFilterType::Peaking, frequency_hz: 1.0e9, gain_db: -99.0, q_factor: f32::NAN, enabled: true }.clamped();
        assert_eq!((wild.frequency_hz, wild.gain_db, wild.q_factor), (20_000.0, -24.0, 1.0));
    }


    #[test]
    fn carts_roundtrip_in_the_show_and_old_shows_load_without_them() {
        use crate::cue_dto::{CartMode, CartPad, CueColor};
        let mut s = show(vec![]);
        assert!(!s.to_json().contains("carts"), "sin pads no se escribe nada");
        s.carts.pads = vec![CartPad { path: "C:\\fx\\aplauso.wav".into(), name: "Aplauso".into(), color: CueColor::Green, gain_db: -3.0, key: "f5".into(), mode: CartMode::Hold }];
        let json = s.to_json();
        let back = ShowFile::from_json(&json).unwrap();
        assert_eq!(back.carts.pads[0].mode, CartMode::Hold);
        let clamped = back.carts.clone().clamped();
        assert_eq!(clamped.pads.len(), crate::cue_dto::CART_PADS);
        assert_eq!((clamped.pads[0].key.as_str(), clamped.pads[0].title().as_str()), ("F5", "Aplauso"));
        assert!(clamped.pads[1].is_empty());
        // Un show anterior a la botonera (sin el campo) se sigue abriendo.
        let old = r#"{"format":"qlab-rs/v1","name":"viejo","sample_rate":48000,"cues":[]}"#;
        assert!(ShowFile::from_json(old).unwrap().carts.is_empty());
        // Valores locos se corrigen.
        let wild = CartBank { pads: vec![CartPad { gain_db: f32::NAN, ..Default::default() }, CartPad { gain_db: 99.0, key: "  abcdefg ".into(), ..Default::default() }] }.clamped();
        assert_eq!((wild.pads[0].gain_db, wild.pads[1].gain_db, wild.pads[1].key.as_str()), (0.0, 12.0, "ABCD"));
        assert_eq!(CartPad { path: "/x/y/golpe.fuerte.wav".into(), ..Default::default() }.title(), "golpe.fuerte");
    }
}
