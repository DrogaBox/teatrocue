//! Conversión entre los DTOs editables (`CueNodeDto`, lo que ve el Inspector) y el modelo del
//! núcleo, y cálculo de lo que hay que avisar al motor cuando se edita una cue en vivo.

use qlab_core::cue::{AudioCueData, ControlAction, ControlCueData, Cue, CueKind, OscCueData, FadeCell, FadeCueData, GroupCueData, GroupMode};
use qlab_core::fade_processor::FadeCurve;
use qlab_engine::rt_command::{AudioCommand, FadeMode};
use qlab_protocol::cue_dto::CueId;
use qlab_protocol::matrix::AudioLevelMatrixData;
use qlab_protocol::show_format::{
    AudioBody, ControlActionDto, ControlBody, CueBody, CueNodeDto, OscBody, FadeBody, FadeCellDto, FadeCurveDto, FadeModeDto, GroupBody, GroupModeDto,
};

pub fn control_action_from_dto(a: ControlActionDto) -> ControlAction {
    match a {
        ControlActionDto::Stop => ControlAction::Stop,
        ControlActionDto::Pause => ControlAction::Pause,
        ControlActionDto::Resume => ControlAction::Resume,
        ControlActionDto::Devamp => ControlAction::Devamp,
        ControlActionDto::GoTo => ControlAction::GoTo,
        ControlActionDto::Load => ControlAction::Load,
        ControlActionDto::Arm => ControlAction::Arm,
        ControlActionDto::Disarm => ControlAction::Disarm,
    }
}

pub fn fade_curve_from_dto(c: FadeCurveDto) -> FadeCurve {
    match c {
        FadeCurveDto::Linear => FadeCurve::Linear,
        FadeCurveDto::SCurve => FadeCurve::SCurve,
        FadeCurveDto::Exponential { steepness } => FadeCurve::Exponential { steepness },
    }
}

/// Las curvas a medida (`Custom`) no existen en el formato; se aproximan por la curva en S.
pub fn fade_curve_to_dto(c: FadeCurve) -> FadeCurveDto {
    match c {
        FadeCurve::Linear => FadeCurveDto::Linear,
        FadeCurve::SCurve | FadeCurve::Custom(_) => FadeCurveDto::SCurve,
        FadeCurve::Exponential { steepness } => FadeCurveDto::Exponential { steepness },
    }
}

pub fn group_mode_from_dto(m: GroupModeDto) -> GroupMode {
    match m {
        GroupModeDto::Timeline => GroupMode::Timeline,
        GroupModeDto::FireFirstGoToNext => GroupMode::FireFirstGoToNext,
        GroupModeDto::FireAllSimultaneously => GroupMode::FireAllSimultaneously,
    }
}

pub fn group_mode_to_dto(m: GroupMode) -> GroupModeDto {
    match m {
        GroupMode::Timeline => GroupModeDto::Timeline,
        GroupMode::FireFirstGoToNext => GroupModeDto::FireFirstGoToNext,
        GroupMode::FireAllSimultaneously => GroupModeDto::FireAllSimultaneously,
    }
}

