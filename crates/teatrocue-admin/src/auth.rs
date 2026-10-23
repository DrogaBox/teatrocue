//! Sesión del desarrollador: la clave privada Ed25519 vive solo en RAM mientras la sesión esté desbloqueada.

use std::path::{Path, PathBuf};

use ed25519_dalek::SigningKey;
use qlab_license::activation::{sign_challenge, ActivationKey, ChallengeCode, MAX_EMERGENCY_DAYS};
use qlab_license::{embedded_public_key, issue, MachineId};

/// Duración de una licencia fija.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Vigencia {
    Meses1,
    Meses3,
    Meses6,
    Anio1,
    Perpetua,
    Dias(u32),
}

impl Vigencia {
    pub const FIJAS: [Self; 5] = [Self::Meses1, Self::Meses3, Self::Meses6, Self::Anio1, Self::Perpetua];

    pub fn label(self) -> String {
        match self {
            Self::Meses1 => "1 mes".into(),
            Self::Meses3 => "3 meses".into(),
            Self::Meses6 => "6 meses".into(),
            Self::Anio1 => "1 año".into(),
            Self::Perpetua => "Perpetua".into(),
            Self::Dias(d) => format!("{d} días"),
        }
    }

    /// Días que concede (`0` = perpetua).
    pub fn dias(self) -> u32 {
        match self {
            Self::Meses1 => 30,
            Self::Meses3 => 90,
            Self::Meses6 => 180,
            Self::Anio1 => 365,
            Self::Perpetua => 0,
            Self::Dias(d) => d,
        }
    }

    /// Fecha de vencimiento (Unix) a partir de `now`; `0` = perpetua.
    pub fn vence(self, now: u64) -> u64 {
        match self.dias() {
            0 => 0,
            d => now + u64::from(d) * 86_400,
        }
    }
}

/// Primer `master.key.enc` que exista: variable, junto al ejecutable o en la carpeta de claves del desarrollador.
pub fn find_master_key() -> Option<PathBuf> {
    installer_tool::find_key(None).ok()
}

#[derive(Default)]
pub struct Session {
    key: Option<SigningKey>,
}

impl Session {
    pub fn is_unlocked(&self) -> bool {
        self.key.is_some()
    }

    /// Descifra `master.key.enc` con la contraseña. Si es incorrecta, la sesión sigue bloqueada.
    pub fn unlock(&mut self, path: &Path, password: &str) -> Result<(), String> {
        self.key = None;
        let key = installer_tool::load_key(path, password)?;
        // Si este programa conoce la clave pública del paquete, no firma con una clave que los equipos no reconocerían.
        if let Some(public) = embedded_public_key() {
            if public != key.verifying_key() {
                return Err("la clave maestra no corresponde a la clave pública de este paquete: las licencias emitidas no serían válidas".into());
            }
        }
        self.key = Some(key);
        Ok(())
    }

    pub fn lock(&mut self) {
        self.key = None;
    }

    fn key(&self) -> Result<&SigningKey, String> {
        self.key.as_ref().ok_or_else(|| "la sesión está bloqueada: ingrese la contraseña maestra".to_string())
    }

    /// Texto de un `license.key` firmado para `machine`.
    pub fn license_file(&self, institucion: &str, machine: &MachineId, vigencia: Vigencia, now: u64) -> Result<String, String> {
        let key = self.key()?;
        if institucion.trim().is_empty() {
            return Err("falta la institución".into());
        }
        Ok(issue(institucion, machine, now, vigencia.vence(now), key))
    }

