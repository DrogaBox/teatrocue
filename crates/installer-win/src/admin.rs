//! Sección «Administrador» del instalador: lo que el administrador hace en la PC del teatro con el pendrive puesto,
//! sin otra herramienta ni opciones escritas. Se entra con la contraseña maestra (la misma que firma las licencias).
//!
//! Cada operación devuelve el texto a mostrar, así la consola solo imprime y las pruebas comparan.

use std::fs;
use std::path::{Path, PathBuf};

use ed25519_dalek::{SigningKey, VerifyingKey};
use qlab_license::activation::{best_attached, effective_expires, sign_challenge, ChallengeCode, MAX_EMERGENCY_DAYS};
use qlab_license::clock::{parse_mark_full, MarkStore, TOLERANCE_SECS};
use qlab_license::{fmt_date, verify_with, License, MachineId, LICENSE_FILE_NAME};
use sha2::{Digest, Sha256};

use crate::{files_in_use, list_files, LICENSEE, PAYLOAD_DIR, REGISTRY_FILE};

/// Opciones del menú de administración: (tecla, texto). `0` vuelve al menú principal.
pub const ADMIN_MENU: [(&str, &str); 10] = [
    ("1", "Estado de esta PC (licencia, fecha registrada, programas instalados)"),
    ("2", "Equipos licenciados (licencias.db del pendrive) e historial de uno"),
    ("3", "Clave de emergencia: pegar el código REQ-… y obtener la clave RESP-…"),
    ("4", "Cambiar el puesto / ubicación de esta PC en el registro"),
    ("5", "Verificar el pendrive (sumas SHA-256 y clave maestra)"),
    ("6", "Quitar la licencia de esta PC (los programas quedan)"),
    ("7", "Desinstalar TeatroCue de esta PC (programas y licencia)"),
    ("8", "Respaldo del registro de licencias (JSON y CSV en el pendrive)"),
    ("9", "Abrir teatrocue-admin (la ventana de administración)"),
    ("0", "Volver al menú principal"),
];

/// Lo que el administrador tiene a mano en la PC del teatro.
pub struct AdminCtx<'a> {
    /// Carpeta del instalador en el pendrive (donde están `payload/`, `master.key.enc`, `SHA256SUMS.txt`).
    pub media_root: &'a Path,
    /// Carpeta de TeatroCue en esta PC.
    pub target: &'a Path,
    pub machine: &'a MachineId,
    pub public: &'a VerifyingKey,
    /// Segundos Unix de ahora.
    pub now: u64,
    pub stores: &'a [Box<dyn MarkStore>],
    /// `licencias.db` del pendrive (`None` = sin registro).
    pub registry: Option<&'a Path>,
}

/// Fecha legible con los días que faltan.
fn describe_expiry(expires: u64, now: u64) -> String {
    if expires == 0 {
        return "sin vencimiento".to_string();
    }
    let days = (expires as i64 - now as i64).div_euclid(86_400);
    if days < 0 {
        format!("VENCIDA el {} (hace {} días)", fmt_date(expires), -days)
    } else {
        format!("vence el {} (quedan {days} días)", fmt_date(expires))
    }
}

fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut f = fs::File::open(path)?;
    let mut h = Sha256::new();
    std::io::copy(&mut f, &mut h)?;
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Licencia instalada en `target`, si la hay y verifica para esta PC.
fn installed_license(ctx: &AdminCtx<'_>) -> (Option<String>, Option<License>) {
    let text = fs::read_to_string(ctx.target.join(LICENSE_FILE_NAME)).ok();
    let lic = text.as_deref().and_then(|t| verify_with(t, ctx.public, ctx.machine).ok());
    (text, lic)
}

