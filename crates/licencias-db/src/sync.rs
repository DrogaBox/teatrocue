//! Fusión bidireccional de dos `licencias.db` (la del pendrive y la local de la oficina).
//!
//! * Los equipos se comparan por `machine_id`. Si falta en un lado se copia; si está en los dos se conserva lo más completo:
//!   la fecha de alta más antigua, institución y puesto de la emisión más reciente (o el que no esté vacío) y las notas de
//!   ambos lados (si difieren se juntan, en un orden que no depende de quién sincroniza).
//! * Las licencias emitidas **no** se comparan por su `id` autoincremental (cada base tiene los suyos) sino por la tupla
//!   (`machine_id`, `fecha_emision`, `token_generado`). Lo que falta de un lado se inserta en el otro.
//! * El resultado es el mismo en ambas bases (salvo los `id`), y volver a sincronizar no cambia nada.
//! * Si el pendrive es de solo lectura, se importa lo suyo a la base local y se avisa de que no se pudo escribir en él.

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::{params, Connection};

/// Error de la fusión.
#[derive(Debug)]
pub struct DbError(pub String);

impl std::fmt::Display for DbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DbError {}

impl From<rusqlite::Error> for DbError {
    fn from(e: rusqlite::Error) -> Self {
        Self(e.to_string())
    }
}

/// Qué cambió en la sincronización (lado «local» = la base de la oficina; «remoto» = la del pendrive).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SyncSummary {
    /// Equipos que la base local no tenía y trajo del pendrive.
    pub equipos_nuevos: usize,
    /// Equipos que el pendrive no tenía y se le enviaron.
    pub equipos_exportados: usize,
    /// Licencias del pendrive que faltaban en la base local.
    pub licencias_importadas: usize,
    /// Licencias locales que faltaban en el pendrive.
    pub licencias_exportadas: usize,
    /// `false` si no se pudo escribir en la base remota (pendrive de solo lectura): lo local se actualizó igual.
    pub remoto_escribible: bool,
}

