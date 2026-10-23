//! Reactivación de emergencia por desafío-respuesta (offline).
//!
//! Si la licencia venció y el desarrollador no puede ir con el pendrive:
//!
//! 1. TeatroCue muestra un **código de solicitud** (`REQ-XXXX-XXXX-XXXX-XXXX-EEC`): el Machine ID de la PC y la
//!    «época» (cuántas reactivaciones de emergencia se han canjeado ya), más un carácter de control.
//! 2. El operador lo manda por mensaje; el desarrollador lo firma con `installer-tool sign-challenge --days N` y
//!    devuelve una **clave de activación** (`RESP-…`, Base32 Crockford, sin caracteres ambiguos ni símbolos).
//! 3. El operador la pega en TeatroCue: se valida la firma, que sea de esta PC y que la época no sea anterior a la
//!    local (anti-replay); si todo cuadra se adjunta a `license.key`, la época sube y las marcas de reloj se renuevan.
//!
//! La clave lleva una **fecha absoluta** de fin (`valid_until`) y un plazo para canjearla (`redeem_by`, 7 días):
//! repetir una clave vieja nunca da más tiempo del que ya dio, y no se pueden acumular claves para usarlas más tarde.
//! La aplicación solo conoce la clave pública: no puede firmar nada por sí misma, por eso la clave se **adjunta** a
//! `license.key` (línea `activation:`) en lugar de reescribir la licencia base.

use std::path::Path;

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};

use crate::clock::{self, MarkStore};
use crate::{from_hex, License, MachineId};

/// Alfabeto Base32 de Crockford: sin I, L, O ni U (que se confunden con 1, 0 y V).
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
const VERSION: u8 = 1;
const DOMAIN: &[u8] = b"TeatroCue/activation/v1\n";
/// Plazo para canjear una clave desde que se firma.
pub const REDEEM_WINDOW_SECS: u64 = 7 * 86_400;
/// Máximo de días que se puede conceder en una reactivación de emergencia.
pub const MAX_EMERGENCY_DAYS: u32 = 365;
const PAYLOAD_LEN: usize = 1 + 8 + 4 + 4 + 4;
const BLOB_LEN: usize = PAYLOAD_LEN + 64;
const ACTIVATION_PREFIX: &str = "activation:";

// ---------------------------------------------------------------------------------------------
// Base32 de Crockford
// ---------------------------------------------------------------------------------------------

pub fn b32_encode(bytes: &[u8]) -> String {
    let (mut out, mut buf, mut bits) = (String::new(), 0u32, 0u32);
    for &b in bytes {
        buf = (buf << 8) | b as u32;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPHABET[((buf >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(ALPHABET[((buf << (5 - bits)) & 31) as usize] as char);
    }
    out
}

fn symbol(c: char) -> Option<u8> {
    // I y L se leen como 1; O como 0 (las confusiones típicas al dictar o copiar).
    let c = match c.to_ascii_uppercase() {
        'O' => '0',
        'I' | 'L' => '1',
        o => o,
    };
    ALPHABET.iter().position(|&a| a as char == c).map(|p| p as u8)
}

/// Decodifica ignorando espacios y guiones. Error con el carácter inválido y su posición.
pub fn b32_decode(text: &str) -> Result<Vec<u8>, String> {
    let (mut out, mut buf, mut bits) = (Vec::new(), 0u32, 0u32);
    for (i, c) in text.chars().filter(|c| !c.is_whitespace() && *c != '-').enumerate() {
        let v = symbol(c).ok_or_else(|| format!("carácter inválido `{c}` en la posición {}", i + 1))?;
        buf = (buf << 5) | v as u32;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xff) as u8);
        }
    }
    // Los bits sobrantes son relleno y deben ser cero (si no, el texto no es de una codificación nuestra).
    if bits > 0 && (buf & ((1 << bits) - 1)) != 0 {
        return Err("relleno final inválido".into());
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// Desafío (lo que muestra la PC)
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChallengeCode {
    pub machine: MachineId,
    /// Reactivaciones de emergencia ya canjeadas en esta PC.
    pub epoch: u32,
}

fn epoch_digits(epoch: u32) -> String {
    let mut digits = Vec::new();
    let mut e = epoch;
    loop {
        digits.push(ALPHABET[(e & 31) as usize] as char);
        e >>= 5;
        if e == 0 {
            break;
        }
    }
    while digits.len() < 2 {
        digits.push('0');
    }
    digits.iter().rev().collect()
}

/// Carácter de control: suma ponderada (pesos impares, invertibles módulo 32) módulo 32 de todos los símbolos del código.
fn check_char(data: &str) -> char {
    let sum: u32 = data.chars().filter_map(symbol).enumerate().map(|(i, v)| (v as u32 + 1) * (2 * i as u32 + 1)).sum();
    ALPHABET[(sum % 32) as usize] as char
}

impl std::fmt::Display for ChallengeCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let digits = epoch_digits(self.epoch);
        let data = format!("{}{digits}", self.machine.as_str().replace('-', ""));
        write!(f, "REQ-{}-{digits}{}", self.machine, check_char(&data))
    }
}

