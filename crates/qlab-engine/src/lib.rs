//! Motor de audio en tiempo real, sin bloqueos. No conoce cues ni UI: solo voces, patches y matrices.
//!
//! Los backends de audio (`backend`) siguen vacíos hasta su fase.

pub mod alloc_guard;
pub mod backend;
pub mod biquad;
pub mod disk_streamer;
pub mod drift;
pub mod ffmpeg_pcm;
pub mod mixer_simd;
pub mod rt_command;
pub mod voice;
pub mod voice_pool;

pub use disk_streamer::{DiskStreamerPool, FileInfo, ReaderView, StreamConfig, StreamError, StreamingVoiceReader};
pub use drift::{DriftConfig, DriftController, DriftControllerPI, DriftResampler, FrameRing};
pub use mixer_simd::{catmull_rom_4pt, enable_ftz_daz_on_current_thread, process_rt_callback};
