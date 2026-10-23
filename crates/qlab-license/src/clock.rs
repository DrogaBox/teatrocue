//! Defensa contra el retroceso del reloj (offline).
//!
//! Una licencia con vencimiento solo sirve si no se puede «volver atrás» la fecha de la PC. Sin internet no hay
//! una fuente de tiempo fiable, así que se combinan varias señales, todas verificables sin red:
//!
//! 1. **Marca máxima** (`mark`): la fecha más alta que TeatroCue ha visto en esta PC. Se guarda en varios sitios
//!    (archivo junto a la aplicación, `ProgramData`, registro) con un HMAC atado a esta PC y a esta licencia: no se
//!    puede editar a mano, ni copiar de otra PC. Si el reloj actual está claramente por debajo de ella, se rechaza.
//! 2. **Borrar las marcas no sirve**: si no queda ninguna marca (y la licencia no es recién emitida), se rechaza.
//! 3. **Anclas del sistema**: fechas de modificación de archivos de Windows que el sistema actualiza solo
//!    (registro, registros de eventos) y la fecha de compilación del programa: una fecha anterior es falsa.
//!
//! Se comprueba al **arrancar**, nunca durante una función: una licencia que vence con el show en marcha no corta el
//! audio. Ninguna defensa offline es absoluta: quien modifique el ejecutable puede saltarse todo esto.

use std::path::{Path, PathBuf};

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

use crate::{License, MachineId};

/// Margen que se tolera por debajo de la marca máxima: cambios de zona horaria, ajustes de la BIOS, deriva del reloj.
pub const TOLERANCE_SECS: u64 = 36 * 3600;
const MAGIC: &str = "TEATROCUE-CLOCK-1";
/// Fecha de compilación del programa (segundos Unix).
pub const BUILD_UNIX: u64 = parse_u64(env!("TEATROCUE_BUILD_UNIX_VALUE"));

const fn parse_u64(s: &str) -> u64 {
    let b = s.as_bytes();
    let (mut i, mut n) = (0, 0u64);
    while i < b.len() {
        n = n * 10 + (b[i] - b'0') as u64;
        i += 1;
    }
    n
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClockFault {
    /// El reloj está por debajo de lo que ya se había visto (o de la fecha de compilación).
    Rollback { seen: u64, now: u64 },
    /// Una marca existe pero su sello no corresponde (editada, o de otra PC/licencia).
    Tampered(String),
    /// No queda ninguna marca y la licencia no es recién emitida: alguien las borró.
    MarksMissing,
}

/// Un sitio donde se guarda la marca.
pub trait MarkStore: Send + Sync {
    fn label(&self) -> String;
    fn read(&self) -> Option<String>;
    fn write(&self, text: &str) -> std::io::Result<()>;
}

/// Marca en un archivo (escritura atómica: temporal + renombrado).
pub struct FileStore(pub PathBuf);

impl MarkStore for FileStore {
    fn label(&self) -> String {
        self.0.display().to_string()
    }

    fn read(&self) -> Option<String> {
        std::fs::read_to_string(&self.0).ok()
    }

    fn write(&self, text: &str) -> std::io::Result<()> {
        if let Some(dir) = self.0.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.0.with_extension(format!("tmp{}", std::process::id()));
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, &self.0).inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })
    }
}

/// Clave HMAC de las marcas: atada a la PC y a esta licencia concreta.
fn mark_key(machine: &MachineId, license: &License) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(b"TeatroCue/clock-mark/v1|");
    h.update(machine.as_str().as_bytes());
    h.update(b"|");
    h.update(license.fingerprint().as_bytes());
    h.finalize().into()
}

/// Contenido de una marca: la fecha máxima vista y cuántas reactivaciones de emergencia se han canjeado (anti-replay).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mark {
    pub mark: u64,
    pub epoch: u32,
}

fn mac_over(machine: &MachineId, license: &License, data: &str) -> String {
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(&mark_key(machine, license)).expect("HMAC acepta cualquier longitud");
    m.update(data.as_bytes());
    m.finalize().into_bytes().iter().map(|b| format!("{b:02x}")).collect()
}

