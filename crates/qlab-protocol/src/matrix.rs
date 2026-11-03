//! Matriz de niveles `AudioLevelMatrix` (spec §1.3): conversión dB↔lineal, gangs y máscaras mute/solo.
//!
//! Las celdas guardan dB finitos en `[MIN_VOLUME_DB, MAX_VOLUME_DB]`; `-inf` se representa como
//! `MIN_VOLUME_DB` para que el JSON siga siendo válido.

use std::fmt;
use std::num::NonZeroU8;

use serde::{Deserialize, Serialize};

pub const MIN_VOLUME_DB: f32 = -120.0;
pub const MAX_VOLUME_DB: f32 = 12.0;
pub const MAX_INPUT_CHANNELS: usize = 32;
pub const MAX_OUTPUT_CHANNELS: usize = 64;
/// Nivel de cada lado cuando un archivo mono sale por un par estéreo: el centro de una consola (−3 dB por lado, la misma
/// potencia que un solo canal a 0 dB). Lo usan las cues y los pads de Carts.
pub const MONO_CENTER_DB: f32 = -3.0;

/// Qué fichas de un par: las dos (estéreo) o una sola (mono por esa ficha). Así una placa de dos salidas sirve también
/// como dos salidas mono independientes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteSide {
    #[default]
    Both,
    First,
    Second,
}

/// Por dónde sale una cue o un pad: la placa (`output`: 0 = la principal, 1 = «Salida 2», 2 = «Salida 3»…) y qué
/// fichas de su par.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct OutputRoute {
    pub output: u8,
    pub side: RouteSide,
}

impl OutputRoute {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// Todas las rutas posibles con `extra_outputs` salidas adicionales, en el orden en que se ofrecen.
    pub fn all(extra_outputs: usize) -> Vec<Self> {
        (0..=extra_outputs.min(u8::MAX as usize - 1) as u8).flat_map(|output| [RouteSide::Both, RouteSide::First, RouteSide::Second].map(|side| Self { output, side })).collect()
    }

    /// Columnas de la mezcla (desde 0) a las que va: la primera y, a las dos fichas, la segunda. Las salidas
    /// adicionales empiezan después de los `main_channels` canales de la principal, un par cada una. Con una principal
    /// de un solo canal, sus dos fichas son la misma.
    pub fn columns(&self, main_channels: usize) -> (usize, Option<usize>) {
        let first = if self.output == 0 { 0 } else { main_channels.max(1) + 2 * (self.output as usize - 1) };
        let pair = self.output > 0 || main_channels >= 2;
        match self.side {
            RouteSide::Both => (first, pair.then_some(first + 1)),
            RouteSide::First => (first, None),
            RouteSide::Second => (if pair { first + 1 } else { first }, None),
        }
    }

    /// «Principal (1 y 2)», «Salida 2 · ficha 1»…
    pub fn label(&self) -> String {
        let place = if self.output == 0 { "Principal".to_string() } else { format!("Salida {}", self.output + 1) };
        match self.side {
            RouteSide::Both => format!("{place} · las dos fichas"),
            RouteSide::First => format!("{place} · solo ficha 1"),
            RouteSide::Second => format!("{place} · solo ficha 2"),
        }
    }
}

/// Identificador de gang (agrupación de faders). `None` en la celda = sin gang.
pub type GangId = NonZeroU8;

/// `0.0` exacto para `db <= -120.0` o NaN; en otro caso `10^(min(db, 12) / 20)`.
#[inline]
pub fn db_to_linear(db: f32) -> f32 {
    if db.is_nan() || db <= MIN_VOLUME_DB {
        return 0.0;
    }
    10.0f32.powf(db.min(MAX_VOLUME_DB) / 20.0)
}

/// Inversa de [`db_to_linear`]; ganancia `<= 0` o NaN devuelve `MIN_VOLUME_DB`.
#[inline]
pub fn linear_to_db(linear: f32) -> f32 {
    if linear.is_nan() || linear <= 0.0 {
        return MIN_VOLUME_DB;
    }
    clamp_db(20.0 * linear.log10())
}