impl std::str::FromStr for ChallengeCode {
    type Err = ActivationError;

    fn from_str(s: &str) -> Result<Self, ActivationError> {
        let bad = |m: &str| ActivationError::Malformed(format!("código de solicitud inválido: {m}"));
        let clean: String = s.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect::<String>().to_ascii_uppercase();
        let parts: Vec<&str> = clean.split('-').filter(|p| !p.is_empty()).collect();
        if parts.len() != 6 || parts[0] != "REQ" {
            return Err(bad("debe tener la forma REQ-XXXX-XXXX-XXXX-XXXX-EEC"));
        }
        let tail = parts[5];
        if tail.len() < 3 || tail.len() > 9 {
            return Err(bad("falta la época o el carácter de control"));
        }
        // La parte del Machine ID es hexadecimal: las confusiones O/I/L se corrigen igual que en Base32.
        let norm = |g: &str| g.chars().map(|c| match c { 'O' => '0', 'I' | 'L' => '1', o => o }).collect::<String>();
        let machine: MachineId = parts[1..5].iter().map(|g| norm(g)).collect::<Vec<_>>().join("-").parse().map_err(|e: String| bad(&e))?;
        let (digits, check) = tail.split_at(tail.len() - 1);
        let mut epoch: u64 = 0;
        for c in digits.chars() {
            epoch = (epoch << 5) | symbol(c).ok_or_else(|| bad("época con caracteres inválidos"))? as u64;
        }
        let epoch = u32::try_from(epoch).map_err(|_| bad("época fuera de rango"))?;
        let canonical = ChallengeCode { machine, epoch };
        let want = canonical.to_string();
        // El control se calcula sobre la forma canónica: así un carácter mal copiado se detecta antes de firmar nada.
        let got_check = check.chars().next().and_then(|c| symbol(c).map(|v| ALPHABET[v as usize] as char));
        if want.chars().last() != got_check {
            return Err(bad("el carácter de control no coincide (¿se copió mal alguna letra?)"));
        }
        Ok(canonical)
    }
}

// ---------------------------------------------------------------------------------------------
// Respuesta (lo que firma el desarrollador)
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActivationKey {
    pub machine: MachineId,
    /// Época del desafío al que responde.
    pub epoch: u32,
    /// Fin de la licencia que concede (segundos Unix, absoluto).
    pub valid_until: u64,
    /// Última fecha en que se puede canjear.
    pub redeem_by: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ActivationError {
    Malformed(String),
    BadSignature,
    WrongMachine { expected: String, found: String },
    /// La época de la clave es anterior a la de esta PC: la clave ya se usó (o es de un desafío antiguo).
    Replay { key_epoch: u32, local_epoch: u32 },
    /// Pasó el plazo para canjearla.
    KeyTooOld { redeem_by: u64 },
    /// La clave no añade tiempo: la licencia actual ya vence igual o después.
    NoExtension { current: u64, granted: u64 },
    /// La licencia no vence: no hay nada que extender.
    Perpetual,
    NoPublicKey,
    /// No hay una licencia base válida de esta PC sobre la que extender.
    NoLicense(String),
    Clock(String),
    Io(String),
}

impl std::fmt::Display for ActivationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed(m) => write!(f, "{m}"),
            Self::BadSignature => write!(f, "la clave de activación no es auténtica (firma inválida): revise que esté completa y sin cambios"),
            Self::WrongMachine { expected, found } => write!(f, "esta clave es para otra PC (Machine ID {expected}; esta PC es {found})"),
            Self::Replay { key_epoch, local_epoch } => write!(f, "esta clave ya fue usada o es de una solicitud anterior (época {key_epoch}; esta PC está en la {local_epoch}). Pida un código nuevo"),
            Self::KeyTooOld { redeem_by } => write!(f, "la clave caducó el {}: pida un código nuevo", crate::fmt_date(*redeem_by)),
            Self::NoExtension { current, granted } => write!(f, "la clave no extiende la licencia (vence el {} y la clave llega hasta el {})", crate::fmt_date(*current), crate::fmt_date(*granted)),
            Self::Perpetual => write!(f, "la licencia de esta PC no vence: no necesita reactivación"),
            Self::NoPublicKey => write!(f, "esta compilación no incluye clave pública de licencia"),
            Self::NoLicense(m) => write!(f, "no hay una licencia base válida sobre la que extender: {m}"),
            Self::Clock(m) => write!(f, "el reloj de esta PC no es creíble: {m}"),
            Self::Io(m) => write!(f, "no se pudo escribir la licencia: {m}"),
        }
    }
}

