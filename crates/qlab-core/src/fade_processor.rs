//! Procesador de `FadeCue`: curvas, objetivo y traducción a `AudioCommand` (spec §1.4.B).
//!
//! El fade se evalúa **dentro del callback de audio**; aquí solo se prepara en el hilo de control:
//! se resuelve la curva a una `CurveTable` de 33 puntos y se emite un `BeginCellFade` por celda
//! afectada. La coalescencia con fades previos sobre la misma celda la resuelve el motor.

use std::collections::HashMap;

use qlab_engine::rt_command::{AudioCommand, CurveTable, FadeId, FadeMode, VoiceId};
use qlab_protocol::cue_dto::CueId;
use qlab_protocol::matrix::{clamp_db, AudioLevelMatrixData};

use crate::clock::Frames;
use crate::cue::FadeCueData;

/// Forma de un fade: `v(t)`, con `t` y `v` en `[0, 1]`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FadeCurve {
    /// `v(t) = t` (lineal en dB).
    Linear,
    /// Coseno: `(1 − cos πt) / 2`.
    SCurve,
    /// `(e^{kt} − 1) / (e^k − 1)`. `k > 0` arranca lento y acaba rápido; `k < 0`, al revés; `k ≈ 0` es lineal.
    Exponential { steepness: f32 },
    /// Tabla precalculada (Bézier/Hermite arbitrario).
    Custom(CurveTable),
}

impl Default for FadeCurve {
    fn default() -> Self {
        Self::SCurve
    }
}

impl FadeCurve {
    pub fn eval(&self, t: f32) -> f32 {
        let t = if t.is_nan() { 0.0 } else { t.clamp(0.0, 1.0) };
        match *self {
            Self::Linear => t,
            Self::SCurve => 0.5 * (1.0 - (core::f32::consts::PI * t).cos()),
            Self::Exponential { steepness: k } => {
                if k.abs() < 1e-4 {
                    t
                } else {
                    ((k * t).exp() - 1.0) / (k.exp() - 1.0)
                }
            }
            Self::Custom(table) => table.eval(t),
        }
    }

    /// Tabla de 33 puntos que consume el motor.
    pub fn to_table(&self) -> CurveTable {
        match *self {
            Self::Linear => CurveTable::LINEAR,
            Self::Custom(t) => t,
            other => CurveTable::from_fn(|t| other.eval(t)),
        }
    }