/// Limita a `[MIN_VOLUME_DB, MAX_VOLUME_DB]`; NaN y `-inf` pasan a `MIN_VOLUME_DB`.
#[inline]
pub fn clamp_db(db: f32) -> f32 {
    if db.is_nan() {
        MIN_VOLUME_DB
    } else {
        db.clamp(MIN_VOLUME_DB, MAX_VOLUME_DB)
    }
}

/// Bit de la salida `output` (base 1, `1..=64`) en las máscaras mute/solo.
#[inline]
pub const fn output_bit(output: usize) -> Option<u64> {
    if output >= 1 && output <= MAX_OUTPUT_CHANNELS {
        Some(1u64 << (output - 1))
    } else {
        None
    }
}

/// `mute_solo_gate(j)`: 0.0 si está muteada, o si hay algún solo y `j` no lo tiene; 1.0 si no.
/// Salidas fuera de `1..=64` devuelven 0.0.
#[inline]
pub const fn mute_solo_gate(mute_mask: u64, solo_mask: u64, output: usize) -> f32 {
    let bit = match output_bit(output) {
        Some(b) => b,
        None => return 0.0,
    };
    if mute_mask & bit != 0 || (solo_mask != 0 && solo_mask & bit == 0) {
        0.0
    } else {
        1.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MatrixError {
    /// `inputs` fuera de `1..=32` o `outputs` fuera de `1..=64`.
    InvalidDimensions { inputs: usize, outputs: usize },
    /// `(row, col)` fuera de `(M+1) x (N+1)`.
    OutOfBounds { row: usize, col: usize },
    /// Salida fuera de `1..=64`.
    InvalidOutput(usize),
    /// `cells.len()` no coincide con `(M+1)*(N+1)` (típico tras deserializar).
    CellCountMismatch { expected: usize, found: usize },
}

impl fmt::Display for MatrixError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::InvalidDimensions { inputs, outputs } => {
                write!(f, "dimensiones inválidas: {inputs} entradas (1..=32) x {outputs} salidas (1..=64)")
            }
            Self::OutOfBounds { row, col } => write!(f, "celda ({row}, {col}) fuera de la matriz"),
            Self::InvalidOutput(j) => write!(f, "salida {j} fuera de 1..=64"),
            Self::CellCountMismatch { expected, found } => {
                write!(f, "se esperaban {expected} celdas y hay {found}")
            }
        }
    }
}

impl std::error::Error for MatrixError {}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct MatrixCell {
    pub db: f32,
    /// `false` = celda inactiva (no la toca un `FadeCue` absoluto).
    pub live: bool,
    pub gang_id: Option<GangId>,
}

impl MatrixCell {
    pub const fn new(db: f32) -> Self {
        Self { db, live: true, gang_id: None }
    }

    #[inline]
    pub fn linear(&self) -> f32 {
        db_to_linear(self.db)
    }
}

/// Matriz `(M+1) x (N+1)` en orden de filas: `(0,0)` master, `(0,j)` master de salida `j`,
/// `(i,0)` trim de entrada `i`, `(i,j)` crosspoint.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AudioLevelMatrixData {
    pub inputs: u8,
    pub outputs: u8,
    pub cells: Vec<MatrixCell>,
    pub mute_mask: u64,
    pub solo_mask: u64,
}

impl AudioLevelMatrixData {
    /// Defecto de `AudioCue`: masters y trims a 0 dB, crosspoints `i == j` a 0 dB y el resto a -inf. Un archivo mono
    /// con dos salidas o más sale por las dos primeras a [`MONO_CENTER_DB`] (al centro), no solo por la izquierda.
    pub fn new_one_to_one(inputs: usize, outputs: usize) -> Result<Self, MatrixError> {
        if !(1..=MAX_INPUT_CHANNELS).contains(&inputs) || !(1..=MAX_OUTPUT_CHANNELS).contains(&outputs) {
            return Err(MatrixError::InvalidDimensions { inputs, outputs });
        }
        let cols = outputs + 1;
        let mut cells = Vec::with_capacity((inputs + 1) * cols);
        for row in 0..=inputs {
            for col in 0..=outputs {
                let db = if row == 0 || col == 0 {
                    0.0
                } else if inputs == 1 && outputs >= 2 {
                    if col <= 2 { MONO_CENTER_DB } else { MIN_VOLUME_DB }
                } else if row == col {
                    0.0
                } else {
                    MIN_VOLUME_DB
                };
                cells.push(MatrixCell::new(db));
            }
        }
        Ok(Self { inputs: inputs as u8, outputs: outputs as u8, cells, mute_mask: 0, solo_mask: 0 })
    }

