//! Reproductores independientes de la lista de cues: la botonera de Carts (sale por la sala) y la audición / PFL
//! (sale solo por los auriculares). Cada uno es una voz del motor con un id fuera del rango de las cues y con su propia
//! matriz de niveles; el playhead y el Standby ni se enteran.

use qlab_protocol::matrix::{AudioLevelMatrixData, MatrixCell, MAX_INPUT_CHANNELS, MAX_VOLUME_DB, MIN_VOLUME_DB};

/// Las cues usan ids pequeños y crecientes; los reproductores independientes viven por encima de este.
pub const ADHOC_BASE: u64 = 1 << 48;
/// Ranuras permitidas (16 pads de Carts, la audición y margen).
pub const MAX_SLOTS: u32 = 64;

/// Ranura de la audición (PFL).
pub const AUDITION_SLOT: u32 = 40;

pub fn voice_id(slot: u32) -> u64 {
    ADHOC_BASE + u64::from(slot)
}

/// Matriz de niveles de un archivo de `file_channels` canales hacia un dispositivo de `device_channels`, empezando en el
/// canal `first_out` (0 = sala, o el primer canal del par de auriculares). Un archivo mono suena por los dos lados del par
/// (−3 dB cada uno); uno estéreo o multicanal va uno a uno. `gain_db` es el volumen propio del reproductor.
pub fn levels(file_channels: usize, device_channels: usize, first_out: usize, gain_db: f32) -> AudioLevelMatrixData {
    let inputs = file_channels.clamp(1, MAX_INPUT_CHANNELS);
    let outputs = device_channels.max(1);
    let cols = outputs + 1;
    let mut cells = vec![MatrixCell::new(MIN_VOLUME_DB); (inputs + 1) * cols];
    // Fila 0 y columna 0 son los volúmenes maestro y de entrada: 0 dB, salvo el maestro del reproductor.
    for row in 0..=inputs {
        cells[row * cols] = MatrixCell::new(0.0);
    }
    for col in 0..=outputs {
        cells[col] = MatrixCell::new(0.0);
    }
    cells[0] = MatrixCell::new(gain_db.clamp(MIN_VOLUME_DB, MAX_VOLUME_DB));
    let mut route = |input: usize, out: usize, db: f32| {
        if out < outputs {
            cells[input * cols + out + 1] = MatrixCell::new(db);
        }
    };
    if inputs == 1 {
        route(1, first_out, -3.0);
        route(1, first_out + 1, -3.0);
    } else {
        for i in 1..=inputs {
            route(i, first_out + i - 1, 0.0);
        }
    }
    AudioLevelMatrixData { inputs: inputs as u8, outputs: outputs as u8, cells, mute_mask: 0, solo_mask: 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db(m: &AudioLevelMatrixData, row: usize, col: usize) -> f32 {
        m.cells[row * (m.outputs as usize + 1) + col].db
    }

    #[test]
    fn stereo_goes_one_to_one_to_the_main_pair_or_to_the_headphones() {
        let main = levels(2, 4, 0, -6.0);
        assert!(main.validate().is_ok());
        assert_eq!((db(&main, 0, 0), db(&main, 1, 1), db(&main, 2, 2)), (-6.0, 0.0, 0.0));
        assert_eq!((db(&main, 1, 3), db(&main, 2, 4)), (MIN_VOLUME_DB, MIN_VOLUME_DB), "la sala no llega a los auriculares");
        let pfl = levels(2, 4, 2, 0.0);
        assert!(pfl.validate().is_ok());
        assert_eq!((db(&pfl, 1, 3), db(&pfl, 2, 4)), (0.0, 0.0));
        assert_eq!((db(&pfl, 1, 1), db(&pfl, 2, 2)), (MIN_VOLUME_DB, MIN_VOLUME_DB), "la audición no sale por la sala");
    }

    #[test]
    fn mono_feeds_both_sides_of_the_pair_and_never_overflows_the_device() {
        let m = levels(1, 4, 2, 0.0);
        assert_eq!((db(&m, 1, 3), db(&m, 1, 4)), (-3.0, -3.0));
        // Un par que no cabe se recorta sin romper la matriz.
        let tight = levels(2, 2, 1, 0.0);
        assert!(tight.validate().is_ok());
        assert_eq!(db(&tight, 1, 2), 0.0);
        assert_eq!(voice_id(3), ADHOC_BASE + 3);
    }
}