    /// Nivel en dB tras `t` (0..1) de un fade de `start_db` a `end_db`.
    pub fn trajectory_db(&self, start_db: f32, end_db: f32, t: f32) -> f32 {
        start_db + (end_db - start_db) * self.eval(t)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ActiveFade {
    pub fade_id: FadeId,
    pub target: CueId,
}

/// Convierte `FadeCue`s en comandos y recuerda qué fade está en vuelo para poder cancelarlo.
#[derive(Debug, Default)]
pub struct FadeProcessor {
    next_id: FadeId,
    active: HashMap<CueId, ActiveFade>,
}

/// Voz del motor que corresponde a una cue de audio.
#[inline]
pub fn voice_of(cue: CueId) -> VoiceId {
    cue.0
}

impl FadeProcessor {
    pub fn new() -> Self {
        Self { next_id: 1, active: HashMap::new() }
    }

    pub fn is_active(&self, fade_cue: CueId) -> bool {
        self.active.contains_key(&fade_cue)
    }

    pub fn active(&self, fade_cue: CueId) -> Option<ActiveFade> {
        self.active.get(&fade_cue).copied()
    }

    /// Lanza el fade de `fade_cue` sobre `data.target`. Devuelve su `FadeId`, o `None` si no hay
    /// objetivo o ninguna celda activa (no se emite nada). Si ya había uno en vuelo, lo cancela antes.
    ///
    /// `target_matrix` permite honrar los gangs (`honor_gangs`): las celdas del mismo gang reciben el
    /// mismo desplazamiento en dB.
    pub fn start(
        &mut self,
        fade_cue: CueId,
        data: &FadeCueData,
        duration_frames: Frames,
        target_matrix: Option<&AudioLevelMatrixData>,
        out: &mut Vec<AudioCommand>,
    ) -> Option<FadeId> {
        let target = data.target?;
        let mut cells: Vec<(u8, u8, f32)> =
            data.cells.iter().filter(|c| c.active).map(|c| (c.row, c.col, c.target_db)).collect();
        if cells.is_empty() {
            return None;
        }
        if data.honor_gangs {
            if let Some(m) = target_matrix {
                let mut extra: Vec<(u8, u8, f32)> = Vec::new();
                for &(row, col, db) in &cells {
                    let Ok(cell) = m.cell(row as usize, col as usize) else { continue };
                    let Some(gang) = cell.gang_id else { continue };
                    let delta = match data.mode {
                        FadeMode::Absolute => clamp_db(db) - cell.db,
                        FadeMode::Relative => db,
                    };
                    for (r, c) in m.gang_members(gang) {
                        let (r, c) = (r as u8, c as u8);
                        if cells.iter().chain(&extra).any(|&(rr, cc, _)| rr == r && cc == c) {
                            continue;
                        }
                        let base = m.db(r as usize, c as usize).unwrap_or(0.0);
                        let value = match data.mode {
                            FadeMode::Absolute => clamp_db(base + delta),
                            FadeMode::Relative => delta,
                        };
                        extra.push((r, c, value));
                    }
                }
                cells.extend(extra);
            }
        }

        self.cancel(fade_cue, out);
        let fade_id = self.next_id;
        self.next_id += 1;
        let curve = data.curve.to_table();
        let duration = duration_frames.min(u32::MAX as u64) as u32;
        for (row, col, target_db) in cells {
            out.push(AudioCommand::BeginCellFade {
                voice_id: voice_of(target),
                fade_id,
                mode: data.mode,
                row,
                col,
                target_db,
                duration_frames: duration,
                curve,
                stop_target_when_done: data.stop_target_when_done,
            });
        }
        self.active.insert(fade_cue, ActiveFade { fade_id, target });
        Some(fade_id)
    }

    /// Cancela el fade en vuelo de `fade_cue`, si lo hay.
    pub fn cancel(&mut self, fade_cue: CueId, out: &mut Vec<AudioCommand>) {
        if let Some(active) = self.active.remove(&fade_cue) {
            out.push(AudioCommand::CancelFade { voice_id: voice_of(active.target), fade_id: active.fade_id });
        }
    }

    /// Olvida el fade sin cancelarlo en el motor (ya terminó).
    pub fn forget(&mut self, fade_cue: CueId) {
        self.active.remove(&fade_cue);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cue::FadeCell;
    use qlab_protocol::matrix::GangId;

    fn data(mode: FadeMode, cells: &[(u8, u8, f32)]) -> FadeCueData {
        FadeCueData {
            target: Some(CueId(7)),
            mode,
            cells: cells.iter().map(|&(row, col, target_db)| FadeCell { row, col, target_db, active: true }).collect(),
            curve: FadeCurve::Linear,
            stop_target_when_done: false,
            honor_gangs: false,
        }
    }

    #[test]
    fn analytic_curves() {
        for c in [FadeCurve::Linear, FadeCurve::SCurve, FadeCurve::Exponential { steepness: 3.0 }, FadeCurve::Exponential { steepness: -3.0 }] {
            assert_eq!(c.eval(0.0), 0.0, "{c:?}");
            assert!((c.eval(1.0) - 1.0).abs() < 1e-6, "{c:?}");
            let mut prev = 0.0;
            for k in 0..=100 {
                let v = c.eval(k as f32 / 100.0);
                assert!(v >= prev - 1e-6, "{c:?} no monótona");
                prev = v;
            }
            assert_eq!(c.eval(-1.0), 0.0);
            assert!((c.eval(2.0) - 1.0).abs() < 1e-6);
            assert_eq!(c.eval(f32::NAN), 0.0);
        }
        assert!((FadeCurve::SCurve.eval(0.5) - 0.5).abs() < 1e-6);
        assert!(FadeCurve::Exponential { steepness: 3.0 }.eval(0.5) < 0.3, "arranca lento");
        assert!(FadeCurve::Exponential { steepness: -3.0 }.eval(0.5) > 0.7, "arranca rápido");
        assert_eq!(FadeCurve::Exponential { steepness: 0.0 }.eval(0.3), 0.3);
    }

    #[test]
    fn tables_match_the_analytic_curves_and_the_engine_interpolator() {
        for c in [FadeCurve::Linear, FadeCurve::SCurve, FadeCurve::Exponential { steepness: 2.5 }] {
            let t = c.to_table();
            for i in 0..33 {
                assert!((t.samples[i] - c.eval(i as f32 / 32.0)).abs() < 1e-6);
            }
            for k in 0..=200 {
                let x = k as f32 / 200.0;
                assert!((t.eval(x) - c.eval(x)).abs() < 2e-3, "{c:?} en {x}");
            }
        }
        let custom = CurveTable::constant(0.25);
        assert_eq!(FadeCurve::Custom(custom).to_table(), custom);
        assert_eq!(FadeCurve::Linear.to_table(), CurveTable::LINEAR);
    }

    #[test]
    fn trajectory() {
        let c = FadeCurve::SCurve;
        assert_eq!(c.trajectory_db(0.0, -20.0, 0.0), 0.0);
        assert!((c.trajectory_db(0.0, -20.0, 0.5) + 10.0).abs() < 1e-5);
        assert!((c.trajectory_db(-6.0, 6.0, 1.0) - 6.0).abs() < 1e-5);
    }

    #[test]
    fn start_emits_one_command_per_active_cell() {
        let mut p = FadeProcessor::new();
        let mut d = data(FadeMode::Absolute, &[(0, 0, -12.0), (1, 1, -20.0), (2, 2, 3.0)]);
        d.cells[2].active = false;
        d.stop_target_when_done = true;
        d.curve = FadeCurve::SCurve;
        let mut out = Vec::new();
        let id = p.start(CueId(100), &d, 96_000, None, &mut out).unwrap();
        assert_eq!(out.len(), 2);
        for (cmd, (row, db)) in out.iter().zip([(0u8, -12.0f32), (1, -20.0)]) {
            match *cmd {
                AudioCommand::BeginCellFade {
                    voice_id, fade_id, mode, row: r, target_db, duration_frames, curve, stop_target_when_done, ..
                } => {
                    assert_eq!((voice_id, fade_id, mode, r), (7, id, FadeMode::Absolute, row));
                    assert_eq!((target_db, duration_frames), (db, 96_000));
                    assert!(stop_target_when_done);
                    assert_eq!(curve, FadeCurve::SCurve.to_table());
                }
                other => panic!("comando inesperado: {other:?}"),
            }
        }
        assert!(p.is_active(CueId(100)));
        assert_eq!(p.active(CueId(100)), Some(ActiveFade { fade_id: id, target: CueId(7) }));
    }

    #[test]
    fn missing_target_or_cells_emit_nothing() {
        let mut p = FadeProcessor::new();
        let mut out = Vec::new();
        let mut d = data(FadeMode::Absolute, &[(0, 0, 0.0)]);
        d.target = None;
        assert!(p.start(CueId(1), &d, 100, None, &mut out).is_none());
        let mut d = data(FadeMode::Absolute, &[(0, 0, 0.0)]);
        d.cells[0].active = false;
        assert!(p.start(CueId(1), &d, 100, None, &mut out).is_none());
        assert!(out.is_empty() && !p.is_active(CueId(1)));
    }

    #[test]
    fn restart_cancels_the_previous_fade_and_ids_are_unique() {
        let mut p = FadeProcessor::new();
        let d = data(FadeMode::Relative, &[(1, 1, -6.0)]);
        let mut out = Vec::new();
        let a = p.start(CueId(5), &d, 100, None, &mut out).unwrap();
        out.clear();
        let b = p.start(CueId(5), &d, 100, None, &mut out).unwrap();
        assert_ne!(a, b);
        assert_eq!(out[0], AudioCommand::CancelFade { voice_id: 7, fade_id: a });
        assert!(matches!(out[1], AudioCommand::BeginCellFade { fade_id, mode: FadeMode::Relative, .. } if fade_id == b));
        out.clear();
        p.cancel(CueId(5), &mut out);
        assert_eq!(out, vec![AudioCommand::CancelFade { voice_id: 7, fade_id: b }]);
        p.cancel(CueId(5), &mut out);
        assert_eq!(out.len(), 1, "cancelar dos veces no repite");
        p.start(CueId(5), &d, 100, None, &mut out);
        p.forget(CueId(5));
        assert!(!p.is_active(CueId(5)));
    }

    #[test]
    fn long_durations_saturate_u32() {
        let mut p = FadeProcessor::new();
        let mut out = Vec::new();
        p.start(CueId(1), &data(FadeMode::Absolute, &[(0, 0, 0.0)]), u64::MAX, None, &mut out);
        assert!(matches!(out[0], AudioCommand::BeginCellFade { duration_frames: u32::MAX, .. }));
    }

    #[test]
    fn gangs_move_together_with_the_same_delta() {
        let mut m = AudioLevelMatrixData::new_one_to_one(2, 2).unwrap();
        let g = GangId::new(1).unwrap();
        m.set_db(1, 1, -10.0).unwrap();
        m.set_db(2, 2, -4.0).unwrap();
        m.set_gang(1, 1, Some(g)).unwrap();
        m.set_gang(2, 2, Some(g)).unwrap();
        let mut d = data(FadeMode::Absolute, &[(1, 1, -16.0)]); // delta −6
        d.honor_gangs = true;
        let mut p = FadeProcessor::new();
        let mut out = Vec::new();
        p.start(CueId(1), &d, 480, Some(&m), &mut out);
        let cells: Vec<_> = out
            .iter()
            .filter_map(|c| match *c {
                AudioCommand::BeginCellFade { row, col, target_db, .. } => Some((row, col, target_db)),
                _ => None,
            })
            .collect();
        assert_eq!(cells, vec![(1, 1, -16.0), (2, 2, -10.0)]);

        // Sin honor_gangs solo se mueve la celda pedida.
        d.honor_gangs = false;
        out.clear();
        p.start(CueId(2), &d, 480, Some(&m), &mut out);
        assert_eq!(out.len(), 1);

        // Relativo: el mismo delta a todo el gang; una celda pedida explícitamente no se duplica.
        let mut d = data(FadeMode::Relative, &[(1, 1, -3.0), (2, 2, -9.0)]);
        d.honor_gangs = true;
        out.clear();
        p.start(CueId(3), &d, 480, Some(&m), &mut out);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn gang_expansion_clamps_to_the_valid_range() {
        let mut m = AudioLevelMatrixData::new_one_to_one(2, 2).unwrap();
        let g = GangId::new(2).unwrap();
        m.set_db(1, 1, 0.0).unwrap();
        m.set_db(2, 2, 10.0).unwrap();
        m.set_gang(1, 1, Some(g)).unwrap();
        m.set_gang(2, 2, Some(g)).unwrap();
        let mut d = data(FadeMode::Absolute, &[(1, 1, 6.0)]); // delta +6 ⇒ la otra pediría 16
        d.honor_gangs = true;
        let mut out = Vec::new();
        FadeProcessor::new().start(CueId(1), &d, 10, Some(&m), &mut out);
        assert!(matches!(out[1], AudioCommand::BeginCellFade { row: 2, col: 2, target_db, .. } if target_db == 12.0));
    }
}