    /// La matriz de fábrica que tenían las cues mono hasta ahora (solo por la salida 1): un show guardado así la trae
    /// escrita. Es la que el operador nunca tocó, así que pasa al defecto actual (al centro).
    pub fn is_old_mono_default(&self) -> bool {
        let (inputs, outputs) = (self.inputs as usize, self.outputs as usize);
        if inputs != 1 || outputs < 2 || self.mute_mask != 0 || self.solo_mask != 0 || self.cells.len() != 2 * (outputs + 1) {
            return false;
        }
        self.cells.iter().enumerate().all(|(i, c)| {
            let (row, col) = (i / (outputs + 1), i % (outputs + 1));
            let want = if row == 0 || col == 0 || col == 1 { 0.0 } else { MIN_VOLUME_DB };
            c.db == want
        })
    }

    #[inline]
    fn cols(&self) -> usize {
        self.outputs as usize + 1
    }

    #[inline]
    fn index(&self, row: usize, col: usize) -> Result<usize, MatrixError> {
        if row > self.inputs as usize || col > self.outputs as usize {
            return Err(MatrixError::OutOfBounds { row, col });
        }
        Ok(row * self.cols() + col)
    }

    /// La misma matriz con `outputs` columnas de salida: las que ya tenía se conservan y las nuevas quedan en
    /// silencio (con su maestro de salida a 0 dB).
    pub fn with_outputs(&self, outputs: usize) -> Self {
        let outputs = outputs.clamp(1, MAX_OUTPUT_CHANNELS);
        let (inputs, old) = (self.inputs as usize, self.outputs as usize);
        let mut cells = Vec::with_capacity((inputs + 1) * (outputs + 1));
        for row in 0..=inputs {
            for col in 0..=outputs {
                cells.push(if col <= old { self.cells[row * (old + 1) + col].clone() } else { MatrixCell::new(if row == 0 { 0.0 } else { MIN_VOLUME_DB }) });
            }
        }
        Self { inputs: self.inputs, outputs: outputs as u8, cells, mute_mask: self.mute_mask, solo_mask: self.solo_mask }
    }

    /// Rutea la cue por `route`: rehace los cruces (los maestros, los trims y los gangs no se tocan) y agranda la
    /// matriz si la salida queda más allá de sus columnas. A las dos fichas, un archivo mono va al centro y uno estéreo
    /// uno a uno; a una sola ficha, todo se suma en mono (un estéreo, a −3 dB por canal). `main_channels`: canales de
    /// la placa principal.
    pub fn apply_route(&mut self, route: OutputRoute, main_channels: usize) {
        let (a, b) = route.columns(main_channels);
        let last = b.unwrap_or(a) + 1;
        if last > MAX_OUTPUT_CHANNELS {
            return;
        }
        if last > self.outputs as usize {
            *self = self.with_outputs(last);
        }
        let (inputs, cols) = (self.inputs as usize, self.outputs as usize + 1);
        for row in 1..=inputs {
            for col in 1..cols {
                self.cells[row * cols + col].db = MIN_VOLUME_DB;
            }
        }
        for row in 1..=inputs {
            match b {
                Some(b) if inputs == 1 => {
                    self.cells[cols + a + 1].db = MONO_CENTER_DB;
                    self.cells[cols + b + 1].db = MONO_CENTER_DB;
                }
                Some(b) => self.cells[row * cols + if row % 2 == 1 { a } else { b } + 1].db = 0.0,
                None => self.cells[row * cols + a + 1].db = if inputs == 1 { 0.0 } else { MONO_CENTER_DB },
            }
        }
    }

