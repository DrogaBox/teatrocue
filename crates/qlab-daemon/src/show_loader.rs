//! De `ShowFile` (formato nativo `qlab-rs/v1`) a una `CueList` lista para ejecutar, cargando de paso
//! las voces de audio. No es de tiempo real: corre en el hilo del IPC o en el arranque.

use qlab_core::clock::Timebase;
use qlab_core::cue::{Cue, CueKind};
use qlab_core::cue_list::CueList;
use qlab_protocol::cue_dto::CueId;
use qlab_protocol::matrix::{AudioLevelMatrixData, MAX_INPUT_CHANNELS, MAX_OUTPUT_CHANNELS};
use qlab_protocol::show_format::{CueBody, CueNodeDto, ShowFile};

use crate::cue_edit::node_to_kind;
use crate::voice_loader::{LoadError, VoiceLoader};

pub struct BuiltShow {
    pub list: CueList,
    pub name: String,
    pub cue_count: usize,
    /// Cues que no pudieron prepararse (quedan desarmadas) y avisos de compatibilidad.
    pub warnings: Vec<String>,
}

/// Construye la lista y precarga todas las voces. Descarga antes las del show anterior.
///
/// `show` debe haber pasado `ShowFile::validate` (lo hace `ShowFile::from_json`).
pub fn build_show(show: &ShowFile, loader: &mut VoiceLoader, timebase: Timebase, list_id: u64) -> BuiltShow {
    loader.unload_all();
    let mut list = CueList::new(list_id, timebase);
    let mut warnings = Vec::new();
    if show.sample_rate != timebase.sample_rate() {
        warnings.push(format!("el show declara {} Hz y el daemon corre a {} Hz", show.sample_rate, timebase.sample_rate()));
    }
    for node in &show.cues {
        add_node(&mut list, loader, node, None, &mut warnings);
    }
    BuiltShow { list, name: show.name.clone(), cue_count: show.cue_count(), warnings }
}

fn add_node(list: &mut CueList, loader: &mut VoiceLoader, node: &CueNodeDto, parent: Option<CueId>, warnings: &mut Vec<String>) {
    let id = node.basics.unique_id;
    let mut basics = node.basics.clone();
    let kind = match node.body.audio() {
        Some(a) => {
            let loaded = match basics.file_target.as_ref().map(|t| t.absolute_path.clone()).filter(|p| !p.is_empty()) {
                None => Err(LoadError::NoFileTarget),
                Some(path) => loader.load_path(id, std::path::Path::new(&path), &a.time_loops, a.levels.as_ref()),
            };
            let default_levels = match &loaded {
                Ok(info) => one_to_one(info.file.channels, loader.device_channels()),
                Err(_) => one_to_one(2, loader.device_channels()),
            };
            let mut kind = node_to_kind(node, None, default_levels);
            match loaded {
                Ok(info) => {
                    // La región queda explícita: con ella el núcleo conoce la duración real de la cue.
                    if let Some(data) = match &mut kind {
                        CueKind::Audio(d) => Some(d),
                        CueKind::Video(v) => Some(&mut v.audio),
                        _ => None,
                    } {
                        normalize_time(&mut data.time_loops, &info);
                    }
                }
                Err(e) => {
                    warnings.push(format!("cue {} ({}): {e}", basics.number, id.0));
                    basics.armed = false;
                }
            }
            kind
        }
        None => node_to_kind(node, None, one_to_one(2, 2)),
    };
    let mut cue = Cue::new(basics, kind);
    cue.triggers = node.triggers.clone();
    cue.dead = node.dead;
    let added = match parent {
        None => list.add_cue(cue),
        Some(p) => list.add_child(p, cue),
    };
    if let Err(e) = added {
        warnings.push(format!("cue {}: {e}", id.0));
        return;
    }
    if let CueBody::Group(g) = &node.body {
        for child in &g.children {
            add_node(list, loader, child, Some(id), warnings);
        }
    }
}

/// Fija la frecuencia del archivo y, si la cue no definía final, el final real.
pub fn normalize_time(t: &mut qlab_protocol::cue_dto::AudioTimeAndLoopsData, info: &crate::voice_loader::LoadedInfo) {
    let sr = info.file.sample_rate.max(1) as f64;
    t.file_sample_rate = info.file.sample_rate;
    if t.end_time_sec <= 0.0 {
        t.end_time_sec = info.end_frame as f64 / sr;
    }
}

