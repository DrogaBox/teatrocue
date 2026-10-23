//! Licencia offline de TeatroCue.
//!
//! * **Machine ID**: `MachineGuid` de Windows (`HKLM\SOFTWARE\Microsoft\Cryptography`) normalizado con SHA-256 a 16
//!   caracteres legibles `XXXX-XXXX-XXXX-XXXX`.
//! * **Licencia** (`license.key`): texto con la institución, el Machine ID y la fecha de emisión, firmado con
//!   Ed25519. El binario lleva incrustada solo la clave **pública**; la privada vive cifrada en el pendrive
//!   (`master.key.enc`, ver [`keyfile`]).
//!
//! Es un mecanismo de licenciamiento contractual y disuasivo, no una protección infalible: quien pueda modificar
//! el ejecutable puede quitarla. Lo que sí garantiza es que una licencia emitida para una PC no se acepta en otra
//! ni se puede fabricar sin la clave privada.

pub mod activation;
pub mod clock;
pub mod keyfile;
pub mod machine;

use std::path::{Path, PathBuf};

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};

pub use machine::MachineId;

pub const LICENSE_FILE_NAME: &str = "license.key";
/// Formato actual (con vencimiento). El anterior (`-1`, sin vencimiento) se sigue leyendo como licencia sin fecha.
const MAGIC: &str = "TEATROCUE-LICENSE-2";
const MAGIC_V1: &str = "TEATROCUE-LICENSE-1";
/// Clave pública incrustada al compilar (vacía si no se configuró `TEATROCUE_PUBLIC_KEY_FILE`).
const EMBEDDED_PUBLIC_KEY_HEX: &str = include_str!(concat!(env!("OUT_DIR"), "/public_key.hex"));

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct License {
    pub licensee: String,
    pub machine: MachineId,
    /// Segundos Unix de emisión.
    pub issued_unix: u64,
    /// Segundos Unix en que vence; `0` = sin vencimiento.
    pub expires_unix: u64,
}

impl License {
    pub fn is_perpetual(&self) -> bool {
        self.expires_unix == 0
    }

    /// Días enteros que faltan para el vencimiento a las `now` (negativos si ya venció; `None` si no vence).
    pub fn days_left(&self, now: u64) -> Option<i64> {
        (!self.is_perpetual()).then(|| (self.expires_unix as i64 - now as i64).div_euclid(86_400))
    }

    /// Huella corta de lo firmado: ata las marcas de reloj a esta licencia concreta.
    pub fn fingerprint(&self) -> String {
        use sha2::{Digest, Sha256};
        Sha256::digest(signed_message(self)).iter().take(8).map(|b| format!("{b:02x}")).collect()
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum LicenseError {
    /// No hay `license.key` en la ruta indicada.
    NotFound(PathBuf),
    Io(String),
    Malformed(String),
    /// La firma no corresponde al contenido (archivo alterado o firmado con otra clave).
    BadSignature,
    /// La licencia es válida pero de otra PC.
    WrongMachine { expected: String, found: String },
    /// Este binario se compiló sin clave pública: no puede validar nada.
    NoPublicKey,
    /// No se pudo leer el Machine ID de esta PC.
    NoMachineId(String),
    /// La licencia venció en `expires` (segundos Unix).
    Expired { expires: u64 },
    /// El reloj de la PC no es creíble (retrocedido, marcas alteradas o borradas).
    Clock(clock::ClockFault),
}

impl std::fmt::Display for LicenseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(p) => write!(f, "no se encontró la licencia ({})", p.display()),
            Self::Io(e) => write!(f, "no se pudo leer la licencia: {e}"),
            Self::Malformed(e) => write!(f, "licencia ilegible: {e}"),
            Self::BadSignature => write!(f, "la firma de la licencia no es válida"),
            Self::WrongMachine { expected, found } => write!(f, "la licencia es para otra PC (Machine ID {expected}; esta PC es {found})"),
            Self::NoPublicKey => write!(f, "esta compilación no incluye clave pública de licencia"),
            Self::NoMachineId(e) => write!(f, "no se pudo obtener el Machine ID: {e}"),
            Self::Expired { expires } => write!(f, "la licencia venció el {}", fmt_date(*expires)),
            Self::Clock(clock::ClockFault::Rollback { seen, now }) => write!(f, "el reloj de esta PC está atrasado: ya se había usado TeatroCue el {} y ahora la fecha es {}. Corrija la fecha y hora de Windows", fmt_date(*seen), fmt_date(*now)),
            Self::Clock(clock::ClockFault::Tampered(why)) => write!(f, "el registro de uso de TeatroCue fue alterado ({why})"),
            Self::Clock(clock::ClockFault::MarksMissing) => write!(f, "falta el registro de uso de TeatroCue en esta PC (reinstale con el pendrive)"),
        }
    }
}