pub fn mark_text(license: &License, mark: u64) -> String {
    mark_text_with(license, Mark { mark, epoch: 0 })
}

pub fn mark_text_with(license: &License, m: Mark) -> String {
    format!("{MAGIC}\nmark={}\nepoch={}\nmac={}\n", m.mark, m.epoch, mac_over(&license.machine, license, &format!("mark={};epoch={}", m.mark, m.epoch)))
}

/// `Ok(marca)` si el texto es una marca auténtica para esta licencia; `Err` si existe pero no lo es. Las marcas del
/// formato anterior (sin época) se aceptan con época 0.
pub fn parse_mark_full(text: &str, license: &License) -> Result<Mark, String> {
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    if lines.next() != Some(MAGIC) {
        return Err("cabecera inválida".into());
    }
    let (mut mark, mut epoch, mut tag) = (None, None, None);
    for l in lines {
        if let Some(v) = l.strip_prefix("mark=") {
            mark = v.parse::<u64>().ok();
        } else if let Some(v) = l.strip_prefix("epoch=") {
            epoch = v.parse::<u32>().ok();
        } else if let Some(v) = l.strip_prefix("mac=") {
            tag = Some(v.to_string());
        }
    }
    let (Some(mark), Some(tag)) = (mark, tag) else { return Err("faltan campos".into()) };
    let (data, epoch) = match epoch {
        Some(e) => (format!("mark={mark};epoch={e}"), e),
        None => (format!("mark={mark}"), 0), // formato anterior
    };
    // Comparación del sello sin atajos por prefijo.
    let want = mac_over(&license.machine, license, &data);
    let ok = want.len() == tag.len() && want.bytes().zip(tag.bytes()).fold(0u8, |a, (x, y)| a | (x ^ y)) == 0;
    if ok { Ok(Mark { mark, epoch }) } else { Err("el sello no corresponde a esta PC y licencia".into()) }
}

pub fn parse_mark(text: &str, license: &License) -> Result<u64, String> {
    parse_mark_full(text, license).map(|m| m.mark)
}

/// Contador de reactivaciones de emergencia ya canjeadas en esta PC (el mayor de las marcas auténticas; 0 si no hay).
pub fn epoch(license: &License, stores: &[Box<dyn MarkStore>]) -> u32 {
    stores.iter().filter_map(|s| s.read()).filter_map(|t| parse_mark_full(&t, license).ok()).map(|m| m.epoch).max().unwrap_or(0)
}

/// Decide si el reloj `now` es creíble. Devuelve la fecha efectiva (la mayor entre `now` y todo lo visto).
///
/// * `anchors`: fechas que el sistema fija por sí mismo (archivos de Windows, fecha de compilación…).
/// * Sin marcas: solo se acepta si la licencia es recién emitida (`|now − issued|` dentro de la tolerancia), que es
///   el caso de una licencia emitida a mano y usada por primera vez.
pub fn check(now: u64, license: &License, stores: &[Box<dyn MarkStore>], anchors: &[u64]) -> Result<u64, ClockFault> {
    let mut high = 0u64;
    let mut present = 0;
    for s in stores {
        let Some(text) = s.read() else { continue };
        present += 1;
        match parse_mark(&text, license) {
            Ok(m) => high = high.max(m),
            Err(why) => return Err(ClockFault::Tampered(format!("{}: {why}", s.label()))),
        }
    }
    if present == 0 {
        let fresh = now.abs_diff(license.issued_unix) <= TOLERANCE_SECS;
        if !fresh {
            return Err(ClockFault::MarksMissing);
        }
    }
    let anchor = anchors.iter().copied().max().unwrap_or(0).max(BUILD_UNIX);
    let seen = high.max(anchor);
    if now + TOLERANCE_SECS < seen {
        return Err(ClockFault::Rollback { seen, now });
    }
    Ok(now.max(high))
}

