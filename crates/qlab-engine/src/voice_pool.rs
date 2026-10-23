//! `VoicePool`: publicación wait-free de instantáneas de voces hacia el hilo de audio, con
//! retención diferida de 4 generaciones (spec §2.2.2 y §4.2.3).
//!
//! El secuenciador es el único que modifica `master` y publica instantáneas inmutables en un
//! `ArcSwap`. El hilo de audio solo hace `load()`. Cada instantánea sustituida pasa a
//! `retired_snapshots` y solo se libera, en el hilo del secuenciador, cuando es la última
//! referencia (`strong_count == 1`) y hay más de [`VOICE_POOL_RETAIN`] generaciones retenidas. Así
//! el hilo RT nunca ejecuta el `Drop` de un `Vec<Arc<Voice>>` ni de un `Arc<Voice>`.

use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use arc_swap::ArcSwap;

use crate::rt_command::VoiceId;
use crate::voice::{Voice, VoiceState};

/// Generaciones mínimas que se retienen antes de liberar (nombre usado en §2.2).
pub const VOICE_POOL_RETAIN: usize = 4;
const MIN_RETAINED_GENERATIONS: usize = VOICE_POOL_RETAIN;

pub type VoiceSnapshot = Arc<Vec<Arc<Voice>>>;

pub struct VoicePool {
    /// Lista maestra, mutada únicamente en el hilo `qlab-sequencer`.
    master: Vec<Arc<Voice>>,
    /// Instantánea leída por el hilo de audio con `load()`.
    rt_snapshot: Arc<ArcSwap<Vec<Arc<Voice>>>>,
    /// Retiene instantáneas para que el hilo de audio nunca sea el último propietario.
    retired_snapshots: VecDeque<VoiceSnapshot>,
}

impl VoicePool {
    pub fn new(capacity: usize) -> Self {
        let initial = Arc::new(Vec::with_capacity(capacity));
        Self {
            master: Vec::with_capacity(capacity),
            rt_snapshot: Arc::new(ArcSwap::from(initial)),
            retired_snapshots: VecDeque::with_capacity(16),
        }
    }

    /// Asa que se entrega al hilo de audio (ver [`RtVoiceView`]).
    pub fn rt_handle(&self) -> Arc<ArcSwap<Vec<Arc<Voice>>>> {
        Arc::clone(&self.rt_snapshot)
    }

    /// Añade una voz ya construida y publica una nueva instantánea sin bloquear al hilo de audio.
    pub fn insert_voice(&mut self, voice: Arc<Voice>) {
        self.master.push(voice);
        self.publish_snapshot();
    }

    /// Quita una voz de la lista y publica la nueva instantánea. La voz se libera cuando
    /// desaparezca de las instantáneas retenidas.
    pub fn remove_voice(&mut self, id: VoiceId) -> Option<Arc<Voice>> {
        let pos = self.master.iter().position(|v| v.id == id)?;
        let removed = self.master.remove(pos);
        self.publish_snapshot();
        Some(removed)
    }

    pub fn get(&self, id: VoiceId) -> Option<&Arc<Voice>> {
        self.master.iter().find(|v| v.id == id)
    }

    pub fn len(&self) -> usize {
        self.master.len()
    }

    pub fn is_empty(&self) -> bool {
        self.master.is_empty()
    }

    /// Instantáneas retenidas, aún no liberadas.
    pub fn retired_len(&self) -> usize {
        self.retired_snapshots.len()
    }

    /// Llamar periódicamente (p. ej. cada 33 ms) desde el hilo `qlab-sequencer`, nunca desde el RT.
    pub fn gc_stopped_voices(&mut self) {
        let before = self.master.len();
        self.master.retain(|v| v.state.load(Ordering::Acquire) != VoiceState::Stopped as u8);
        if self.master.len() != before {
            self.publish_snapshot();
        }
        self.reclaim_retired();
    }

    /// Libera, en este hilo, las instantáneas antiguas que el hilo de audio ya soltó.
    fn reclaim_retired(&mut self) {
        while self.retired_snapshots.len() > MIN_RETAINED_GENERATIONS {
            match self.retired_snapshots.front() {
                // Cuenta 1 = solo nosotros: tras `swap`, ArcSwap ya cobró las deudas de los lectores
                // y nadie puede obtener una nueva referencia a una instantánea retirada.
                Some(front) if Arc::strong_count(front) == 1 => {
                    let _dropped_on_non_rt_thread = self.retired_snapshots.pop_front();
                }
                _ => break,
            }
        }
    }

    fn publish_snapshot(&mut self) {
        let new_snap = Arc::new(self.master.clone());
        let old_snap = self.rt_snapshot.swap(new_snap);
        self.retired_snapshots.push_back(old_snap);
        self.reclaim_retired();
    }
}

/// Vista del hilo de audio sobre el pool.
///
/// `load()` de `arc-swap` usa estado por hilo que se crea la primera vez que el hilo lo llama
/// (puede asignar). Llamar a [`RtVoiceView::warm_up`] en el hilo de audio antes de entrar en el
/// primer [`crate::alloc_guard::RtScope`].
#[derive(Clone)]
pub struct RtVoiceView {
    handle: Arc<ArcSwap<Vec<Arc<Voice>>>>,
}

