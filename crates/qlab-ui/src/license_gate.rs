//! Estado de la licencia tal como lo ve la interfaz: pantalla de bloqueo y datos para la ayuda.

use qlab_license::{LicenseError, Status};

/// Desde cuántos días antes del vencimiento se avisa al operador.
pub const EXPIRY_WARNING_DAYS: i64 = 14;

/// Resultado de comprobar `license.key`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LicenseState {
    /// Firma válida y de esta PC.
    Valid { licensee: String, issued_unix: u64, expires_unix: u64, days_left: Option<i64> },
    /// Rechazada: la aplicación queda bloqueada.
    Invalid { reason: String },
    /// Compilación de desarrollo: la licencia no se exige.
    NotEnforced,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LicenseInfo {
    pub state: LicenseState,
    /// Machine ID de esta PC (`XXXX-XXXX-XXXX-XXXX`), si se pudo leer.
    pub machine_id: Option<String>,
    /// Texto completo para el operador cuando está bloqueada.
    pub lock_text: Option<String>,
    /// La licencia venció: se puede pedir una reactivación de emergencia (desafío-respuesta).
    pub reactivable: bool,
}

/// `DD/MM/AAAA` (UTC) de unos segundos Unix.
pub fn fmt_dmy(unix: u64) -> String {
    let iso = qlab_license::fmt_date(unix);
    let mut p = iso.split('-');
    match (p.next(), p.next(), p.next()) {
        (Some(y), Some(m), Some(d)) => format!("{d}/{m}/{y}"),
        _ => iso,
    }
}

impl LicenseInfo {
    /// Sin exigir licencia (pruebas y compilaciones de desarrollo).
    pub fn not_enforced() -> Self {
        Self { state: LicenseState::NotEnforced, machine_id: qlab_license::MachineId::local().ok().map(|m| m.to_string()), lock_text: None, reactivable: false }
    }

    pub fn from_status(status: &Status) -> Self {
        let machine_id = status.machine.as_ref().map(ToString::to_string);
        match &status.result {
            Ok(l) => {
                // El vencimiento vigente puede ser el de una reactivación de emergencia adjunta, no el de la licencia base.
                let expires = status.effective_expires_unix;
                let days_left = (expires != 0).then(|| (expires as i64 - status.now as i64).div_euclid(86_400));
                Self { state: LicenseState::Valid { licensee: l.licensee.clone(), issued_unix: l.issued_unix, expires_unix: expires, days_left }, machine_id, lock_text: None, reactivable: false }
            }
            Err(e) => Self { state: LicenseState::Invalid { reason: reason_text(e) }, machine_id, lock_text: qlab_license::lock_message(status), reactivable: matches!(e, LicenseError::Expired { .. }) },
        }
    }

    /// Comprueba la licencia instalada si esta compilación la exige.
    pub fn detect() -> Self {
        if cfg!(feature = "enforce-license") { Self::from_status(&qlab_license::check_installed()) } else { Self::not_enforced() }
    }

    /// ¿Hay que bloquear la aplicación?
    pub fn is_locked(&self) -> bool {
        matches!(self.state, LicenseState::Invalid { .. })
    }

    /// Aviso para el operador si la licencia vence pronto (`None` si no vence o falta mucho).
    pub fn expiry_warning(&self) -> Option<String> {
        let LicenseState::Valid { expires_unix, days_left: Some(d), .. } = &self.state else { return None };
        (*d <= EXPIRY_WARNING_DAYS).then(|| {
            let when = qlab_license::fmt_date(*expires_unix);
            match *d {
                0 => format!("La licencia de TeatroCue vence HOY ({when}). Renuévela con el pendrive antes de la próxima función."),
                1 => format!("La licencia de TeatroCue vence MAÑANA ({when}). Renuévela con el pendrive."),
                n => format!("La licencia de TeatroCue vence en {n} días ({when}). Renuévela con el pendrive."),
            }
        })
    }
}

fn reason_text(e: &LicenseError) -> String {
    e.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use qlab_license::{License, MachineId};

    fn status(result: Result<License, LicenseError>) -> Status {
        let effective_expires_unix = result.as_ref().map_or(0, |l| l.expires_unix);
        Status { machine: Some(MachineId::from_guid("pc")), result, path: "license.key".into(), now: 0, effective_expires_unix }
    }

    #[test]
    fn a_valid_license_unlocks_and_names_the_licensee() {
        let l = License { licensee: "Ballet Oficial de Córdoba".into(), machine: MachineId::from_guid("pc"), issued_unix: 5, expires_unix: 5 + 20 * 86_400 };
        let info = LicenseInfo::from_status(&status(Ok(l)));
        assert!(!info.is_locked() && info.lock_text.is_none());
        assert_eq!(info.state, LicenseState::Valid { licensee: "Ballet Oficial de Córdoba".into(), issued_unix: 5, expires_unix: 5 + 20 * 86_400, days_left: Some(20) });
        assert_eq!(info.machine_id, Some(MachineId::from_guid("pc").to_string()));
    }

    #[test]
    fn the_operator_is_warned_in_the_last_two_weeks_only() {
        let at = |days: Option<i64>| LicenseInfo { state: LicenseState::Valid { licensee: "x".into(), issued_unix: 0, expires_unix: 86_400 * 100, days_left: days }, machine_id: None, lock_text: None, reactivable: false };
        assert!(at(None).expiry_warning().is_none(), "sin vencimiento: nada que avisar");
        assert!(at(Some(15)).expiry_warning().is_none());
        assert!(at(Some(14)).expiry_warning().unwrap().contains("vence en 14 días"));
        assert!(at(Some(1)).expiry_warning().unwrap().contains("MAÑANA"));
        assert!(at(Some(0)).expiry_warning().unwrap().contains("HOY"));
        assert!(LicenseInfo::not_enforced().expiry_warning().is_none());
    }

    #[test]
    fn a_rejected_license_locks_and_shows_the_machine_id() {
        for e in [LicenseError::BadSignature, LicenseError::NotFound("x".into()), LicenseError::NoPublicKey] {
            let info = LicenseInfo::from_status(&status(Err(e)));
            assert!(info.is_locked());
            assert!(info.lock_text.as_deref().is_some_and(|t| t.contains(&MachineId::from_guid("pc").to_string())));
        }
    }

    #[test]
    fn development_builds_do_not_lock() {
        let info = LicenseInfo::not_enforced();
        assert!(!info.is_locked() && info.state == LicenseState::NotEnforced);
        if !cfg!(feature = "enforce-license") {
            assert_eq!(LicenseInfo::detect().state, LicenseState::NotEnforced);
        }
    }
}