impl std::error::Error for ActivationError {}

fn payload(k: &ActivationKey) -> Vec<u8> {
    let mut p = Vec::with_capacity(PAYLOAD_LEN);
    p.push(VERSION);
    // El Machine ID son 16 caracteres hexadecimales = 8 bytes.
    p.extend(from_hex(&k.machine.as_str().replace('-', "")).expect("un MachineId siempre es hexadecimal"));
    p.extend((k.epoch).to_be_bytes());
    p.extend(u32::try_from(k.valid_until).unwrap_or(u32::MAX).to_be_bytes());
    p.extend(u32::try_from(k.redeem_by).unwrap_or(u32::MAX).to_be_bytes());
    p
}

fn message(payload: &[u8]) -> Vec<u8> {
    [DOMAIN, payload].concat()
}

/// Firma la respuesta a `challenge`: concede `days` días contados desde `now` (la fecha del desarrollador).
pub fn sign_challenge(challenge: &ChallengeCode, days: u32, now: u64, key: &SigningKey) -> (ActivationKey, String) {
    let k = ActivationKey { machine: challenge.machine.clone(), epoch: challenge.epoch, valid_until: now + u64::from(days.clamp(1, MAX_EMERGENCY_DAYS)) * 86_400, redeem_by: now + REDEEM_WINDOW_SECS };
    let p = payload(&k);
    let sig = key.sign(&message(&p));
    let blob = [p, sig.to_bytes().to_vec()].concat();
    let body = b32_encode(&blob);
    let groups: Vec<&str> = body.as_bytes().chunks(8).map(|c| std::str::from_utf8(c).expect("ascii")).collect();
    (k, format!("RESP-{}", groups.join("-")))
}

/// Extrae el texto de una clave aunque venga rodeado de corchetes, saltos de línea o texto del mensaje.
pub fn extract_response(text: &str) -> Option<String> {
    let up = text.to_ascii_uppercase();
    let at = up.find("RESP-").or_else(|| up.find("RESP"))?;
    Some(up[at..].chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || c.is_whitespace()).collect::<String>().trim().to_string())
}

pub fn parse_activation(text: &str) -> Result<(ActivationKey, Signature), ActivationError> {
    let t = extract_response(text).ok_or_else(|| ActivationError::Malformed("la clave debe empezar con RESP-".into()))?;
    let body = t.trim_start_matches("RESP");
    let blob = b32_decode(body).map_err(|e| ActivationError::Malformed(format!("clave de activación inválida: {e}")))?;
    if blob.len() != BLOB_LEN {
        return Err(ActivationError::Malformed(format!("clave de activación incompleta o con caracteres de más ({} de {BLOB_LEN} bytes): cópiela entera", blob.len())));
    }
    let (p, sig) = blob.split_at(PAYLOAD_LEN);
    if p[0] != VERSION {
        return Err(ActivationError::Malformed("versión de clave desconocida".into()));
    }
    let hex: String = p[1..9].iter().map(|b| format!("{b:02X}")).collect();
    let machine: MachineId = format!("{}-{}-{}-{}", &hex[0..4], &hex[4..8], &hex[8..12], &hex[12..16]).parse().map_err(ActivationError::Malformed)?;
    let u32_at = |i: usize| u32::from_be_bytes(p[i..i + 4].try_into().expect("4 bytes"));
    let key = ActivationKey { machine, epoch: u32_at(9), valid_until: u64::from(u32_at(13)), redeem_by: u64::from(u32_at(17)) };
    let sig: [u8; 64] = sig.try_into().expect("64 bytes");
    Ok((key, Signature::from_bytes(&sig)))
}

/// Verifica la firma. No mira la PC, la época ni las fechas: eso lo decide [`apply_activation_response`].
pub fn verify_activation(text: &str, public: &VerifyingKey) -> Result<ActivationKey, ActivationError> {
    let (key, sig) = parse_activation(text)?;
    public.verify_strict(&message(&payload(&key)), &sig).map_err(|_| ActivationError::BadSignature)?;
    Ok(key)
}

/// La mejor clave adjunta a un `license.key` (firma y PC válidas): la que llega más lejos.
pub fn best_attached(license_text: &str, public: &VerifyingKey, local: &MachineId) -> Option<ActivationKey> {
    license_text
        .lines()
        .filter_map(|l| l.trim().strip_prefix(ACTIVATION_PREFIX))
        .filter_map(|t| verify_activation(t, public).ok())
        .filter(|k| &k.machine == local)
        .max_by_key(|k| k.valid_until)
}

/// Todo lo que necesita aplicar una clave (inyectable para poder probarlo).
pub struct Env<'a> {
    pub license_path: &'a Path,
    pub now: u64,
    pub stores: &'a [Box<dyn MarkStore>],
    pub anchors: &'a [u64],
    pub public: &'a VerifyingKey,
    pub local: &'a MachineId,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Applied {
    pub valid_until: u64,
    /// Época de esta PC tras canjear la clave.
    pub epoch: u32,
}

