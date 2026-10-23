//! Segmento de memoria compartida `SharedTelemetryShm` (spec §2.1).
//!
//! * Linux: archivo en `/dev/shm` (`/teatrocue_rt_telemetry_v1`) proyectado con `memmap2`.
//! * Windows: `CreateFileMappingW` con el nombre `Local\TeatroCueRtTelemetryV1`.
//!
//! El daemon lo **crea** (y al cerrar lo elimina); la UI y las herramientas lo **abren**. Un
//! segmento recién creado está a cero, que es un estado válido de la estructura.

use std::io;

use qlab_protocol::ipc_shm::{SharedTelemetryShm, SHM_NAME_LINUX, SHM_NAME_WINDOWS};

pub fn default_shm_name() -> &'static str {
    if cfg!(windows) { SHM_NAME_WINDOWS } else { SHM_NAME_LINUX }
}

const SHM_SIZE: usize = std::mem::size_of::<SharedTelemetryShm>();

pub struct SharedMemory {
    ptr: *const SharedTelemetryShm,
    name: String,
    owner: bool,
    backing: Backing,
}

enum Backing {
    #[cfg(unix)]
    File {
        // Mantiene viva la proyección mientras exista `ptr`.
        _map: memmap2::MmapMut,
        path: std::path::PathBuf,
    },
    #[cfg(windows)]
    Windows {
        handle: windows::Win32::Foundation::HANDLE,
        view: *mut core::ffi::c_void,
    },
}

// SAFETY: la región solo contiene atómicos (`SharedTelemetryShm`), que se pueden compartir entre
// hilos; el puntero apunta a una proyección que vive tanto como `SharedMemory`.
unsafe impl Send for SharedMemory {}
unsafe impl Sync for SharedMemory {}

impl SharedMemory {
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Quita el nombre del segmento para que nadie nuevo pueda abrirlo (solo el dueño). Las
    /// proyecciones existentes siguen siendo válidas hasta que se suelten. Idempotente.
    pub fn unlink(&self) {
        imp::unlink(&self.backing, self.owner);
    }

    pub fn is_owner(&self) -> bool {
        self.owner
    }

    /// La página de telemetría proyectada.
    #[inline]
    pub fn telemetry(&self) -> &SharedTelemetryShm {
        // SAFETY: `ptr` apunta a `SHM_SIZE` bytes alineados a 64 (comprobado al proyectar), vivos
        // mientras `self` exista, y toda la estructura es atómica.
        unsafe { &*self.ptr }
    }

    fn from_raw(ptr: *const u8, name: &str, owner: bool, backing: Backing) -> io::Result<Self> {
        if ptr.align_offset(std::mem::align_of::<SharedTelemetryShm>()) != 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "proyección sin alinear a 64 bytes"));
        }
        Ok(Self { ptr: ptr.cast(), name: name.to_string(), owner, backing })
    }
}

#[cfg(unix)]
mod imp {
    use super::*;
    use std::fs::OpenOptions;
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::{Path, PathBuf};