/// Opción 1 — Estado de esta PC.
pub fn status_report(ctx: &AdminCtx<'_>) -> String {
    let mut out = vec![format!("Machine ID de esta PC: {}", ctx.machine), format!("Carpeta de TeatroCue: {}", ctx.target.display())];

    // Licencia.
    let (text, lic) = installed_license(ctx);
    match (&text, &lic) {
        (None, _) => out.push("Licencia: no hay license.key en esta PC".to_string()),
        (Some(t), None) => out.push(match verify_with(t, ctx.public, ctx.machine) {
            Err(e) => format!("Licencia: hay un license.key pero NO sirve para esta PC ({e})"),
            Ok(_) => "Licencia: hay un license.key que no se pudo interpretar".to_string(),
        }),
        (Some(t), Some(l)) => {
            out.push(format!("Licencia: emitida el {} a «{}», {}", fmt_date(l.issued_unix), l.licensee, describe_expiry(l.expires_unix, ctx.now)));
            if let Some(a) = best_attached(t, ctx.public, ctx.machine) {
                let eff = effective_expires(l, Some(&a));
                out.push(format!("Reactivación de emergencia adjunta: {}", describe_expiry(eff, ctx.now)));
            }
            // Marcas de reloj.
            let mut seen: Option<u64> = None;
            let mut epoch = 0;
            let mut where_ok = Vec::new();
            let mut where_bad = Vec::new();
            for st in ctx.stores {
                match st.read().map(|t| parse_mark_full(&t, l)) {
                    Some(Ok(m)) => {
                        seen = Some(seen.map_or(m.mark, |s| s.max(m.mark)));
                        epoch = epoch.max(m.epoch);
                        where_ok.push(st.label());
                    }
                    Some(Err(_)) => where_bad.push(st.label()),
                    None => {}
                }
            }
            match seen {
                None => out.push("Fecha registrada: ninguna marca (licencia recién emitida, o marcas borradas)".to_string()),
                Some(s) => {
                    let today = fmt_date(ctx.now);
                    let verdict = if ctx.now + TOLERANCE_SECS < s { "¡la PC está ATRASADA respecto a lo registrado: TeatroCue se bloquea!" } else { "coincide con hoy" };
                    out.push(format!("Fecha registrada: {} (hoy {today}); {verdict}", fmt_date(s)));
                    out.push(format!("  marcas en: {}", where_ok.join(", ")));
                }
            }
            if !where_bad.is_empty() {
                out.push(format!("  marcas ILEGIBLES o de otra licencia en: {}", where_bad.join(", ")));
            }
            if epoch > 0 {
                out.push(format!("Reactivaciones de emergencia canjeadas en esta PC: {epoch}"));
            }
        }
    }

    // Programas: iguales al pendrive o no.
    let payload = ctx.media_root.join(PAYLOAD_DIR);
    match list_files(&payload) {
        Err(e) => out.push(format!("Programas: no se pudo leer {} ({e})", payload.display())),
        Ok(files) => {
            let (mut same, mut differ, mut missing) = (0, Vec::new(), Vec::new());
            for rel in &files {
                let here = ctx.target.join(rel);
                if !here.is_file() {
                    missing.push(rel.to_string_lossy().replace('\\', "/"));
                    continue;
                }
                match (sha256_file(&here), sha256_file(&payload.join(rel))) {
                    (Ok(a), Ok(b)) if a == b => same += 1,
                    _ => differ.push(rel.to_string_lossy().replace('\\', "/")),
                }
            }
            if same == 0 && differ.is_empty() {
                out.push("Programas: TeatroCue no está instalado en esta carpeta".to_string());
            } else if differ.is_empty() && missing.is_empty() {
                out.push(format!("Programas: los {same} archivos son iguales a los del pendrive (versión al día)"));
            } else {
                out.push(format!("Programas: {same} iguales al pendrive, {} distintos, {} faltan", differ.len(), missing.len()));
                for d in differ.iter().take(6) {
                    out.push(format!("  distinto: {d}"));
                }
                for m in missing.iter().take(6) {
                    out.push(format!("  falta: {m}"));
                }
                out.push("  (opción 1 del menú principal para actualizar)".to_string());
            }
        }
    }

    // Registro del pendrive.
    match ctx.registry {
        None => out.push("Registro: sin licencias.db".to_string()),
        Some(path) if !path.is_file() => out.push(format!("Registro: todavía no existe {}", path.display())),
        Some(path) => match licencias_db::Db::open(path).and_then(|db| db.equipo_por_machine(ctx.machine.as_str())) {
            Ok(Some(e)) => out.push(format!("Registro: esta PC figura como «{}» ({})", e.puesto_sala, e.institucion)),
            Ok(None) => out.push("Registro: esta PC no figura todavía en licencias.db".to_string()),
            Err(e) => out.push(format!("Registro: no se pudo leer licencias.db ({e})")),
        },
    }
    out.join("\n")
}