/// Época de esta PC: la mayor entre las marcas de reloj y la de la clave adjunta (+1). Así borrar las marcas no
/// «rebobina» el contador mientras siga la clave en `license.key`.
pub fn local_epoch(license: &License, stores: &[Box<dyn MarkStore>], attached: Option<&ActivationKey>) -> u32 {
    clock::epoch(license, stores).max(attached.map_or(0, |a| a.epoch.saturating_add(1)))
}

/// Solicitud de reactivación de esta PC: su Machine ID y la época actual.
pub fn challenge_for(license: &License, stores: &[Box<dyn MarkStore>], attached: Option<&ActivationKey>) -> ChallengeCode {
    ChallengeCode { machine: license.machine.clone(), epoch: local_epoch(license, stores, attached) }
}

/// Valida `response` y, si es buena, la adjunta a `license.key` y actualiza las marcas.
///
/// Orden de comprobaciones (cada una falla sin tocar nada): firma → PC → licencia base de esta PC → reloj creíble →
/// época ≥ la local (anti-replay) → plazo de canje → que realmente extienda.
pub fn apply_activation_response(env: &Env, response: &str) -> Result<Applied, ActivationError> {
    let key = verify_activation(response, env.public)?;
    if &key.machine != env.local {
        return Err(ActivationError::WrongMachine { expected: key.machine.to_string(), found: env.local.to_string() });
    }
    let text = std::fs::read_to_string(env.license_path).map_err(|e| ActivationError::NoLicense(format!("{}: {e}", env.license_path.display())))?;
    let license = crate::verify_with(&text, env.public, env.local).map_err(|e| ActivationError::NoLicense(e.to_string()))?;
    // Con la fecha efectiva (la mayor entre el reloj y lo ya visto): atrasar el reloj no sirve para canjear una clave caduca.
    let now = clock::check(env.now, &license, env.stores, env.anchors).map_err(|f| ActivationError::Clock(crate::LicenseError::Clock(f).to_string()))?;
    let attached = best_attached(&text, env.public, env.local);
    let local_epoch = local_epoch(&license, env.stores, attached.as_ref());
    if key.epoch < local_epoch {
        return Err(ActivationError::Replay { key_epoch: key.epoch, local_epoch });
    }
    if now > key.redeem_by {
        return Err(ActivationError::KeyTooOld { redeem_by: key.redeem_by });
    }
    if license.is_perpetual() {
        return Err(ActivationError::Perpetual);
    }
    let current = effective_expires(&license, best_attached(&text, env.public, env.local).as_ref());
    if key.valid_until <= current {
        return Err(ActivationError::NoExtension { current, granted: key.valid_until });
    }
    // Se adjunta (sustituyendo claves anteriores) y se escribe de forma atómica.
    let base: String = text.lines().filter(|l| !l.trim_start().starts_with(ACTIVATION_PREFIX)).map(|l| format!("{l}\n")).collect();
    let new_text = format!("{base}{ACTIVATION_PREFIX} {}\n", response_canonical(response));
    crate::write_atomic(env.license_path, &new_text).map_err(ActivationError::Io)?;
    let epoch = key.epoch.saturating_add(1);
    let high = env.stores.iter().filter_map(|s| s.read()).filter_map(|t| clock::parse_mark(&t, &license).ok()).max().unwrap_or(0).max(now);
    if clock::reset_with_epoch(high, epoch, &license, env.stores) == 0 {
        return Err(ActivationError::Io("no se pudo escribir el registro de uso en ningún sitio".into()));
    }
    Ok(Applied { valid_until: key.valid_until, epoch })
}

fn response_canonical(response: &str) -> String {
    extract_response(response).unwrap_or_default().split_whitespace().collect::<String>()
}

/// Vencimiento efectivo: el de la licencia base o el de la mejor clave adjunta, el que llegue más lejos (`0` = nunca vence).
pub fn effective_expires(license: &License, attached: Option<&ActivationKey>) -> u64 {
    if license.is_perpetual() {
        return 0;
    }
    license.expires_unix.max(attached.map_or(0, |a| a.valid_until))
}

/// Todo lo del entorno de una PC (licencia, marcas, reloj, clave pública, Machine ID) en un solo valor clonable.
/// El daemon y la interfaz lo comparten; las pruebas inyectan el suyo.
#[derive(Clone)]
pub struct Setup {
    pub license_path: std::path::PathBuf,
    pub stores: std::sync::Arc<Vec<Box<dyn MarkStore>>>,
    pub anchors: Vec<u64>,
    pub public: Option<VerifyingKey>,
    pub machine: Result<MachineId, String>,
    /// Fecha fija en las pruebas; `None` = el reloj del sistema.
    pub now: Option<u64>,
}