impl std::error::Error for LicenseError {}

/// Bytes que se firman: independientes del formato del archivo para que no haya ambigüedad.
fn signed_message(l: &License) -> Vec<u8> {
    format!("{MAGIC}\nlicensee={}\nmachine={}\nissued={}\nexpires={}\n", l.licensee, l.machine, l.issued_unix, l.expires_unix).into_bytes()
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn from_hex(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if s.len() % 2 != 0 || !s.is_ascii() {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
}

/// Mensaje firmado del formato anterior (sin vencimiento).
fn signed_message_v1(l: &License) -> Vec<u8> {
    format!("{MAGIC_V1}\nlicensee={}\nmachine={}\nissued={}\n", l.licensee, l.machine, l.issued_unix).into_bytes()
}

/// Emite el texto de `license.key` firmado con `key`. `expires_unix == 0` = sin vencimiento.
pub fn issue(licensee: &str, machine: &MachineId, issued_unix: u64, expires_unix: u64, key: &SigningKey) -> String {
    let l = License { licensee: licensee.trim().to_string(), machine: machine.clone(), issued_unix, expires_unix };
    let sig = key.sign(&signed_message(&l));
    format!("{MAGIC}\nlicensee: {}\nmachine: {}\nissued: {}\nexpires: {}\nsignature: {}\n", l.licensee, l.machine, l.issued_unix, l.expires_unix, to_hex(&sig.to_bytes()))
}

fn parse(text: &str) -> Result<(License, Signature, bool), LicenseError> {
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    let legacy = match lines.next() {
        Some(MAGIC) => false,
        Some(MAGIC_V1) => true,
        _ => return Err(LicenseError::Malformed("falta la cabecera TEATROCUE-LICENSE-2".into())),
    };
    let (mut licensee, mut machine, mut issued, mut expires, mut sig) = (None, None, None, None, None);
    for l in lines {
        let Some((k, v)) = l.split_once(':') else { return Err(LicenseError::Malformed(format!("línea inválida: `{l}`"))) };
        let v = v.trim();
        match k.trim() {
            "licensee" => licensee = Some(v.to_string()),
            "machine" => machine = Some(v.parse::<MachineId>().map_err(LicenseError::Malformed)?),
            "issued" => issued = Some(v.parse::<u64>().map_err(|_| LicenseError::Malformed("fecha de emisión inválida".into()))?),
            "expires" => expires = Some(v.parse::<u64>().map_err(|_| LicenseError::Malformed("fecha de vencimiento inválida".into()))?),
            "signature" => sig = Some(v.to_string()),
            _ => {}
        }
    }
    let missing = |what: &str| LicenseError::Malformed(format!("falta `{what}`"));
    let sig_bytes = from_hex(&sig.ok_or_else(|| missing("signature"))?).ok_or_else(|| LicenseError::Malformed("firma no hexadecimal".into()))?;
    let sig_arr: [u8; 64] = sig_bytes.try_into().map_err(|_| LicenseError::Malformed("la firma debe medir 64 bytes".into()))?;
    // Una licencia nueva SIEMPRE trae `expires` (si faltara, quitarlo no puede convertirla en eterna: está firmado).
    let expires_unix = if legacy { 0 } else { expires.ok_or_else(|| missing("expires"))? };
    Ok((
        License { licensee: licensee.ok_or_else(|| missing("licensee"))?, machine: machine.ok_or_else(|| missing("machine"))?, issued_unix: issued.ok_or_else(|| missing("issued"))?, expires_unix },
        Signature::from_bytes(&sig_arr),
        legacy,
    ))
}

/// Verifica `text` con `public` y comprueba que sea de la PC `local`.
pub fn verify_with(text: &str, public: &VerifyingKey, local: &MachineId) -> Result<License, LicenseError> {
    let (license, sig, legacy) = parse(text)?;
    // `verify_strict` rechaza firmas maleables y claves públicas de orden pequeño.
    let message = if legacy { signed_message_v1(&license) } else { signed_message(&license) };
    public.verify_strict(&message, &sig).map_err(|_| LicenseError::BadSignature)?;
    if &license.machine != local {
        return Err(LicenseError::WrongMachine { expected: license.machine.to_string(), found: local.to_string() });
    }
    Ok(license)
}

/// La clave pública incrustada, si el binario se compiló con una.
pub fn embedded_public_key() -> Option<VerifyingKey> {
    let bytes: [u8; 32] = from_hex(EMBEDDED_PUBLIC_KEY_HEX)?.try_into().ok()?;
    VerifyingKey::from_bytes(&bytes).ok()
}

pub fn verify_embedded(text: &str, local: &MachineId) -> Result<License, LicenseError> {
    let key = embedded_public_key().ok_or(LicenseError::NoPublicKey)?;
    verify_with(text, &key, local)
}

/// `license.key` junto al ejecutable en marcha.
pub fn license_path() -> PathBuf {
    std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join(LICENSE_FILE_NAME))).unwrap_or_else(|| PathBuf::from(LICENSE_FILE_NAME))
}

