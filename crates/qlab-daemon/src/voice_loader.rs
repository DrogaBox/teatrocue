//! Pre-carga de voces: convierte una cue de audio en una `Voice` del motor y la publica en el
//! `VoicePool`. Corre en hilos que **no** son de tiempo real (carga del show, IPC).
//!
//! Las voces se mantienen cargadas mientras dure el show: el motor las reutiliza (una voz terminada
//! queda en `Stopped` y `Play` la rebobina), así que no hay que recargar para repetir una cue.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use arc_swap::ArcSwap;
use qlab_core::cue::AudioCueData;
use qlab_engine::disk_streamer::{DiskStreamerPool, FileInfo, StreamConfig, StreamError};
use qlab_engine::rt_command::{MAX_INPUT_CHANNELS, MAX_OUTPUT_CHANNELS, MAX_SLICES_PER_VOICE};
use qlab_engine::voice::{AudioLevelMatrixRt, SliceSegment, Voice, VoiceError, VoiceInit, VoiceRtState};
use qlab_engine::voice_pool::VoicePool;
use qlab_protocol::cue_dto::{AudioTimeAndLoopsData, CueBasicsData, CueId, INFINITE_SLICE_LOOP};
use qlab_protocol::matrix::AudioLevelMatrixData;

#[derive(Debug)]
pub enum LoadError {
    NoFileTarget,
    Stream(StreamError),
    /// El contenedor no declara la duración y la cue no fija un final.
    UnknownLength,
    BadRegion { start: u64, end: u64 },
    BadSlices(String),
    Voice(VoiceError),
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoFileTarget => write!(f, "la cue no tiene archivo de audio"),
            Self::Stream(e) => write!(f, "{e}"),
            Self::UnknownLength => write!(f, "el archivo no declara su duración y la cue no define un final"),
            Self::BadRegion { start, end } => write!(f, "región vacía: inicio {start} >= final {end}"),
            Self::BadSlices(s) => write!(f, "slices inválidos: {s}"),
            Self::Voice(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for LoadError {}

impl From<StreamError> for LoadError {
    fn from(e: StreamError) -> Self {
        Self::Stream(e)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoadedInfo {
    pub file: FileInfo,
    pub start_frame: u64,
    pub end_frame: u64,
    pub segments: usize,
}

pub struct VoiceLoader {
    pool: VoicePool,
    disk: DiskStreamerPool,
    device_channels: usize,
    loaded: HashMap<CueId, LoadedInfo>,
}

impl VoiceLoader {
    pub fn new(disk_threads: usize, device_channels: usize) -> Self {
        Self {
            pool: VoicePool::new(64),
            disk: DiskStreamerPool::new(disk_threads),
            device_channels: device_channels.clamp(1, MAX_OUTPUT_CHANNELS),
            loaded: HashMap::new(),
        }
    }

    /// Asa de lectura que usa el callback de audio.
    pub fn rt_handle(&self) -> Arc<ArcSwap<Vec<Arc<Voice>>>> {
        self.pool.rt_handle()
    }

    pub fn device_channels(&self) -> usize {
        self.device_channels
    }

    pub fn loaded_count(&self) -> usize {
        self.loaded.len()
    }

    pub fn info(&self, id: CueId) -> Option<&LoadedInfo> {
        self.loaded.get(&id)
    }

    pub fn voice(&self, id: CueId) -> Option<&Arc<Voice>> {
        self.pool.get(id.0)
    }

    pub fn probe(path: &Path) -> Result<FileInfo, StreamError> {
        DiskStreamerPool::probe(path)
    }

    /// Carga (o recarga) la voz de una cue de audio. El `id` de la voz es el de la cue.
    pub fn load_audio(&mut self, id: CueId, basics: &CueBasicsData, data: &AudioCueData) -> Result<LoadedInfo, LoadError> {
        let path = basics.file_target.as_ref().map(|t| t.absolute_path.as_str()).filter(|p| !p.is_empty()).ok_or(LoadError::NoFileTarget)?;
        self.load_path(id, Path::new(path), &data.time_loops, Some(&data.levels))
    }

    pub fn load_path(
        &mut self,
        id: CueId,
        path: &Path,
        time: &AudioTimeAndLoopsData,
        levels: Option<&AudioLevelMatrixData>,
    ) -> Result<LoadedInfo, LoadError> {
        let view = time.playback_view();
        let time = &*view;
        let probe = DiskStreamerPool::probe(path)?;
        let sr = probe.sample_rate;
        let secs = |s: f64| (s.max(0.0) * sr as f64).round() as u64;
        let start = secs(time.start_time_sec);
        let end = match (time.end_time_sec > 0.0, probe.total_frames) {
            (true, Some(total)) => secs(time.end_time_sec).min(total),
            (true, None) => secs(time.end_time_sec),
            (false, Some(total)) => total,
            (false, None) => return Err(LoadError::UnknownLength),
        };
        if start >= end {
            return Err(LoadError::BadRegion { start, end });
        }
        let segments = compile_segments(time, start, end)?;
        let extra_pinned: Vec<u64> = segments.iter().filter(|s| s.is_infinite() || s.play_count > 1).map(|s| s.start_frame).collect();

        let (reader, info) = self.disk.open(
            path,
            &StreamConfig { start_frame: start, extra_pinned_starts: extra_pinned, ..Default::default() },
        )?;
        let matrix = build_matrix(info.channels, self.device_channels, levels);
        let rt = VoiceRtState::new(start, &segments, matrix).map_err(LoadError::Voice)?;
        let voice = Voice::with_reader(
            VoiceInit {
                id: id.0,
                cue_unique_id: id.0 as u128,
                patch_id: 0,
                file_sample_rate: info.sample_rate,
                start_frame: start,
                end_frame: end,
                infinite_loop: time.infinite_loop && !time.slices.is_empty(),
                rate: time.rate,
            },
            rt,
            reader,
        );
        self.pool.remove_voice(id.0);
        self.pool.insert_voice(Arc::new(voice));
        let loaded = LoadedInfo { file: info, start_frame: start, end_frame: end, segments: segments.len() };
        self.loaded.insert(id, loaded);
        Ok(loaded)
    }

    pub fn unload(&mut self, id: CueId) {
        self.pool.remove_voice(id.0);
        self.loaded.remove(&id);
    }

    pub fn unload_all(&mut self) {
        for id in self.loaded.keys().copied().collect::<Vec<_>>() {
            self.pool.remove_voice(id.0);
        }
        self.loaded.clear();
    }
}

/// Programa de segmentos de la voz.
///
/// * Sin slices: un segmento `[inicio, fin)` con `play_count` repeticiones (o bucle infinito).
/// * Con slices: `[inicio, s1)` suena una vez y cada marcador abre un segmento hasta el siguiente (o
///   el final) con su propio `play_count`. El bucle de todo el programa lo da `infinite_loop`.
fn compile_segments(t: &AudioTimeAndLoopsData, start: u64, end: u64) -> Result<Vec<SliceSegment>, LoadError> {
    if t.slices.is_empty() {
        let count = if t.infinite_loop { INFINITE_SLICE_LOOP } else { t.play_count.max(1) };
        return Ok(vec![SliceSegment::new(start, end, count)]);
    }
    if t.slices.len() + 1 > MAX_SLICES_PER_VOICE {
        return Err(LoadError::BadSlices(format!("{} marcadores superan el máximo de {}", t.slices.len(), MAX_SLICES_PER_VOICE - 1)));
    }
    let mut prev = start;
    for m in &t.slices {
        if m.frame <= prev || m.frame >= end {
            return Err(LoadError::BadSlices(format!("el marcador {} no está entre {prev} y {end} o no crece", m.frame)));
        }
        prev = m.frame;
    }
    let mut segs = vec![SliceSegment::new(start, t.slices[0].frame, 1)];
    for (i, m) in t.slices.iter().enumerate() {
        let seg_end = t.slices.get(i + 1).map_or(end, |n| n.frame);
        segs.push(SliceSegment::new(m.frame, seg_end, m.play_count.max(1)));
    }
    Ok(segs)
}

/// Matriz en vivo `M × N`: uno-a-uno por defecto, con las celdas de `levels` encima si las hay.
fn build_matrix(file_channels: usize, device_channels: usize, levels: Option<&AudioLevelMatrixData>) -> AudioLevelMatrixRt {
    let m = file_channels.clamp(1, MAX_INPUT_CHANNELS);
    let mut matrix = AudioLevelMatrixRt::new(m, device_channels);
    if let Some(l) = levels {
        if l.validate().is_ok() {
            let cols = l.outputs as usize + 1;
            for row in 0..=(l.inputs as usize).min(m) {
                for col in 0..=(l.outputs as usize).min(matrix.out_channels) {
                    matrix.base_db[row][col] = l.cells[row * cols + col].db;
                }
            }
            matrix.mute_mask = l.mute_mask;
            matrix.solo_mask = l.solo_mask;
            matrix.matrix_dirty = true;
            matrix.recompute_targets(1.0, 1.0);
            matrix.snap_to_targets();
        }
    }
    matrix
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{write_wav, Cleanup};
    use qlab_protocol::cue_dto::AudioSliceMarker;

    fn loader() -> VoiceLoader {
        VoiceLoader::new(2, 2)
    }

    fn time(start: f64, end: f64) -> AudioTimeAndLoopsData {
        AudioTimeAndLoopsData { start_time_sec: start, end_time_sec: end, ..Default::default() }
    }

    #[test]
    fn loads_a_whole_file_and_exposes_the_voice() {
        let wav = write_wav("whole", 2, 48_000, 48_000, |f, _| f as f32 / 1000.0);
        let _c = Cleanup(vec![wav.clone()]);
        let mut l = loader();
        let info = l.load_path(CueId(7), &wav, &time(0.0, 0.0), None).unwrap();
        assert_eq!((info.start_frame, info.end_frame, info.segments), (0, 48_000, 1));
        assert_eq!((info.file.channels, info.file.sample_rate), (2, 48_000));
        let v = l.voice(CueId(7)).expect("voz publicada");
        assert_eq!((v.id, v.start_frame, v.end_frame, v.file_sample_rate), (7, 0, 48_000, 48_000));
        assert_eq!(l.loaded_count(), 1);
        assert_eq!(l.rt_handle().load().len(), 1, "visible para el callback");
        l.unload(CueId(7));
        assert!(l.voice(CueId(7)).is_none() && l.loaded_count() == 0);
        assert_eq!(l.rt_handle().load().len(), 0);
    }

    #[test]
    fn region_rate_and_reload() {
        let wav = write_wav("region", 1, 96_000, 48_000, |_, _| 0.1);
        let _c = Cleanup(vec![wav.clone()]);
        let mut l = loader();
        let mut t = time(0.5, 1.5);
        t.rate = 2.0;
        let info = l.load_path(CueId(1), &wav, &t, None).unwrap();
        assert_eq!((info.start_frame, info.end_frame), (24_000, 72_000));
        assert_eq!(l.voice(CueId(1)).unwrap().rate(), 2.0);
        // Recargar la misma cue reemplaza la voz, no la duplica.
        l.load_path(CueId(1), &wav, &time(0.0, 1.0), None).unwrap();
        assert_eq!(l.loaded_count(), 1);
        assert_eq!(l.rt_handle().load().len(), 1);
        assert_eq!(l.voice(CueId(1)).unwrap().end_frame, 48_000);
        // Un final más allá del archivo se recorta.
        assert_eq!(l.load_path(CueId(1), &wav, &time(0.0, 99.0), None).unwrap().end_frame, 96_000);
    }

    #[test]
    fn segments_from_loops_and_slices() {
        let mut t = time(0.0, 1.0);
        t.play_count = 3;
        let s = compile_segments(&t, 0, 48_000).unwrap();
        assert_eq!(s, vec![SliceSegment::new(0, 48_000, 3)]);
        t.infinite_loop = true;
        assert!(compile_segments(&t, 0, 48_000).unwrap()[0].is_infinite());

        let mut t = time(0.0, 1.0);
        t.slices = vec![AudioSliceMarker::new(10_000, 2), AudioSliceMarker::new(30_000, INFINITE_SLICE_LOOP)];
        let s = compile_segments(&t, 0, 48_000).unwrap();
        assert_eq!(
            s,
            vec![SliceSegment::new(0, 10_000, 1), SliceSegment::new(10_000, 30_000, 2), SliceSegment::new(30_000, 48_000, SliceSegment::INFINITE)]
        );
        t.slices[1].frame = 5_000;
        assert!(matches!(compile_segments(&t, 0, 48_000), Err(LoadError::BadSlices(_))));
        t.slices = vec![AudioSliceMarker::new(48_000, 1)];
        assert!(matches!(compile_segments(&t, 0, 48_000), Err(LoadError::BadSlices(_))));
        t.slices = (1..=64).map(|i| AudioSliceMarker::new(i * 100, 1)).collect();
        assert!(matches!(compile_segments(&t, 0, 48_000), Err(LoadError::BadSlices(_))));
    }

    #[test]
    fn slices_shape_the_loaded_voice() {
        let wav = write_wav("slices", 1, 48_000, 48_000, |_, _| 0.1);
        let _c = Cleanup(vec![wav.clone()]);
        let mut l = loader();
        let mut t = time(0.0, 0.0);
        t.slices = vec![AudioSliceMarker::new(12_000, 4)];
        t.infinite_loop = true;
        let info = l.load_path(CueId(2), &wav, &t, None).unwrap();
        assert_eq!(info.segments, 2);
        // SAFETY: nadie más usa la voz en esta prueba.
        let rt = unsafe { l.voice(CueId(2)).unwrap().rt_mut() };
        assert_eq!(rt.slice_count, 2);
        assert_eq!(rt.slices[1].remaining_loops, 4);
        drop(rt);
        assert!(l.voice(CueId(2)).unwrap().infinite_loop.load(std::sync::atomic::Ordering::Relaxed));
    }

    #[test]
    fn levels_are_mapped_onto_the_device_matrix() {
        let wav = write_wav("levels", 2, 4_800, 48_000, |_, _| 0.1);
        let _c = Cleanup(vec![wav.clone()]);
        let mut levels = AudioLevelMatrixData::new_one_to_one(2, 2).unwrap();
        levels.set_db(1, 1, -6.0).unwrap();
        levels.set_db(1, 2, -3.0).unwrap();
        levels.set_db(0, 0, -10.0).unwrap();
        levels.set_mute(2, true).unwrap();
        let mut l = loader();
        l.load_path(CueId(3), &wav, &time(0.0, 0.0), Some(&levels)).unwrap();
        // SAFETY: ídem.
        let rt = unsafe { l.voice(CueId(3)).unwrap().rt_mut() };
        let m = &rt.matrix;
        assert_eq!((m.in_channels, m.out_channels), (2, 2));
        assert_eq!((m.base_db[1][1], m.base_db[1][2], m.base_db[0][0]), (-6.0, -3.0, -10.0));
        assert_eq!(m.mute_mask, 0b10);
        let master = 10f32.powf(-10.0 / 20.0);
        assert!((m.target_crosspoint_lin[0][0] - master * 10f32.powf(-6.0 / 20.0)).abs() < 1e-6);
        assert_eq!(m.target_crosspoint_lin[1][1], 0.0, "salida 2 silenciada");
        assert_eq!(m.current_crosspoint_lin[0][0], m.target_crosspoint_lin[0][0], "sin rampa inicial");
    }

    #[test]
    fn mono_files_and_wider_devices() {
        let wav = write_wav("mono", 1, 4_800, 48_000, |_, _| 0.1);
        let _c = Cleanup(vec![wav.clone()]);
        let mut l = VoiceLoader::new(1, 6);
        l.load_path(CueId(1), &wav, &time(0.0, 0.0), None).unwrap();
        // SAFETY: ídem.
        let rt = unsafe { l.voice(CueId(1)).unwrap().rt_mut() };
        assert_eq!((rt.matrix.in_channels, rt.matrix.out_channels), (1, 6));
        assert_eq!(l.device_channels(), 6);
    }

    #[test]
    fn errors_are_typed() {
        let mut l = loader();
        let basics = CueBasicsData::default();
        let data = AudioCueData::new(time(0.0, 0.0), AudioLevelMatrixData::new_one_to_one(2, 2).unwrap());
        assert!(matches!(l.load_audio(CueId(1), &basics, &data), Err(LoadError::NoFileTarget)));
        assert!(matches!(l.load_path(CueId(1), Path::new("/no/existe.wav"), &time(0.0, 0.0), None), Err(LoadError::Stream(_))));
        let wav = write_wav("short", 1, 4_800, 48_000, |_, _| 0.1);
        let _c = Cleanup(vec![wav.clone()]);
        let err = l.load_path(CueId(1), &wav, &time(5.0, 6.0), None).err().unwrap();
        assert!(matches!(err, LoadError::BadRegion { .. }), "{err}");
        assert_eq!(l.loaded_count(), 0);
        assert!(!err.to_string().is_empty());
        assert_eq!(VoiceLoader::probe(&wav).unwrap().total_frames, Some(4_800));
    }

    #[test]
    fn load_audio_reads_the_file_target() {
        let wav = write_wav("target", 2, 4_800, 48_000, |_, _| 0.1);
        let _c = Cleanup(vec![wav.clone()]);
        let mut basics = CueBasicsData::default();
        basics.file_target = Some(qlab_protocol::cue_dto::MediaTarget { absolute_path: wav.to_string_lossy().into(), ..Default::default() });
        let data = AudioCueData::new(time(0.0, 0.0), AudioLevelMatrixData::new_one_to_one(2, 2).unwrap());
        let mut l = loader();
        assert!(l.load_audio(CueId(4), &basics, &data).is_ok());
        l.unload_all();
        assert_eq!(l.loaded_count(), 0);
    }
}