    /// La ruta que describe la matriz, si es una de las de [`Self::apply_route`] (`None`: la cambiaron a mano).
    pub fn route(&self, main_channels: usize, extra_outputs: usize) -> Option<OutputRoute> {
        let crosspoint = |m: &Self, row: usize, col: usize| if col <= m.outputs as usize { m.cells[row * (m.outputs as usize + 1) + col].db } else { MIN_VOLUME_DB };
        let width = |m: &Self| m.outputs as usize;
        OutputRoute::all(extra_outputs).into_iter().find(|&r| {
            let mut want = self.clone();
            want.apply_route(r, main_channels);
            let cols = width(self).max(width(&want));
            (1..=self.inputs as usize).all(|row| (1..=cols).all(|col| crosspoint(self, row, col) == crosspoint(&want, row, col)))
        })
    }

    /// Comprueba dimensiones y número de celdas (llamar tras deserializar).
    pub fn validate(&self) -> Result<(), MatrixError> {
        let (m, n) = (self.inputs as usize, self.outputs as usize);
        if !(1..=MAX_INPUT_CHANNELS).contains(&m) || !(1..=MAX_OUTPUT_CHANNELS).contains(&n) {
            return Err(MatrixError::InvalidDimensions { inputs: m, outputs: n });
        }
        let expected = (m + 1) * (n + 1);
        if self.cells.len() != expected {
            return Err(MatrixError::CellCountMismatch { expected, found: self.cells.len() });
        }
        Ok(())
    }

    pub fn cell(&self, row: usize, col: usize) -> Result<&MatrixCell, MatrixError> {
        let i = self.index(row, col)?;
        Ok(&self.cells[i])
    }

    pub fn db(&self, row: usize, col: usize) -> Result<f32, MatrixError> {
        self.cell(row, col).map(|c| c.db)
    }

    /// Fija una sola celda (sin gangs), con clamp.
    pub fn set_db(&mut self, row: usize, col: usize, db: f32) -> Result<(), MatrixError> {
        let i = self.index(row, col)?;
        self.cells[i].db = clamp_db(db);
        Ok(())
    }

    /// Fija la celda y propaga el mismo delta en dB al resto de su gang; cada celda con clamp.
    pub fn set_db_ganged(&mut self, row: usize, col: usize, db: f32) -> Result<(), MatrixError> {
        let i = self.index(row, col)?;
        let new_db = clamp_db(db);
        let Some(gang) = self.cells[i].gang_id else {
            self.cells[i].db = new_db;
            return Ok(());
        };
        let delta = new_db - self.cells[i].db;
        for cell in self.cells.iter_mut().filter(|c| c.gang_id == Some(gang)) {
            cell.db = clamp_db(cell.db + delta);
        }
        self.cells[i].db = new_db;
        Ok(())
    }

    pub fn set_gang(&mut self, row: usize, col: usize, gang: Option<GangId>) -> Result<(), MatrixError> {
        let i = self.index(row, col)?;
        self.cells[i].gang_id = gang;
        Ok(())
    }