    fn path_for(name: &str) -> io::Result<PathBuf> {
        let trimmed = name.trim_start_matches('/');
        if trimmed.is_empty() || trimmed.contains('/') || trimmed.contains("..") {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("nombre de memoria compartida inválido: {name}")));
        }
        let dir = if Path::new("/dev/shm").is_dir() { PathBuf::from("/dev/shm") } else { std::env::temp_dir() };
        Ok(dir.join(trimmed))
    }

    impl SharedMemory {
        /// Crea (y reemplaza si quedó uno viejo) el segmento y lo inicializa.
        pub fn create(name: &str) -> io::Result<Self> {
            let path = path_for(name)?;
            let _ = std::fs::remove_file(&path);
            let file = OpenOptions::new().read(true).write(true).create_new(true).mode(0o600).open(&path)?;
            file.set_len(SHM_SIZE as u64)?;
            // SAFETY: archivo recién creado y de uso exclusivo del daemon hasta que se inicializa.
            let mut map = unsafe { memmap2::MmapMut::map_mut(&file)? };
            let me = Self::from_raw(map.as_mut_ptr(), name, true, Backing::File { _map: map, path })?;
            me.telemetry().initialize();
            Ok(me)
        }

        /// Abre el segmento que creó el daemon.
        pub fn open(name: &str) -> io::Result<Self> {
            let path = path_for(name)?;
            let file = OpenOptions::new().read(true).write(true).open(&path)?;
            if (file.metadata()?.len() as usize) < SHM_SIZE {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "segmento demasiado pequeño"));
            }
            // SAFETY: el daemon mantiene el archivo; la estructura es atómica.
            let mut map = unsafe { memmap2::MmapMut::map_mut(&file)? };
            let me = Self::from_raw(map.as_mut_ptr(), name, false, Backing::File { _map: map, path })?;
            me.telemetry().check().map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            Ok(me)
        }
    }

    pub(super) fn unlink(backing: &Backing, owner: bool) {
        let Backing::File { path, .. } = backing;
        if owner {
            let _ = std::fs::remove_file(path);
        }
    }

    pub(super) fn cleanup(backing: &Backing, owner: bool) {
        unlink(backing, owner);
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE, INVALID_HANDLE_VALUE};
    use windows::Win32::System::Memory::{
        CreateFileMappingW, MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, FILE_MAP_ALL_ACCESS, MEMORY_MAPPED_VIEW_ADDRESS,
        PAGE_READWRITE,
    };

    fn wide(name: &str) -> Vec<u16> {
        name.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn map(handle: HANDLE, name: &str, owner: bool) -> io::Result<SharedMemory> {
        // SAFETY: `handle` es un mapeo válido de al menos `SHM_SIZE` bytes.
        let view = unsafe { MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, SHM_SIZE) };
        if view.Value.is_null() {
            let err = io::Error::last_os_error();
            let _ = unsafe { CloseHandle(handle) };
            return Err(err);
        }
        let me = SharedMemory::from_raw(view.Value as *const u8, name, owner, Backing::Windows { handle, view: view.Value });
        if me.is_err() {
            unsafe {
                let _ = UnmapViewOfFile(view);
                let _ = CloseHandle(handle);
            }
        }
        me
    }

    impl SharedMemory {
        pub fn create(name: &str) -> io::Result<Self> {
            let w = wide(name);
            // SAFETY: `w` termina en NUL y vive durante la llamada.
            let handle = unsafe {
                CreateFileMappingW(INVALID_HANDLE_VALUE, None, PAGE_READWRITE, 0, SHM_SIZE as u32, PCWSTR(w.as_ptr()))
            }
            .map_err(|e| io::Error::other(e.to_string()))?;
            if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
                let _ = unsafe { CloseHandle(handle) };
                return Err(io::Error::new(io::ErrorKind::AlreadyExists, "ya hay un daemon con ese segmento de memoria"));
            }
            let me = map(handle, name, true)?;
            me.telemetry().initialize();
            Ok(me)
        }

        pub fn open(name: &str) -> io::Result<Self> {
            let w = wide(name);
            // SAFETY: igual que en `create`.
            let handle = unsafe { OpenFileMappingW(FILE_MAP_ALL_ACCESS.0, false, PCWSTR(w.as_ptr())) }
                .map_err(|e| io::Error::new(io::ErrorKind::NotFound, e.to_string()))?;
            let me = map(handle, name, false)?;
            me.telemetry().check().map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            Ok(me)
        }
    }

    /// En Windows el nombre desaparece cuando se cierra el último manejador.
    pub(super) fn unlink(_backing: &Backing, _owner: bool) {}

    pub(super) fn cleanup(backing: &Backing, _owner: bool) {
        let Backing::Windows { handle, view } = backing;
        // SAFETY: se liberan una sola vez, en `Drop`, los recursos que `map` obtuvo.
        unsafe {
            let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: *view });
            let _ = CloseHandle(*handle);
        }
    }
}

