//! Inventario local: `equipos` y `licencias_emitidas` en `licencias.db`.
//!
//! Lo comparten `teatrocue-admin` (que lo consulta y lo edita) y `Instalar_TeatroCue.exe` (que registra cada instalación o
//! renovación del pendrive con [`Db::registrar_emision`]).

use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension};

pub mod sync;
pub use sync::{sync_databases, sync_with_file, DbError, SyncSummary};

pub const DB_FILE: &str = "licencias.db";
/// Una licencia que vence en menos de estos días se marca en ámbar.
pub const WARN_DAYS: i64 = 15;
pub const TIPO_PENDRIVE: &str = "Pendrive Instalador";
pub const TIPO_EMERGENCIA: &str = "Emergencia REQ/RESP";

/// Dónde vive la base: en Linux `~/.config/teatrocue-keys/licencias.db`; en Windows (o pendrive) junto al ejecutable,
/// o en la carpeta de arriba si ya hay una allí.
pub fn default_db_path() -> PathBuf {
    let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf));
    if cfg!(windows) {
        let dir = exe_dir.unwrap_or_else(|| PathBuf::from("."));
        let up = dir.join("..").join(DB_FILE);
        return if !dir.join(DB_FILE).exists() && up.exists() { up } else { dir.join(DB_FILE) };
    }
    match std::env::var_os("HOME") {
        Some(h) => PathBuf::from(h).join(".config/teatrocue-keys").join(DB_FILE),
        None => exe_dir.unwrap_or_else(|| PathBuf::from(".")).join(DB_FILE),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Equipo {
    pub id: i64,
    pub institucion: String,
    pub puesto_sala: String,
    pub machine_id: String,
    pub created_at: i64,
    pub notas: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Emision {
    pub id: i64,
    pub equipo_id: i64,
    pub tipo_emision: String,
    pub fecha_emision: i64,
    /// `0` = perpetua.
    pub fecha_vencimiento: i64,
    pub dias_otorgados: i64,
    pub activation_epoch: i64,
    pub token_generado: String,
    pub notas: String,
}

/// Semáforo de una sala.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Estado {
    Activa,
    PorVencer,
    Vencida,
    /// Registrada pero sin ninguna licencia emitida.
    SinLicencia,
}

impl Estado {
    pub fn label(self) -> &'static str {
        match self {
            Self::Activa => "Activa",
            Self::PorVencer => "Por vencer",
            Self::Vencida => "Vencida",
            Self::SinLicencia => "Sin licencia",
        }
    }
}

/// Estado según el vencimiento (`0` = perpetua) y la fecha de hoy.
pub fn estado_de(vencimiento: i64, now: i64) -> Estado {
    if vencimiento == 0 {
        return Estado::Activa;
    }
    match dias_restantes(vencimiento, now) {
        Some(d) if d < 0 => Estado::Vencida,
        Some(d) if d < WARN_DAYS => Estado::PorVencer,
        _ => Estado::Activa,
    }
}

/// Días enteros que faltan (negativos si ya venció); `None` si no vence.
pub fn dias_restantes(vencimiento: i64, now: i64) -> Option<i64> {
    (vencimiento != 0).then(|| (vencimiento - now).div_euclid(86_400))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fila {
    pub equipo: Equipo,
    /// Vencimiento efectivo: `0` = perpetua, `None` = nunca se le emitió nada.
    pub vencimiento: Option<i64>,
    pub estado: Estado,
    pub emisiones: usize,
}

/// Datos de una emisión para [`Db::registrar_emision`].
#[derive(Clone, Debug)]
pub struct Registro<'a> {
    pub institucion: &'a str,
    pub puesto: &'a str,
    pub machine_id: &'a str,
    /// Notas con las que se da de alta un equipo nuevo (si ya existía con notas, no se tocan).
    pub notas_equipo: &'a str,
    pub tipo_emision: &'a str,
    pub fecha_emision: i64,
    /// `0` = perpetua.
    pub fecha_vencimiento: i64,
    pub dias_otorgados: i64,
    pub activation_epoch: i64,
    pub token: &'a str,
    pub notas: &'a str,
}