impl std::fmt::Debug for Setup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Setup").field("license_path", &self.license_path).field("machine", &self.machine).finish_non_exhaustive()
    }
}

impl Setup {
    /// La licencia instalada junto al ejecutable, con los sitios de marcas y anclas reales de esta PC.
    pub fn installed() -> Self {
        let path = crate::license_path();
        let dir = path.parent().map_or_else(|| std::path::PathBuf::from("."), Path::to_path_buf);
        Self { stores: std::sync::Arc::new(clock::default_stores(&dir)), anchors: clock::system_anchors(), public: crate::embedded_public_key(), machine: MachineId::local(), license_path: path, now: None }
    }

    fn now(&self) -> u64 {
        self.now.unwrap_or_else(crate::now_unix)
    }

    /// Estado de la licencia (como al arrancar). Registra la fecha vista, igual que el arranque.
    pub fn status(&self) -> crate::Status {
        crate::check_with_machine(&self.license_path, self.now(), &self.stores, &self.anchors, self.public.as_ref(), self.machine.clone())
    }

    fn base(&self) -> Result<(String, License, MachineId, VerifyingKey), ActivationError> {
        let public = self.public.ok_or(ActivationError::NoPublicKey)?;
        let local = self.machine.clone().map_err(ActivationError::NoLicense)?;
        let text = std::fs::read_to_string(&self.license_path).map_err(|e| ActivationError::NoLicense(format!("{}: {e}", self.license_path.display())))?;
        let license = crate::verify_with(&text, &public, &local).map_err(|e| ActivationError::NoLicense(e.to_string()))?;
        Ok((text, license, local, public))
    }

    /// Código de solicitud de esta PC (necesita una licencia base válida de esta PC).
    pub fn challenge(&self) -> Result<ChallengeCode, ActivationError> {
        let (text, license, local, public) = self.base()?;
        Ok(challenge_for(&license, &self.stores, best_attached(&text, &public, &local).as_ref()))
    }