    /// Firma un código `REQ-…`: devuelve el desafío, la clave de activación y el texto `RESP-…`.
    pub fn sign_emergency(&self, code: &str, days: u32, now: u64) -> Result<(ChallengeCode, ActivationKey, String), String> {
        let key = self.key()?;
        if !(1..=MAX_EMERGENCY_DAYS).contains(&days) {
            return Err(format!("los días deben estar entre 1 y {MAX_EMERGENCY_DAYS}"));
        }
        let challenge: ChallengeCode = code.parse().map_err(|e: qlab_license::activation::ActivationError| e.to_string())?;
        let (activation, response) = sign_challenge(&challenge, days, now, key);
        Ok((challenge, activation, response))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qlab_license::activation::{challenge_for, verify_activation};
    use qlab_license::clock::MarkStore;
    use qlab_license::{keyfile, verify_with};

    fn keyfile_in(tag: &str, password: &str) -> (PathBuf, SigningKey) {
        let dir = std::env::temp_dir().join(format!("teatrocue-admin-auth-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sk = SigningKey::from_bytes(&[9u8; 32]);
        let p = dir.join("master.key.enc");
        std::fs::write(&p, keyfile::encrypt(&sk, password, 1_000)).unwrap();
        (p, sk)
    }

    #[test]
    fn a_wrong_password_keeps_the_session_locked_and_blocks_signing() {
        let (p, _) = keyfile_in("pw", "laclave");
        let mut s = Session::default();
        assert!(s.unlock(&p, "otra").is_err() && !s.is_unlocked());
        let machine = MachineId::from_guid("pc");
        assert!(s.license_file("Ballet", &machine, Vigencia::Meses1, 1_800_000_000).unwrap_err().contains("bloqueada"));
        assert!(s.sign_emergency("REQ-X", 7, 1).unwrap_err().contains("bloqueada"));
        s.unlock(&p, "laclave").unwrap();
        assert!(s.is_unlocked());
        s.lock();
        assert!(!s.is_unlocked());
    }

    #[test]
    fn issued_license_files_verify_for_that_machine_only_and_perpetual_never_expires() {
        let (p, sk) = keyfile_in("lic", "pw");
        let mut s = Session::default();
        s.unlock(&p, "pw").unwrap();
        let (m, other) = (MachineId::from_guid("sala-1"), MachineId::from_guid("sala-2"));
        let now = qlab_license::clock::BUILD_UNIX + 86_400;
        let text = s.license_file("  Ballet Oficial  ", &m, Vigencia::Meses3, now).unwrap();
        let l = verify_with(&text, &sk.verifying_key(), &m).unwrap();
        assert_eq!((l.licensee.as_str(), l.issued_unix, l.expires_unix), ("Ballet Oficial", now, now + 90 * 86_400));
        assert!(verify_with(&text, &sk.verifying_key(), &other).is_err());
        let forever = verify_with(&s.license_file("B", &m, Vigencia::Perpetua, now).unwrap(), &sk.verifying_key(), &m).unwrap();
        assert!(forever.is_perpetual());
        assert_eq!(Vigencia::Dias(45).vence(now), now + 45 * 86_400);
        assert!(s.license_file("", &m, Vigencia::Anio1, now).is_err());
    }

    #[test]
    fn emergency_codes_are_signed_with_the_same_result_the_command_line_tool_gives() {
        let (p, sk) = keyfile_in("req", "pw");
        let mut s = Session::default();
        s.unlock(&p, "pw").unwrap();
        let machine = MachineId::from_guid("cabina");
        let t0 = qlab_license::clock::BUILD_UNIX + 10 * 86_400;
        let lic = qlab_license::License { licensee: "Ballet".into(), machine: machine.clone(), issued_unix: t0, expires_unix: t0 + 86_400 };
        let stores: Vec<Box<dyn MarkStore>> = Vec::new();
        let code = challenge_for(&lic, &stores, None).to_string();
        let now = t0 + 5 * 86_400;
        let (challenge, act, resp) = s.sign_emergency(&code, 7, now).unwrap();
        assert_eq!((challenge.machine.clone(), act.valid_until), (machine.clone(), now + 7 * 86_400));
        assert!(resp.starts_with("RESP-"));
        let key = verify_activation(&resp, &sk.verifying_key()).unwrap();
        assert_eq!((key.machine, key.epoch), (machine, 0));
        assert!(s.sign_emergency(&code, 0, now).is_err() && s.sign_emergency(&code, 366, now).is_err());
        let mut typo = code.clone();
        typo.replace_range(5..6, if &code[5..6] == "A" { "B" } else { "A" });
        assert!(s.sign_emergency(&typo, 7, now).is_err(), "un código mal copiado no se firma");
    }
}