/// Guarda `mark` en todos los sitios (monótono: nunca baja una marca ni una época). Devuelve en cuántos se pudo escribir.
pub fn record(mark: u64, license: &License, stores: &[Box<dyn MarkStore>]) -> usize {
    let epoch = epoch(license, stores);
    let text = mark_text_with(license, Mark { mark, epoch });
    stores
        .iter()
        .filter(|s| {
            // No se baja una marca válida más alta que la que se va a escribir.
            let higher = s.read().and_then(|t| parse_mark_full(&t, license).ok()).is_some_and(|m| m.mark > mark || m.epoch > epoch);
            !higher && s.write(&text).is_ok()
        })
        .count()
}

/// Escribe `mark` y `epoch` en todos los sitios **sin** respetar marcas anteriores: lo usan el instalador (al emitir o
/// renovar con la contraseña maestra, cuando las marcas viejas son de otra licencia) y la reactivación de emergencia
/// (que sube la época). Devuelve cuántos se escribieron.
pub fn reset_with_epoch(mark: u64, epoch: u32, license: &License, stores: &[Box<dyn MarkStore>]) -> usize {
    let text = mark_text_with(license, Mark { mark, epoch });
    stores.iter().filter(|s| s.write(&text).is_ok()).count()
}

pub fn reset(mark: u64, license: &License, stores: &[Box<dyn MarkStore>]) -> usize {
    reset_with_epoch(mark, 0, license, stores)
}

/// Sitios donde viven las marcas en esta PC. `install_dir` es la carpeta de la aplicación (la de `license.key`).
pub fn default_stores(install_dir: &Path) -> Vec<Box<dyn MarkStore>> {
    let mut v: Vec<Box<dyn MarkStore>> = vec![Box::new(FileStore(install_dir.join("license.state")))];
    if let Some(pd) = std::env::var_os("ProgramData").or_else(|| std::env::var_os("TEATROCUE_STATE_DIR")) {
        v.push(Box::new(FileStore(PathBuf::from(pd).join("TeatroCue").join("clock.dat"))));
    }
    #[cfg(windows)]
    v.push(Box::new(windows_registry::RegistryStore));
    v
}

/// Fechas (segundos Unix) de archivos de Windows que el sistema modifica por su cuenta. En otros sistemas, ninguna.
pub fn system_anchors() -> Vec<u64> {
    #[cfg(windows)]
    {
        let root = std::env::var_os("SystemRoot").map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from);
        [
            r"System32\config\SYSTEM",
            r"System32\config\SOFTWARE",
            r"System32\config\SECURITY",
            r"System32\winevt\Logs\System.evtx",
            r"System32\winevt\Logs\Application.evtx",
            r"System32\winevt\Logs\Security.evtx",
        ]
        .iter()
        .filter_map(|f| std::fs::metadata(root.join(f)).ok()?.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok().map(|d| d.as_secs()))
        .collect()
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}

#[cfg(windows)]
mod windows_registry {
    use super::MarkStore;
    use windows::core::{w, PCWSTR};
    use windows::Win32::System::Registry::{RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ};

    /// `HKCU\Software\TeatroCue\ClockMark` (no necesita permisos de administrador).
    pub struct RegistryStore;

    impl MarkStore for RegistryStore {
        fn label(&self) -> String {
            r"HKCU\Software\TeatroCue\ClockMark".into()
        }

        fn read(&self) -> Option<String> {
            let mut buf = [0u16; 512];
            let mut bytes = std::mem::size_of_val(&buf) as u32;
            // SAFETY: `buf` y `bytes` son locales y válidos durante la llamada.
            let r = unsafe { RegGetValueW(HKEY_CURRENT_USER, w!(r"Software\TeatroCue"), w!("ClockMark"), RRF_RT_REG_SZ, None, Some(buf.as_mut_ptr().cast()), Some(&mut bytes)) };
            if r.0 != 0 {
                return None;
            }
            let len = (bytes as usize / 2).saturating_sub(1).min(buf.len());
            Some(String::from_utf16_lossy(&buf[..len]))
        }

