//! Comprueba, una por una, que las licencias guardadas en `licencias.db` (el texto completo de cada `license.key`) están
//! firmadas por la clave maestra y son de la PC que dicen. Uso:
//!   cargo run -p teatrocue-admin --example verificar_licencias -- [licencias.db] [public.key]

use qlab_license::{fmt_date, now_unix, verify_with, MachineId};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let home = std::env::var("HOME").expect("HOME");
    let db_path = args.first().cloned().unwrap_or_else(|| format!("{home}/.config/teatrocue-keys/licencias.db"));
    let key_path = args.get(1).cloned().unwrap_or_else(|| format!("{home}/.config/teatrocue-keys/public.key"));
    let hex = std::fs::read_to_string(&key_path).expect("no se pudo leer public.key");
    let bytes: [u8; 32] = qlab_license::from_hex(hex.trim()).and_then(|v| v.try_into().ok()).expect("public.key inválida");
    let public = ed25519_dalek::VerifyingKey::from_bytes(&bytes).expect("clave pública inválida");
    let db = licencias_db::Db::open(std::path::Path::new(&db_path)).expect("no se pudo abrir la base");
    let now = now_unix();
    let mut bad = 0;
    for e in db.todos_los_equipos().expect("equipos") {
        let machine: MachineId = e.machine_id.parse().expect("Machine ID inválido");
        for m in db.historial(e.id).expect("historial") {
            match verify_with(&m.token_generado, &public, &machine) {
                Ok(l) => {
                    let estado = match l.days_left(now) {
                        None => "ACTIVA · sin vencimiento".to_string(),
                        Some(d) if d < 0 => format!("VENCIDA el {}", fmt_date(l.expires_unix)),
                        Some(d) => format!("ACTIVA · vence el {} (en {d} días)", fmt_date(l.expires_unix)),
                    };
                    println!("[OK]  {:<8} {}  firma válida, PC correcta · {} · {estado}", e.puesto_sala, e.machine_id, l.licensee);
                }
                Err(err) => {
                    bad += 1;
                    println!("[MAL] {:<8} {}  {err:?}", e.puesto_sala, e.machine_id);
                }
            }
        }
    }
    if bad > 0 {
        std::process::exit(1);
    }
}
