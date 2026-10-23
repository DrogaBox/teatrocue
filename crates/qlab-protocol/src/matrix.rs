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
    /// Defecto de `AudioCue`: masters y trims a 0 dB, crosspoints `i == j` a 0 dB y el resto a -inf.
    pub fn new_one_to_one(inputs: usize, outputs: usize) -> Result<Self, MatrixError> {
        if !(1..=MAX_INPUT_CHANNELS).contains(&inputs) || !(1..=MAX_OUTPUT_CHANNELS).contains(&outputs) {
            return Err(MatrixError::InvalidDimensions { inputs, outputs });
        }
        let cols = outputs + 1;
        let mut cells = Vec::with_capacity((inputs + 1) * cols);
        for row in 0..=inputs {
            for col in 0..=outputs {
                let db = if row == 0 || col == 0 || row == col { 0.0 } else { MIN_VOLUME_DB };
                cells.push(MatrixCell::new(db));
            }
        }
        Ok(Self { inputs: inputs as u8, outputs: outputs as u8, cells, mute_mask: 0, solo_mask: 0 })
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