fn one_to_one(file_channels: usize, device_channels: usize) -> AudioLevelMatrixData {
    AudioLevelMatrixData::new_one_to_one(file_channels.clamp(1, MAX_INPUT_CHANNELS), device_channels.clamp(1, MAX_OUTPUT_CHANNELS))
        .expect("dimensiones recortadas al rango válido")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{write_wav, Cleanup};
    use qlab_core::cue::CueKind;
    use qlab_protocol::cue_dto::{CueBasicsData, MediaTarget};
    use qlab_protocol::show_format::{AudioBody, FadeBody, FadeCellDto, FadeCurveDto, FadeModeDto, GroupBody, GroupModeDto, SHOW_FORMAT};
    use qlab_core::cue::GroupMode;
    use qlab_core::fade_processor::FadeCurve;
    use qlab_engine::rt_command::FadeMode;

    fn audio(id: u64, path: Option<&std::path::Path>) -> CueNodeDto {
        CueNodeDto {
            triggers: Default::default(),
            basics: CueBasicsData {
                unique_id: CueId(id),
                number: id.to_string(),
                file_target: path.map(|p| MediaTarget { absolute_path: p.to_string_lossy().into(), ..Default::default() }),
                ..Default::default()
            },
            dead: false,
            body: CueBody::Audio(AudioBody { time_loops: Default::default(), levels: None, eq: Default::default() }),
        }
    }

    fn show(cues: Vec<CueNodeDto>) -> ShowFile {
        ShowFile { format: SHOW_FORMAT.into(), name: "demo".into(), sample_rate: 48_000, carts: Default::default(), cues }
    }

    #[test]
    fn builds_cues_groups_and_fades_and_loads_voices() {
        let wav = write_wav("show", 2, 4_800, 48_000, |_, _| 0.1);
        let _c = Cleanup(vec![wav.clone()]);
        let mut fade = audio(2, None);
        fade.basics.cue_target_id = Some(CueId(1));
        fade.basics.action_duration = 2.0;
        fade.body = CueBody::Fade(FadeBody {
            mode: FadeModeDto::Relative,
            cells: vec![FadeCellDto { row: 1, col: 1, target_db: -6.0, active: true }],
            curve: FadeCurveDto::Exponential { steepness: 2.0 },
            stop_target_when_done: true,
            honor_gangs: false,
        });
        let group = CueNodeDto {
            triggers: Default::default(),
            basics: CueBasicsData { unique_id: CueId(3), ..Default::default() },
            dead: false,
            body: CueBody::Group(GroupBody { mode: GroupModeDto::Timeline, children: vec![audio(4, Some(&wav)), audio(5, Some(&wav))] }),
        };
        let mut dead = audio(6, Some(&wav));
        dead.dead = true;
        let mut loader = VoiceLoader::new(1, 2);
        let built = build_show(&show(vec![audio(1, Some(&wav)), fade, group, dead]), &mut loader, Timebase::new(48_000), 9);
        assert_eq!((built.name.as_str(), built.cue_count), ("demo", 6));
        assert!(built.warnings.is_empty(), "{:?}", built.warnings);
        assert_eq!(built.list.id(), 9);
        assert_eq!(built.list.order(), &[CueId(1), CueId(2), CueId(3), CueId(6)]);
        assert_eq!(built.list.parent_of(CueId(4)), Some(CueId(3)));
        assert!(built.list.cue(CueId(6)).unwrap().dead);
        assert_eq!(loader.loaded_count(), 4, "1, 4, 5 y 6 tienen archivo");

        let CueKind::Fade(f) = &built.list.cue(CueId(2)).unwrap().kind else { panic!() };
        assert_eq!((f.target, f.mode, f.stop_target_when_done), (Some(CueId(1)), FadeMode::Relative, true));
        assert_eq!(f.curve, FadeCurve::Exponential { steepness: 2.0 });
        let CueKind::Group(g) = &built.list.cue(CueId(3)).unwrap().kind else { panic!() };
        assert_eq!((g.mode, g.children.as_slice()), (GroupMode::Timeline, &[CueId(4), CueId(5)][..]));
        let CueKind::Audio(a) = &built.list.cue(CueId(1)).unwrap().kind else { panic!() };
        assert_eq!((a.levels.inputs, a.levels.outputs), (2, 2));
    }

    #[test]
    fn broken_cues_are_disarmed_with_a_warning_not_fatal() {
        let mut loader = VoiceLoader::new(1, 2);
        let missing = std::path::Path::new("/no/existe.wav");
        let built = build_show(&show(vec![audio(1, Some(missing)), audio(2, None)]), &mut loader, Timebase::new(48_000), 1);
        assert_eq!(built.warnings.len(), 2, "{:?}", built.warnings);
        assert!(!built.list.cue(CueId(1)).unwrap().basics.armed && !built.list.cue(CueId(2)).unwrap().basics.armed);
        assert_eq!(loader.loaded_count(), 0);
        assert!(built.warnings[1].contains("archivo"));
    }

    #[test]
    fn rebuilding_unloads_the_previous_show_and_warns_on_sample_rate() {
        let wav = write_wav("rebuild", 1, 4_800, 48_000, |_, _| 0.1);
        let _c = Cleanup(vec![wav.clone()]);
        let mut loader = VoiceLoader::new(1, 2);
        build_show(&show(vec![audio(1, Some(&wav)), audio(2, Some(&wav))]), &mut loader, Timebase::new(48_000), 1);
        assert_eq!(loader.loaded_count(), 2);
        let mut s = show(vec![audio(3, Some(&wav))]);
        s.sample_rate = 44_100;
        let built = build_show(&s, &mut loader, Timebase::new(48_000), 2);
        assert_eq!(loader.loaded_count(), 1);
        assert!(loader.voice(CueId(1)).is_none() && loader.voice(CueId(3)).is_some());
        assert!(built.warnings[0].contains("44100"));
    }
}