        fn write(&self, text: &str) -> std::io::Result<()> {
            let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
            // SAFETY: `wide` vive durante la llamada y su tamaño en bytes es el correcto.
            let r = unsafe { RegSetKeyValueW(HKEY_CURRENT_USER, w!(r"Software\TeatroCue"), w!("ClockMark"), REG_SZ.0, Some(wide.as_ptr().cast()), (wide.len() * 2) as u32) };
            if r.0 == 0 { Ok(()) } else { Err(std::io::Error::from_raw_os_error(r.0 as i32)) }
        }
    }

    #[allow(dead_code)]
    fn _unused(_: PCWSTR) {}
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// Sitio en memoria para las pruebas.
    #[derive(Clone, Default)]
    pub struct Mem(pub Arc<Mutex<Option<String>>>);

    impl MarkStore for Mem {
        fn label(&self) -> String {
            "memoria".into()
        }
        fn read(&self) -> Option<String> {
            self.0.lock().unwrap().clone()
        }
        fn write(&self, text: &str) -> std::io::Result<()> {
            *self.0.lock().unwrap() = Some(text.to_string());
            Ok(())
        }
    }

    pub fn lic(issued: u64, expires: u64) -> License {
        License { licensee: "Ballet Oficial de Córdoba".into(), machine: MachineId::from_guid("pc"), issued_unix: issued, expires_unix: expires }
    }

    fn three() -> (Vec<Box<dyn MarkStore>>, [Mem; 3]) {
        let m = [Mem::default(), Mem::default(), Mem::default()];
        (m.iter().cloned().map(|x| Box::new(x) as Box<dyn MarkStore>).collect(), m)
    }

    const DAY: u64 = 86_400;
    /// Una fecha de «ahora» posterior a la de compilación (si no, la ancla de compilación bloquearía todo).
    fn t0() -> u64 {
        BUILD_UNIX + 30 * DAY
    }

    #[test]
    fn marks_are_authenticated_and_bound_to_the_license() {
        let l = lic(t0(), 0);
        let text = mark_text(&l, 1234);
        assert_eq!(parse_mark(&text, &l), Ok(1234));
        // Editar el número o el sello, o usar la marca con otra licencia/PC, falla.
        assert!(parse_mark(&text.replace("mark=1234", "mark=99"), &l).is_err());
        assert!(parse_mark(&text.replace("mac=", "mac=0"), &l).is_err());
        assert!(parse_mark(&text, &lic(t0() + 1, 0)).is_err(), "otra licencia");
        let other_pc = License { machine: MachineId::from_guid("otra-pc"), ..l.clone() };
        assert!(parse_mark(&text, &other_pc).is_err(), "otra PC");
        assert!(parse_mark("basura", &l).is_err());
    }

    #[test]
    fn a_fresh_license_without_marks_is_accepted_and_the_first_run_records_them() {
        let l = lic(t0(), 0);
        let (stores, mems) = three();
        assert_eq!(check(t0() + 60, &l, &stores, &[]), Ok(t0() + 60));
        assert_eq!(record(t0() + 60, &l, &stores), 3);
        assert!(mems.iter().all(|m| m.read().is_some()));
        // La siguiente vez ya hay marcas y todo sigue normal.
        assert_eq!(check(t0() + DAY, &l, &stores, &[]), Ok(t0() + DAY));
    }

    #[test]
    fn rolling_the_clock_back_is_detected_even_by_a_few_days() {
        let l = lic(t0(), 0);
        let (stores, _) = three();
        record(t0() + 10 * DAY, &l, &stores);
        // Reloj en el pasado: rechazado, y el mensaje conoce ambas fechas.
        assert_eq!(check(t0() + 3 * DAY, &l, &stores, &[]), Err(ClockFault::Rollback { seen: t0() + 10 * DAY, now: t0() + 3 * DAY }));
        // Pequeños desajustes (menos de la tolerancia) no molestan: cambio de zona, horario de verano…
        assert!(check(t0() + 10 * DAY - 3 * 3600, &l, &stores, &[]).is_ok());
        assert!(check(t0() + 10 * DAY - TOLERANCE_SECS - 1, &l, &stores, &[]).is_err());
        // Y avanzar sí está permitido.
        assert!(check(t0() + 40 * DAY, &l, &stores, &[]).is_ok());
    }