/// Datos de núcleo de un nodo. Un audio sin matriz usa la que ya tenía la cue o, si no, `default_levels`.
/// Los grupos salen sin hijos (se conservan los existentes al editar).
pub fn node_to_kind(node: &CueNodeDto, existing_levels: Option<&AudioLevelMatrixData>, default_levels: AudioLevelMatrixData) -> CueKind {
    match &node.body {
        CueBody::Audio(a) => {
            let levels = a.levels.clone().or_else(|| existing_levels.cloned()).unwrap_or(default_levels);
            CueKind::Audio(AudioCueData::new(a.time_loops.clone(), levels).with_eq(a.eq))
        }
        CueBody::Video(v) => {
            let levels = v.audio.levels.clone().or_else(|| existing_levels.cloned()).unwrap_or(default_levels);
            CueKind::Video(qlab_core::cue::VideoCueData { audio: AudioCueData::new(v.audio.time_loops.clone(), levels).with_eq(v.audio.eq), fit: v.fit })
        }
        CueBody::Fade(f) => CueKind::Fade(FadeCueData {
            target: node.basics.cue_target_id,
            mode: match f.mode {
                FadeModeDto::Absolute => FadeMode::Absolute,
                FadeModeDto::Relative => FadeMode::Relative,
            },
            cells: f.cells.iter().map(|c| FadeCell { row: c.row, col: c.col, target_db: c.target_db, active: c.active }).collect(),
            curve: fade_curve_from_dto(f.curve),
            stop_target_when_done: f.stop_target_when_done,
            honor_gangs: f.honor_gangs,
        }),
        CueBody::Control(c) => CueKind::Control(ControlCueData {
            target: node.basics.cue_target_id,
            action: match c.action {
                ControlActionDto::Stop => ControlAction::Stop,
                ControlActionDto::Pause => ControlAction::Pause,
                ControlActionDto::Resume => ControlAction::Resume,
                ControlActionDto::Devamp => ControlAction::Devamp,
                ControlActionDto::GoTo => ControlAction::GoTo,
                ControlActionDto::Load => ControlAction::Load,
                ControlActionDto::Arm => ControlAction::Arm,
                ControlActionDto::Disarm => ControlAction::Disarm,
            },
        }),
        CueBody::Memo => CueKind::Memo,
        CueBody::Osc(o) => CueKind::Osc(OscCueData { destination: o.destination.clone(), message: o.message.clone() }),
        CueBody::Group(g) => CueKind::Group(GroupCueData::new(group_mode_from_dto(g.mode), vec![])),
    }
}

/// El DTO editable de una cue. Los grupos no incluyen a sus hijos (la UI los obtiene de los resúmenes).
pub fn cue_to_node(cue: &Cue) -> CueNodeDto {
    let body = match &cue.kind {
        CueKind::Audio(a) => CueBody::Audio(AudioBody { time_loops: a.time_loops.clone(), levels: Some(a.levels.clone()), eq: a.eq }),
        CueKind::Video(v) => CueBody::Video(qlab_protocol::show_format::VideoBody { audio: AudioBody { time_loops: v.audio.time_loops.clone(), levels: Some(v.audio.levels.clone()), eq: v.audio.eq }, fit: v.fit }),
        CueKind::Fade(f) => CueBody::Fade(FadeBody {
            mode: match f.mode {
                FadeMode::Absolute => FadeModeDto::Absolute,
                FadeMode::Relative => FadeModeDto::Relative,
            },
            cells: f.cells.iter().map(|c| FadeCellDto { row: c.row, col: c.col, target_db: c.target_db, active: c.active }).collect(),
            curve: fade_curve_to_dto(f.curve),
            stop_target_when_done: f.stop_target_when_done,
            honor_gangs: f.honor_gangs,
        }),
        CueKind::Control(c) => CueBody::Control(ControlBody {
            action: match c.action {
                ControlAction::Stop => ControlActionDto::Stop,
                ControlAction::Pause => ControlActionDto::Pause,
                ControlAction::Resume => ControlActionDto::Resume,
                ControlAction::Devamp => ControlActionDto::Devamp,
                ControlAction::GoTo => ControlActionDto::GoTo,
                ControlAction::Load => ControlActionDto::Load,
                ControlAction::Arm => ControlActionDto::Arm,
                ControlAction::Disarm => ControlActionDto::Disarm,
            },
        }),
        CueKind::Memo => CueBody::Memo,
        CueKind::Osc(o) => CueBody::Osc(OscBody { destination: o.destination.clone(), message: o.message.clone() }),
        CueKind::Group(g) => CueBody::Group(GroupBody { mode: group_mode_to_dto(g.mode), children: vec![] }),
    };
    CueNodeDto { basics: cue.basics.clone(), triggers: cue.triggers.clone(), dead: cue.dead, body }
}