    /// Canjea una clave de activación.
    pub fn apply(&self, response: &str) -> Result<Applied, ActivationError> {
        let (_, _, local, public) = self.base()?;
        apply_activation_response(&Env { license_path: &self.license_path, now: self.now(), stores: &self.stores, anchors: &self.anchors, public: &public, local: &local }, response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::tests::Mem;
    use crate::{issue, verify_with};
    use rand_core::OsRng;

    const DAY: u64 = 86_400;

    #[test]
    fn base32_roundtrips_and_tolerates_confusable_characters_and_separators() {
        for len in [0usize, 1, 5, 21, 85, 100] {
            let data: Vec<u8> = (0..len).map(|i| (i * 37 + 11) as u8).collect();
            let text = b32_encode(&data);
            assert!(text.chars().all(|c| ALPHABET.contains(&(c as u8))), "{text}");
            assert_eq!(b32_decode(&text).unwrap(), data, "len {len}");
            let spaced: String = text.as_bytes().chunks(6).map(|c| std::str::from_utf8(c).unwrap()).collect::<Vec<_>>().join(" - ");
            assert_eq!(b32_decode(&spaced.to_lowercase()).unwrap(), data);
        }
        // O → 0, I/L → 1.
        assert_eq!(b32_decode("O1ILO1IL").unwrap(), b32_decode("01110111").unwrap());
        assert!(b32_decode("ABCU").unwrap_err().contains('U'));
        assert!(b32_decode("AB!D").is_err());
    }

    #[test]
    fn challenge_codes_have_the_documented_shape_and_roundtrip() {
        let m = MachineId::from_guid("pc-del-teatro");
        for epoch in [0u32, 1, 31, 32, 1023, 1024, 70_000, u32::MAX] {
            let c = ChallengeCode { machine: m.clone(), epoch };
            let text = c.to_string();
            assert!(text.starts_with("REQ-") && text.matches('-').count() == 5, "{text}");
            assert_eq!(text.parse::<ChallengeCode>().unwrap(), c, "{text}");
        }
        let c = ChallengeCode { machine: m.clone(), epoch: 0 };
        // El operador puede escribirlo en minúsculas, con espacios o con la confusión O/0.
        let sloppy = c.to_string().to_lowercase().replace('-', " - ");
        assert_eq!(sloppy.parse::<ChallengeCode>().unwrap(), c);
        assert!(c.to_string().replace('0', "O").parse::<ChallengeCode>().is_ok());
    }

    #[test]
    fn a_mistyped_challenge_is_caught_by_the_check_character() {
        let c = ChallengeCode { machine: MachineId::from_guid("x"), epoch: 7 };
        let text = c.to_string();
        let alphabet: Vec<char> = ALPHABET.iter().map(|&b| b as char).collect();
        let mut caught = 0;
        let mut total = 0;
        for (i, ch) in text.char_indices().filter(|(_, c)| *c != '-') {
            if i < 4 {
                continue; // «REQ-»
            }
            for &alt in alphabet.iter().filter(|&&a| a != ch) {
                let mut t = text.clone();
                t.replace_range(i..=i, &alt.to_string());
                total += 1;
                match t.parse::<ChallengeCode>() {
                    Err(_) => caught += 1,
                    Ok(p) => assert_ne!(p, c, "una alteración no puede dar el mismo código: {t}"),
                }
            }
        }
        // El control atrapa todo cambio de un solo carácter (la suma ponderada es inyectiva por posición).
        assert_eq!(caught, total, "{caught}/{total}");
        for bad in ["", "REQ", "REQ-1234", "RESP-AAAA-BBBB-CCCC-DDDD-001", "REQ-ZZZZ-ZZZZ-ZZZZ-ZZZZ-001"] {
            assert!(bad.parse::<ChallengeCode>().is_err(), "{bad}");
        }
    }

    // -- Escenario completo -------------------------------------------------------------------------

    struct Rig {
        sk: SigningKey,
        path: std::path::PathBuf,
        stores: Vec<Box<dyn MarkStore>>,
        machine: MachineId,
        t0: u64,
    }

    impl Rig {
        /// Licencia de 30 días ya vencida a +40 días, con las marcas de uso grabadas (como tras el instalador).
        fn new(tag: &str) -> Self {
            let sk = SigningKey::generate(&mut OsRng);
            let machine = MachineId::from_guid("pc-del-teatro");
            let t0 = clock::BUILD_UNIX + 100 * DAY;
            let path = std::env::temp_dir().join(format!("teatrocue-act-{}-{tag}.key", std::process::id()));
            let _ = std::fs::remove_file(&path);
            std::fs::write(&path, issue("Ballet Oficial de Córdoba", &machine, t0, t0 + 30 * DAY, &sk)).unwrap();
            let stores: Vec<Box<dyn MarkStore>> = (0..3).map(|_| Box::new(Mem::default()) as Box<dyn MarkStore>).collect();
            let r = Self { sk, path, stores, machine, t0 };
            let lic = r.license();
            clock::reset(t0, &lic, &r.stores);
            r
        }

        fn license(&self) -> License {
            verify_with(&std::fs::read_to_string(&self.path).unwrap(), &self.sk.verifying_key(), &self.machine).unwrap()
        }

        fn env<'a>(&'a self, now: u64, public: &'a VerifyingKey, local: &'a MachineId) -> Env<'a> {
            Env { license_path: &self.path, now, stores: &self.stores, anchors: &[], public, local }
        }

        fn challenge(&self) -> ChallengeCode {
            challenge_for(&self.license(), &self.stores, best_attached(&std::fs::read_to_string(&self.path).unwrap(), &self.sk.verifying_key(), &self.machine).as_ref())
        }

        fn status(&self, now: u64) -> crate::Status {
            crate::check_with_machine(&self.path, now, &self.stores, &[], Some(&self.sk.verifying_key()), Ok(self.machine.clone()))
        }
    }