    /// Coordenadas `(row, col)` de todas las celdas de un gang.
    pub fn gang_members(&self, gang: GangId) -> impl Iterator<Item = (usize, usize)> + '_ {
        let cols = self.cols();
        self.cells
            .iter()
            .enumerate()
            .filter(move |(_, c)| c.gang_id == Some(gang))
            .map(move |(i, _)| (i / cols, i % cols))
    }

    pub fn set_mute(&mut self, output: usize, muted: bool) -> Result<(), MatrixError> {
        let bit = output_bit(output).ok_or(MatrixError::InvalidOutput(output))?;
        if muted { self.mute_mask |= bit } else { self.mute_mask &= !bit }
        Ok(())
    }

    pub fn set_solo(&mut self, output: usize, soloed: bool) -> Result<(), MatrixError> {
        let bit = output_bit(output).ok_or(MatrixError::InvalidOutput(output))?;
        if soloed { self.solo_mask |= bit } else { self.solo_mask &= !bit }
        Ok(())
    }

    #[inline]
    pub fn gate(&self, output: usize) -> f32 {
        mute_solo_gate(self.mute_mask, self.solo_mask, output)
    }

    /// `mute_solo_gate(j) * g(0,0) * g(0,j) * g(i,0) * g(i,j)`, sin ducking ni fade integrado.
    pub fn base_gain(&self, input: usize, output: usize) -> Result<f32, MatrixError> {
        Ok(self.gate(output)
            * self.cell(0, 0)?.linear()
            * self.cell(0, output)?.linear()
            * self.cell(input, 0)?.linear()
            * self.cell(input, output)?.linear())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_rewrite_the_crosspoints_grow_the_matrix_and_are_recognised_back() {
        let db = |m: &AudioLevelMatrixData, r: usize, c: usize| m.cells[r * (m.outputs as usize + 1) + c].db;
        let mut stereo = AudioLevelMatrixData::new_one_to_one(2, 2).unwrap();
        stereo.cells[0].db = -4.0;
        assert_eq!(stereo.route(2, 1), Some(OutputRoute::default()));
        // Solo la ficha 2 de la principal: el estéreo se suma en mono a -3 dB.
        stereo.apply_route(OutputRoute { output: 0, side: RouteSide::Second }, 2);
        assert_eq!((db(&stereo, 1, 1), db(&stereo, 1, 2), db(&stereo, 2, 2)), (MIN_VOLUME_DB, MONO_CENTER_DB, MONO_CENTER_DB));
        assert_eq!(db(&stereo, 0, 0), -4.0, "el maestro no se toca");
        // A la Salida 2 (columnas 3 y 4 de una principal de dos canales): la matriz crece.
        let to_second = OutputRoute { output: 1, side: RouteSide::Both };
        stereo.apply_route(to_second, 2);
        assert_eq!(stereo.outputs, 4);
        assert!(stereo.validate().is_ok());
        assert_eq!((db(&stereo, 1, 3), db(&stereo, 2, 4), db(&stereo, 1, 1)), (0.0, 0.0, MIN_VOLUME_DB));
        assert_eq!(stereo.route(2, 1), Some(to_second));
        let mut mono = AudioLevelMatrixData::new_one_to_one(1, 2).unwrap();
        assert_eq!(mono.route(2, 0), Some(OutputRoute::default()), "el mono al centro es la ruta de fábrica");
        mono.apply_route(OutputRoute { output: 0, side: RouteSide::First }, 2);
        assert_eq!((db(&mono, 1, 1), db(&mono, 1, 2)), (0.0, MIN_VOLUME_DB));
        mono.cells[5].db = -10.0;
        assert_eq!(mono.route(2, 0), None, "tocada a mano no es ninguna ruta");
        assert_eq!(OutputRoute::all(2).len(), 9);
        assert_eq!(OutputRoute { output: 2, side: RouteSide::First }.label(), "Salida 3 · solo ficha 1");
    }

    #[test]
    fn a_mono_default_goes_to_the_center_and_the_old_one_is_recognised() {
        let m = AudioLevelMatrixData::new_one_to_one(1, 4).unwrap();
        let row1: Vec<f32> = m.cells[5..10].iter().map(|c| c.db).collect();
        assert_eq!(row1, [0.0, MONO_CENTER_DB, MONO_CENTER_DB, MIN_VOLUME_DB, MIN_VOLUME_DB]);
        assert_eq!(AudioLevelMatrixData::new_one_to_one(1, 1).unwrap().cells[3].db, 0.0, "con una sola salida va entera");
        assert_eq!(AudioLevelMatrixData::new_one_to_one(2, 2).unwrap().cells[5].db, MIN_VOLUME_DB, "el estéreo sigue uno-a-uno");
        let mut old = m.clone();
        for (i, db) in [0.0, 0.0, MIN_VOLUME_DB, MIN_VOLUME_DB, MIN_VOLUME_DB].into_iter().enumerate() {
            old.cells[5 + i].db = db;
        }
        assert!(old.is_old_mono_default() && !m.is_old_mono_default());
        old.cells[6].db = -6.0;
        assert!(!old.is_old_mono_default(), "un nivel tocado por el operador se respeta");
    }

    #[test]
    fn db_to_linear_edges() {
        assert_eq!(db_to_linear(-120.0), 0.0);
        assert_eq!(db_to_linear(-200.0), 0.0);
        assert_eq!(db_to_linear(f32::NEG_INFINITY), 0.0);
        assert_eq!(db_to_linear(f32::NAN), 0.0);
        assert!(db_to_linear(-119.9) > 0.0);
        assert_eq!(db_to_linear(0.0), 1.0);
        assert!((db_to_linear(12.0) - 3.981_071_7).abs() < 1e-6);
        assert_eq!(db_to_linear(40.0), db_to_linear(12.0));
        assert!((db_to_linear(-6.0) - 0.501_187_2).abs() < 1e-6);
    }

    #[test]
    fn linear_roundtrip() {
        for db in [-90.0f32, -20.0, -6.0, 0.0, 6.0, 12.0] {
            assert!((linear_to_db(db_to_linear(db)) - db).abs() < 1e-3);
        }
        assert_eq!(linear_to_db(0.0), MIN_VOLUME_DB);
    }

    #[test]
    fn one_to_one_defaults() {
        let m = AudioLevelMatrixData::new_one_to_one(2, 4).unwrap();
        assert_eq!(m.cells.len(), 3 * 5);
        assert_eq!(m.db(1, 1).unwrap(), 0.0);
        assert_eq!(m.db(2, 2).unwrap(), 0.0);
        assert_eq!(m.db(1, 2).unwrap(), MIN_VOLUME_DB);
        assert_eq!(m.db(0, 0).unwrap(), 0.0);
        assert_eq!(m.base_gain(1, 2).unwrap(), 0.0);
        assert_eq!(m.base_gain(2, 2).unwrap(), 1.0);
        assert!(m.cell(3, 0).is_err());
        assert!(AudioLevelMatrixData::new_one_to_one(0, 2).is_err());
        assert!(AudioLevelMatrixData::new_one_to_one(33, 2).is_err());
        assert!(AudioLevelMatrixData::new_one_to_one(2, 65).is_err());
    }

    #[test]
    fn gangs_apply_same_delta_with_clamp() {
        let mut m = AudioLevelMatrixData::new_one_to_one(2, 2).unwrap();
        let g = GangId::new(1).unwrap();
        m.set_db(1, 1, -10.0).unwrap();
        m.set_db(2, 2, 6.0).unwrap();
        m.set_gang(1, 1, Some(g)).unwrap();
        m.set_gang(2, 2, Some(g)).unwrap();
        m.set_db_ganged(1, 1, -4.0).unwrap(); // delta +6
        assert_eq!(m.db(1, 1).unwrap(), -4.0);
        assert_eq!(m.db(2, 2).unwrap(), MAX_VOLUME_DB); // 12.0, recortado
        assert_eq!(m.gang_members(g).collect::<Vec<_>>(), vec![(1, 1), (2, 2)]);
        // fuera de gang: una sola celda
        m.set_db_ganged(1, 2, -3.0).unwrap();
        assert_eq!(m.db(1, 2).unwrap(), -3.0);
    }

    #[test]
    fn mute_solo_masks() {
        assert_eq!(mute_solo_gate(0, 0, 1), 1.0);
        assert_eq!(mute_solo_gate(0b1, 0, 1), 0.0);
        assert_eq!(mute_solo_gate(0, 0b10, 1), 0.0);
        assert_eq!(mute_solo_gate(0, 0b10, 2), 1.0);
        assert_eq!(mute_solo_gate(0b10, 0b10, 2), 0.0); // mute gana
        assert_eq!(mute_solo_gate(1 << 63, 0, 64), 0.0);
        assert_eq!(mute_solo_gate(0, 0, 0), 0.0);
        assert_eq!(mute_solo_gate(0, 0, 65), 0.0);
        let mut m = AudioLevelMatrixData::new_one_to_one(1, 64).unwrap();
        m.set_solo(64, true).unwrap();
        assert_eq!(m.solo_mask, 1 << 63);
        assert!(m.set_mute(65, true).is_err());
    }

    #[test]
    fn json_roundtrip_and_validate() {
        let m = AudioLevelMatrixData::new_one_to_one(2, 2).unwrap();
        let back: AudioLevelMatrixData = serde_json::from_str(&serde_json::to_string(&m).unwrap()).unwrap();
        assert_eq!(m, back);
        let mut bad = back;
        bad.cells.pop();
        assert!(matches!(bad.validate(), Err(MatrixError::CellCountMismatch { .. })));
    }
}