/// Toda la lista como árbol de nodos del formato nativo (los grupos con sus hijos), lista para guardar.
pub fn list_to_nodes(list: &qlab_core::cue_list::CueList) -> Vec<CueNodeDto> {
    fn node(list: &qlab_core::cue_list::CueList, id: CueId) -> Option<CueNodeDto> {
        let cue = list.cue(id)?;
        let mut n = cue_to_node(cue);
        if let (CueKind::Group(g), CueBody::Group(body)) = (&cue.kind, &mut n.body) {
            body.children = g.children.iter().filter_map(|&c| node(list, c)).collect();
        }
        Some(n)
    }
    list.order().iter().filter_map(|&id| node(list, id)).collect()
}

pub fn file_path(node: &CueNodeDto) -> Option<&str> {
    node.basics.file_target.as_ref().map(|t| t.absolute_path.as_str()).filter(|p| !p.is_empty())
}

/// Mismos marcadores y repeticiones; el nombre (`label`) no cuenta porque no afecta a la reproducción.
fn same_slices(a: &[qlab_protocol::cue_dto::AudioSliceMarker], b: &[qlab_protocol::cue_dto::AudioSliceMarker]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.frame == y.frame && x.play_count == y.play_count)
}

/// ¿Hay que volver a cargar la voz? Archivo, región, repeticiones, bucle o slices obligan; la
/// velocidad, los niveles y los metadatos se aplican en vivo sin recargar.
pub fn needs_voice_reload(old: &CueNodeDto, new: &CueNodeDto) -> bool {
    let (Some(o), Some(n)) = (old.body.audio(), new.body.audio()) else { return false };
    let (a, b) = (&o.time_loops, &n.time_loops);
    file_path(old) != file_path(new)
        || a.start_time_sec != b.start_time_sec
        || a.start_override_secs != b.start_override_secs
        || a.end_time_sec != b.end_time_sec
        || a.play_count != b.play_count
        || a.infinite_loop != b.infinite_loop
        || !same_slices(&a.slices, &b.slices)
}