/// Estado de la licencia de esta PC, listo para mostrar.
#[derive(Debug)]
pub struct Status {
    pub machine: Option<MachineId>,
    pub result: Result<License, LicenseError>,
    pub path: PathBuf,
    /// Fecha efectiva (segundos Unix) con la que se juzgó la licencia.
    pub now: u64,
    /// Vencimiento vigente: el de la licencia o el de una reactivación de emergencia adjunta (`0` = no vence).
    pub effective_expires_unix: u64,
}

impl Status {
    pub fn is_valid(&self) -> bool {
        self.result.is_ok()
    }
}

/// Lee y verifica el `license.key` de `path` contra la clave pública incrustada, y luego el vencimiento y el reloj.
pub fn check_file(path: &Path) -> Status {
    let dir = path.parent().map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    check_with(path, now_unix(), &clock::default_stores(&dir), &clock::system_anchors(), embedded_public_key().as_ref())
}

/// Como [`check_file`], con todo inyectable (reloj, sitios de marcas, anclas y clave pública): lo usan las pruebas.
pub fn check_with(path: &Path, now: u64, stores: &[Box<dyn clock::MarkStore>], anchors: &[u64], public: Option<&VerifyingKey>) -> Status {
    check_with_machine(path, now, stores, anchors, public, MachineId::local())
}

/// Como [`check_with`], con el Machine ID de la PC también inyectado.
pub fn check_with_machine(path: &Path, now: u64, stores: &[Box<dyn clock::MarkStore>], anchors: &[u64], public: Option<&VerifyingKey>, machine: Result<MachineId, String>) -> Status {
    let mut effective_expires_unix = 0;
    let result = (|| {
        let local = machine.clone().map_err(LicenseError::NoMachineId)?;
        let text = std::fs::read_to_string(path).map_err(|e| if e.kind() == std::io::ErrorKind::NotFound { LicenseError::NotFound(path.to_path_buf()) } else { LicenseError::Io(e.to_string()) })?;
        let public = public.ok_or(LicenseError::NoPublicKey)?;
        let license = verify_with(&text, public, &local)?;
        // Una clave de reactivación de emergencia adjunta (firmada, de esta PC) puede alargar el vencimiento.
        effective_expires_unix = activation::effective_expires(&license, activation::best_attached(&text, public, &local).as_ref());
        // El reloj creíble es el mayor entre el de la PC y todo lo que se vio antes; con él se juzga el vencimiento.
        let effective = clock::check(now, &license, stores, anchors).map_err(LicenseError::Clock)?;
        // La fecha vista se registra SIEMPRE (también si ya venció): si no, bastaría adelantar el reloj, ver el
        // rechazo y volver atrás para que se «olvidara».
        clock::record(effective, &license, stores);
        if effective_expires_unix != 0 && effective >= effective_expires_unix {
            return Err(LicenseError::Expired { expires: effective_expires_unix });
        }
        Ok(license)
    })();
    Status { machine: machine.ok(), result, path: path.to_path_buf(), now, effective_expires_unix }
}

/// Escribe `text` en `path` de forma atómica (archivo temporal + renombrado).
pub(crate) fn write_atomic(path: &Path, text: &str) -> Result<(), String> {
    let tmp = path.with_extension("key.tmp");
    std::fs::write(&tmp, text).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("{}: {e}", path.display())
    })
}

/// Licencia instalada junto al ejecutable.
pub fn check_installed() -> Status {
    check_file(&license_path())
}

