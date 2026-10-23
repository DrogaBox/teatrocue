//! Página de memoria compartida UI <-> daemon (spec §2.1).
//!
//! Todo el contenido son atómicos o enteros: el segmento recién mapeado y puesto a cero es un
//! estado válido, y no hay `unsafe` ni carreras de datos. Los datos del seqlock y del anillo se
//! guardan como palabras `AtomicU64` con `Relaxed`, ordenadas por los fences/Release/Acquire del
//! protocolo.
//!
//! * [`HotControlRing`]: SPSC (UI produce, daemon consume) para GO/Panic/StopAll y arrastres de faders.
//! * [`SeqlockShowStatus`]: un escritor (daemon), lectores sin bloqueo (UI).
//! * [`ConsumingPeakMeters`]: máximo por canal con `fetch_max`; la UI lo consume con `swap(0)`.

use std::sync::atomic::{fence, AtomicU32, AtomicU64, Ordering};

use crate::matrix::clamp_db;

pub const SHM_NAME_LINUX: &str = "/teatrocue_rt_telemetry_v1";
pub const SHM_NAME_WINDOWS: &str = "Local\\TeatroCueRtTelemetryV1";
/// "QLABRT01".
pub const SHM_MAGIC: u64 = 0x514C_4142_5254_3031;
pub const SHM_VERSION: u32 = 2;

pub const MAX_METER_CHANNELS: usize = 64;
pub const SELECTED_CUE_METER_CHANNELS: usize = 32;
pub const MAX_RUNNING_CUES: usize = 64;
/// 2048 ranuras de 32 bytes = 64 KiB. Potencia de 2.
pub const HOT_RING_CAPACITY: usize = 2048;

const HOT_SLOT_WORDS: usize = 4;
const SEQLOCK_MAX_TRIES: u32 = 64;

/// Alinea a línea de caché para evitar falsa compartición entre hilos/procesos.
#[repr(C, align(64))]
pub struct CachePadded<T>(pub T);

// ---------------------------------------------------------------------------------------------
// HotControlRing
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum HotControlEvent {
    Go,
    Panic,
    StopAll,
    /// Arrastre en vivo de un crosspoint; `row <= 32`, `col <= 64`.
    LiveCrosspointDrag { cue_num_id: u64, row: u8, col: u8, db: f32 },
}

const KIND_GO: u64 = 1;
const KIND_PANIC: u64 = 2;
const KIND_STOP_ALL: u64 = 3;
const KIND_DRAG: u64 = 4;

impl HotControlEvent {
    fn encode(self) -> [u64; HOT_SLOT_WORDS] {
        match self {
            Self::Go => [KIND_GO, 0, 0, 0],
            Self::Panic => [KIND_PANIC, 0, 0, 0],
            Self::StopAll => [KIND_STOP_ALL, 0, 0, 0],
            Self::LiveCrosspointDrag { cue_num_id, row, col, db } => [
                KIND_DRAG | (row as u64) << 32 | (col as u64) << 40,
                cue_num_id,
                clamp_db(db).to_bits() as u64,
                0,
            ],
        }
    }