/// Comandos para que el motor siga una edición de niveles, máscaras de mute/solo y velocidad.
pub fn level_commands(id: CueId, old: &AudioCueData, new: &AudioCueData) -> Vec<AudioCommand> {
    let mut out = Vec::new();
    let (rows, cols) = (old.levels.inputs.min(new.levels.inputs) as usize, old.levels.outputs.min(new.levels.outputs) as usize);
    for r in 0..=rows {
        for c in 0..=cols {
            if let (Ok(a), Ok(b)) = (old.levels.db(r, c), new.levels.db(r, c)) {
                if a != b {
                    out.push(AudioCommand::SetCrosspointDb { voice_id: id.0, row: r as u8, col: c as u8, db: b });
                }
            }
        }
    }
    if old.levels.mute_mask != new.levels.mute_mask || old.levels.solo_mask != new.levels.solo_mask {
        out.push(AudioCommand::SetMuteSoloMask { voice_id: id.0, mute_mask: new.levels.mute_mask, solo_mask: new.levels.solo_mask });
    }
    if old.time_loops.rate != new.time_loops.rate {
        out.push(AudioCommand::SetRate { voice_id: id.0, rate: new.time_loops.rate });
    }
    // El EQ se aplica en vivo (el callback suaviza la transición): sin recargar la voz.
    if old.eq != new.eq {
        out.push(AudioCommand::SetEq { voice_id: id.0, eq: qlab_core::cue::eq_params(&new.eq) });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use qlab_core::cue::Cue;
    use qlab_protocol::cue_dto::{AudioSliceMarker, AudioTimeAndLoopsData, CueBasicsData, MediaTarget};

    fn audio_node() -> CueNodeDto {
        CueNodeDto {
            basics: CueBasicsData {
                unique_id: CueId(4),
                file_target: Some(MediaTarget { absolute_path: "/a.wav".into(), ..Default::default() }),
                ..Default::default()
            },
            triggers: Default::default(),
            dead: false,
            body: CueBody::Audio(AudioBody { time_loops: AudioTimeAndLoopsData { end_time_sec: 5.0, ..Default::default() }, levels: None, eq: Default::default() }),
        }
    }

    fn audio_data(node: &CueNodeDto) -> AudioCueData {
        match node_to_kind(node, None, AudioLevelMatrixData::new_one_to_one(2, 2).unwrap()) {
            CueKind::Audio(a) => a,
            _ => unreachable!(),
        }
    }

    #[test]
    fn nodes_roundtrip_through_the_core_model() {
        let mut node = audio_node();
        node.triggers.duck_others_enabled = true;
        node.dead = true;
        let kind = node_to_kind(&node, None, AudioLevelMatrixData::new_one_to_one(2, 2).unwrap());
        let mut cue = Cue::new(node.basics.clone(), kind);
        cue.triggers = node.triggers.clone();
        cue.dead = node.dead;
        let back = cue_to_node(&cue);
        assert_eq!(back.basics, node.basics);
        assert!(back.triggers.duck_others_enabled && back.dead);
        let CueBody::Audio(a) = &back.body else { panic!() };
        assert!(a.levels.is_some(), "el detalle siempre lleva la matriz");

        let fade = CueNodeDto {
            basics: CueBasicsData { unique_id: CueId(5), cue_target_id: Some(CueId(4)), ..Default::default() },
            triggers: Default::default(),
            dead: false,
            body: CueBody::Fade(FadeBody {
                mode: FadeModeDto::Relative,
                cells: vec![FadeCellDto { row: 1, col: 2, target_db: -6.0, active: true }],
                curve: FadeCurveDto::Exponential { steepness: 3.0 },
                stop_target_when_done: true,
                honor_gangs: false,
            }),
        };
        let kind = node_to_kind(&fade, None, AudioLevelMatrixData::new_one_to_one(1, 1).unwrap());
        assert!(matches!(&kind, CueKind::Fade(f) if f.target == Some(CueId(4)) && f.mode == FadeMode::Relative));
        assert_eq!(cue_to_node(&Cue::new(fade.basics.clone(), kind)).body, fade.body);

        let group = CueNodeDto {
            basics: CueBasicsData { unique_id: CueId(6), ..Default::default() },
            triggers: Default::default(),
            dead: false,
            body: CueBody::Group(GroupBody { mode: GroupModeDto::FireAllSimultaneously, children: vec![audio_node()] }),
        };
        let kind = node_to_kind(&group, None, AudioLevelMatrixData::new_one_to_one(1, 1).unwrap());
        let CueBody::Group(g) = cue_to_node(&Cue::new(group.basics.clone(), kind)).body else { panic!() };
        assert_eq!((g.mode, g.children.len()), (GroupModeDto::FireAllSimultaneously, 0), "sin hijos");
    }

    #[test]
    fn missing_levels_fall_back_to_existing_then_default() {
        let node = audio_node();
        let mut existing = AudioLevelMatrixData::new_one_to_one(3, 3).unwrap();
        existing.set_db(1, 1, -9.0).unwrap();
        let CueKind::Audio(a) = node_to_kind(&node, Some(&existing), AudioLevelMatrixData::new_one_to_one(2, 2).unwrap()) else { panic!() };
        assert_eq!(a.levels.inputs, 3);
        let a = audio_data(&node);
        assert_eq!(a.levels.inputs, 2);
    }

    #[test]
    fn curve_mapping_is_stable() {
        for c in [FadeCurveDto::Linear, FadeCurveDto::SCurve, FadeCurveDto::Exponential { steepness: -2.0 }] {
            assert_eq!(fade_curve_to_dto(fade_curve_from_dto(c)), c);
        }
        assert_eq!(fade_curve_to_dto(FadeCurve::Custom(qlab_engine::rt_command::CurveTable::constant(0.5))), FadeCurveDto::SCurve);
        for m in [GroupMode::Timeline, GroupMode::FireFirstGoToNext, GroupMode::FireAllSimultaneously] {
            assert_eq!(group_mode_from_dto(group_mode_to_dto(m)), m);
        }
    }

    #[test]
    fn reload_is_needed_only_for_file_region_and_loops() {
        let old = audio_node();
        let edit = |f: &dyn Fn(&mut CueNodeDto)| {
            let mut n = old.clone();
            f(&mut n);
            needs_voice_reload(&old, &n)
        };
        let tl = |n: &mut CueNodeDto| match &mut n.body {
            CueBody::Audio(a) => &mut a.time_loops as *mut AudioTimeAndLoopsData,
            _ => unreachable!(),
        };
        assert!(!edit(&|n| n.basics.name = "otro".into()));
        assert!(!edit(&|n| n.basics.pre_wait = 2.0));
        assert!(!edit(&|n| n.triggers.duck_others_enabled = true));
        // SAFETY: el puntero apunta al cuerpo del nodo `n`, vivo durante la llamada.
        assert!(!edit(&|n| unsafe { (*tl(n)).rate = 2.0 }), "la velocidad va en vivo");
        assert!(edit(&|n| n.basics.file_target = None));
        assert!(edit(&|n| unsafe { (*tl(n)).start_time_sec = 1.0 }));
        assert!(edit(&|n| unsafe { (*tl(n)).end_time_sec = 4.0 }));
        assert!(edit(&|n| unsafe { (*tl(n)).play_count = 3 }));
        assert!(edit(&|n| unsafe { (*tl(n)).infinite_loop = true }));
        assert!(edit(&|n| unsafe { (*tl(n)).slices.push(AudioSliceMarker::new(100, 2)) }));
        let fade = CueNodeDto { body: CueBody::Fade(FadeBody { mode: FadeModeDto::Absolute, cells: vec![], curve: FadeCurveDto::Linear, stop_target_when_done: false, honor_gangs: true }), ..old.clone() };
        assert!(!needs_voice_reload(&fade, &fade));
    }

    #[test]
    fn live_edit_commands() {
        let node = audio_node();
        let old = audio_data(&node);
        assert!(level_commands(CueId(4), &old, &old).is_empty());
        let mut new = old.clone();
        new.levels.set_db(1, 1, -6.0).unwrap();
        new.levels.set_db(0, 2, -3.0).unwrap();
        new.levels.set_mute(2, true).unwrap();
        new.time_loops.rate = 1.5;
        let cmds = level_commands(CueId(4), &old, &new);
        assert_eq!(
            cmds,
            vec![
                AudioCommand::SetCrosspointDb { voice_id: 4, row: 0, col: 2, db: -3.0 },
                AudioCommand::SetCrosspointDb { voice_id: 4, row: 1, col: 1, db: -6.0 },
                AudioCommand::SetMuteSoloMask { voice_id: 4, mute_mask: 0b10, solo_mask: 0 },
                AudioCommand::SetRate { voice_id: 4, rate: 1.5 },
            ]
        );
        // Matrices de distinto tamaño: solo se comparan las celdas comunes.
        let mut wide = old.clone();
        wide.levels = AudioLevelMatrixData::new_one_to_one(4, 4).unwrap();
        wide.levels.set_db(1, 1, -2.0).unwrap();
        assert_eq!(level_commands(CueId(4), &old, &wide), vec![AudioCommand::SetCrosspointDb { voice_id: 4, row: 1, col: 1, db: -2.0 }]);
    }

    #[test]
    fn renaming_a_slice_does_not_reload_the_voice_but_moving_or_recounting_does() {
        use qlab_protocol::cue_dto::AudioSliceMarker;
        let mut a = CueNodeDto::new_default(qlab_protocol::show_format::NewCueKind::Audio, 1, "1".into());
        if let CueBody::Audio(b) = &mut a.body {
            b.time_loops.slices = vec![AudioSliceMarker::new(100, 2)];
        }
        let edit = |f: &dyn Fn(&mut AudioSliceMarker)| {
            let mut b = a.clone();
            if let CueBody::Audio(x) = &mut b.body {
                f(&mut x.time_loops.slices[0]);
            }
            needs_voice_reload(&a, &b)
        };
        assert!(!edit(&|m| m.label = "Danza en puntas".into()));
        assert!(edit(&|m| m.frame = 200));
        assert!(edit(&|m| m.play_count = 3));
    }
}
