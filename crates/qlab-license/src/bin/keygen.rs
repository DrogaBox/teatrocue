//! `teatrocue-keygen`: genera el par de claves del desarrollador y emite licencias (uso en la máquina del
//! desarrollador; en el teatro lo hace `Instalar_TeatroCue.exe`).
//!
//! ```text
//! teatrocue-keygen init  --dir DIR            crea DIR/master.key.enc (privada, cifrada) y DIR/public.key
//! teatrocue-keygen issue --dir DIR --machine XXXX-XXXX-XXXX-XXXX (--days N | --perpetual) [--licensee "…"] [--out license.key]
//! teatrocue-keygen machine                    muestra el Machine ID de esta PC
//! ```
//! La contraseña maestra se pide por consola sin eco, o se toma de `TEATROCUE_MASTER_PASSWORD` (automatización).

use std::path::PathBuf;
use std::process::ExitCode;

use ed25519_dalek::SigningKey;
use qlab_license::{issue, keyfile, now_unix, MachineId};
use rand_core::OsRng;

const DEFAULT_LICENSEE: &str = "Ballet Oficial de Córdoba";

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

fn password(prompt: &str) -> Result<String, String> {
    if let Ok(p) = std::env::var("TEATROCUE_MASTER_PASSWORD") {
        return Ok(p);
    }
    rpassword::prompt_password(prompt).map_err(|e| format!("no se pudo leer la contraseña: {e}"))
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(1)
        }
    }
}

fn run(args: &[String]) -> Result<(), String> {
    match args.first().map(String::as_str) {
        Some("machine") => {
            println!("{}", MachineId::local()?);
            Ok(())
        }
        Some("init") => {
            let dir = PathBuf::from(arg(args, "--dir").ok_or("falta --dir")?);
            let (enc_path, pub_path) = (dir.join("master.key.enc"), dir.join("public.key"));
            if enc_path.exists() && pub_path.exists() && !args.iter().any(|a| a == "--force") {
                println!("ya existen las claves en {} (usa --force para regenerarlas: invalida TODAS las licencias emitidas)", dir.display());
                return Ok(());
            }
            let p1 = password("Contraseña maestra nueva: ")?;
            if p1.chars().count() < 12 {
                return Err("la contraseña maestra debe tener al menos 12 caracteres".into());
            }
            if std::env::var("TEATROCUE_MASTER_PASSWORD").is_err() && password("Repite la contraseña: ")? != p1 {
                return Err("las contraseñas no coinciden".into());
            }
            std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            let key = SigningKey::generate(&mut OsRng);
            std::fs::write(&enc_path, keyfile::encrypt(&key, &p1, keyfile::DEFAULT_ITERATIONS)).map_err(|e| e.to_string())?;
            let hex: String = key.verifying_key().to_bytes().iter().map(|b| format!("{b:02x}")).collect();
            std::fs::write(&pub_path, format!("{hex}\n")).map_err(|e| e.to_string())?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&enc_path, std::fs::Permissions::from_mode(0o600));
            }
            println!("claves creadas en {}\n  privada cifrada: {}\n  pública:         {}", dir.display(), enc_path.display(), pub_path.display());
            Ok(())
        }
        Some("issue") => {
            let dir = PathBuf::from(arg(args, "--dir").ok_or("falta --dir")?);
            let machine: MachineId = arg(args, "--machine").ok_or("falta --machine")?.parse()?;
            let licensee = arg(args, "--licensee").unwrap_or_else(|| DEFAULT_LICENSEE.to_string());
            let enc = std::fs::read(dir.join("master.key.enc")).map_err(|e| format!("master.key.enc: {e}"))?;
            let key = keyfile::decrypt(&enc, &password("Contraseña maestra: ")?).map_err(|e| e.to_string())?;
            let now = now_unix();
            let expires = match (arg(args, "--days"), args.iter().any(|a| a == "--perpetual")) {
                (Some(d), false) => now + d.parse::<u64>().map_err(|_| "--days debe ser un número de días")?.clamp(1, 3650) * 86_400,
                (None, true) => 0,
                _ => return Err("indique la duración: --days N o --perpetual".into()),
            };
            let text = issue(&licensee, &machine, now, expires, &key);
            match arg(args, "--out") {
                Some(out) => {
                    std::fs::write(&out, text).map_err(|e| format!("{out}: {e}"))?;
                    println!("licencia para {licensee} / {machine} escrita en {out}");
                }
                None => print!("{text}"),
            }
            Ok(())
        }
        _ => Err("uso: teatrocue-keygen init --dir DIR | issue --dir DIR --machine ID (--days N | --perpetual) [--licensee TXT] [--out ARCHIVO] | machine".into()),
    }
}
