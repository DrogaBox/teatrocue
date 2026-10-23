//! `installer-tool sign-challenge --code REQ-… --days N`: firma la clave de reactivación de emergencia.
//!
//! Corre solo en la máquina del desarrollador: descifra `master.key.enc` en memoria con la contraseña maestra,
//! firma y descarta la clave. La clave privada nunca se escribe en claro.

use std::path::{Path, PathBuf};

use ed25519_dalek::SigningKey;
use qlab_license::activation::{sign_challenge, ChallengeCode, MAX_EMERGENCY_DAYS};
use qlab_license::{embedded_public_key, keyfile};

pub const USAGE: &str = "Uso:\n  installer-tool sign-challenge --code <REQ-XXXX-XXXX-XXXX-XXXX-EEC> --days <N> [--key <master.key.enc>]\n\nLa contraseña maestra se pide por consola (o TEATROCUE_MASTER_PASSWORD para automatizar).";

#[derive(Debug, PartialEq, Eq)]
pub struct SignArgs {
    pub code: String,
    pub days: u32,
    pub key_path: Option<PathBuf>,
}

pub fn parse_args(args: &[String]) -> Result<SignArgs, String> {
    let mut it = args.iter();
    match it.next().map(String::as_str) {
        Some("sign-challenge") => {}
        Some(other) => return Err(format!("comando desconocido `{other}`\n{USAGE}")),
        None => return Err(USAGE.to_string()),
    }
    let (mut code, mut days, mut key_path) = (None, None, None);
    while let Some(flag) = it.next() {
        let mut value = |name: &str| it.next().cloned().ok_or_else(|| format!("{name} necesita un valor"));
        match flag.as_str() {
            "--code" => code = Some(value("--code")?),
            "--days" => {
                let v = value("--days")?;
                days = Some(v.trim().parse::<u32>().map_err(|_| format!("--days debe ser un número entero de días, no `{v}`"))?);
            }
            "--key" => key_path = Some(PathBuf::from(value("--key")?)),
            other => return Err(format!("opción desconocida `{other}`\n{USAGE}")),
        }
    }
    let days = days.ok_or("falta --days")?;
    if !(1..=MAX_EMERGENCY_DAYS).contains(&days) {
        return Err(format!("--days debe estar entre 1 y {MAX_EMERGENCY_DAYS}"));
    }
    Ok(SignArgs { code: code.ok_or("falta --code")?, days, key_path })
}

/// Dónde buscar `master.key.enc` si no se indicó: variable, junto al ejecutable, o la carpeta de claves del desarrollador.
pub fn default_key_candidates() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Ok(p) = std::env::var("TEATROCUE_MASTER_KEY") {
        v.push(PathBuf::from(p));
    }
    if let Some(dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)) {
        v.push(dir.join("master.key.enc"));
    }
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        v.push(PathBuf::from(home).join(".config/teatrocue-keys/master.key.enc"));
    }
    v.push(PathBuf::from("master.key.enc"));
    v
}

pub fn find_key(explicit: Option<&Path>) -> Result<PathBuf, String> {
    if let Some(p) = explicit {
        return p.exists().then(|| p.to_path_buf()).ok_or_else(|| format!("no existe {}", p.display()));
    }
    default_key_candidates().into_iter().find(|p| p.exists()).ok_or("no se encontró master.key.enc (use --key <ruta>)".into())
}

/// Firma. `now` es la fecha del desarrollador (segundos Unix). Devuelve el bloque de texto para imprimir.
pub fn sign(args: &SignArgs, key: &SigningKey, now: u64) -> Result<String, String> {
    let challenge: ChallengeCode = args.code.parse().map_err(|e: qlab_license::activation::ActivationError| e.to_string())?;
    // Si este binario conoce la clave pública del paquete, no firma con una clave que los programas no reconocerían.
    if let Some(public) = embedded_public_key() {
        if public != key.verifying_key() {
            return Err("la clave maestra no corresponde a la clave pública de este paquete; las claves emitidas no serían válidas".into());
        }
    }
    let (activation, response) = sign_challenge(&challenge, args.days, now, key);
    Ok(format!(
        "Solicitud: {challenge}\nPC (Machine ID): {}\nValida hasta: {}  (canjear antes del {})\n\nCLAVE DE ACTIVACIÓN GENERADA (Válida por {} días): [{response}]",
        challenge.machine,
        qlab_license::fmt_date(activation.valid_until),
        qlab_license::fmt_date(activation.redeem_by),
        args.days
    ))
}