    #[test]
    fn deleting_some_marks_is_not_enough_and_deleting_all_is_refused() {
        let l = lic(t0(), 0);
        let (stores, mems) = three();
        record(t0() + 10 * DAY, &l, &stores);
        *mems[0].0.lock().unwrap() = None;
        *mems[1].0.lock().unwrap() = None;
        // Queda una marca: basta para detectar el retroceso.
        assert!(matches!(check(t0() + DAY, &l, &stores, &[]), Err(ClockFault::Rollback { .. })));
        // Borradas todas, una licencia que ya lleva días en uso se rechaza.
        *mems[2].0.lock().unwrap() = None;
        assert_eq!(check(t0() + 10 * DAY, &l, &stores, &[]), Err(ClockFault::MarksMissing));
    }

    #[test]
    fn editing_a_mark_or_copying_it_from_another_pc_is_tampering() {
        let l = lic(t0(), 0);
        let (stores, mems) = three();
        record(t0() + 10 * DAY, &l, &stores);
        // Intento clásico: bajar el número de la marca a mano.
        let text = mems[0].read().unwrap().replace(&format!("mark={}", t0() + 10 * DAY), &format!("mark={}", t0()));
        *mems[0].0.lock().unwrap() = Some(text);
        assert!(matches!(check(t0() + DAY, &l, &stores, &[]), Err(ClockFault::Tampered(_))));
        // Marca válida pero de otra PC.
        let foreign = License { machine: MachineId::from_guid("otra-pc"), ..l.clone() };
        *mems[0].0.lock().unwrap() = Some(mark_text(&foreign, t0()));
        assert!(matches!(check(t0() + DAY, &l, &stores, &[]), Err(ClockFault::Tampered(_))));
    }

    #[test]
    fn system_anchors_and_the_build_date_catch_a_clock_set_before_them() {
        let l = lic(t0(), 0);
        let (stores, _) = three();
        record(t0(), &l, &stores);
        // Un archivo del sistema modificado «en el futuro» respecto al reloj delata el retroceso aunque no haya marcas altas.
        assert!(matches!(check(t0() + DAY, &l, &stores, &[t0() + 20 * DAY]), Err(ClockFault::Rollback { .. })));
        assert!(check(t0() + 21 * DAY, &l, &stores, &[t0() + 20 * DAY]).is_ok());
        // Un reloj anterior a la compilación del programa es imposible.
        let (empty, _) = three();
        let old = lic(BUILD_UNIX - 10 * DAY, 0);
        assert!(matches!(check(BUILD_UNIX - 10 * DAY, &old, &empty, &[]), Err(ClockFault::Rollback { .. })));
    }

    #[test]
    fn recording_never_lowers_a_mark() {
        let l = lic(t0(), 0);
        let (stores, mems) = three();
        record(t0() + 10 * DAY, &l, &stores);
        assert_eq!(record(t0() + 2 * DAY, &l, &stores), 0, "una marca más baja no pisa la alta");
        assert_eq!(parse_mark(&mems[0].read().unwrap(), &l), Ok(t0() + 10 * DAY));
    }

    #[test]
    fn file_stores_write_atomically_and_read_back() {
        let dir = std::env::temp_dir().join(format!("teatrocue-clock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let s = FileStore(dir.join("a/b/clock.dat"));
        assert!(s.read().is_none());
        s.write("hola").unwrap();
        assert_eq!(s.read().as_deref(), Some("hola"));
        assert_eq!(std::fs::read_dir(dir.join("a/b")).unwrap().count(), 1, "sin temporales");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