/// Opción 2 — Equipos licenciados. Devuelve el texto y los Machine ID en el orden listado (para pedir el historial de uno).
pub fn equipos_report(registry: &Path, now: u64) -> Result<(String, Vec<String>), String> {
    if !registry.is_file() {
        return Err(format!("todavía no existe {} en el pendrive", registry.display()));
    }
    let db = licencias_db::Db::open(registry)?;
    let filas = db.inventario(now as i64)?;
    if filas.is_empty() {
        return Ok(("No hay equipos registrados.".to_string(), Vec::new()));
    }
    let mut out = vec![format!("{:<3} {:<26} {:<20} {:<12} {}", "#", "Puesto", "Machine ID", "Estado", "Vencimiento")];
    let mut ids = Vec::new();
    for (i, f) in filas.iter().enumerate() {
        let venc = match f.vencimiento {
            None => "—".to_string(),
            Some(0) => "sin vencimiento".to_string(),
            Some(v) => describe_expiry(v as u64, now),
        };
        out.push(format!("{:<3} {:<26} {:<20} {:<12} {venc}", i + 1, truncate(&f.equipo.puesto_sala, 26), f.equipo.machine_id, f.estado.label()));
        ids.push(f.equipo.machine_id.clone());
    }
    out.push(format!("{} equipos.", filas.len()));
    Ok((out.join("\n"), ids))
}

/// Opción 2, historial — Historial de un equipo.
pub fn historial_report(registry: &Path, machine_id: &str, now: u64) -> Result<String, String> {
    let db = licencias_db::Db::open(registry)?;
    let Some(e) = db.equipo_por_machine(machine_id)? else { return Err(format!("no hay ningún equipo con Machine ID {machine_id}")) };
    let mut out = vec![format!("«{}» — {} — Machine ID {}", e.puesto_sala, e.institucion, e.machine_id)];
    if !e.notas.is_empty() {
        out.push(format!("Notas: {}", e.notas));
    }
    let hist = db.historial(e.id)?;
    if hist.is_empty() {
        out.push("Sin licencias emitidas.".to_string());
    }
    for m in hist {
        let venc = if m.fecha_vencimiento == 0 { "sin vencimiento".to_string() } else { describe_expiry(m.fecha_vencimiento as u64, now) };
        out.push(format!("  {}  {:<22} {venc}{}", fmt_date(m.fecha_emision as u64), m.tipo_emision, if m.notas.is_empty() { String::new() } else { format!("  — {}", m.notas) }));
    }
    Ok(out.join("\n"))
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { format!("{}…", s.chars().take(n - 1).collect::<String>()) }
}

/// Opción 3 — Clave de emergencia. Firma el código y la anota en el registro. Devuelve el texto completo y la clave sola.
pub fn emergency_key(ctx: &AdminCtx<'_>, key: &SigningKey, code: &str, days: u32) -> Result<(String, String), String> {
    if !(1..=MAX_EMERGENCY_DAYS).contains(&days) {
        return Err(format!("los días deben estar entre 1 y {MAX_EMERGENCY_DAYS}"));
    }
    let challenge: ChallengeCode = code.trim().parse().map_err(|e: qlab_license::activation::ActivationError| e.to_string())?;
    if key.verifying_key() != *ctx.public {
        return Err("la clave maestra del pendrive no corresponde a la de los programas; la clave emitida no serviría".into());
    }
    let (act, resp) = sign_challenge(&challenge, days, ctx.now, key);
    let mut out = vec![
        format!("Solicitud: {challenge}"),
        format!("PC (Machine ID): {}", challenge.machine),
        format!("Válida hasta: {}  (canjear antes del {})", fmt_date(act.valid_until), fmt_date(act.redeem_by)),
        String::new(),
        format!("CLAVE DE ACTIVACIÓN (válida por {days} días): [{resp}]"),
        String::new(),
        format!("Texto para WhatsApp: Clave de activación TeatroCue: [{resp}]"),
    ];
    if let Some(path) = ctx.registry {
        match register_emergency(path, &challenge, &act, &resp, days, ctx.now) {
            Ok(()) => out.push(format!("Anotada en {REGISTRY_FILE} del pendrive.")),
            Err(e) => out.push(format!("AVISO: no se pudo anotar en {REGISTRY_FILE} ({e}); la clave sirve igual.")),
        }
    }
    Ok((out.join("\n"), resp))
}