pub fn load_key(path: &Path, password: &str) -> Result<SigningKey, String> {
    let data = std::fs::read(path).map_err(|e| format!("no se pudo leer {}: {e}", path.display()))?;
    keyfile::decrypt(&data, password).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use qlab_license::activation::challenge_for;
    use qlab_license::clock::MarkStore;
    use qlab_license::MachineId;

    fn argv(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn parses_arguments_and_rejects_bad_ones() {
        let a = parse_args(&argv("sign-challenge --code REQ-1 --days 15")).unwrap();
        assert_eq!((a.code.as_str(), a.days, a.key_path), ("REQ-1", 15, None));
        assert_eq!(parse_args(&argv("sign-challenge --days 3 --code X --key /k")).unwrap().key_path, Some("/k".into()));
        for bad in ["", "firmar --code X --days 3", "sign-challenge --code X", "sign-challenge --days 3", "sign-challenge --code X --days 0", "sign-challenge --code X --days 366", "sign-challenge --code X --days tres", "sign-challenge --code X --days 3 --raro"] {
            assert!(parse_args(&argv(bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn encrypted_key_signs_a_challenge_the_app_side_accepts() {
        let dir = std::env::temp_dir().join(format!("teatrocue-tool-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let enc = dir.join("master.key.enc");
        std::fs::write(&enc, keyfile::encrypt(&sk, "laclave", 1_000)).unwrap();
        assert!(load_key(&enc, "otra").is_err());
        let loaded = load_key(&enc, "laclave").unwrap();
        assert_eq!(find_key(Some(&enc)).unwrap(), enc);

        // Lado app: licencia vencida de una PC y su código de solicitud.
        let machine = MachineId::from_guid("pc-del-teatro");
        let t0 = qlab_license::clock::BUILD_UNIX + 10 * 86_400;
        let lic_path = dir.join("license.key");
        std::fs::write(&lic_path, qlab_license::issue("Ballet", &machine, t0, t0 + 86_400, &sk)).unwrap();
        let license = qlab_license::verify_with(&std::fs::read_to_string(&lic_path).unwrap(), &sk.verifying_key(), &machine).unwrap();
        let stores: Vec<Box<dyn MarkStore>> = Vec::new();
        let code = challenge_for(&license, &stores, None).to_string();

        // Lado desarrollador.
        let now = t0 + 5 * 86_400;
        let out = sign(&SignArgs { code: code.clone(), days: 10, key_path: None }, &loaded, now).unwrap();
        assert!(out.contains("CLAVE DE ACTIVACIÓN GENERADA (Válida por 10 días): [RESP-"), "{out}");
        let response = &out[out.find("[RESP-").unwrap() + 1..out.rfind(']').unwrap()];
        // Un código con un carácter mal copiado no se firma.
        let mut typo = code.clone();
        typo.replace_range(5..6, if &code[5..6] == "A" { "B" } else { "A" });
        assert!(sign(&SignArgs { code: typo, days: 10, key_path: None }, &loaded, now).is_err());
        // Sin marcas de reloj el lado app no puede canjear (la prueba completa con marcas está en qlab-license);
        // lo que se comprueba aquí es que la respuesta se verifica con la pública.
        let key = qlab_license::activation::verify_activation(response, &sk.verifying_key()).unwrap();
        assert_eq!((key.machine, key.epoch, key.valid_until), (machine.clone(), 0, now + 10 * 86_400));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Vector de prueba compartido con `tools/generador_licencia_movil.html`: el mismo archivo lo verifica el JS
    /// (`tools/probar_generador.mjs`), así Rust y el navegador firman byte a byte lo mismo.
    #[test]
    fn mobile_generator_test_vector_matches_rust_signature() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools/vector_firma.json");
        let machine = MachineId::from_guid("pc-del-teatro");
        let sk_bytes = [7u8; 32];
        let sk = SigningKey::from_bytes(&sk_bytes);
        let now = 1_800_000_000u64;
        let cases: Vec<(u32, u32)> = vec![(0, 7), (3, 30), (40, 365)];
        if std::env::var("REGENERAR_VECTOR").is_ok() {
            let enc = keyfile::encrypt(&sk, "laclave", 1_000);
            let items: Vec<String> = cases
                .iter()
                .map(|&(epoch, days)| {
                    let code = ChallengeCode { machine: machine.clone(), epoch };
                    let (_, resp) = qlab_license::activation::sign_challenge(&code, days, now, &sk);
                    format!("{{\"req\":\"{code}\",\"days\":{days},\"resp\":\"{resp}\"}}")
                })
                .collect();
            let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
            std::fs::write(&path, format!("{{\"keyfile_hex\":\"{}\",\"password\":\"laclave\",\"now\":{now},\"cases\":[{}]}}\n", hex(&enc), items.join(","))).unwrap();
        }
        let text = std::fs::read_to_string(&path).expect("tools/vector_firma.json (REGENERAR_VECTOR=1 cargo test)");
        // El archivo cifrado del vector se descifra con la rutina de Rust y las firmas se reproducen exactas.
        let hex = text.split("\"keyfile_hex\":\"").nth(1).unwrap().split('"').next().unwrap();
        let bytes: Vec<u8> = (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap()).collect();
        assert_eq!(keyfile::decrypt(&bytes, "laclave").unwrap().to_bytes(), sk_bytes);
        for &(epoch, days) in &cases {
            let code = ChallengeCode { machine: machine.clone(), epoch };
            let (_, resp) = qlab_license::activation::sign_challenge(&code, days, now, &sk);
            assert!(text.contains(&format!("\"req\":\"{code}\",\"days\":{days},\"resp\":\"{resp}\"")));
        }
    }
}
