//! Fusiona dos `licencias.db` desde la terminal (la misma fusión que usa `teatrocue-admin`) e imprime el resumen.
//! Uso: cargo run -p licencias-db --example sync -- <base-local> <base-del-pendrive>

use licencias_db::{dias_restantes, Db, Estado};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [local, remote] = args.as_slice() else {
        eprintln!("uso: sync <base-local> <base-del-pendrive>");
        std::process::exit(2);
    };
    let db = Db::open(std::path::Path::new(local)).expect("no se pudo abrir la base local");
    let s = licencias_db::sync_with_file(&db, std::path::Path::new(remote)).expect("falló la sincronización");
    println!("{s:#?}");
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
    println!("\n{:<10} {:<22} {:<8} {:<14} {}", "Sala", "Machine ID", "Estado", "Vence", "Licencias");
    for f in db.inventario(now).expect("inventario") {
        let vence = match f.vencimiento {
            None => "—".to_string(),
            Some(0) => "perpetua".to_string(),
            Some(v) => format!("{} ({} d)", v, dias_restantes(v, now).unwrap_or(0)),
        };
        let estado = match f.estado {
            Estado::Activa => "ACTIVA",
            Estado::PorVencer => "por vencer",
            Estado::Vencida => "VENCIDA",
            Estado::SinLicencia => "sin licencia",
        };
        println!("{:<10} {:<22} {:<8} {:<14} {}", f.equipo.puesto_sala, f.equipo.machine_id, estado, vence, f.emisiones);
    }
}
