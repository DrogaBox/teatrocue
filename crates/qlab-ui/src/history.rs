//! Historial de ediciones de cues para Deshacer/Rehacer (Ctrl+Z / Ctrl+Mayús+Z).
//!
//! Cada guardado confirmado por el daemon guarda el estado anterior y el nuevo de esa cue. Los cambios seguidos sobre
//! la misma cue (arrastrar un fader genera decenas) se funden en uno solo para que un Ctrl+Z deshaga el gesto entero.

use std::time::Duration;

use qlab_protocol::show_format::CueNodeDto;

/// Cambios sobre la misma cue más juntos que esto cuentan como un solo gesto.
pub const COALESCE: Duration = Duration::from_millis(1200);
const MAX_ENTRIES: usize = 200;

#[derive(Clone, Debug, PartialEq)]
struct Entry {
    id: u64,
    before: CueNodeDto,
    after: CueNodeDto,
    at: Duration,
}

#[derive(Default)]
pub struct History {
    undo: Vec<Entry>,
    redo: Vec<Entry>,
}

impl History {
    /// Registra un cambio confirmado. `now` es el tiempo transcurrido desde un origen cualquiera (monótono).
    pub fn record(&mut self, id: u64, before: CueNodeDto, after: CueNodeDto, now: Duration) {
        if before == after {
            return;
        }
        self.redo.clear();
        if let Some(last) = self.undo.last_mut() {
            if last.id == id && now.saturating_sub(last.at) < COALESCE {
                last.after = after;
                last.at = now;
                // Si el gesto volvió al punto de partida no queda nada que deshacer.
                if last.before == last.after {
                    self.undo.pop();
                }
                return;
            }
        }
        self.undo.push(Entry { id, before, after, at: now });
        if self.undo.len() > MAX_ENTRIES {
            self.undo.remove(0);
        }
    }

    /// Siguiente cambio a deshacer: `(cue, estado al que volver)`.
    pub fn undo(&mut self) -> Option<(u64, CueNodeDto)> {
        let e = self.undo.pop()?;
        let out = (e.id, e.before.clone());
        self.redo.push(e);
        Some(out)
    }

    pub fn redo(&mut self) -> Option<(u64, CueNodeDto)> {
        let e = self.redo.pop()?;
        let out = (e.id, e.after.clone());
        self.undo.push(e);
        Some(out)
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// Un show nuevo o recién abierto empieza sin historial.
    pub fn clear(&mut self) {
        self.undo.clear();
        self.redo.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(name: &str) -> CueNodeDto {
        let mut n = CueNodeDto::new_default(qlab_protocol::show_format::NewCueKind::Memo, 1, "1".into());
        n.basics.name = name.into();
        n
    }

    const T: fn(u64) -> Duration = Duration::from_millis;

    #[test]
    fn undo_and_redo_walk_the_edits_in_order() {
        let mut h = History::default();
        h.record(1, named("a"), named("b"), T(0));
        h.record(1, named("b"), named("c"), T(5_000));
        assert_eq!(h.undo().map(|(_, n)| n.basics.name), Some("b".into()));
        assert_eq!(h.undo().map(|(_, n)| n.basics.name), Some("a".into()));
        assert!(h.undo().is_none() && !h.can_undo());
        assert_eq!(h.redo().map(|(_, n)| n.basics.name), Some("b".into()));
        assert_eq!(h.redo().map(|(_, n)| n.basics.name), Some("c".into()));
        assert!(h.redo().is_none());
    }

    #[test]
    fn a_new_edit_discards_the_redo_branch() {
        let mut h = History::default();
        h.record(1, named("a"), named("b"), T(0));
        h.undo();
        assert!(h.can_redo());
        h.record(1, named("a"), named("z"), T(9_000));
        assert!(!h.can_redo());
    }

    #[test]
    fn quick_changes_to_the_same_cue_are_one_gesture_but_other_cues_and_pauses_are_not() {
        let mut h = History::default();
        for (i, (from, to)) in [("a", "b"), ("b", "c"), ("c", "d")].into_iter().enumerate() {
            h.record(1, named(from), named(to), T(100 * i as u64));
        }
        assert_eq!(h.undo().map(|(_, n)| n.basics.name), Some("a".into()), "un solo Ctrl+Z deshace el gesto entero");
        assert!(!h.can_undo());
        h.record(1, named("a"), named("b"), T(0));
        h.record(2, named("x"), named("y"), T(50));
        assert_eq!(h.undo.len(), 2, "otra cue no se funde");
        h.record(2, named("y"), named("w"), T(10_000));
        assert_eq!(h.undo.len(), 3, "tras una pausa tampoco");
    }

    #[test]
    fn a_gesture_that_returns_to_the_start_leaves_nothing_to_undo_and_no_op_edits_are_ignored() {
        let mut h = History::default();
        h.record(1, named("a"), named("b"), T(0));
        h.record(1, named("b"), named("a"), T(100));
        assert!(!h.can_undo());
        h.record(1, named("a"), named("a"), T(5_000));
        assert!(!h.can_undo());
        h.record(1, named("a"), named("b"), T(6_000));
        h.clear();
        assert!(!h.can_undo() && !h.can_redo());
    }

    #[test]
    fn the_history_is_bounded() {
        let mut h = History::default();
        for i in 0..(MAX_ENTRIES as u64 + 50) {
            h.record(i + 1, named("a"), named("b"), T(i * 10_000));
        }
        assert_eq!(h.undo.len(), MAX_ENTRIES);
    }
}