impl Drop for SharedMemory {
    fn drop(&mut self) {
        imp::cleanup(&self.backing, self.owner);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qlab_protocol::ipc_shm::{HotControlEvent, ShowStatus};

    fn unique(tag: &str) -> String {
        format!("qlab_test_{}_{tag}", std::process::id())
    }

    #[test]
    fn creator_and_client_see_the_same_memory() {
        let name = unique("share");
        let daemon = SharedMemory::create(&name).unwrap();
        assert!(daemon.is_owner());
        let client = SharedMemory::open(&name).unwrap();
        assert!(!client.is_owner());
        assert_ne!(daemon.telemetry() as *const _, client.telemetry() as *const _, "dos proyecciones distintas");

        // UI → daemon por el anillo caliente.
        client.telemetry().hot_ring.push(HotControlEvent::Go).unwrap();
        client.telemetry().hot_ring.push(HotControlEvent::LiveCrosspointDrag { cue_num_id: 3, row: 1, col: 2, db: -6.0 }).unwrap();
        assert_eq!(daemon.telemetry().hot_ring.pop(), Some(HotControlEvent::Go));
        assert!(matches!(daemon.telemetry().hot_ring.pop(), Some(HotControlEvent::LiveCrosspointDrag { cue_num_id: 3, .. })));

        // daemon → UI por el seqlock y los medidores.
        let status = ShowStatus { active_playhead_cue_id: 42, is_playing: true, total_xruns: 3, ..Default::default() };
        daemon.telemetry().status.publish(&status);
        let seen = client.telemetry().status.snapshot().unwrap();
        assert_eq!((seen.active_playhead_cue_id, seen.is_playing, seen.total_xruns), (42, true, 3));
        daemon.telemetry().meters.report_output(1, 0.75);
        assert_eq!(client.telemetry().meters.consume_output(1), 0.75);
        assert_eq!(client.telemetry().meters.consume_output(1), 0.0, "la lectura consume el pico");

        daemon.telemetry().beat(123);
        assert_eq!(client.telemetry().last_heartbeat_ns(), 123);
    }

    #[test]
    fn the_mapping_is_cache_line_aligned_and_checked() {
        let name = unique("align");
        let shm = SharedMemory::create(&name).unwrap();
        assert_eq!(shm.telemetry() as *const _ as usize % 64, 0);
        assert!(shm.telemetry().check().is_ok());
        assert_eq!(shm.name(), name);
    }

    #[cfg(unix)]
    #[test]
    fn owner_removes_the_segment_on_drop_and_stale_ones_are_replaced() {
        let name = unique("drop");
        let first = SharedMemory::create(&name).unwrap();
        first.telemetry().beat(7);
        let second = SharedMemory::create(&name).unwrap(); // reemplaza el viejo
        assert_eq!(second.telemetry().last_heartbeat_ns(), 0);
        drop(second);
        assert!(SharedMemory::open(&name).is_err(), "al cerrar el daemon desaparece");
        drop(first);
    }

    #[cfg(unix)]
    #[test]
    fn opening_garbage_or_missing_segments_fails_cleanly() {
        assert!(SharedMemory::open(&unique("missing")).is_err());
        assert!(SharedMemory::create("a/b").is_err());
        assert!(SharedMemory::create("").is_err());
        assert!(SharedMemory::create("..").is_err());
        let name = unique("junk");
        let path = std::path::Path::new("/dev/shm").join(&name);
        std::fs::write(&path, vec![0xFFu8; SHM_SIZE]).unwrap();
        let err = SharedMemory::open(&name).err().expect("magic inválido");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn default_names_follow_the_spec() {
        assert!(default_shm_name().to_lowercase().contains("teatrocue"));
    }
}