fn register_emergency(path: &Path, challenge: &ChallengeCode, act: &qlab_license::activation::ActivationKey, resp: &str, days: u32, now: u64) -> Result<(), String> {
    let db = licencias_db::Db::open(path)?;
    let known = db.equipo_por_machine(challenge.machine.as_str())?;
    let puesto = known.as_ref().map_or("", |e| e.puesto_sala.as_str());
    let id = db.upsert_equipo(LICENSEE, puesto, challenge.machine.as_str(), now as i64)?;
    db.add_emision(id, licencias_db::TIPO_EMERGENCIA, now as i64, act.valid_until as i64, i64::from(days), i64::from(challenge.epoch), resp, "Emitida desde el instalador del pendrive")?;
    Ok(())
}

/// Opción 4 — Cambiar el puesto de esta PC.
pub fn set_puesto(registry: &Path, machine: &MachineId, puesto: &str, now: u64) -> Result<String, String> {
    let puesto = puesto.trim();
    if puesto.is_empty() {
        return Err("el puesto no puede quedar vacío".into());
    }
    let db = licencias_db::Db::open(registry)?;
    let before = db.puesto_de(machine.as_str());
    db.upsert_equipo(LICENSEE, puesto, machine.as_str(), now as i64)?;
    Ok(match before {
        Some(b) if b != puesto => format!("Puesto de esta PC: «{b}» → «{puesto}»"),
        Some(_) => format!("El puesto ya era «{puesto}»"),
        None => format!("Esta PC queda registrada como «{puesto}»"),
    })
}

/// Opción 5 — Verificar el pendrive contra `SHA256SUMS.txt` (junto al instalador o en la carpeta de arriba) y la clave maestra.
pub fn verify_pen(media_root: &Path, key_matches_programs: bool) -> Result<String, String> {
    let sums = [media_root.join("SHA256SUMS.txt"), media_root.join("..").join("SHA256SUMS.txt")];
    let Some(sums) = sums.iter().find(|p| p.is_file()) else { return Err("no hay SHA256SUMS.txt en el pendrive".into()) };
    let base = sums.parent().unwrap_or(media_root);
    let text = fs::read_to_string(sums).map_err(|e| format!("{}: {e}", sums.display()))?;
    let (mut ok, mut bad, mut missing) = (0usize, Vec::new(), Vec::new());
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((hash, name)) = line.split_once(char::is_whitespace) else { continue };
        let name = name.trim().trim_start_matches('*').trim_start_matches("./");
        let path = base.join(name.replace('/', std::path::MAIN_SEPARATOR_STR));
        match sha256_file(&path) {
            Ok(h) if h.eq_ignore_ascii_case(hash.trim()) => ok += 1,
            Ok(_) => bad.push(name.to_string()),
            Err(_) => missing.push(name.to_string()),
        }
    }
    let mut out = vec![format!("Sumas: {ok} archivos correctos, {} alterados, {} faltan ({})", bad.len(), missing.len(), sums.display())];
    for b in &bad {
        out.push(format!("  ALTERADO: {b}"));
    }
    for m in &missing {
        out.push(format!("  falta: {m}"));
    }
    out.push(if key_matches_programs { "Clave maestra: corresponde a la clave pública de los programas de este pendrive.".to_string() } else { "Clave maestra: NO corresponde a los programas de este pendrive (mezcla de versiones).".to_string() });
    out.push(if bad.is_empty() && missing.is_empty() && key_matches_programs { "El pendrive está íntegro.".to_string() } else { "El pendrive NO está íntegro: vuelva a armarlo antes de instalar.".to_string() });
    Ok(out.join("\n"))
}