impl SyncSummary {
    /// ¿Cambió algo en alguna de las dos bases?
    pub fn hay_cambios(&self) -> bool {
        self.equipos_nuevos + self.equipos_exportados + self.licencias_importadas + self.licencias_exportadas > 0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct EquipoRow {
    institucion: String,
    puesto: String,
    created_at: i64,
    notas: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct EmisionRow {
    machine_id: String,
    tipo: String,
    fecha: i64,
    venc: i64,
    dias: i64,
    epoch: i64,
    token: String,
    notas: String,
}

type EmisionKey = (String, i64, String);

impl EmisionRow {
    fn key(&self) -> EmisionKey {
        (self.machine_id.clone(), self.fecha, self.token.clone())
    }
}

fn read_equipos(c: &Connection) -> Result<BTreeMap<String, EquipoRow>, DbError> {
    let mut st = c.prepare("SELECT machine_id, institucion, puesto_sala, created_at, notas FROM equipos")?;
    let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, EquipoRow { institucion: r.get(1)?, puesto: r.get(2)?, created_at: r.get(3)?, notas: r.get(4)? })))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

fn read_emisiones(c: &Connection) -> Result<BTreeMap<EmisionKey, EmisionRow>, DbError> {
    let mut st = c.prepare(
        "SELECT e.machine_id, l.tipo_emision, l.fecha_emision, l.fecha_vencimiento, l.dias_otorgados, l.activation_epoch, l.token_generado, l.notas
         FROM licencias_emitidas l JOIN equipos e ON e.id = l.equipo_id",
    )?;
    let rows = st.query_map([], |r| Ok(EmisionRow { machine_id: r.get(0)?, tipo: r.get(1)?, fecha: r.get(2)?, venc: r.get(3)?, dias: r.get(4)?, epoch: r.get(5)?, token: r.get(6)?, notas: r.get(7)? }))?;
    let mut out = BTreeMap::new();
    for row in rows {
        let row = row?;
        out.insert(row.key(), row);
    }
    Ok(out)
}

/// Junta dos notas sin perder ninguna: iguales o una vacía → la otra; si una contiene a la otra, la más larga; si no, las dos
/// separadas por una línea, en orden alfabético (así `merge(a, b) == merge(b, a)`).
pub fn merge_notes(a: &str, b: &str) -> String {
    let (a, b) = (a.trim(), b.trim());
    if a == b || b.is_empty() {
        return a.to_string();
    }
    if a.is_empty() || a.contains(b) {
        return if a.is_empty() { b.to_string() } else { a.to_string() };
    }
    if b.contains(a) {
        return b.to_string();
    }
    let (first, second) = if a <= b { (a, b) } else { (b, a) };
    format!("{first}\n---\n{second}")
}

fn merged_equipo(local: &EquipoRow, remote: &EquipoRow, local_last: i64, remote_last: i64) -> EquipoRow {
    let created_at = match (local.created_at, remote.created_at) {
        (0, r) => r,
        (l, 0) => l,
        (l, r) => l.min(r),
    };
    // Institución y puesto: el lado con la emisión más reciente manda (es lo último que alguien confirmó); si empata o un
    // campo está vacío se usa el otro. En un empate total gana el orden alfabético para que el resultado sea simétrico.
    let pick = |l: &str, r: &str| -> String {
        match (l.is_empty(), r.is_empty()) {
            (true, _) => r.to_string(),
            (_, true) => l.to_string(),
            _ if l == r => l.to_string(),
            _ if local_last != remote_last => (if local_last > remote_last { l } else { r }).to_string(),
            _ => (if l >= r { l } else { r }).to_string(),
        }
    };
    EquipoRow { institucion: pick(&local.institucion, &remote.institucion), puesto: pick(&local.puesto, &remote.puesto), created_at, notas: merge_notes(&local.notas, &remote.notas) }
}

/// Fecha de la emisión más reciente de cada equipo.
fn last_emission(e: &BTreeMap<EmisionKey, EmisionRow>) -> BTreeMap<String, i64> {
    let mut m = BTreeMap::new();
    for (k, _) in e.iter() {
        let v = m.entry(k.0.clone()).or_insert(0);
        *v = (*v).max(k.1);
    }
    m
}

/// Escribe en `c` lo que haga falta para que su contenido sea `equipos` y `emisiones`. Devuelve cuántos equipos y licencias
/// se insertaron nuevos.
fn apply(c: &Connection, equipos: &BTreeMap<String, EquipoRow>, emisiones: &BTreeMap<EmisionKey, EmisionRow>) -> Result<(usize, usize), DbError> {
    let tx = c.unchecked_transaction()?;
    let existing = read_equipos(&tx)?;
    let mut new_equipos = 0;
    for (machine, e) in equipos {
        match existing.get(machine) {
            None => {
                tx.execute("INSERT INTO equipos (institucion, puesto_sala, machine_id, created_at, notas) VALUES (?1,?2,?3,?4,?5)", params![e.institucion, e.puesto, machine, e.created_at, e.notas])?;
                new_equipos += 1;
            }
            Some(old) if old != e => {
                tx.execute("UPDATE equipos SET institucion = ?1, puesto_sala = ?2, created_at = ?3, notas = ?4 WHERE machine_id = ?5", params![e.institucion, e.puesto, e.created_at, e.notas, machine])?;
            }
            Some(_) => {}
        }
    }
    let have = read_emisiones(&tx)?;
    let mut new_emisiones = 0;
    for (key, m) in emisiones {
        match have.get(key) {
            None => {
                tx.execute(
                    "INSERT INTO licencias_emitidas (equipo_id, tipo_emision, fecha_emision, fecha_vencimiento, dias_otorgados, activation_epoch, token_generado, notas)
                     VALUES ((SELECT id FROM equipos WHERE machine_id = ?1), ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![m.machine_id, m.tipo, m.fecha, m.venc, m.dias, m.epoch, m.token, m.notas],
                )?;
                new_emisiones += 1;
            }
            Some(old) if old != m => {
                tx.execute(
                    "UPDATE licencias_emitidas SET notas = ?1 WHERE fecha_emision = ?2 AND token_generado = ?3 AND equipo_id = (SELECT id FROM equipos WHERE machine_id = ?4)",
                    params![m.notas, m.fecha, m.token, m.machine_id],
                )?;
            }
            Some(_) => {}
        }
    }
    tx.commit()?;
    Ok((new_equipos, new_emisiones))
}

/// Fusiona `local_db` y `remote_db` y deja las dos con el mismo contenido. Ver el módulo para las reglas.
pub fn sync_databases(local_db: &Connection, remote_db: &Connection) -> Result<SyncSummary, DbError> {
    // Una base recién creada o vacía del pendrive puede no tener tablas.
    crate::ensure_schema(local_db)?;
    let remote_ready = crate::ensure_schema(remote_db).is_ok();

    let (l_eq, r_eq) = (read_equipos(local_db)?, if remote_ready { read_equipos(remote_db)? } else { BTreeMap::new() });
    let (l_em, r_em) = (read_emisiones(local_db)?, if remote_ready { read_emisiones(remote_db)? } else { BTreeMap::new() });
    let (l_last, r_last) = (last_emission(&l_em), last_emission(&r_em));

    // Equipos de la unión.
    let machines: BTreeSet<&String> = l_eq.keys().chain(r_eq.keys()).collect();
    let mut equipos = BTreeMap::new();
    for m in machines {
        let merged = match (l_eq.get(m), r_eq.get(m)) {
            (Some(l), Some(r)) => merged_equipo(l, r, l_last.get(m).copied().unwrap_or(0), r_last.get(m).copied().unwrap_or(0)),
            (Some(x), None) | (None, Some(x)) => x.clone(),
            (None, None) => continue,
        };
        equipos.insert(m.clone(), merged);
    }
    // Emisiones de la unión; la misma emisión con notas distintas junta las notas.
    let mut emisiones = l_em.clone();
    for (k, r) in &r_em {
        emisiones
            .entry(k.clone())
            .and_modify(|l| l.notas = merge_notes(&l.notas, &r.notas))
            .or_insert_with(|| r.clone());
    }

    let (equipos_nuevos, licencias_importadas) = apply(local_db, &equipos, &emisiones)?;
    let mut summary = SyncSummary { equipos_nuevos, licencias_importadas, remoto_escribible: false, ..Default::default() };
    if remote_ready {
        match apply(remote_db, &equipos, &emisiones) {
            Ok((e, l)) => {
                summary.equipos_exportados = e;
                summary.licencias_exportadas = l;
                summary.remoto_escribible = true;
            }
            // Pendrive de solo lectura (o desconectado a mitad): lo local ya quedó al día.
            Err(_) => summary.remoto_escribible = false,
        }
    }
    Ok(summary)
}

/// Fusiona la base local con el archivo `remote_path` (la del pendrive). Se abre para leer y escribir; si el pendrive no deja
/// escribir (solo lectura), se abre solo para leer y se importa igual.
pub fn sync_with_file(local: &crate::Db, remote_path: &std::path::Path) -> Result<SyncSummary, DbError> {
    use rusqlite::OpenFlags;
    let remote = Connection::open_with_flags(remote_path, OpenFlags::SQLITE_OPEN_READ_WRITE).or_else(|_| Connection::open_with_flags(remote_path, OpenFlags::SQLITE_OPEN_READ_ONLY))?;
    // Si otro proceso (o el instalador) tiene la base ocupada, se espera un poco antes de rendirse.
    let _ = remote.busy_timeout(std::time::Duration::from_secs(3));
    sync_databases(local.connection(), &remote)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Db, Registro, TIPO_EMERGENCIA, TIPO_PENDRIVE};

    const DAY: i64 = 86_400;
    const NOW: i64 = 1_800_000_000;

    fn temp_db(tag: &str) -> (Db, std::path::PathBuf) {
        let p = std::env::temp_dir().join(format!("teatrocue-sync-{tag}-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&p);
        (Db::open(&p).unwrap(), p)
    }

    fn reg<'a>(machine: &'a str, puesto: &'a str, tipo: &'a str, fecha: i64, token: &'a str, notas: &'a str) -> Registro<'a> {
        Registro { institucion: "Ballet Oficial de Córdoba", puesto, machine_id: machine, notas_equipo: "", tipo_emision: tipo, fecha_emision: fecha, fecha_vencimiento: fecha + 30 * DAY, dias_otorgados: 30, activation_epoch: 0, token, notas }
    }

    /// Contenido comparable de una base: equipos y emisiones sin los `id`.
    fn snapshot(c: &Connection) -> (BTreeMap<String, EquipoRow>, Vec<EmisionRow>) {
        (read_equipos(c).unwrap(), read_emisiones(c).unwrap().into_values().collect())
    }

    #[test]
    fn two_out_of_date_databases_end_up_identical_without_duplicates_and_syncing_again_changes_nothing() {
        let (mut local, lp) = temp_db("loc");
        let (mut usb, up) = temp_db("usb");
        // Oficina: sala A (con una nota escrita a mano) y sala B.
        let a = local.registrar_emision(&reg("AAAA-AAAA-AAAA-AAAA", "Cabina Principal", TIPO_PENDRIVE, NOW, "licA-1", "alta")).unwrap();
        local.set_notas(a, "cable roto en el foso").unwrap();
        local.registrar_emision(&reg("BBBB-BBBB-BBBB-BBBB", "Sala 2", TIPO_PENDRIVE, NOW, "licB-1", "")).unwrap();
        // Pendrive (instalado en el teatro): la misma sala A ya con su licencia vieja copiada + una renovación nueva y una sala C que
        // la oficina no conoce. La sala B no está.
        usb.registrar_emision(&reg("AAAA-AAAA-AAAA-AAAA", "Cabina Acer", TIPO_PENDRIVE, NOW, "licA-1", "alta")).unwrap();
        let ua = usb.equipo_por_machine("AAAA-AAAA-AAAA-AAAA").unwrap().unwrap().id;
        usb.set_notas(ua, "se cambió el cable").unwrap();
        usb.registrar_emision(&reg("AAAA-AAAA-AAAA-AAAA", "Cabina Acer", TIPO_PENDRIVE, NOW + 20 * DAY, "licA-2", "Renovación")).unwrap();
        usb.registrar_emision(&reg("CCCC-CCCC-CCCC-CCCC", "Sala 3", TIPO_EMERGENCIA, NOW + 21 * DAY, "RESP-C", "")).unwrap();

        let s = sync_databases(local.connection(), usb.connection()).unwrap();
        assert_eq!(s, SyncSummary { equipos_nuevos: 1, equipos_exportados: 1, licencias_importadas: 2, licencias_exportadas: 1, remoto_escribible: true });
        let (le, ll) = snapshot(local.connection());
        let (ue, ul) = snapshot(usb.connection());
        assert_eq!((le.clone(), ll.clone()), (ue, ul), "las dos bases quedan idénticas");
        assert_eq!(le.len(), 3, "A, B y C sin duplicar");
        assert_eq!(ll.len(), 4, "licA-1 (una sola vez), licA-2, licB-1 y RESP-C");
        // A: la emisión más reciente es la del pendrive, así que su puesto manda; las dos notas se conservan.
        let a = &le["AAAA-AAAA-AAAA-AAAA"];
        assert_eq!(a.puesto, "Cabina Acer");
        assert!(a.notas.contains("cable roto en el foso") && a.notas.contains("se cambió el cable"), "{:?}", a.notas);
        // Los id no se usaron como clave: el historial de A en el pendrive tiene sus dos emisiones.
        let ua = usb.equipo_por_machine("AAAA-AAAA-AAAA-AAAA").unwrap().unwrap().id;
        assert_eq!(usb.historial(ua).unwrap().len(), 2);
        // Idempotente.
        let again = sync_databases(local.connection(), usb.connection()).unwrap();
        assert!(!again.hay_cambios(), "{again:?}");
        assert_eq!(snapshot(local.connection()), snapshot(usb.connection()));
        // Y simétrica: sincronizar al revés da el mismo contenido.
        let again2 = sync_databases(usb.connection(), local.connection()).unwrap();
        assert!(!again2.hay_cambios());
        let _ = (std::fs::remove_file(lp), std::fs::remove_file(up));
    }

    #[test]
    fn the_oldest_creation_date_is_kept_notes_combine_symmetrically_and_a_missing_schema_is_created() {
        let (mut local, lp) = temp_db("old");
        let usb_conn = Connection::open_in_memory().unwrap(); // pendrive nuevo: base vacía sin tablas
        local.registrar_emision(&reg("AAAA-AAAA-AAAA-AAAA", "x", TIPO_PENDRIVE, NOW, "t", "")).unwrap();
        let s = sync_databases(local.connection(), &usb_conn).unwrap();
        assert_eq!((s.equipos_exportados, s.licencias_exportadas, s.remoto_escribible), (1, 1, true));
        // Alta más antigua en el pendrive y notas distintas a ambos lados.
        usb_conn.execute("UPDATE equipos SET created_at = ?1, notas = 'B nota'", [NOW - 100 * DAY]).unwrap();
        local.connection().execute("UPDATE equipos SET notas = 'A nota'", []).unwrap();
        sync_databases(local.connection(), &usb_conn).unwrap();
        let e = read_equipos(local.connection()).unwrap();
        assert_eq!(e["AAAA-AAAA-AAAA-AAAA"].created_at, NOW - 100 * DAY, "se conserva el alta más antigua");
        assert_eq!(e["AAAA-AAAA-AAAA-AAAA"].notas, "A nota\n---\nB nota");
        assert_eq!(read_equipos(&usb_conn).unwrap(), e);
        assert_eq!(merge_notes("a", "b"), merge_notes("b", "a"));
        assert_eq!(merge_notes("hola mundo", "mundo"), "hola mundo");
        assert_eq!(merge_notes("", "x"), "x");
        assert_eq!(merge_notes("x", ""), "x");
        let _ = std::fs::remove_file(lp);
    }

    #[test]
    fn a_read_only_pendrive_still_imports_into_the_local_database() {
        let (mut local, lp) = temp_db("ro-l");
        let (mut usb, up) = temp_db("ro-u");
        local.registrar_emision(&reg("AAAA-AAAA-AAAA-AAAA", "x", TIPO_PENDRIVE, NOW, "l1", "")).unwrap();
        usb.registrar_emision(&reg("BBBB-BBBB-BBBB-BBBB", "y", TIPO_PENDRIVE, NOW, "u1", "")).unwrap();
        drop(usb);
        // Se abre el pendrive en modo solo lectura, como un USB con el interruptor de protección.
        let ro = Connection::open_with_flags(&up, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let s = sync_databases(local.connection(), &ro).unwrap();
        assert_eq!((s.equipos_nuevos, s.licencias_importadas, s.remoto_escribible), (1, 1, false));
        assert_eq!(s.licencias_exportadas, 0);
        assert_eq!(read_equipos(local.connection()).unwrap().len(), 2, "lo del pendrive llegó a la base local");
        assert_eq!(read_equipos(&ro).unwrap().len(), 1, "el pendrive no se tocó");
        let _ = (std::fs::remove_file(lp), std::fs::remove_file(up));
    }
}
