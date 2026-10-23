//! Núcleo temporal y modelo de cues (spec §1.1–1.4).
//!
//! Capas: [`clock`] (frames y microsegundos) → [`cue_state`] (máquina de estados de una cue) →
//! [`cue`] (tipos Audio/Fade/Group) → [`cue_list`] (playhead, GO y encadenamientos) →
//! [`fade_processor`] (FadeCue → comandos) → [`scheduler`] (tick loop hacia el motor).
//!
//! Este crate no toca el hilo de audio: habla con el motor solo por `AudioCommand`/`AudioStatus`.

pub mod clock;
pub mod cue;
pub mod cue_list;
pub mod cue_state;
pub mod fade_processor;
pub mod scheduler;

pub use clock::{Frames, ManualClock, Micros, MonotonicClock, SystemClock, Timebase};
pub use cue::{AudioCueData, Cue, CueKind, FadeCell, FadeCueData, GroupCueData, GroupMode};
pub use cue_list::{CueList, Effect, GoResult, ListError, TriggerSource};
pub use cue_state::{ActionEnd, CueMachine, CueState, CueTiming, StateError, Transitions};
pub use fade_processor::{FadeCurve, FadeProcessor};
pub use scheduler::{Scheduler, SchedulerConfig, SchedulerStats};