/// Opción 6 — Quitar la licencia de esta PC: `license.key` y la marca de fecha que vive junto a la aplicación.
pub fn remove_license(ctx: &AdminCtx<'_>) -> Result<String, String> {
    let lic = ctx.target.join(LICENSE_FILE_NAME);
    if !lic.is_file() {
        return Err(format!("no hay {LICENSE_FILE_NAME} en {}", ctx.target.display()));
    }
    fs::remove_file(&lic).map_err(|e| format!("{}: {e}", lic.display()))?;
    let _ = fs::remove_file(ctx.target.join("license.state"));
    Ok(format!("Licencia quitada de {}. TeatroCue pedirá una licencia al abrirse; los programas y los shows quedan.", ctx.target.display()))
}

/// Opción 7 — Desinstalar: borra los archivos que puso el instalador (los del `payload/` del pendrive), la licencia y la carpeta si
/// queda vacía. Lo que no puso el instalador (shows, grabaciones) no se toca y se informa.
pub fn uninstall(ctx: &AdminCtx<'_>) -> Result<String, String> {
    if !ctx.target.is_dir() {
        return Err(format!("no existe {}", ctx.target.display()));
    }
    let payload = ctx.media_root.join(PAYLOAD_DIR);
    let files = list_files(&payload).map_err(|e| format!("{}: {e}", payload.display()))?;
    let busy = files_in_use(ctx.target, &files);
    if !busy.is_empty() {
        return Err(format!("TeatroCue está abierto ({}). Ciérrelo y vuelva a intentarlo.", busy.join(", ")));
    }
    let mut removed = 0usize;
    let mut errors = Vec::new();
    let extras = [LICENSE_FILE_NAME, "license.state", "license.key.tmp"];
    for rel in files.iter().map(PathBuf::as_path).chain(extras.iter().map(Path::new)) {
        let p = ctx.target.join(rel);
        if p.is_file() {
            match fs::remove_file(&p) {
                Ok(()) => removed += 1,
                Err(e) => errors.push(format!("{}: {e}", p.display())),
            }
        }
    }
    // Carpetas que dejó el payload (p. ej. `licencias/`), si quedaron vacías.
    let mut dirs: Vec<PathBuf> = files.iter().filter_map(|f| f.parent().map(Path::to_path_buf)).filter(|d| !d.as_os_str().is_empty()).collect();
    dirs.sort();
    dirs.dedup();
    for d in dirs.iter().rev() {
        let _ = fs::remove_dir(ctx.target.join(d));
    }
    let leftovers: Vec<String> = fs::read_dir(ctx.target).map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default();
    let mut out = vec![format!("Se borraron {removed} archivos de {}.", ctx.target.display())];
    if leftovers.is_empty() {
        let _ = fs::remove_dir(ctx.target);
        out.push("La carpeta quedó vacía y se quitó.".to_string());
    } else {
        out.push(format!("Quedan en la carpeta (no los puso el instalador, no se tocan): {}", leftovers.join(", ")));
    }
    for e in errors {
        out.push(format!("AVISO: {e}"));
    }
    Ok(out.join("\n"))
}

/// Opción 8 — Respaldo del registro: `admin/respaldos/licencias_AAAA-MM-DD.json` y `.csv` junto a la base.
pub fn backup_registry(registry: &Path, now: u64) -> Result<(String, Vec<PathBuf>), String> {
    if !registry.is_file() {
        return Err(format!("todavía no existe {}", registry.display()));
    }
    let db = licencias_db::Db::open(registry)?;
    let dir = registry.parent().unwrap_or(Path::new(".")).join("respaldos");
    fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let stem = format!("licencias_{}", fmt_date(now));
    let mut written = Vec::new();
    for (ext, text) in [("json", db.exportar_json()?), ("csv", db.exportar_csv()?)] {
        let mut path = dir.join(format!("{stem}.{ext}"));
        let mut n = 2;
        while path.exists() {
            path = dir.join(format!("{stem}_{n}.{ext}"));
            n += 1;
        }
        fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))?;
        written.push(path);
    }
    let out = format!("Respaldo escrito:\n{}", written.iter().map(|p| format!("  {}", p.display())).collect::<Vec<_>>().join("\n"));
    Ok((out, written))
}

/// 9. `teatrocue-admin.exe` del pendrive, si está.
pub fn admin_tool_path(media_root: &Path) -> Option<PathBuf> {
    ["admin/teatrocue-admin.exe", "../admin/teatrocue-admin.exe", "teatrocue-admin.exe"].iter().map(|r| media_root.join(r)).find(|p| p.is_file())
}
