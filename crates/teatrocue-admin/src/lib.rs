//! `teatrocue-admin`: administración de licencias de TeatroCue, 100% offline.
//!
//! * [`db`]: inventario de equipos y licencias emitidas (la capa SQLite vive en `licencias-db`, compartida con el instalador).
//! * [`auth`]: contraseña maestra, clave privada solo en RAM, `license.key` y respuestas a códigos de emergencia.
//! * [`app`]: la interfaz.

pub mod app;
pub mod auth;
pub mod usb;
pub use licencias_db as db;