/// Texto para el operador cuando la licencia se rechaza (`None` si es válida): motivo y Machine ID de esta PC.
pub fn lock_message(status: &Status) -> Option<String> {
    let Err(e) = &status.result else { return None };
    let id = status.machine.as_ref().map_or("(no disponible)".to_string(), ToString::to_string);
    Some(format!("TeatroCue no está autorizado en esta PC.\nMotivo: {e}\nMachine ID de esta PC: {id}\nEnvíe este Machine ID al desarrollador para recibir su license.key y cópielo junto a la aplicación."))
}

/// `AAAA-MM-DD` (UTC) de unos segundos Unix.
pub fn fmt_date(unix: u64) -> String {
    // Días civiles (Howard Hinnant).
    let z = (unix / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    format!("{:04}-{:02}-{:02}", y + (m <= 2) as i64, m, d)
}

/// Segundos Unix actuales.
pub fn now_unix() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand_core::OsRng;

    fn pair() -> (SigningKey, VerifyingKey) {
        let k = SigningKey::generate(&mut OsRng);
        let v = k.verifying_key();
        (k, v)
    }

    fn id(s: &str) -> MachineId {
        MachineId::from_guid(s)
    }

    #[test]
    fn a_license_verifies_for_its_machine_only() {
        let (sk, pk) = pair();
        let (here, there) = (id("guid-de-la-pc-del-teatro"), id("otra-pc"));
        let text = issue("Ballet Oficial de Córdoba", &here, 1_800_000_000, 1_900_000_000, &sk);
        let l = verify_with(&text, &pk, &here).unwrap();
        assert_eq!((l.licensee.as_str(), l.issued_unix, l.expires_unix), ("Ballet Oficial de Córdoba", 1_800_000_000, 1_900_000_000));
        assert_eq!(l.machine, here);
        match verify_with(&text, &pk, &there) {
            Err(LicenseError::WrongMachine { expected, found }) => assert_eq!((expected, found), (here.to_string(), there.to_string())),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn tampering_or_a_foreign_key_is_rejected() {
        let (sk, pk) = pair();
        let m = id("pc");
        let text = issue("Ballet Oficial de Córdoba", &m, 1, 2, &sk);
        // Cambiar la institución, la fecha o la máquina invalida la firma.
        for (from, to) in [("Ballet Oficial de Córdoba", "Otro Teatro"), ("issued: 1", "issued: 9"), ("expires: 2", "expires: 99999999999")] {
            assert_eq!(verify_with(&text.replace(from, to), &pk, &m), Err(LicenseError::BadSignature), "{from}");
        }
        let swapped = text.replace(&m.to_string(), &id("otra").to_string());
        assert_eq!(verify_with(&swapped, &pk, &id("otra")), Err(LicenseError::BadSignature));
        // Firmada por otra clave (un tercero que quiera fabricar licencias).
        let (other_sk, _) = pair();
        assert_eq!(verify_with(&issue("Ballet Oficial de Córdoba", &m, 1, 2, &other_sk), &pk, &m), Err(LicenseError::BadSignature));
        // Firma truncada, cabecera ajena y texto vacío.
        let cut = text.lines().filter(|l| !l.starts_with("signature")).collect::<Vec<_>>().join("\n");
        assert!(matches!(verify_with(&cut, &pk, &m), Err(LicenseError::Malformed(_))));
        assert!(matches!(verify_with("hola", &pk, &m), Err(LicenseError::Malformed(_))));
        assert!(matches!(verify_with("", &pk, &m), Err(LicenseError::Malformed(_))));
        let short = text.replace("signature: ", "signature: 00");
        assert!(matches!(verify_with(&short, &pk, &m), Err(LicenseError::Malformed(_))));
    }

    #[test]
    fn hex_roundtrip_and_the_embedded_key_is_optional() {
        assert_eq!(from_hex("00ff10"), Some(vec![0, 255, 16]));
        assert_eq!(from_hex("0"), None);
        assert_eq!(from_hex("zz"), None);
        // Sin `TEATROCUE_PUBLIC_KEY_FILE` al compilar, nada se valida (y se dice por qué).
        if embedded_public_key().is_none() {
            assert_eq!(verify_embedded("x", &id("a")), Err(LicenseError::NoPublicKey));
        }
    }

    #[test]
    fn missing_license_files_are_reported_with_their_path() {
        let p = std::env::temp_dir().join(format!("teatrocue-no-license-{}.key", std::process::id()));
        let st = check_file(&p);
        assert!(!st.is_valid());
        assert!(matches!(&st.result, Err(LicenseError::NotFound(q)) if q == &p) || matches!(&st.result, Err(LicenseError::NoMachineId(_)) | Err(LicenseError::NoPublicKey)));
    }

    #[test]
    fn the_lock_message_names_the_reason_and_the_machine_id() {
        let ok = Status { machine: Some(id("a")), result: Ok(License { licensee: "x".into(), machine: id("a"), issued_unix: 0, expires_unix: 0 }), path: PathBuf::new(), now: 0, effective_expires_unix: 0 };
        assert!(ok.is_valid() && lock_message(&ok).is_none());
        let bad = Status { machine: Some(id("a")), result: Err(LicenseError::BadSignature), path: PathBuf::new(), now: 0, effective_expires_unix: 0 };
        let m = lock_message(&bad).unwrap();
        assert!(m.contains(&id("a").to_string()) && m.contains("firma") && m.contains("no está autorizado"), "{m}");
        let none = Status { machine: None, result: Err(LicenseError::NoPublicKey), path: PathBuf::new(), now: 0, effective_expires_unix: 0 };
        assert!(lock_message(&none).unwrap().contains("(no disponible)"));
    }

    // -- Vencimiento y reloj ------------------------------------------------------------------------

    use crate::clock::{self, tests::Mem, ClockFault, MarkStore, BUILD_UNIX};

    const DAY: u64 = 86_400;

    /// Escenario completo: licencia emitida para esta PC (la local del que prueba), tres sitios de marcas en memoria.
    struct Rig {
        sk: SigningKey,
        path: PathBuf,
        stores: Vec<Box<dyn MarkStore>>,
        mems: [Mem; 3],
        t0: u64,
    }

    impl Rig {
        fn new(tag: &str, days: Option<u64>) -> Option<Self> {
            let machine = MachineId::local().ok()?; // sin /etc/machine-id no hay forma de probarlo aquí
            let sk = SigningKey::generate(&mut OsRng);
            let t0 = BUILD_UNIX + 60 * DAY;
            let path = std::env::temp_dir().join(format!("teatrocue-lic-{}-{tag}.key", std::process::id()));
            let expires = days.map_or(0, |d| t0 + d * DAY);
            std::fs::write(&path, issue("Ballet Oficial de Córdoba", &machine, t0, expires, &sk)).unwrap();
            let mems = [Mem::default(), Mem::default(), Mem::default()];
            let stores = mems.iter().cloned().map(|m| Box::new(m) as Box<dyn MarkStore>).collect();
            Some(Self { sk, path, stores, mems, t0 })
        }

        fn at(&self, now: u64) -> Status {
            check_with(&self.path, now, &self.stores, &[], Some(&self.sk.verifying_key()))
        }
    }

    #[test]
    fn a_license_expires_on_its_date_and_warns_before() {
        let Some(r) = Rig::new("expiry", Some(30)) else { return };
        let ok = r.at(r.t0 + 1);
        let l = ok.result.as_ref().unwrap();
        assert_eq!((l.expires_unix, l.days_left(r.t0 + 1)), (r.t0 + 30 * DAY, Some(29)));
        assert_eq!(r.at(r.t0 + 29 * DAY).result.unwrap().days_left(r.t0 + 29 * DAY), Some(1));
        // El día del vencimiento y después: rechazada, con la fecha en el mensaje.
        let st = r.at(r.t0 + 30 * DAY);
        assert_eq!(st.result, Err(LicenseError::Expired { expires: r.t0 + 30 * DAY }));
        assert!(lock_message(&st).unwrap().contains(&fmt_date(r.t0 + 30 * DAY)));
        assert!(matches!(r.at(r.t0 + 90 * DAY).result, Err(LicenseError::Expired { .. })));
    }

    #[test]
    fn a_perpetual_license_never_expires_but_still_refuses_a_rolled_back_clock() {
        let Some(r) = Rig::new("perp", None) else { return };
        assert!(r.at(r.t0).result.unwrap().is_perpetual());
        assert!(r.at(r.t0 + 3000 * DAY).result.is_ok());
        assert!(matches!(r.at(r.t0 + DAY).result, Err(LicenseError::Clock(ClockFault::Rollback { .. }))), "la marca ya llegó a +3000 días");
    }

    #[test]
    fn setting_the_clock_back_to_stretch_a_license_does_not_work() {
        let Some(r) = Rig::new("rollback", Some(30)) else { return };
        assert!(r.at(r.t0 + 60).result.is_ok(), "primer arranque: graba las marcas (el instalador también lo hace)");
        assert!(r.at(r.t0 + 20 * DAY).result.is_ok(), "uso normal hasta el día 20");
        // Vence el día 30; el operador retrasa el reloj al día 5 para «recuperar» días.
        assert!(matches!(r.at(r.t0 + 5 * DAY).result, Err(LicenseError::Clock(ClockFault::Rollback { .. }))));
        // Aunque borre dos de las tres marcas…
        *r.mems[0].0.lock().unwrap() = None;
        *r.mems[1].0.lock().unwrap() = None;
        assert!(matches!(r.at(r.t0 + 5 * DAY).result, Err(LicenseError::Clock(ClockFault::Rollback { .. }))));
        // …o las tres, con una licencia que ya lleva semanas en uso.
        *r.mems[2].0.lock().unwrap() = None;
        assert!(matches!(r.at(r.t0 + 5 * DAY).result, Err(LicenseError::Clock(ClockFault::MarksMissing))));
        // Y el reloj, con la fecha real, tras corregirlo: ya no se puede borrar el rastro para salir del bloqueo
        // a mano, pero con las marcas intactas basta volver a la fecha correcta.
    }

    #[test]
    fn the_clock_cannot_be_set_forward_and_back_to_escape_either() {
        let Some(r) = Rig::new("forward", Some(30)) else { return };
        assert!(r.at(r.t0 + 60).result.is_ok());
        // Adelantar el reloj pasada la fecha de vencimiento deja una marca alta: ya no se puede «arreglar» volviendo.
        assert!(matches!(r.at(r.t0 + 40 * DAY).result, Err(LicenseError::Expired { .. })));
        assert!(matches!(r.at(r.t0 + 10 * DAY).result, Err(LicenseError::Clock(ClockFault::Rollback { .. }))), "volver atrás tras ver el rechazo no «olvida» la fecha");
    }

    #[test]
    fn expiry_cannot_be_removed_from_the_file() {
        let Some(r) = Rig::new("strip", Some(1)) else { return };
        let text = std::fs::read_to_string(&r.path).unwrap();
        // Quitar la línea de vencimiento: una licencia nueva siempre la trae.
        let stripped: String = text.lines().filter(|l| !l.starts_with("expires")).map(|l| format!("{l}\n")).collect();
        std::fs::write(&r.path, &stripped).unwrap();
        assert!(matches!(r.at(r.t0).result, Err(LicenseError::Malformed(_))));
        // Y presentarla con la cabecera antigua (sin vencimiento) rompe la firma.
        std::fs::write(&r.path, stripped.replace("LICENSE-2", "LICENSE-1")).unwrap();
        assert_eq!(r.at(r.t0).result, Err(LicenseError::BadSignature));
        // Cambiar la fecha de vencimiento también.
        std::fs::write(&r.path, text.replace(&format!("expires: {}", r.t0 + DAY), &format!("expires: {}", r.t0 + 999 * DAY))).unwrap();
        assert_eq!(r.at(r.t0).result, Err(LicenseError::BadSignature));
    }

    #[test]
    fn licenses_in_the_old_format_still_verify_as_perpetual() {
        let (sk, pk) = pair();
        let m = id("pc");
        // Lo que firmaba la versión anterior.
        let old = License { licensee: "Ballet Oficial de Córdoba".into(), machine: m.clone(), issued_unix: 7, expires_unix: 0 };
        let sig = sk.sign(&signed_message_v1(&old));
        let text = format!("{MAGIC_V1}\nlicensee: {}\nmachine: {m}\nissued: 7\nsignature: {}\n", old.licensee, to_hex(&sig.to_bytes()));
        let l = verify_with(&text, &pk, &m).unwrap();
        assert!(l.is_perpetual());
    }

    #[test]
    fn dates_and_remaining_days_are_computed_in_utc() {
        assert_eq!(fmt_date(0), "1970-01-01");
        assert_eq!(fmt_date(1_798_761_600), "2027-01-01");
        let l = clock::tests::lic(0, 10 * DAY);
        assert_eq!((l.days_left(0), l.days_left(10 * DAY - 1), l.days_left(10 * DAY), l.days_left(12 * DAY)), (Some(10), Some(0), Some(0), Some(-2)));
        assert_eq!(clock::tests::lic(0, 0).days_left(5), None);
    }
}
