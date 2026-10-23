//! Interfaz gráfica de TeatroCue (egui + wgpu), reactiva y de bajo consumo.
//!
//! * [`app`]: estado, atajos, reparto de eventos y repintado a demanda.
//! * [`link`]: hilo que habla con el daemon (IPC) y vigila la memoria compartida.
//! * [`model`]: filas de la tabla, selección, telemetría y vúmetros (sin egui).
//! * [`views`]: barra superior, tabla de cues, Inspector de 4 pestañas y pantalla de reconexión.
//! * [`standalone`]: motor embebido o daemon como proceso aparte.

pub mod app;
pub mod audio_tools;
pub mod autosave;
pub mod file_picker;
pub mod format;
pub mod history;
pub mod license_gate;
pub mod link;
pub mod media;
pub mod model;
pub mod native_dialog;
pub mod novedades;
pub mod prefs;
pub mod repair;
pub mod settings;
pub mod single_instance;
pub mod standalone;
pub mod system_sounds;
pub mod theme;
pub mod video;
pub mod views;
pub mod waveform;
pub mod widgets;

#[cfg(test)]
mod app_tests;

#[cfg(test)]
mod click_tests;