/// Crea las tablas `equipos` y `licencias_emitidas` si faltan.
pub fn ensure_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "PRAGMA foreign_keys = ON;
         CREATE TABLE IF NOT EXISTS equipos (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             institucion TEXT NOT NULL DEFAULT '',
             puesto_sala TEXT NOT NULL DEFAULT '',
             machine_id TEXT UNIQUE NOT NULL,
             created_at INTEGER NOT NULL DEFAULT 0,
             notas TEXT NOT NULL DEFAULT ''
         );
         CREATE TABLE IF NOT EXISTS licencias_emitidas (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             equipo_id INTEGER NOT NULL REFERENCES equipos(id),
             tipo_emision TEXT NOT NULL,
             fecha_emision INTEGER NOT NULL,
             fecha_vencimiento INTEGER NOT NULL,
             dias_otorgados INTEGER NOT NULL DEFAULT 0,
             activation_epoch INTEGER NOT NULL DEFAULT 0,
             token_generado TEXT NOT NULL DEFAULT '',
             notas TEXT NOT NULL DEFAULT ''
         );
         CREATE INDEX IF NOT EXISTS idx_lic_equipo ON licencias_emitidas(equipo_id);",
    )
}

pub struct Db {
    conn: Connection,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self, String> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        Self::init(Connection::open(path).map_err(|e| e.to_string())?)
    }

    pub fn open_memory() -> Result<Self, String> {
        Self::init(Connection::open_in_memory().map_err(|e| e.to_string())?)
    }

    fn init(conn: Connection) -> Result<Self, String> {
        ensure_schema(&conn).map_err(|e| e.to_string())?;
        Ok(Self { conn })
    }

    /// La conexión SQLite (para [`sync_databases`]).
    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    fn equipo_from(r: &rusqlite::Row<'_>) -> rusqlite::Result<Equipo> {
        Ok(Equipo { id: r.get(0)?, institucion: r.get(1)?, puesto_sala: r.get(2)?, machine_id: r.get(3)?, created_at: r.get(4)?, notas: r.get(5)? })
    }

    /// Puesto/sala con el que ya está registrado un equipo, si lo está.
    pub fn puesto_de(&self, machine_id: &str) -> Option<String> {
        self.equipo_por_machine(machine_id).ok().flatten().map(|e| e.puesto_sala).filter(|p| !p.is_empty())
    }

    pub fn equipo_por_machine(&self, machine_id: &str) -> Result<Option<Equipo>, String> {
        self.conn
            .query_row("SELECT id, institucion, puesto_sala, machine_id, created_at, notas FROM equipos WHERE machine_id = ?1", [machine_id], Self::equipo_from)
            .optional()
            .map_err(|e| e.to_string())
    }

    /// Alta del equipo, o, si ya existía, actualización de institución y puesto (solo con valores no vacíos).
    pub fn upsert_equipo(&self, institucion: &str, puesto: &str, machine_id: &str, now: i64) -> Result<i64, String> {
        if let Some(e) = self.equipo_por_machine(machine_id)? {
            let inst = if institucion.trim().is_empty() { e.institucion } else { institucion.trim().to_string() };
            let pto = if puesto.trim().is_empty() { e.puesto_sala } else { puesto.trim().to_string() };
            self.conn.execute("UPDATE equipos SET institucion = ?1, puesto_sala = ?2 WHERE id = ?3", params![inst, pto, e.id]).map_err(|e| e.to_string())?;
            return Ok(e.id);
        }
        self.conn
            .execute("INSERT INTO equipos (institucion, puesto_sala, machine_id, created_at, notas) VALUES (?1, ?2, ?3, ?4, '')", params![institucion.trim(), puesto.trim(), machine_id, now])
            .map_err(|e| e.to_string())?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Registra una emisión de forma atómica (una sola transacción): alta o actualización del equipo por su Machine ID
    /// (`ON CONFLICT(machine_id) DO UPDATE`; un nombre vacío no borra el que ya había y las notas que alguien escribió
    /// se conservan) y una fila nueva en `licencias_emitidas`. Devuelve el id del equipo.
    pub fn registrar_emision(&mut self, r: &Registro<'_>) -> Result<i64, String> {
        let tx = self.conn.transaction().map_err(|e| e.to_string())?;
        tx.execute(
            "INSERT INTO equipos (institucion, puesto_sala, machine_id, created_at, notas) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(machine_id) DO UPDATE SET
               institucion = CASE WHEN excluded.institucion <> '' THEN excluded.institucion ELSE equipos.institucion END,
               puesto_sala = CASE WHEN excluded.puesto_sala <> '' THEN excluded.puesto_sala ELSE equipos.puesto_sala END,
               notas = CASE WHEN equipos.notas = '' THEN excluded.notas ELSE equipos.notas END",
            params![r.institucion.trim(), r.puesto.trim(), r.machine_id, r.fecha_emision, r.notas_equipo],
        )
        .map_err(|e| e.to_string())?;
        let equipo_id: i64 = tx.query_row("SELECT id FROM equipos WHERE machine_id = ?1", [r.machine_id], |row| row.get(0)).map_err(|e| e.to_string())?;
        tx.execute(
            "INSERT INTO licencias_emitidas (equipo_id, tipo_emision, fecha_emision, fecha_vencimiento, dias_otorgados, activation_epoch, token_generado, notas) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            params![equipo_id, r.tipo_emision, r.fecha_emision, r.fecha_vencimiento, r.dias_otorgados, r.activation_epoch, r.token, r.notas],
        )
        .map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())?;
        Ok(equipo_id)
    }

    pub fn set_notas(&self, equipo_id: i64, notas: &str) -> Result<(), String> {
        self.conn.execute("UPDATE equipos SET notas = ?1 WHERE id = ?2", params![notas, equipo_id]).map(|_| ()).map_err(|e| e.to_string())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn add_emision(&self, equipo_id: i64, tipo: &str, fecha_emision: i64, fecha_vencimiento: i64, dias: i64, epoch: i64, token: &str, notas: &str) -> Result<i64, String> {
        self.conn
            .execute(
                "INSERT INTO licencias_emitidas (equipo_id, tipo_emision, fecha_emision, fecha_vencimiento, dias_otorgados, activation_epoch, token_generado, notas) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                params![equipo_id, tipo, fecha_emision, fecha_vencimiento, dias, epoch, token, notas],
            )
            .map_err(|e| e.to_string())?;
        Ok(self.conn.last_insert_rowid())
    }

    fn emision_from(r: &rusqlite::Row<'_>) -> rusqlite::Result<Emision> {
        Ok(Emision {
            id: r.get(0)?,
            equipo_id: r.get(1)?,
            tipo_emision: r.get(2)?,
            fecha_emision: r.get(3)?,
            fecha_vencimiento: r.get(4)?,
            dias_otorgados: r.get(5)?,
            activation_epoch: r.get(6)?,
            token_generado: r.get(7)?,
            notas: r.get(8)?,
        })
    }

    const EMISION_COLS: &'static str = "id, equipo_id, tipo_emision, fecha_emision, fecha_vencimiento, dias_otorgados, activation_epoch, token_generado, notas";

    /// Historial de un equipo, de la emisión más reciente a la más vieja.
    pub fn historial(&self, equipo_id: i64) -> Result<Vec<Emision>, String> {
        let mut st = self.conn.prepare(&format!("SELECT {} FROM licencias_emitidas WHERE equipo_id = ?1 ORDER BY fecha_emision DESC, id DESC", Self::EMISION_COLS)).map_err(|e| e.to_string())?;
        let rows = st.query_map([equipo_id], Self::emision_from).map_err(|e| e.to_string())?;
        rows.collect::<Result<_, _>>().map_err(|e| e.to_string())
    }

    pub fn todos_los_equipos(&self) -> Result<Vec<Equipo>, String> {
        let mut st = self.conn.prepare("SELECT id, institucion, puesto_sala, machine_id, created_at, notas FROM equipos ORDER BY institucion COLLATE NOCASE, puesto_sala COLLATE NOCASE, id").map_err(|e| e.to_string())?;
        let rows = st.query_map([], Self::equipo_from).map_err(|e| e.to_string())?;
        rows.collect::<Result<_, _>>().map_err(|e| e.to_string())
    }

    pub fn todas_las_emisiones(&self) -> Result<Vec<Emision>, String> {
        let mut st = self.conn.prepare(&format!("SELECT {} FROM licencias_emitidas ORDER BY id", Self::EMISION_COLS)).map_err(|e| e.to_string())?;
        let rows = st.query_map([], Self::emision_from).map_err(|e| e.to_string())?;
        rows.collect::<Result<_, _>>().map_err(|e| e.to_string())
    }

    /// Inventario con el vencimiento efectivo de cada equipo: una licencia perpetua manda; si no, la que vence más tarde.
    pub fn inventario(&self, now: i64) -> Result<Vec<Fila>, String> {
        let todas = self.todas_las_emisiones()?;
        Ok(self
            .todos_los_equipos()?
            .into_iter()
            .map(|equipo| {
                let propias: Vec<&Emision> = todas.iter().filter(|e| e.equipo_id == equipo.id).collect();
                let vencimiento = if propias.is_empty() {
                    None
                } else if propias.iter().any(|e| e.fecha_vencimiento == 0) {
                    Some(0)
                } else {
                    propias.iter().map(|e| e.fecha_vencimiento).max()
                };
                let estado = vencimiento.map_or(Estado::SinLicencia, |v| estado_de(v, now));
                Fila { equipo, vencimiento, estado, emisiones: propias.len() }
            })
            .collect())
    }

    /// Respaldo completo en JSON (equipos con su historial).
    pub fn exportar_json(&self) -> Result<String, String> {
        use serde_json::json;
        let emisiones = self.todas_las_emisiones()?;
        let equipos: Vec<_> = self
            .todos_los_equipos()?
            .into_iter()
            .map(|e| {
                let lic: Vec<_> = emisiones
                    .iter()
                    .filter(|m| m.equipo_id == e.id)
                    .map(|m| {
                        json!({"id": m.id, "tipo_emision": m.tipo_emision, "fecha_emision": m.fecha_emision, "fecha_vencimiento": m.fecha_vencimiento, "dias_otorgados": m.dias_otorgados,
                               "activation_epoch": m.activation_epoch, "token_generado": m.token_generado, "notas": m.notas})
                    })
                    .collect();
                json!({"id": e.id, "institucion": e.institucion, "puesto_sala": e.puesto_sala, "machine_id": e.machine_id, "created_at": e.created_at, "notas": e.notas, "licencias_emitidas": lic})
            })
            .collect();
        serde_json::to_string_pretty(&json!({"formato": "teatrocue-licencias/v1", "equipos": equipos})).map_err(|e| e.to_string())
    }

    /// Respaldo plano en CSV: una fila por licencia emitida (los equipos sin licencia salen con las columnas vacías).
    pub fn exportar_csv(&self) -> Result<String, String> {
        fn cell(s: &str) -> String {
            // Quita el prefijo que Excel interpretaría como fórmula y escapa comillas.
            let t = if s.starts_with(['=', '+', '-', '@']) { format!("'{s}") } else { s.to_string() };
            format!("\"{}\"", t.replace('"', "\"\""))
        }
        let emisiones = self.todas_las_emisiones()?;
        let mut out = String::from("institucion,puesto_sala,machine_id,equipo_notas,tipo_emision,fecha_emision,fecha_vencimiento,dias_otorgados,activation_epoch,token_generado,licencia_notas\n");
        for e in self.todos_los_equipos()? {
            let propias: Vec<&Emision> = emisiones.iter().filter(|m| m.equipo_id == e.id).collect();
            let base = [cell(&e.institucion), cell(&e.puesto_sala), cell(&e.machine_id), cell(&e.notas)].join(",");
            if propias.is_empty() {
                out.push_str(&format!("{base},,,,,,,\n"));
            }
            for m in propias {
                out.push_str(&format!("{base},{},{},{},{},{},{},{}\n", cell(&m.tipo_emision), m.fecha_emision, m.fecha_vencimiento, m.dias_otorgados, m.activation_epoch, cell(&m.token_generado), cell(&m.notas)));
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 86_400;
    const NOW: i64 = 1_800_000_000;

    #[test]
    fn the_schema_is_created_and_reopening_keeps_the_data() {
        let path = std::env::temp_dir().join(format!("teatrocue-admin-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        {
            let db = Db::open(&path).unwrap();
            db.upsert_equipo("Ballet Oficial de Córdoba", "Cabina Principal Acer", "AAAA-BBBB-CCCC-DDDD", NOW).unwrap();
        }
        let db = Db::open(&path).unwrap();
        let e = db.equipo_por_machine("AAAA-BBBB-CCCC-DDDD").unwrap().unwrap();
        assert_eq!((e.institucion.as_str(), e.puesto_sala.as_str(), e.created_at), ("Ballet Oficial de Córdoba", "Cabina Principal Acer", NOW));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn machine_ids_are_unique_and_upsert_updates_names_without_creating_duplicates() {
        let db = Db::open_memory().unwrap();
        let a = db.upsert_equipo("Teatro", "Cabina", "1111-2222-3333-4444", NOW).unwrap();
        let b = db.upsert_equipo("Teatro del Libertador", "", "1111-2222-3333-4444", NOW + 5).unwrap();
        assert_eq!(a, b);
        let e = db.equipo_por_machine("1111-2222-3333-4444").unwrap().unwrap();
        assert_eq!((e.institucion.as_str(), e.puesto_sala.as_str(), e.created_at), ("Teatro del Libertador", "Cabina", NOW), "un nombre vacío no borra el anterior");
        assert_eq!(db.todos_los_equipos().unwrap().len(), 1);
    }

    #[test]
    fn traffic_lights_follow_the_expiry_with_a_fifteen_day_warning() {
        assert_eq!(estado_de(0, NOW), Estado::Activa, "perpetua");
        assert_eq!(estado_de(NOW + 40 * DAY, NOW), Estado::Activa);
        assert_eq!(estado_de(NOW + 14 * DAY + 100, NOW), Estado::PorVencer);
        assert_eq!(estado_de(NOW + 15 * DAY, NOW), Estado::Activa);
        assert_eq!(estado_de(NOW + 100, NOW), Estado::PorVencer);
        assert_eq!(estado_de(NOW - 1, NOW), Estado::Vencida);
        assert_eq!(dias_restantes(NOW + 3 * DAY, NOW), Some(3));
        assert_eq!(dias_restantes(NOW - DAY, NOW), Some(-1));
        assert_eq!(dias_restantes(0, NOW), None);
    }

    #[test]
    fn the_inventory_uses_the_latest_expiry_and_a_perpetual_license_wins() {
        let db = Db::open_memory().unwrap();
        let a = db.upsert_equipo("A", "x", "AAAA-AAAA-AAAA-AAAA", NOW).unwrap();
        let b = db.upsert_equipo("B", "y", "BBBB-BBBB-BBBB-BBBB", NOW).unwrap();
        let c = db.upsert_equipo("C", "z", "CCCC-CCCC-CCCC-CCCC", NOW).unwrap();
        let _sin = db.upsert_equipo("D", "w", "DDDD-DDDD-DDDD-DDDD", NOW).unwrap();
        db.add_emision(a, TIPO_PENDRIVE, NOW - 60 * DAY, NOW - 30 * DAY, 30, 0, "", "").unwrap();
        db.add_emision(a, TIPO_EMERGENCIA, NOW - DAY, NOW + 6 * DAY, 7, 1, "RESP-X", "").unwrap();
        db.add_emision(b, TIPO_PENDRIVE, NOW - 10 * DAY, NOW - DAY, 9, 0, "", "").unwrap();
        db.add_emision(c, TIPO_PENDRIVE, NOW, NOW + 30 * DAY, 30, 0, "", "").unwrap();
        db.add_emision(c, TIPO_PENDRIVE, NOW, 0, 0, 0, "", "perpetua").unwrap();
        let inv = db.inventario(NOW).unwrap();
        let by = |m: &str| inv.iter().find(|f| f.equipo.machine_id.starts_with(m)).unwrap();
        assert_eq!((by("AAAA").estado, by("AAAA").vencimiento, by("AAAA").emisiones), (Estado::PorVencer, Some(NOW + 6 * DAY), 2));
        assert_eq!(by("BBBB").estado, Estado::Vencida);
        assert_eq!((by("CCCC").estado, by("CCCC").vencimiento), (Estado::Activa, Some(0)));
        assert_eq!((by("DDDD").estado, by("DDDD").vencimiento), (Estado::SinLicencia, None));
        let h = db.historial(a).unwrap();
        assert_eq!((h.len(), h[0].tipo_emision.as_str(), h[0].token_generado.as_str()), (2, TIPO_EMERGENCIA, "RESP-X"), "la más reciente primero");
    }

    #[test]
    fn notes_are_saved_and_backups_contain_everything() {
        let db = Db::open_memory().unwrap();
        let a = db.upsert_equipo("Ballet \"Oficial\"", "Cabina, Acer", "AAAA-AAAA-AAAA-AAAA", NOW).unwrap();
        db.set_notas(a, "Cable \u{00f1}and\u{00fa}").unwrap();
        db.add_emision(a, TIPO_EMERGENCIA, NOW, NOW + 7 * DAY, 7, 2, "RESP-AAAA", "=cmd()").unwrap();
        db.upsert_equipo("Sin licencia", "", "BBBB-BBBB-BBBB-BBBB", NOW).unwrap();
        let json: serde_json::Value = serde_json::from_str(&db.exportar_json().unwrap()).unwrap();
        assert_eq!(json["equipos"].as_array().unwrap().len(), 2);
        assert_eq!(json["equipos"][0]["notas"], "Cable \u{00f1}and\u{00fa}");
        assert_eq!(json["equipos"][0]["licencias_emitidas"][0]["token_generado"], "RESP-AAAA");
        let csv = db.exportar_csv().unwrap();
        assert_eq!(csv.lines().count(), 3, "cabecera + una licencia + un equipo sin licencia");
        assert!(csv.contains("\"Ballet \"\"Oficial\"\"\",\"Cabina, Acer\""), "comillas y comas escapadas: {csv}");
        assert!(csv.contains("\"'=cmd()\""), "una celda que parece fórmula se neutraliza");
    }

    fn registro<'a>(machine: &'a str, puesto: &'a str, venc: i64, token: &'a str) -> Registro<'a> {
        Registro { institucion: "Ballet Oficial de Córdoba", puesto, machine_id: machine, notas_equipo: "Alta desde el instalador", tipo_emision: TIPO_PENDRIVE, fecha_emision: NOW, fecha_vencimiento: venc, dias_otorgados: 30, activation_epoch: 0, token, notas: "Instalación nueva" }
    }

    #[test]
    fn registering_an_emission_upserts_the_machine_and_adds_one_row_per_event_in_one_transaction() {
        let mut db = Db::open_memory().unwrap();
        let a = db.registrar_emision(&registro("AAAA-AAAA-AAAA-AAAA", "Cabina Principal", NOW + 30 * DAY, "TEATROCUE-LICENSE-2\n...")).unwrap();
        db.set_notas(a, "cable roto en el foso").unwrap();
        // Renovación: mismo equipo, puesto vacío no borra el nombre, las notas escritas a mano se conservan.
        let b = db.registrar_emision(&Registro { puesto: "", notas: "Renovación", fecha_emision: NOW + 5, fecha_vencimiento: 0, ..registro("AAAA-AAAA-AAAA-AAAA", "", 0, "otra") }).unwrap();
        assert_eq!(a, b);
        let e = db.equipo_por_machine("AAAA-AAAA-AAAA-AAAA").unwrap().unwrap();
        assert_eq!((e.puesto_sala.as_str(), e.notas.as_str()), ("Cabina Principal", "cable roto en el foso"));
        assert_eq!(db.todos_los_equipos().unwrap().len(), 1);
        let h = db.historial(a).unwrap();
        assert_eq!((h.len(), h[0].notas.as_str(), h[0].fecha_vencimiento, h[1].token_generado.as_str()), (2, "Renovación", 0, "TEATROCUE-LICENSE-2\n..."));
        // Cambiar el puesto sí se actualiza; el alta tomó las notas del instalador.
        db.registrar_emision(&registro("AAAA-AAAA-AAAA-AAAA", "Cabina Acer", NOW + DAY, "x")).unwrap();
        assert_eq!(db.puesto_de("AAAA-AAAA-AAAA-AAAA").as_deref(), Some("Cabina Acer"));
        let n = db.registrar_emision(&registro("BBBB-BBBB-BBBB-BBBB", "Sala 2", NOW + DAY, "y")).unwrap();
        assert_eq!(db.equipo_por_machine("BBBB-BBBB-BBBB-BBBB").unwrap().unwrap().notas, "Alta desde el instalador");
        assert_ne!(n, a);
        assert!(db.puesto_de("CCCC-CCCC-CCCC-CCCC").is_none());
    }
}