    #[test]
    fn challenge_sign_activate_unlocks_an_expired_license() {
        let r = Rig::new("flow");
        let pk = r.sk.verifying_key();
        let now = r.t0 + 40 * DAY; // la licencia venció hace 10 días
        assert!(matches!(r.status(now).result, Err(crate::LicenseError::Expired { .. })));
        let code = r.challenge().to_string();
        assert!(code.starts_with("REQ-"));
        // El desarrollador firma el código recibido por mensaje: 15 días desde su «hoy».
        let (key, response) = sign_challenge(&code.parse().unwrap(), 15, now, &r.sk);
        assert!(response.starts_with("RESP-") && response.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
        assert_eq!(key.valid_until, now + 15 * DAY);
        // El operador la pega (con el texto del mensaje alrededor, como llega por WhatsApp).
        let pasted = format!("Hola! Tu clave: [{response}] Saludos");
        let applied = apply_activation_response(&r.env(now + 60, &pk, &r.machine), &pasted).unwrap();
        assert_eq!((applied.valid_until, applied.epoch), (now + 15 * DAY, 1));
        // La licencia base no cambió; la clave quedó adjunta y es lo que extiende.
        let text = std::fs::read_to_string(&r.path).unwrap();
        assert_eq!(text.matches("activation:").count(), 1);
        assert_eq!(r.license().expires_unix, r.t0 + 30 * DAY, "la licencia base sigue igual (está firmada)");
        let attached = best_attached(&text, &pk, &r.machine).unwrap();
        assert_eq!(effective_expires(&r.license(), Some(&attached)), now + 15 * DAY);
        // La época subió en las marcas y el desafío siguiente ya la refleja.
        assert_eq!(clock::epoch(&r.license(), &r.stores), 1);
        assert_eq!(r.challenge().epoch, 1);
        // El chequeo completo (como al arrancar) desbloquea.
        let st = r.status(now + 120);
        assert!(st.result.is_ok(), "{:?}", st.result);
        assert_eq!(st.effective_expires_unix, now + 15 * DAY);
        // …y vuelve a bloquear cuando pasa el nuevo vencimiento.
        assert!(matches!(r.status(now + 16 * DAY).result, Err(crate::LicenseError::Expired { .. })));
    }

    #[test]
    fn reusing_an_old_key_fails_anti_replay() {
        let r = Rig::new("replay");
        let pk = r.sk.verifying_key();
        let now = r.t0 + 40 * DAY;
        let (_, first) = sign_challenge(&r.challenge(), 15, now, &r.sk);
        apply_activation_response(&r.env(now, &pk, &r.machine), &first).unwrap();
        // La misma clave otra vez (quizá con otro espacio o minúsculas): ya no.
        let err = apply_activation_response(&r.env(now + 5, &pk, &r.machine), &first.to_lowercase()).unwrap_err();
        assert_eq!(err, ActivationError::Replay { key_epoch: 0, local_epoch: 1 });
        assert!(err.to_string().contains("ya fue usada"));
        // Una respuesta a un desafío posterior (época 1) sí vale, aunque la anterior no.
        let (_, second) = sign_challenge(&r.challenge(), 30, now + 20 * DAY, &r.sk);
        let ok = apply_activation_response(&r.env(now + 20 * DAY, &pk, &r.machine), &second).unwrap();
        assert_eq!(ok.epoch, 2);
        // Y la primera sigue sin servir.
        assert!(matches!(apply_activation_response(&r.env(now + 20 * DAY, &pk, &r.machine), &first), Err(ActivationError::Replay { .. })));
    }

    #[test]
    fn a_key_with_a_newer_epoch_than_the_local_one_is_accepted() {
        let r = Rig::new("newer");
        let pk = r.sk.verifying_key();
        let now = r.t0 + 40 * DAY;
        let challenge = ChallengeCode { machine: r.machine.clone(), epoch: 5 }; // el desarrollador firmó una época adelantada
        let (_, resp) = sign_challenge(&challenge, 10, now, &r.sk);
        let a = apply_activation_response(&r.env(now, &pk, &r.machine), &resp).unwrap();
        assert_eq!(a.epoch, 6, "tras canjear, la época local pasa a la de la clave + 1");
    }

    #[test]
    fn a_key_for_another_machine_is_refused() {
        let r = Rig::new("mismatch");
        let pk = r.sk.verifying_key();
        let now = r.t0 + 40 * DAY;
        let other = MachineId::from_guid("otra-pc");
        let (_, resp) = sign_challenge(&ChallengeCode { machine: other.clone(), epoch: 0 }, 10, now, &r.sk);
        match apply_activation_response(&r.env(now, &pk, &r.machine), &resp) {
            Err(ActivationError::WrongMachine { expected, found }) => assert_eq!((expected, found), (other.to_string(), r.machine.to_string())),
            other => panic!("{other:?}"),
        }
        assert!(!std::fs::read_to_string(&r.path).unwrap().contains("activation:"), "no se tocó nada");
        assert_eq!(clock::epoch(&r.license(), &r.stores), 0);
    }

    #[test]
    fn changing_any_single_character_of_the_key_is_detected() {
        let r = Rig::new("tamper");
        let pk = r.sk.verifying_key();
        let now = r.t0 + 40 * DAY;
        let (_, resp) = sign_challenge(&r.challenge(), 15, now, &r.sk);
        let alphabet: Vec<char> = ALPHABET.iter().map(|&b| b as char).collect();
        let body_start = "RESP-".len();
        let (mut tried, mut rejected) = (0, 0);
        for (i, ch) in resp.char_indices().filter(|(i, c)| *i >= body_start && *c != '-') {
            // Un carácter distinto en cada posición (el siguiente del alfabeto, saltando equivalentes O/0, I/L/1).
            let alt = alphabet[(alphabet.iter().position(|&a| a == ch).unwrap() + 7) % 32];
            let mut t = resp.clone();
            t.replace_range(i..=i, &alt.to_string());
            tried += 1;
            match apply_activation_response(&r.env(now, &pk, &r.machine), &t) {
                Err(ActivationError::BadSignature | ActivationError::Malformed(_) | ActivationError::WrongMachine { .. }) => rejected += 1,
                other => panic!("posición {i}: se aceptó una clave alterada o falló distinto: {other:?} ({t})"),
            }
        }
        assert_eq!((tried, rejected), (136, 136), "las 136 posiciones del cuerpo");
        // Una clave truncada o con caracteres extra tampoco.
        assert!(matches!(apply_activation_response(&r.env(now, &pk, &r.machine), &resp[..resp.len() - 9]), Err(ActivationError::Malformed(_))));
        assert!(matches!(apply_activation_response(&r.env(now, &pk, &r.machine), &format!("{resp}AB")), Err(ActivationError::Malformed(_))));
        // Y la original sigue valiendo: nada se estropeó por los intentos.
        assert!(apply_activation_response(&r.env(now, &pk, &r.machine), &resp).is_ok());
    }

    #[test]
    fn a_key_signed_by_someone_else_is_not_authentic() {
        let r = Rig::new("foreign");
        let pk = r.sk.verifying_key();
        let impostor = SigningKey::generate(&mut OsRng);
        let (_, resp) = sign_challenge(&r.challenge(), 365, r.t0 + 40 * DAY, &impostor);
        assert_eq!(apply_activation_response(&r.env(r.t0 + 40 * DAY, &pk, &r.machine), &resp), Err(ActivationError::BadSignature));
    }

    #[test]
    fn keys_expire_if_not_redeemed_in_time_and_cannot_be_hoarded() {
        let r = Rig::new("stale");
        let pk = r.sk.verifying_key();
        let signed_at = r.t0 + 40 * DAY;
        let (key, resp) = sign_challenge(&r.challenge(), 15, signed_at, &r.sk);
        assert_eq!(key.redeem_by, signed_at + REDEEM_WINDOW_SECS);
        let late = signed_at + REDEEM_WINDOW_SECS + 60;
        assert_eq!(apply_activation_response(&r.env(late, &pk, &r.machine), &resp), Err(ActivationError::KeyTooOld { redeem_by: key.redeem_by }));
        // Atrasar el reloj para «recuperar» el plazo no funciona: se usa la fecha efectiva (la mayor vista).
        let lic = r.license();
        clock::record(late, &lic, &r.stores);
        let back = apply_activation_response(&r.env(signed_at + 60, &pk, &r.machine), &resp);
        assert!(matches!(back, Err(ActivationError::Clock(_))), "{back:?}");
    }

    #[test]
    fn a_key_that_adds_no_time_is_refused_and_stacked_keys_keep_the_best() {
        let r = Rig::new("noext");
        let pk = r.sk.verifying_key();
        let now = r.t0 + 40 * DAY;
        // Concede hasta ANTES de lo que ya tiene la licencia (vencía a +30 d; esta llega a +35 d, desde 'now' = +40 → nada).
        let early = ChallengeCode { machine: r.machine.clone(), epoch: 0 };
        let (_, resp) = sign_challenge(&early, 1, r.t0, &r.sk); // valid_until = t0 + 1 d < t0 + 30 d
        let err = apply_activation_response(&r.env(r.t0 + 60, &pk, &r.machine), &resp).unwrap_err();
        assert!(matches!(err, ActivationError::NoExtension { .. }), "{err:?}");
        // Dos reactivaciones sucesivas: queda adjunta solo la última y la licencia efectiva es la mejor.
        let (_, a) = sign_challenge(&r.challenge(), 10, now, &r.sk);
        apply_activation_response(&r.env(now, &pk, &r.machine), &a).unwrap();
        let (_, b) = sign_challenge(&r.challenge(), 20, now + 11 * DAY, &r.sk);
        apply_activation_response(&r.env(now + 11 * DAY, &pk, &r.machine), &b).unwrap();
        let text = std::fs::read_to_string(&r.path).unwrap();
        assert_eq!(text.matches("activation:").count(), 1);
        assert_eq!(best_attached(&text, &pk, &r.machine).unwrap().valid_until, now + 11 * DAY + 20 * DAY);
    }

    #[test]
    fn without_a_valid_base_license_there_is_nothing_to_extend() {
        let r = Rig::new("nobase");
        let pk = r.sk.verifying_key();
        let (_, resp) = sign_challenge(&r.challenge(), 15, r.t0 + 40 * DAY, &r.sk);
        std::fs::remove_file(&r.path).unwrap();
        assert!(matches!(apply_activation_response(&r.env(r.t0 + 40 * DAY, &pk, &r.machine), &resp), Err(ActivationError::NoLicense(_))));
    }

    #[test]
    fn the_signed_payload_is_compact_and_the_response_format_is_stable() {
        let sk = SigningKey::generate(&mut OsRng);
        let (_, resp) = sign_challenge(&ChallengeCode { machine: MachineId::from_guid("a"), epoch: 3 }, 30, 1_800_000_000, &sk);
        let body: String = resp.trim_start_matches("RESP-").chars().filter(|c| *c != '-').collect();
        assert_eq!(body.len(), 136);
        assert_eq!(resp.split('-').count(), 1 + 17, "17 bloques de 8");
        assert_eq!(extract_response("Respuesta: [resp-abcd efgh] x").unwrap(), "RESP-ABCD EFGH");
    }
}
