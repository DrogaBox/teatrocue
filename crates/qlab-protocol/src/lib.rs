//! Tipos compartidos entre daemon, motor y UI: DTOs del Inspector, matriz de niveles e IPC en
//! memoria compartida. Sin dependencias de audio ni de GUI.

pub mod crashlog;
pub mod cue_dto;
pub mod cue_sheet;
pub mod ipc_client;
pub mod ipc_msg;
pub mod ipc_shm;
pub mod matrix;
pub mod show_format;