impl RtVoiceView {
    pub fn new(handle: Arc<ArcSwap<Vec<Arc<Voice>>>>) -> Self {
        Self { handle }
    }

    pub fn warm_up(&self) {
        drop(self.handle.load());
    }

    /// Ejecuta `f` con la instantánea actual. No asigna ni libera memoria.
    #[inline]
    pub fn with_voices<R>(&self, f: impl FnOnce(&[Arc<Voice>]) -> R) -> R {
        let guard = self.handle.load();
        f(guard.as_slice())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alloc_guard::{assert_no_alloc, set_violation_policy, violations_on_this_thread, ViolationPolicy};
    use crate::voice::test_voice;

    #[test]
    fn insert_publishes_and_get_finds() {
        let mut pool = VoicePool::new(8);
        let view = RtVoiceView::new(pool.rt_handle());
        assert_eq!(view.with_voices(|v| v.len()), 0);
        pool.insert_voice(Arc::new(test_voice(1)));
        pool.insert_voice(Arc::new(test_voice(2)));
        assert_eq!(view.with_voices(|v| v.iter().map(|x| x.id).collect::<Vec<_>>()), vec![1, 2]);
        assert!(pool.get(2).is_some() && pool.get(9).is_none());
        assert!(pool.remove_voice(1).is_some());
        assert_eq!(view.with_voices(|v| v.len()), 1);
    }

    #[test]
    fn gc_removes_only_stopped_voices() {
        let mut pool = VoicePool::new(4);
        let (a, b) = (Arc::new(test_voice(1)), Arc::new(test_voice(2)));
        pool.insert_voice(Arc::clone(&a));
        pool.insert_voice(Arc::clone(&b));
        a.set_state(VoiceState::Stopped);
        b.set_state(VoiceState::Playing);
        pool.gc_stopped_voices();
        assert_eq!(pool.len(), 1);
        assert_eq!(pool.get(2).map(|v| v.id), Some(2));
    }

    #[test]
    fn retains_four_generations_without_readers() {
        let mut pool = VoicePool::new(4);
        for id in 0..20 {
            pool.insert_voice(Arc::new(test_voice(id)));
        }
        assert_eq!(pool.retired_len(), VOICE_POOL_RETAIN);
    }

    #[test]
    fn snapshot_held_by_reader_is_never_freed_by_pool() {
        let mut pool = VoicePool::new(4);
        let handle = pool.rt_handle();
        let held = handle.load_full(); // el "hilo de audio" aún usa esta generación
        for id in 0..10 {
            pool.insert_voice(Arc::new(test_voice(id)));
        }
        // La más antigua sigue en uso: el pool se detiene ahí y acumula el resto.
        assert!(pool.retired_len() > VOICE_POOL_RETAIN);
        assert!(Arc::strong_count(&held) >= 2);
        drop(held);
        pool.gc_stopped_voices();
        assert_eq!(pool.retired_len(), VOICE_POOL_RETAIN);
    }

    #[test]
    fn voice_dropped_off_the_audio_thread() {
        // La última referencia a una voz quitada vive en instantáneas retenidas: el hilo de audio
        // al soltar su lectura no libera nada.
        set_violation_policy(ViolationPolicy::CountOnly);
        let mut pool = VoicePool::new(4);
        let view = RtVoiceView::new(pool.rt_handle());
        view.warm_up();
        pool.insert_voice(Arc::new(test_voice(1)));
        let before = violations_on_this_thread();
        assert_no_alloc(|| {
            for _ in 0..1000 {
                let n = view.with_voices(|voices| voices.iter().filter(|v| v.state() != VoiceState::Stopped).count());
                std::hint::black_box(n);
            }
        });
        assert_eq!(violations_on_this_thread(), before);
    }

    #[test]
    fn concurrent_publish_and_read_never_drops_on_reader() {
        use std::sync::atomic::AtomicBool;
        set_violation_policy(ViolationPolicy::CountOnly);
        let mut pool = VoicePool::new(64);
        let view = RtVoiceView::new(pool.rt_handle());
        let done = AtomicBool::new(false);
        std::thread::scope(|s| {
            let reader = s.spawn(|| {
                view.warm_up();
                let mut reads = 0u64;
                let mut violations = 0;
                while !done.load(Ordering::Acquire) {
                    let before = violations_on_this_thread();
                    assert_no_alloc(|| {
                        view.with_voices(|v| {
                            // Tocar todas las voces: si alguna estuviera liberada, esto fallaría.
                            let _ = v.iter().map(|x| x.id).sum::<u64>();
                        });
                    });
                    violations += violations_on_this_thread() - before;
                    reads += 1;
                }
                (reads, violations)
            });
            for round in 0..2000u64 {
                pool.insert_voice(Arc::new(test_voice(round)));
                if round % 3 == 0 {
                    pool.remove_voice(round);
                }
                if round % 50 == 0 {
                    pool.gc_stopped_voices();
                }
            }
            done.store(true, Ordering::Release);
            let (reads, violations) = reader.join().unwrap();
            assert!(reads > 0);
            assert_eq!(violations, 0, "el hilo de lectura asignó o liberó memoria");
        });
    }
}