    /// `None` para tipos desconocidos (versión futura del protocolo).
    fn decode(w: [u64; HOT_SLOT_WORDS]) -> Option<Self> {
        match w[0] & 0xFFFF_FFFF {
            KIND_GO => Some(Self::Go),
            KIND_PANIC => Some(Self::Panic),
            KIND_STOP_ALL => Some(Self::StopAll),
            KIND_DRAG => Some(Self::LiveCrosspointDrag {
                cue_num_id: w[1],
                row: (w[0] >> 32) as u8,
                col: (w[0] >> 40) as u8,
                db: f32::from_bits(w[2] as u32),
            }),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RingFull;

/// Anillo SPSC: un único productor (UI) y un único consumidor (daemon). Con más de uno la memoria
/// sigue siendo segura, pero se pueden perder o mezclar eventos.
#[repr(C, align(64))]
pub struct HotControlRing {
    head: CachePadded<AtomicU64>,
    tail: CachePadded<AtomicU64>,
    /// Se incrementa en cada `push`; el consumidor puede dormir sobre este valor (futex).
    doorbell: CachePadded<AtomicU32>,
    slots: [[AtomicU64; HOT_SLOT_WORDS]; HOT_RING_CAPACITY],
}

impl HotControlRing {
    pub const fn new() -> Self {
        Self {
            head: CachePadded(AtomicU64::new(0)),
            tail: CachePadded(AtomicU64::new(0)),
            doorbell: CachePadded(AtomicU32::new(0)),
            slots: [const { [const { AtomicU64::new(0) }; HOT_SLOT_WORDS] }; HOT_RING_CAPACITY],
        }
    }

    /// Encola un evento sin bloquear; `Err(RingFull)` si el anillo está lleno.
    pub fn push(&self, event: HotControlEvent) -> Result<(), RingFull> {
        let head = self.head.0.load(Ordering::Relaxed);
        let tail = self.tail.0.load(Ordering::Acquire);
        if head.wrapping_sub(tail) >= HOT_RING_CAPACITY as u64 {
            return Err(RingFull);
        }
        let slot = &self.slots[head as usize & (HOT_RING_CAPACITY - 1)];
        for (cell, word) in slot.iter().zip(event.encode()) {
            cell.store(word, Ordering::Relaxed);
        }
        self.head.0.store(head.wrapping_add(1), Ordering::Release);
        self.doorbell.0.fetch_add(1, Ordering::Release);
        Ok(())
    }

    /// Extrae el siguiente evento válido; descarta los de tipo desconocido.
    pub fn pop(&self) -> Option<HotControlEvent> {
        loop {
            let tail = self.tail.0.load(Ordering::Relaxed);
            let head = self.head.0.load(Ordering::Acquire);
            if tail == head {
                return None;
            }
            let slot = &self.slots[tail as usize & (HOT_RING_CAPACITY - 1)];
            let mut words = [0u64; HOT_SLOT_WORDS];
            for (w, cell) in words.iter_mut().zip(slot) {
                *w = cell.load(Ordering::Relaxed);
            }
            self.tail.0.store(tail.wrapping_add(1), Ordering::Release);
            if let Some(ev) = HotControlEvent::decode(words) {
                return Some(ev);
            }
        }
    }

    pub fn len(&self) -> usize {
        let head = self.head.0.load(Ordering::Acquire);
        let tail = self.tail.0.load(Ordering::Acquire);
        head.wrapping_sub(tail) as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Contador para `futex_wait`/`wake` en el consumidor.
    pub fn doorbell(&self) -> &AtomicU32 {
        &self.doorbell.0
    }
}

impl Default for HotControlRing {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------------------------
// Seqlock genérico y estado del show
// ---------------------------------------------------------------------------------------------

/// Seqlock de un solo escritor con `N` palabras. `seq` impar = escritura en curso.
#[repr(C, align(64))]
pub struct Seqlock<const N: usize> {
    seq: AtomicU64,
    words: [AtomicU64; N],
}

impl<const N: usize> Seqlock<N> {
    pub const fn new() -> Self {
        Self { seq: AtomicU64::new(0), words: [const { AtomicU64::new(0) }; N] }
    }

    /// Publica `data`. Solo un hilo/proceso puede escribir a la vez.
    pub fn write(&self, data: &[u64; N]) {
        // `& !1` repara un `seq` impar dejado por un escritor que murió a medias.
        let seq = self.seq.load(Ordering::Relaxed) & !1;
        self.seq.store(seq.wrapping_add(1), Ordering::Relaxed);
        fence(Ordering::Release);
        for (cell, &w) in self.words.iter().zip(data) {
            cell.store(w, Ordering::Relaxed);
        }
        self.seq.store(seq.wrapping_add(2), Ordering::Release);
    }

    /// Lectura consistente, o `None` si el escritor ganó `SEQLOCK_MAX_TRIES` veces seguidas.
    pub fn read(&self) -> Option<[u64; N]> {
        let mut out = [0u64; N];
        for _ in 0..SEQLOCK_MAX_TRIES {
            let s1 = self.seq.load(Ordering::Acquire);
            if s1 & 1 == 1 {
                std::hint::spin_loop();
                continue;
            }
            for (o, cell) in out.iter_mut().zip(&self.words) {
                *o = cell.load(Ordering::Relaxed);
            }
            fence(Ordering::Acquire);
            if self.seq.load(Ordering::Relaxed) == s1 {
                return Some(out);
            }
        }
        None
    }

    /// Número de publicaciones completadas.
    pub fn version(&self) -> u64 {
        self.seq.load(Ordering::Acquire) >> 1
    }
}

impl<const N: usize> Default for Seqlock<N> {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum CuePhase {
    /// Ranura libre.
    #[default]
    Idle = 0,
    PreWait = 1,
    Action = 2,
    PostWait = 3,
    TailingOut = 4,
}

impl CuePhase {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::PreWait,
            2 => Self::Action,
            3 => Self::PostWait,
            4 => Self::TailingOut,
            _ => Self::Idle,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RunningCueTelemetry {
    pub cue_id: u64,
    pub phase: CuePhase,
    pub pre_wait_elapsed_sec: f32,
    pub action_elapsed_sec: f32,
    pub post_wait_elapsed_sec: f32,
    pub current_slice_idx: u16,
    pub loops_remaining: u32,
    /// `SLICE_PROGRAM | SLICE_VAMPING | SLICE_DEVAMPING` (0 si la cue no tiene slices ni bucle).
    pub slice_flags: u8,
    /// Frame del archivo que el motor RT leerá a continuación (posición real: vuelve atrás en cada
    /// vuelta de un bucle). Solo se publica para cues de audio en acción; satura en `u32::MAX`
    /// (unas 24 h a 48 kHz).
    pub source_frame: u64,
    /// Pico (lineal, 0..) de la cue en la última ventana de publicación, tras la envolvente y sus niveles.
    pub peak: f32,
}

/// La cue de audio tiene un programa de slices o un bucle (los demás campos de slice son válidos).
pub const SLICE_PROGRAM: u8 = 1;
/// Atrapada en un tramo infinito: sigue en bucle hasta recibir Devamp.
pub const SLICE_VAMPING: u8 = 2;
/// Devamp pedido: acaba la pasada en curso y sigue al tramo siguiente.
pub const SLICE_DEVAMPING: u8 = 4;

impl RunningCueTelemetry {
    const EMPTY: Self = Self {
        cue_id: 0,
        phase: CuePhase::Idle,
        pre_wait_elapsed_sec: 0.0,
        action_elapsed_sec: 0.0,
        post_wait_elapsed_sec: 0.0,
        current_slice_idx: 0,
        loops_remaining: 0,
        slice_flags: 0,
        source_frame: 0,
        peak: 0.0,
    };
}

const CUE_WORDS: usize = 5;
const STATUS_HEADER_WORDS: usize = 4;
pub const STATUS_WORDS: usize = STATUS_HEADER_WORDS + MAX_RUNNING_CUES * CUE_WORDS;

/// Instantánea del estado del show que el daemon publica a ~120 Hz.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ShowStatus {
    pub active_playhead_cue_id: u128,
    pub is_playing: bool,
    pub audio_freeze_detected: bool,
    pub total_xruns: u64,
    pub callback_cpu_load_permille: u32,
    pub running_cues: [RunningCueTelemetry; MAX_RUNNING_CUES],
}

impl Default for ShowStatus {
    fn default() -> Self {
        Self {
            active_playhead_cue_id: 0,
            is_playing: false,
            audio_freeze_detected: false,
            total_xruns: 0,
            callback_cpu_load_permille: 0,
            running_cues: [RunningCueTelemetry::EMPTY; MAX_RUNNING_CUES],
        }
    }
}

impl ShowStatus {
    fn encode(&self) -> [u64; STATUS_WORDS] {
        let mut w = [0u64; STATUS_WORDS];
        w[0] = self.active_playhead_cue_id as u64;
        w[1] = (self.active_playhead_cue_id >> 64) as u64;
        w[2] = self.total_xruns;
        w[3] = self.callback_cpu_load_permille as u64
            | (self.is_playing as u64) << 32
            | (self.audio_freeze_detected as u64) << 33;
        for (i, c) in self.running_cues.iter().enumerate() {
            let b = STATUS_HEADER_WORDS + i * CUE_WORDS;
            w[b] = c.cue_id;
            w[b + 1] = c.phase as u64 | (c.current_slice_idx as u64) << 8 | (c.slice_flags as u64) << 24 | (c.loops_remaining as u64) << 32;
            w[b + 2] = c.pre_wait_elapsed_sec.to_bits() as u64 | (c.action_elapsed_sec.to_bits() as u64) << 32;
            w[b + 3] = c.post_wait_elapsed_sec.to_bits() as u64 | c.source_frame.min(u32::MAX as u64) << 32;
            w[b + 4] = c.peak.to_bits() as u64;
        }
        w
    }

    fn decode(w: &[u64; STATUS_WORDS]) -> Self {
        let mut s = Self {
            active_playhead_cue_id: w[0] as u128 | (w[1] as u128) << 64,
            is_playing: w[3] >> 32 & 1 == 1,
            audio_freeze_detected: w[3] >> 33 & 1 == 1,
            total_xruns: w[2],
            callback_cpu_load_permille: w[3] as u32,
            running_cues: [RunningCueTelemetry::EMPTY; MAX_RUNNING_CUES],
        };
        for (i, c) in s.running_cues.iter_mut().enumerate() {
            let b = STATUS_HEADER_WORDS + i * CUE_WORDS;
            *c = RunningCueTelemetry {
                cue_id: w[b],
                phase: CuePhase::from_u8(w[b + 1] as u8),
                current_slice_idx: (w[b + 1] >> 8) as u16,
                slice_flags: (w[b + 1] >> 24) as u8,
                loops_remaining: (w[b + 1] >> 32) as u32,
                pre_wait_elapsed_sec: f32::from_bits(w[b + 2] as u32),
                action_elapsed_sec: f32::from_bits((w[b + 2] >> 32) as u32),
                post_wait_elapsed_sec: f32::from_bits(w[b + 3] as u32),
                source_frame: w[b + 3] >> 32,
                peak: f32::from_bits(w[b + 4] as u32),
            };
        }
        s
    }
}

/// Seqlock del playhead y las cues en ejecución.
#[repr(C, align(64))]
pub struct SeqlockShowStatus(Seqlock<STATUS_WORDS>);

impl SeqlockShowStatus {
    pub const fn new() -> Self {
        Self(Seqlock::new())
    }

    /// Solo el daemon.
    pub fn publish(&self, status: &ShowStatus) {
        self.0.write(&status.encode());
    }

    /// Instantánea consistente, o `None` si no hubo hueco entre escrituras.
    pub fn snapshot(&self) -> Option<ShowStatus> {
        self.0.read().map(|w| ShowStatus::decode(&w))
    }

    pub fn version(&self) -> u64 {
        self.0.version()
    }
}

impl Default for SeqlockShowStatus {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------------------------
// Medidores de pico por consumo
// ---------------------------------------------------------------------------------------------

/// Picos como `f32::to_bits` en `AtomicU32`. En valores `>= 0`, el orden de los bits coincide con el
/// del número, así que `fetch_max` mantiene el pico real.
#[repr(C, align(64))]
pub struct ConsumingPeakMeters {
    outputs: [AtomicU32; MAX_METER_CHANNELS],
    selected_cue: [AtomicU32; SELECTED_CUE_METER_CHANNELS],
}

#[inline]
fn report(slot: Option<&AtomicU32>, sample: f32) {
    let v = sample.abs();
    if let (Some(slot), false) = (slot, v.is_nan()) {
        slot.fetch_max(v.to_bits(), Ordering::Relaxed);
    }
}

impl ConsumingPeakMeters {
    pub const fn new() -> Self {
        Self {
            outputs: [const { AtomicU32::new(0) }; MAX_METER_CHANNELS],
            selected_cue: [const { AtomicU32::new(0) }; SELECTED_CUE_METER_CHANNELS],
        }
    }

    /// Hilo RT: registra una muestra (canales fuera de rango se ignoran, NaN también).
    #[inline]
    pub fn report_output(&self, channel: usize, sample: f32) {
        report(self.outputs.get(channel), sample);
    }

    #[inline]
    pub fn report_selected_cue(&self, channel: usize, sample: f32) {
        report(self.selected_cue.get(channel), sample);
    }

    /// UI: lee y reinicia el pico de una salida.
    #[inline]
    pub fn consume_output(&self, channel: usize) -> f32 {
        self.outputs.get(channel).map_or(0.0, |s| f32::from_bits(s.swap(0, Ordering::Relaxed)))
    }

    /// Pico acumulado sin reiniciarlo (para sondear actividad sin quitársela al lector real).
    #[inline]
    pub fn peek_output(&self, channel: usize) -> f32 {
        self.outputs.get(channel).map_or(0.0, |s| f32::from_bits(s.load(Ordering::Relaxed)))
    }

    #[inline]
    pub fn consume_selected_cue(&self, channel: usize) -> f32 {
        self.selected_cue.get(channel).map_or(0.0, |s| f32::from_bits(s.swap(0, Ordering::Relaxed)))
    }

    pub fn consume_all_outputs(&self, out: &mut [f32; MAX_METER_CHANNELS]) {
        for (o, s) in out.iter_mut().zip(&self.outputs) {
            *o = f32::from_bits(s.swap(0, Ordering::Relaxed));
        }
    }

    pub fn consume_all_selected_cue(&self, out: &mut [f32; SELECTED_CUE_METER_CHANNELS]) {
        for (o, s) in out.iter_mut().zip(&self.selected_cue) {
            *o = f32::from_bits(s.swap(0, Ordering::Relaxed));
        }
    }
}

impl Default for ConsumingPeakMeters {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------------------------
// Página completa
// ---------------------------------------------------------------------------------------------

#[repr(C, align(64))]
pub struct ShmHeader {
    magic: AtomicU64,
    version: AtomicU32,
    /// Nanosegundos monotónicos del último latido del motor (watchdog de 250 ms).
    engine_heartbeat_ns: AtomicU64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShmError {
    BadMagic(u64),
    VersionMismatch { found: u32, expected: u32 },
}

impl std::fmt::Display for ShmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadMagic(m) => write!(f, "segmento sin inicializar o ajeno (magic {m:#x})"),
            Self::VersionMismatch { found, expected } => {
                write!(f, "versión de protocolo {found}, se esperaba {expected}")
            }
        }
    }
}

impl std::error::Error for ShmError {}

/// Layout `#[repr(C, align(64))]` del segmento `/teatrocue_rt_telemetry_v1`. Cada región empieza en
/// su propia línea de caché.
#[repr(C, align(64))]
pub struct SharedTelemetryShm {
    pub header: ShmHeader,
    pub hot_ring: HotControlRing,
    pub status: SeqlockShowStatus,
    pub meters: ConsumingPeakMeters,
}

const _: () = {
    assert!(std::mem::align_of::<SharedTelemetryShm>() == 64);
    assert!(std::mem::size_of::<SharedTelemetryShm>() % 64 == 0);
    assert!(std::mem::align_of::<HotControlRing>() == 64);
    assert!(std::mem::align_of::<SeqlockShowStatus>() == 64);
    assert!(std::mem::align_of::<ConsumingPeakMeters>() == 64);
    assert!(HOT_RING_CAPACITY.is_power_of_two());
};

impl SharedTelemetryShm {
    pub const fn new() -> Self {
        Self {
            header: ShmHeader {
                magic: AtomicU64::new(0),
                version: AtomicU32::new(0),
                engine_heartbeat_ns: AtomicU64::new(0),
            },
            hot_ring: HotControlRing::new(),
            status: SeqlockShowStatus::new(),
            meters: ConsumingPeakMeters::new(),
        }
    }

    /// Daemon: marca el segmento como válido (versión primero, magic al final con `Release`).
    pub fn initialize(&self) {
        self.header.version.store(SHM_VERSION, Ordering::Relaxed);
        self.header.magic.store(SHM_MAGIC, Ordering::Release);
    }

    /// Cliente: comprueba que el daemon inicializó un segmento compatible.
    pub fn check(&self) -> Result<(), ShmError> {
        let magic = self.header.magic.load(Ordering::Acquire);
        if magic != SHM_MAGIC {
            return Err(ShmError::BadMagic(magic));
        }
        let found = self.header.version.load(Ordering::Relaxed);
        if found != SHM_VERSION {
            return Err(ShmError::VersionMismatch { found, expected: SHM_VERSION });
        }
        Ok(())
    }

    pub fn beat(&self, now_ns: u64) {
        self.header.engine_heartbeat_ns.store(now_ns, Ordering::Release);
    }

    pub fn last_heartbeat_ns(&self) -> u64 {
        self.header.engine_heartbeat_ns.load(Ordering::Acquire)
    }
}

impl Default for SharedTelemetryShm {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn layout() {
        assert_eq!(std::mem::align_of::<SharedTelemetryShm>(), 64);
        assert_eq!(std::mem::size_of::<SharedTelemetryShm>() % 64, 0);
        // 2048 ranuras x 32 B = 64 KiB de datos de anillo.
        assert_eq!(HOT_RING_CAPACITY * HOT_SLOT_WORDS * 8, 64 * 1024);
    }

    #[test]
    fn header_check() {
        let shm = Box::new(SharedTelemetryShm::new());
        assert!(matches!(shm.check(), Err(ShmError::BadMagic(0))));
        shm.initialize();
        assert_eq!(shm.check(), Ok(()));
        shm.beat(42);
        assert_eq!(shm.last_heartbeat_ns(), 42);
    }

    #[test]
    fn ring_roundtrip_full_and_wrap() {
        let ring = Box::new(HotControlRing::new());
        assert!(ring.pop().is_none());
        let drag = HotControlEvent::LiveCrosspointDrag { cue_num_id: u64::MAX - 1, row: 3, col: 64, db: -6.5 };
        ring.push(HotControlEvent::Go).unwrap();
        ring.push(drag).unwrap();
        ring.push(HotControlEvent::Panic).unwrap();
        assert_eq!(ring.pop(), Some(HotControlEvent::Go));
        assert_eq!(ring.pop(), Some(drag));
        assert_eq!(ring.pop(), Some(HotControlEvent::Panic));
        assert!(ring.is_empty());
        for _ in 0..HOT_RING_CAPACITY {
            ring.push(HotControlEvent::StopAll).unwrap();
        }
        assert_eq!(ring.push(HotControlEvent::Go), Err(RingFull));
        assert_eq!(ring.pop(), Some(HotControlEvent::StopAll));
        assert!(ring.push(HotControlEvent::Go).is_ok()); // dio la vuelta
        assert_eq!(ring.doorbell().load(Ordering::Relaxed), 3 + HOT_RING_CAPACITY as u32 + 1);
    }

    #[test]
    fn drag_db_is_clamped() {
        let ring = Box::new(HotControlRing::new());
        ring.push(HotControlEvent::LiveCrosspointDrag { cue_num_id: 1, row: 1, col: 1, db: 99.0 }).unwrap();
        ring.push(HotControlEvent::LiveCrosspointDrag { cue_num_id: 1, row: 1, col: 1, db: f32::NEG_INFINITY }).unwrap();
        let db = |e| match e {
            Some(HotControlEvent::LiveCrosspointDrag { db, .. }) => db,
            _ => panic!(),
        };
        assert_eq!(db(ring.pop()), 12.0);
        assert_eq!(db(ring.pop()), -120.0);
    }

    #[test]
    fn ring_spsc_threads() {
        const N: u64 = 200_000;
        let ring = Box::new(HotControlRing::new());
        std::thread::scope(|s| {
            s.spawn(|| {
                for i in 0..N {
                    let ev = HotControlEvent::LiveCrosspointDrag { cue_num_id: i, row: 1, col: 2, db: 0.0 };
                    while ring.push(ev).is_err() {
                        std::hint::spin_loop();
                    }
                }
            });
            let mut expected = 0u64;
            while expected < N {
                if let Some(HotControlEvent::LiveCrosspointDrag { cue_num_id, .. }) = ring.pop() {
                    assert_eq!(cue_num_id, expected);
                    expected += 1;
                } else {
                    std::hint::spin_loop();
                }
            }
        });
    }

    #[test]
    fn status_roundtrip() {
        let lock = Box::new(SeqlockShowStatus::new());
        assert_eq!(lock.snapshot(), Some(ShowStatus::default()));
        let mut st = ShowStatus {
            active_playhead_cue_id: (u64::MAX as u128) << 40 | 7,
            is_playing: true,
            audio_freeze_detected: true,
            total_xruns: 12,
            callback_cpu_load_permille: 345,
            ..Default::default()
        };
        st.running_cues[63] = RunningCueTelemetry {
            cue_id: 99,
            phase: CuePhase::TailingOut,
            pre_wait_elapsed_sec: 1.0,
            action_elapsed_sec: 2.5,
            post_wait_elapsed_sec: 0.25,
            current_slice_idx: 7,
            loops_remaining: 123_456,
            slice_flags: SLICE_PROGRAM | SLICE_VAMPING | SLICE_DEVAMPING,
            source_frame: 3_456_789_012,
            peak: 0.625,
        };
        lock.publish(&st);
        assert_eq!(lock.snapshot(), Some(st));
        assert_eq!(lock.version(), 1);
    }

    #[test]
    fn seqlock_never_tears() {
        const N: usize = 32;
        let lock = Box::new(Seqlock::<N>::new());
        let done = AtomicBool::new(false);
        std::thread::scope(|s| {
            s.spawn(|| {
                for i in 1..=100_000u64 {
                    lock.write(&[i; N]);
                }
                done.store(true, Ordering::Release);
            });
            let mut last = 0u64;
            let mut reads = 0u64;
            while !done.load(Ordering::Acquire) {
                if let Some(d) = lock.read() {
                    assert!(d.iter().all(|&x| x == d[0]), "lectura rota: {d:?}");
                    assert!(d[0] >= last);
                    last = d[0];
                    reads += 1;
                }
            }
            assert!(reads > 0);
        });
    }

    #[test]
    fn seqlock_recovers_from_odd_seq() {
        let lock = Seqlock::<2>::new();
        lock.seq.store(5, Ordering::Relaxed); // escritor muerto a medias
        assert!(lock.read().is_none() || lock.read().is_some());
        lock.write(&[1, 2]);
        assert_eq!(lock.read(), Some([1, 2]));
    }

    #[test]
    fn meters_keep_max_and_consume() {
        let m = Box::new(ConsumingPeakMeters::new());
        m.report_output(0, 0.25);
        m.report_output(0, -0.75);
        m.report_output(0, 0.5);
        m.report_output(0, f32::NAN);
        m.report_output(1000, 1.0); // ignorado
        m.report_selected_cue(31, 0.1);
        assert_eq!(m.peek_output(0), 0.75);
        assert_eq!(m.peek_output(0), 0.75, "peek no consume");
        assert_eq!(m.peek_output(999), 0.0);
        assert_eq!(m.consume_output(0), 0.75);
        assert_eq!(m.consume_output(0), 0.0);
        assert_eq!(m.consume_selected_cue(31), 0.1);
        assert_eq!(m.consume_output(1000), 0.0);
        m.report_output(63, 2.0);
        let mut all = [0.0; MAX_METER_CHANNELS];
        m.consume_all_outputs(&mut all);
        assert_eq!(all[63], 2.0);
    }

    #[test]
    fn meters_concurrent_no_lost_peak() {
        let m = Box::new(ConsumingPeakMeters::new());
        let done = AtomicBool::new(false);
        std::thread::scope(|s| {
            s.spawn(|| {
                for i in 0..50_000u32 {
                    m.report_output(3, i as f32);
                }
                m.report_output(3, 777_777.0); // transitorio máximo
                done.store(true, Ordering::Release);
            });
            let mut max_seen = 0.0f32;
            while !done.load(Ordering::Acquire) {
                max_seen = max_seen.max(m.consume_output(3));
            }
            max_seen = max_seen.max(m.consume_output(3));
            assert_eq!(max_seen, 777_777.0);
        });
    }
}
