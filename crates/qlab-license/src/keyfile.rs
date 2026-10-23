//! `master.key.enc`: la clave privada Ed25519 cifrada con una contraseña maestra (PBKDF2-HMAC-SHA256 + AES-256-GCM).
//!
//! ```text
//! "TCKEY1" | iteraciones (u32 BE) | sal (16) | nonce (12) | cifrado (32 + 16 de etiqueta GCM)
//! ```
//! La cabecera entera va como datos autenticados: cambiar las iteraciones o la sal invalida el descifrado.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use ed25519_dalek::SigningKey;
use rand_core::{OsRng, RngCore};
use zeroize::Zeroizing;

const MAGIC: &[u8; 6] = b"TCKEY1";
const HEADER_LEN: usize = 6 + 4 + 16 + 12;
/// Iteraciones de PBKDF2 para claves nuevas.
pub const DEFAULT_ITERATIONS: u32 = 600_000;
/// Cota al leer un archivo ajeno: evita que un archivo manipulado cuelgue la máquina.
const MAX_ITERATIONS: u32 = 20_000_000;

#[derive(Debug, PartialEq, Eq)]
pub enum KeyfileError {
    /// Contraseña incorrecta o archivo alterado (indistinguibles por diseño).
    BadPasswordOrCorrupt,
    Malformed(&'static str),
}

impl std::fmt::Display for KeyfileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadPasswordOrCorrupt => write!(f, "contraseña incorrecta o archivo de clave dañado"),
            Self::Malformed(m) => write!(f, "archivo de clave inválido: {m}"),
        }
    }
}

impl std::error::Error for KeyfileError {}

fn derive(password: &str, salt: &[u8], iterations: u32) -> Zeroizing<[u8; 32]> {
    let mut key = Zeroizing::new([0u8; 32]);
    pbkdf2::pbkdf2_hmac::<sha2::Sha256>(password.as_bytes(), salt, iterations, &mut *key);
    key
}

/// Cifra la clave privada con `password`.
pub fn encrypt(key: &SigningKey, password: &str, iterations: u32) -> Vec<u8> {
    let (mut salt, mut nonce) = ([0u8; 16], [0u8; 12]);
    OsRng.fill_bytes(&mut salt);
    OsRng.fill_bytes(&mut nonce);
    let mut header = Vec::with_capacity(HEADER_LEN);
    header.extend_from_slice(MAGIC);
    header.extend_from_slice(&iterations.to_be_bytes());
    header.extend_from_slice(&salt);
    header.extend_from_slice(&nonce);
    let k = derive(password, &salt, iterations);
    let cipher = Aes256Gcm::new_from_slice(&*k).expect("clave de 32 bytes");
    let seed = Zeroizing::new(key.to_bytes());
    let ct = cipher.encrypt(Nonce::from_slice(&nonce), Payload { msg: &*seed, aad: &header }).expect("cifrado AES-GCM");
    header.extend_from_slice(&ct);
    header
}

/// Descifra la clave privada. En memoria: el llamador la suelta en cuanto emite la licencia.
pub fn decrypt(data: &[u8], password: &str) -> Result<SigningKey, KeyfileError> {
    if data.len() != HEADER_LEN + 32 + 16 || &data[..6] != MAGIC {
        return Err(KeyfileError::Malformed("no es un master.key.enc de TeatroCue"));
    }
    let iterations = u32::from_be_bytes(data[6..10].try_into().expect("4 bytes"));
    if iterations == 0 || iterations > MAX_ITERATIONS {
        return Err(KeyfileError::Malformed("iteraciones fuera de rango"));
    }
    let (header, ct) = data.split_at(HEADER_LEN);
    let salt = &header[10..26];
    let nonce = &header[26..38];
    let k = derive(password, salt, iterations);
    let cipher = Aes256Gcm::new_from_slice(&*k).expect("clave de 32 bytes");
    let seed = Zeroizing::new(cipher.decrypt(Nonce::from_slice(nonce), Payload { msg: ct, aad: header }).map_err(|_| KeyfileError::BadPasswordOrCorrupt)?);
    let arr: [u8; 32] = seed.as_slice().try_into().map_err(|_| KeyfileError::Malformed("semilla de longitud inesperada"))?;
    Ok(SigningKey::from_bytes(&arr))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pocas iteraciones: las pruebas no necesitan el coste de producción.
    const FAST: u32 = 1_000;

    #[test]
    fn the_private_key_roundtrips_with_the_right_password_only() {
        let key = SigningKey::generate(&mut OsRng);
        let enc = encrypt(&key, "clave maestra del Ballet", FAST);
        assert_eq!(enc.len(), HEADER_LEN + 48);
        assert!(!enc.windows(32).any(|w| w == key.to_bytes()), "la clave privada no queda en claro");
        assert_eq!(decrypt(&enc, "clave maestra del Ballet").unwrap().to_bytes(), key.to_bytes());
        assert_eq!(decrypt(&enc, "otra").err(), Some(KeyfileError::BadPasswordOrCorrupt));
        assert_eq!(decrypt(&enc, "").err(), Some(KeyfileError::BadPasswordOrCorrupt));
    }

    #[test]
    fn every_encryption_is_different_and_tampering_is_detected() {
        let key = SigningKey::generate(&mut OsRng);
        let (a, b) = (encrypt(&key, "p", FAST), encrypt(&key, "p", FAST));
        assert_ne!(a, b, "sal y nonce nuevos cada vez");
        for i in [0, 7, 12, 30, HEADER_LEN + 3, a.len() - 1] {
            let mut t = a.clone();
            t[i] ^= 1;
            assert!(decrypt(&t, "p").is_err(), "byte {i}");
        }
        assert!(matches!(decrypt(&a[..a.len() - 1], "p"), Err(KeyfileError::Malformed(_))));
        assert!(matches!(decrypt(b"hola", "p"), Err(KeyfileError::Malformed(_))));
        // Un archivo que pide mil millones de iteraciones se rechaza sin ponerse a calcular.
        let mut huge = a.clone();
        huge[6..10].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(matches!(decrypt(&huge, "p"), Err(KeyfileError::Malformed(_))));
    }
}
