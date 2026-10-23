//! Lógica del instalador de TeatroCue (sin entrada/salida directa para poder probarla).
//!
//! Orden de las operaciones, pensado para que un fallo no deje la PC a medias:
//! 1. leer el Machine ID de esta PC;
//! 2. abrir `master.key.enc` con la contraseña maestra (hasta 3 intentos) — la clave privada solo existe en memoria;
//! 3. comprobar que el paquete está completo **antes** de tocar nada en el disco;
//! 4. firmar la licencia de esta PC, soltar la clave privada y verificar la licencia con la clave pública;
//! 5. copiar `payload/` a la carpeta destino (cada archivo por un temporal y renombrado) y escribir `license.key`;
//! 6. crear el acceso directo del Escritorio;
//! 7. registrar la instalación (o renovación) en `licencias.db` del pendrive, la base que consulta `teatrocue-admin`.
//!    Si ese registro falla (USB de solo lectura, p. ej.) solo se avisa: TeatroCue ya quedó instalado.

use std::fs;
use std::path::{Path, PathBuf};

use ed25519_dalek::VerifyingKey;
use qlab_license::clock::{self, MarkStore, TOLERANCE_SECS};
use qlab_license::{issue, keyfile, verify_with, License, MachineId, LICENSE_FILE_NAME};
use zeroize::Zeroizing;

pub const LICENSEE: &str = "Ballet Oficial de Córdoba";
pub const DEFAULT_TARGET: &str = r"C:\TeatroCue";
pub const MASTER_KEY_FILE: &str = "master.key.enc";
pub const PAYLOAD_DIR: &str = "payload";
/// Lo que el paquete debe traer sí o sí. `ffprobe.exe` y las DLL son opcionales (se copian si están).
pub const REQUIRED_FILES: [&str; 3] = ["teatrocue-ui.exe", "teatrocue-daemon.exe", "ffmpeg.exe"];
pub const LAUNCHER: &str = "teatrocue-ui.exe";
pub const MAX_PASSWORD_ATTEMPTS: usize = 3;
/// Puesto que se propone cuando el equipo no está registrado.
pub const DEFAULT_PUESTO: &str = "Cabina Principal";
pub const REGISTRY_FILE: &str = licencias_db::DB_FILE;
pub const SUCCESS_MESSAGE: &str = "TeatroCue instalado y activado exitosamente para el Ballet Oficial de Córdoba.";

/// Cuánto dura la licencia que se emite.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LicenseTerm {
    Days(u32),
    Perpetual,
}

/// Máximo de días que admite el instalador (10 años).
pub const MAX_DAYS: u32 = 3650;

impl LicenseTerm {
    /// Vencimiento (segundos Unix) de una licencia emitida en `now`; `0` = sin vencimiento.
    pub fn expires_from(self, now: u64) -> u64 {
        match self {
            Self::Days(d) => now + u64::from(d.clamp(1, MAX_DAYS)) * 86_400,
            Self::Perpetual => 0,
        }
    }

    pub fn describe(self, now: u64) -> String {
        match self {
            Self::Days(d) => format!("{d} días (vence el {})", qlab_license::fmt_date(self.expires_from(now))),
            Self::Perpetual => "SIN vencimiento".to_string(),
        }
    }
}

/// Opciones del menú de duración: (tecla, texto, duración). `None` = pedir un número de días.
pub const TERM_MENU: [(&str, &str, Option<LicenseTerm>); 6] = [
    ("1", "1 mes (30 días)", Some(LicenseTerm::Days(30))),
    ("2", "3 meses (90 días)", Some(LicenseTerm::Days(90))),
    ("3", "6 meses (180 días)", Some(LicenseTerm::Days(180))),
    ("4", "1 año (365 días)", Some(LicenseTerm::Days(365))),
    ("5", "Sin vencimiento", Some(LicenseTerm::Perpetual)),
    ("6", "Otra cantidad de días…", None),
];

/// Interpreta una respuesta del menú: `Ok(Some(t))` elegida, `Ok(None)` = «otra cantidad» (hay que pedir los días).
pub fn parse_menu_choice(input: &str) -> Result<Option<LicenseTerm>, String> {
    let key = input.trim();
    TERM_MENU.iter().find(|(k, ..)| *k == key).map(|(_, _, t)| *t).ok_or_else(|| format!("opción inválida: `{key}`"))
}

/// Días escritos a mano: entre 1 y `MAX_DAYS`.
pub fn parse_days(input: &str) -> Result<LicenseTerm, String> {
    let d: u32 = input.trim().parse().map_err(|_| format!("`{}` no es un número de días", input.trim()))?;
    if (1..=MAX_DAYS).contains(&d) { Ok(LicenseTerm::Days(d)) } else { Err(format!("los días deben estar entre 1 y {MAX_DAYS}")) }
}

/// Entrada y salida del instalador (la consola en producción; un guion en las pruebas).
pub trait Console {
    fn say(&mut self, text: &str);
    /// Pide la contraseña maestra (sin eco). `attempt` empieza en 1.
    fn ask_password(&mut self, attempt: usize) -> Result<Zeroizing<String>, String>;
    /// Pide cuánto debe durar la licencia. `current` describe la licencia que ya hay (si se está renovando).
    fn choose_term(&mut self, current: Option<&str>) -> Result<LicenseTerm, String>;
    /// Confirmación sí/no.
    fn confirm(&mut self, question: &str) -> bool;
    /// Pide (o confirma) el puesto/ubicación de esta PC para el registro; `default` es la propuesta.
    fn ask_puesto(&mut self, default: &str) -> String {
        default.to_string()
    }
}

/// Dónde se registra la instalación.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Registry {
    /// No se registra (pruebas que no lo necesitan).
    Off,
    /// `licencias.db` del pendrive: se busca con [`locate_registry`].
    Auto,
    /// Una base concreta.
    At(PathBuf),
}

/// `licencias.db` del pendrive: `admin/licencias.db` o `../admin/licencias.db` o junto al instalador; si no existe ninguna,
/// la que se creará (`admin/licencias.db`).
pub fn locate_registry(media_root: &Path) -> PathBuf {
    let candidates = [media_root.join("admin").join(REGISTRY_FILE), media_root.join("..").join("admin").join(REGISTRY_FILE), media_root.join(REGISTRY_FILE)];
    candidates.iter().find(|p| p.is_file()).cloned().unwrap_or_else(|| candidates[0].clone())
}

pub struct Options {
    /// Raíz del pendrive: contiene `master.key.enc` y `payload/`.
    pub media_root: PathBuf,
    pub target: PathBuf,
    /// `false` en las pruebas y con `--no-shortcut`.
    pub create_shortcut: bool,
    /// Machine ID de esta PC (en producción, el del registro).
    pub machine: MachineId,
    /// Clave pública incrustada en este instalador (si la hay): la clave maestra debe corresponderle.
    pub expected_public: Option<VerifyingKey>,
    /// Duración elegida por línea de órdenes; sin ella se pregunta.
    pub term: Option<LicenseTerm>,
    /// Renovar solo la licencia, sin volver a copiar los programas. `None` = preguntar si ya hay una instalación.
    pub license_only: Option<bool>,
    /// Si el reloj de la PC parece atrasado: `Some(true)` = el desarrollador confirma que la fecha actual es correcta
    /// y se restablece el registro de uso; `Some(false)` = rechazar; `None` = preguntar.
    pub clock_override: Option<bool>,
    /// Reloj de esta PC (segundos Unix).
    pub now: u64,
    /// Fechas que el sistema fija por sí mismo (si el reloj está por debajo, es falso).
    pub anchors: Vec<u64>,
    /// Sitios donde se guardan las marcas de reloj de esta instalación.
    pub stores: Vec<Box<dyn MarkStore>>,
    /// Puesto/ubicación indicado por línea de órdenes (`--puesto`); sin él se pregunta (con propuesta).
    pub puesto: Option<String>,
    /// Registro de la instalación en `licencias.db`.
    pub registry: Registry,
}

#[derive(Debug)]
pub struct Report {
    pub license: License,
    pub copied: Vec<String>,
    pub shortcut: Option<PathBuf>,
    pub warnings: Vec<String>,
    /// Había una licencia anterior que se sustituyó.
    pub renewed: bool,
    /// Solo se renovó la licencia (no se copiaron programas).
    pub license_only: bool,
    /// Base `licencias.db` donde quedó registrada esta emisión (`None` si no se registró; ver `warnings`).
    pub registry: Option<PathBuf>,
}

/// Registra la emisión en `licencias.db`, en una sola transacción. Cualquier error se devuelve como texto: el llamador
/// lo convierte en un aviso sin abortar la instalación.
#[allow(clippy::too_many_arguments)]
fn register_in_db(path: &Path, puesto: &str, machine: &MachineId, term: LicenseTerm, now: u64, expires: u64, epoch: u32, token: &str, notas: &str) -> Result<(), String> {
    let mut db = licencias_db::Db::open(path)?;
    let days = match term {
        LicenseTerm::Days(d) => i64::from(d.clamp(1, MAX_DAYS)),
        LicenseTerm::Perpetual => 0,
    };
    db.registrar_emision(&licencias_db::Registro {
        institucion: LICENSEE,
        puesto,
        machine_id: machine.as_str(),
        notas_equipo: "Alta automática desde el instalador del pendrive",
        tipo_emision: licencias_db::TIPO_PENDRIVE,
        fecha_emision: now as i64,
        fecha_vencimiento: expires as i64,
        dias_otorgados: days,
        activation_epoch: i64::from(epoch),
        token,
        notas,
    })?;
    Ok(())
}

/// Archivos de `dir` (recursivo), como rutas relativas con `/`.
pub fn list_files(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
        let mut entries: Vec<_> = fs::read_dir(dir)?.collect::<Result<_, _>>()?;
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let p = e.path();
            if e.file_type()?.is_dir() {
                walk(base, &p, out)?;
            } else {
                out.push(p.strip_prefix(base).expect("hijo de base").to_path_buf());
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out)?;
    Ok(out)
}

/// Comprueba que el pendrive trae todo lo necesario. Devuelve los archivos a copiar.
pub fn check_package(media_root: &Path) -> Result<Vec<PathBuf>, String> {
    let payload = media_root.join(PAYLOAD_DIR);
    if !payload.is_dir() {
        return Err(format!("no se encuentra la carpeta `{PAYLOAD_DIR}` junto al instalador ({})", payload.display()));
    }
    let files = list_files(&payload).map_err(|e| format!("{}: {e}", payload.display()))?;
    let missing: Vec<&str> = REQUIRED_FILES.iter().copied().filter(|r| !files.iter().any(|f| f.as_os_str().eq_ignore_ascii_case(r))).collect();
    if !missing.is_empty() {
        return Err(format!("el paquete está incompleto: faltan {} en `{PAYLOAD_DIR}`", missing.join(", ")));
    }
    Ok(files)
}

/// Copia `src` sobre `dst` por un temporal en la misma carpeta y renombra: nunca queda un archivo a medias.
pub fn copy_atomic(src: &Path, dst: &Path) -> Result<u64, String> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    let tmp = dst.with_file_name(format!("{}.instalando", dst.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()));
    let n = fs::copy(src, &tmp).map_err(|e| format!("copiando {}: {e}", src.display()))?;
    fs::rename(&tmp, dst).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("{}: {e} (¿TeatroCue está abierto? ciérrelo y vuelva a intentarlo)", dst.display())
    })?;
    Ok(n)
}

/// Archivos de `files` que ya existen en `target` y no se pueden abrir para escribir (Windows: el programa está en
/// marcha). Se mira antes de copiar para no dejar una mezcla de versiones (interfaz nueva con daemon viejo).
pub fn files_in_use(target: &Path, files: &[PathBuf]) -> Vec<String> {
    files
        .iter()
        .filter(|rel| {
            let p = target.join(rel);
            p.is_file() && fs::OpenOptions::new().write(true).open(&p).is_err()
        })
        .map(|rel| rel.to_string_lossy().replace('\\', "/"))
        .collect()
}

pub fn install(opts: &Options, console: &mut dyn Console) -> Result<Report, String> {
    console.say(&format!("Machine ID de esta PC: {}", opts.machine));
    let license_path = opts.target.join(LICENSE_FILE_NAME);
    let existing_text = fs::read_to_string(&license_path).ok();

    // 1. Paquete completo antes de pedir nada ni tocar el disco (salvo que solo se renueve la licencia).
    let license_only_flag = opts.license_only;
    let files = match (license_only_flag, existing_text.is_some()) {
        (Some(true), true) => Vec::new(),
        _ => check_package(&opts.media_root)?,
    };

    // 1b. Si algún programa instalado está abierto, no se toca nada: así no queda una mezcla de versiones.
    let busy = files_in_use(&opts.target, &files);
    if !busy.is_empty() {
        return Err(format!("TeatroCue está abierto ({}). Ciérrelo (la interfaz y, si lo lanzó aparte, teatrocue-daemon) y vuelva a ejecutar el instalador. No se cambió nada.", busy.join(", ")));
    }

    // 2. Clave privada: solo en memoria.
    let enc_path = opts.media_root.join(MASTER_KEY_FILE);
    let enc = fs::read(&enc_path).map_err(|e| format!("no se puede leer {}: {e}", enc_path.display()))?;
    let mut key = None;
    for attempt in 1..=MAX_PASSWORD_ATTEMPTS {
        let pw = console.ask_password(attempt)?;
        match keyfile::decrypt(&enc, &pw) {
            Ok(k) => {
                key = Some(k);
                break;
            }
            Err(keyfile::KeyfileError::BadPasswordOrCorrupt) => console.say(&format!("Contraseña incorrecta ({attempt}/{MAX_PASSWORD_ATTEMPTS}).")),
            Err(e) => return Err(e.to_string()),
        }
    }
    let key = key.ok_or("se agotaron los intentos de contraseña; no se instaló nada")?;

    // 3. La clave del pendrive tiene que ser la que reconocen los programas de este paquete.
    let public = key.verifying_key();
    if let Some(expected) = opts.expected_public {
        if expected != public {
            return Err("master.key.enc no corresponde a la clave pública de este instalador: el pendrive mezcla versiones distintas".into());
        }
    }

    // 4. El reloj de esta PC tiene que ser creíble: una licencia emitida con la fecha falsa nacería adelantada o atrasada.
    let previous = existing_text.as_deref().and_then(|t| verify_with(t, &public, &opts.machine).ok());
    let mut seen = opts.anchors.iter().copied().max().unwrap_or(0).max(clock::BUILD_UNIX);
    if let Some(prev) = &previous {
        for st in &opts.stores {
            if let Some(m) = st.read().and_then(|t| clock::parse_mark(&t, prev).ok()) {
                seen = seen.max(m);
            }
        }
    }
    if opts.now + TOLERANCE_SECS < seen {
        let (today, before) = (qlab_license::fmt_date(opts.now), qlab_license::fmt_date(seen));
        let msg = format!("el reloj de esta PC ({today}) está atrasado respecto a lo que ya se vio ({before})");
        // Solo quien tiene la contraseña maestra puede reponerlo (p. ej. si la fecha se adelantó por error y ya se corrigió).
        let accepted = match opts.clock_override {
            Some(v) => v,
            None => {
                console.say(&format!("AVISO: {msg}."));
                console.confirm(&format!("¿La fecha de hoy es realmente {today}? Si lo confirma se restablece el registro de uso de esta PC"))
            }
        };
        if !accepted {
            return Err(format!("{msg}: corrija la fecha y hora de Windows antes de instalar o renovar"));
        }
        console.say(&format!("Fecha confirmada por el desarrollador: {today}. Se restablece el registro de uso."));
    }

    // 5. ¿Instalación nueva o renovación? Y cuánto dura.
    let renewed = existing_text.is_some();
    let describe_prev = previous.as_ref().map(|p| match p.days_left(opts.now) {
        None => "licencia actual: sin vencimiento".to_string(),
        Some(d) if d < 0 => format!("licencia actual: VENCIDA el {}", qlab_license::fmt_date(p.expires_unix)),
        Some(d) => format!("licencia actual: vence el {} (quedan {d} días)", qlab_license::fmt_date(p.expires_unix)),
    });
    if let Some(d) = &describe_prev {
        console.say(&format!("Ya hay TeatroCue instalado en esta PC — {d}."));
    } else if renewed {
        console.say("Ya hay una licencia en esta PC, pero no es válida: se emitirá una nueva.");
    }
    let license_only = renewed && match opts.license_only {
        Some(v) => v,
        None => console.confirm("¿Renovar solo la licencia (sin volver a copiar los programas)?"),
    };
    let (files, copy_needed) = if license_only { (Vec::new(), false) } else if files.is_empty() { (check_package(&opts.media_root)?, true) } else { (files, true) };
    let (term, asked) = match opts.term {
        Some(t) => (t, false),
        None => (console.choose_term(describe_prev.as_deref())?, true),
    };
    if asked && !console.confirm(&format!("La licencia durará {}. ¿Confirmar?", term.describe(opts.now))) {
        return Err("cancelado: no se emitió ninguna licencia".into());
    }

    // 5b. Puesto de esta PC para el registro (la propuesta es el que ya figura en la base, si lo hay).
    let registry_path = match &opts.registry {
        Registry::Off => None,
        Registry::Auto => Some(locate_registry(&opts.media_root)),
        Registry::At(p) => Some(p.clone()),
    };
    let puesto = match (&opts.puesto, &registry_path) {
        (Some(p), _) => p.trim().to_string(),
        (None, Some(path)) => {
            let known = path.is_file().then(|| licencias_db::Db::open(path).ok().and_then(|db| db.puesto_de(opts.machine.as_str()))).flatten();
            let asked = console.ask_puesto(known.as_deref().unwrap_or(DEFAULT_PUESTO));
            if asked.trim().is_empty() { known.unwrap_or_else(|| DEFAULT_PUESTO.to_string()) } else { asked.trim().to_string() }
        }
        (None, None) => String::new(),
    };

    // 6. Firma, suelta la clave privada y verifica.
    let expires = term.expires_from(opts.now);
    let license_text = issue(LICENSEE, &opts.machine, opts.now, expires, &key);
    drop(key); // la clave privada ya no se necesita: sale de memoria (se pone a cero al soltarla)
    let license = verify_with(&license_text, &public, &opts.machine).map_err(|e| format!("la licencia recién emitida no verifica: {e}"))?;

    // 7. Copia de los programas (salvo renovación de solo licencia).
    let mut copied = Vec::new();
    if copy_needed {
        console.say(&format!("Instalando en {} …", opts.target.display()));
        let payload = opts.media_root.join(PAYLOAD_DIR);
        for rel in &files {
            let bytes = copy_atomic(&payload.join(rel), &opts.target.join(rel))?;
            let name = rel.to_string_lossy().replace('\\', "/");
            console.say(&format!("  {name} ({:.1} MB)", bytes as f64 / 1_048_576.0));
            copied.push(name);
        }
    }
    fs::create_dir_all(&opts.target).map_err(|e| format!("{}: {e}", opts.target.display()))?;
    fs::write(&license_path, &license_text).map_err(|e| format!("{}: {e}", license_path.display()))?;
    // Lo escrito se vuelve a leer y a verificar: lo que se activa es lo que quedó en disco.
    let back = fs::read_to_string(&license_path).map_err(|e| e.to_string())?;
    verify_with(&back, &public, &opts.machine).map_err(|e| format!("la licencia escrita no verifica: {e}"))?;
    copied.push(LICENSE_FILE_NAME.to_string());
    // Las marcas de reloj empiezan de cero para ESTA licencia (las anteriores pertenecían a la anterior).
    // El contador de reactivaciones de emergencia se conserva: renovar no debe volver a habilitar claves ya usadas.
    let epoch = previous
        .as_ref()
        .map_or(0, |prev| {
            let attached = existing_text.as_deref().and_then(|t| qlab_license::activation::best_attached(t, &public, &opts.machine));
            qlab_license::activation::local_epoch(prev, &opts.stores, attached.as_ref())
        });
    let written = clock::reset_with_epoch(opts.now, epoch, &license, &opts.stores);
    if written == 0 {
        return Err("no se pudo escribir el registro de uso de TeatroCue en ningún sitio".into());
    }

    // 8. Acceso directo.
    let mut warnings = Vec::new();
    if written < opts.stores.len() {
        warnings.push(format!("el registro de uso solo se pudo guardar en {written} de {} sitios", opts.stores.len()));
    }
    let shortcut = if opts.create_shortcut && copy_needed {
        match create_desktop_shortcut(&opts.target) {
            Ok(p) => Some(p),
            Err(e) => {
                warnings.push(format!("no se pudo crear el acceso directo del Escritorio: {e}"));
                None
            }
        }
    } else {
        None
    };
    if copy_needed && !files.iter().any(|f| f.as_os_str().eq_ignore_ascii_case("ffprobe.exe")) {
        warnings.push("el paquete no trae ffprobe.exe: la vista previa de vídeo no estará disponible".into());
    }

    // 9. Registro en `licencias.db` del pendrive. Un fallo (USB de solo lectura, disco lleno…) no deshace la instalación.
    let mut registry = None;
    if let Some(path) = registry_path {
        let event = if renewed { format!("Renovación de licencia{}", if license_only { " (solo licencia)" } else { " con reinstalación de los programas" }) } else { format!("Instalación nueva en {}", opts.target.display()) };
        match register_in_db(&path, &puesto, &opts.machine, term, opts.now, expires, epoch, &license_text, &event) {
            Ok(()) => registry = Some(path),
            Err(e) => warnings.push(format!("No se pudo actualizar {REGISTRY_FILE} en el USB, pero TeatroCue se instaló correctamente ({e})")),
        }
    }
    Ok(Report { license, copied, shortcut, warnings, renewed, license_only, registry })
}

/// Crea `TeatroCue.lnk` en el Escritorio del usuario apuntando a `teatrocue-ui.exe`. Usa PowerShell (incluido en
/// Windows, sin dependencias). Devuelve la ruta del acceso directo.
pub fn create_desktop_shortcut(target_dir: &Path) -> Result<PathBuf, String> {
    let q = |p: &Path| p.to_string_lossy().replace('\'', "''");
    let exe = target_dir.join(LAUNCHER);
    let script = format!(
        "$ErrorActionPreference='Stop'; $d=[Environment]::GetFolderPath('Desktop'); $l=Join-Path $d 'TeatroCue.lnk'; \
         $s=(New-Object -ComObject WScript.Shell).CreateShortcut($l); $s.TargetPath='{exe}'; $s.WorkingDirectory='{dir}'; \
         $s.Description='TeatroCue - Ballet Oficial de Córdoba'; $s.IconLocation='{exe},0'; $s.Save(); Write-Output $l",
        exe = q(&exe),
        dir = q(target_dir),
    );
    let out = std::process::Command::new("powershell").args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", &script]).output().map_err(|e| format!("no se pudo ejecutar PowerShell: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use qlab_license::clock::{FileStore, BUILD_UNIX};
    use rand_core::OsRng;

    const DAY: u64 = 86_400;

    struct Script {
        passwords: Vec<&'static str>,
        term: Option<LicenseTerm>,
        confirm: bool,
        said: Vec<String>,
        asked: usize,
        asked_term: usize,
        confirms: Vec<String>,
    }

    impl Script {
        fn new(passwords: Vec<&'static str>) -> Self {
            Self { passwords, term: Some(LicenseTerm::Days(90)), confirm: true, said: vec![], asked: 0, asked_term: 0, confirms: vec![] }
        }
    }

    impl Console for Script {
        fn say(&mut self, text: &str) {
            self.said.push(text.to_string());
        }
        fn ask_password(&mut self, _attempt: usize) -> Result<Zeroizing<String>, String> {
            let p = self.passwords.get(self.asked).copied().ok_or("sin más contraseñas en el guion")?;
            self.asked += 1;
            Ok(Zeroizing::new(p.to_string()))
        }
        fn choose_term(&mut self, _current: Option<&str>) -> Result<LicenseTerm, String> {
            self.asked_term += 1;
            self.term.ok_or_else(|| "sin duración en el guion".to_string())
        }
        fn confirm(&mut self, question: &str) -> bool {
            self.confirms.push(question.to_string());
            self.confirm
        }
    }

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("teatrocue-installer-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// Pendrive de mentira: payload con los ejecutables, una DLL y una subcarpeta, y `master.key.enc`.
    fn usb(tag: &str, password: &str, with: &[&str]) -> (PathBuf, SigningKey) {
        let root = scratch(tag);
        fs::create_dir_all(root.join(PAYLOAD_DIR).join("licencias")).unwrap();
        for f in with {
            fs::write(root.join(PAYLOAD_DIR).join(f), format!("contenido de {f}")).unwrap();
        }
        fs::write(root.join(PAYLOAD_DIR).join("licencias/ffmpeg.txt"), "gpl").unwrap();
        let key = SigningKey::generate(&mut OsRng);
        fs::write(root.join(MASTER_KEY_FILE), keyfile::encrypt(&key, password, 1_000)).unwrap();
        (root, key)
    }

    /// Un «ahora» posterior a la compilación del programa.
    fn t0() -> u64 {
        BUILD_UNIX + 100 * DAY
    }

    fn stores(target: &Path) -> Vec<Box<dyn MarkStore>> {
        vec![Box::new(FileStore(target.join("license.state"))), Box::new(FileStore(target.join("sys").join("clock.dat")))]
    }

    fn opts(root: &Path, target: &Path, key: &SigningKey) -> Options {
        Options {
            media_root: root.to_path_buf(),
            target: target.to_path_buf(),
            create_shortcut: false,
            machine: MachineId::from_guid("pc-del-teatro"),
            expected_public: Some(key.verifying_key()),
            term: None,
            license_only: None,
            now: t0(),
            clock_override: None,
            anchors: vec![],
            stores: stores(target),
            puesto: None,
            registry: Registry::Off,
        }
    }

    const FULL: [&str; 5] = ["teatrocue-ui.exe", "teatrocue-daemon.exe", "ffmpeg.exe", "ffprobe.exe", "libwinpthread-1.dll"];

    #[test]
    fn a_correct_install_copies_the_payload_and_activates_this_pc_for_the_chosen_term() {
        let (root, key) = usb("ok", "contraseña-maestra-larga", &FULL);
        let target = scratch("ok-target").join("TeatroCue");
        let mut c = Script::new(vec!["contraseña-maestra-larga"]);
        let r = install(&opts(&root, &target, &key), &mut c).unwrap();
        for f in FULL {
            assert_eq!(fs::read_to_string(target.join(f)).unwrap(), format!("contenido de {f}"));
        }
        assert!(target.join("licencias/ffmpeg.txt").is_file());
        assert!(r.copied.contains(&"license.key".to_string()) && r.copied.len() == FULL.len() + 2);
        assert!(r.warnings.is_empty(), "{:?}", r.warnings);
        assert!(!r.renewed && !r.license_only);
        // Licencia de 90 días para esta PC.
        let text = fs::read_to_string(target.join("license.key")).unwrap();
        let machine = MachineId::from_guid("pc-del-teatro");
        let l = verify_with(&text, &key.verifying_key(), &machine).unwrap();
        assert_eq!((l.licensee.as_str(), &l.machine, l.issued_unix, l.expires_unix), ("Ballet Oficial de Córdoba", &machine, t0(), t0() + 90 * DAY));
        assert!(verify_with(&text, &key.verifying_key(), &MachineId::from_guid("otra-pc")).is_err());
        // Se preguntó la duración y se confirmó con la fecha.
        assert_eq!(c.asked_term, 1);
        assert!(c.confirms.iter().any(|q| q.contains(&qlab_license::fmt_date(t0() + 90 * DAY))), "{:?}", c.confirms);
        // Las marcas de reloj quedaron en los dos sitios, auténticas para esta licencia.
        for st in stores(&target) {
            assert_eq!(clock::parse_mark(&st.read().expect("marca escrita"), &l), Ok(t0()));
        }
        assert!(!target.join("teatrocue-ui.exe.instalando").exists(), "no quedan temporales");
    }

    #[test]
    fn the_term_can_be_given_on_the_command_line_and_perpetual_means_no_expiry() {
        let (root, key) = usb("perp", "p", &FULL);
        let target = scratch("perp-target").join("TeatroCue");
        let mut c = Script::new(vec!["p"]);
        let mut o = opts(&root, &target, &key);
        o.term = Some(LicenseTerm::Perpetual);
        let r = install(&o, &mut c).unwrap();
        assert!(r.license.is_perpetual());
        assert_eq!((c.asked_term, c.confirms.len()), (0, 0), "con la duración dada no se pregunta ni se confirma");
        assert_eq!(LicenseTerm::Days(30).expires_from(0), 30 * DAY);
        assert_eq!(LicenseTerm::Days(99_999).expires_from(0), u64::from(MAX_DAYS) * DAY, "se recorta a 10 años");
        assert_eq!(LicenseTerm::Perpetual.expires_from(5), 0);
    }

    #[test]
    fn declining_the_confirmation_cancels_without_writing_anything() {
        let (root, key) = usb("noconfirm", "p", &FULL);
        let target = scratch("noconfirm-target").join("TeatroCue");
        let mut c = Script::new(vec!["p"]);
        c.confirm = false;
        let err = install(&opts(&root, &target, &key), &mut c).unwrap_err();
        assert!(err.contains("cancelado"), "{err}");
        assert!(!target.exists());
    }

    #[test]
    fn renewing_replaces_the_license_resets_the_clock_marks_and_keeps_the_user_files() {
        let (root, key) = usb("renew", "p", &FULL);
        let target = scratch("renew-target").join("TeatroCue");
        let mut c = Script::new(vec!["p"]);
        c.term = Some(LicenseTerm::Days(30));
        install(&opts(&root, &target, &key), &mut c).unwrap();
        fs::write(target.join("mis-shows.teatroshow"), "no tocar").unwrap();
        // 25 días después, estando por vencer, se vuelve con el pendrive y se renueva por un año.
        let later = t0() + 25 * DAY;
        let machine = MachineId::from_guid("pc-del-teatro");
        let first = verify_with(&fs::read_to_string(target.join("license.key")).unwrap(), &key.verifying_key(), &machine).unwrap();
        assert_eq!(first.days_left(later), Some(5));
        // Si en esta PC ya se canjearon dos reactivaciones de emergencia, renovar no devuelve el contador a cero.
        clock::reset_with_epoch(later - DAY, 2, &first, &stores(&target));
        let mut c = Script::new(vec!["p"]);
        c.term = Some(LicenseTerm::Days(365));
        c.confirm = true;
        let mut o = opts(&root, &target, &key);
        o.now = later;
        o.license_only = Some(false); // reinstalación completa
        let r = install(&o, &mut c).unwrap();
        assert!(r.renewed && !r.license_only);
        assert!(c.said.iter().any(|l| l.contains("quedan 5 días")), "se muestra lo que quedaba: {:?}", c.said);
        let second = verify_with(&fs::read_to_string(target.join("license.key")).unwrap(), &key.verifying_key(), &machine).unwrap();
        assert_eq!(second.expires_unix, later + 365 * DAY);
        assert_ne!(first.fingerprint(), second.fingerprint());
        assert_eq!(clock::epoch(&second, &stores(&target)), 2, "la época de reactivaciones se conserva al renovar");
        // Las marcas viejas (de la licencia anterior) fueron sustituidas por unas válidas para la nueva.
        for st in stores(&target) {
            assert_eq!(clock::parse_mark(&st.read().unwrap(), &second), Ok(later));
        }
        assert_eq!(fs::read_to_string(target.join("mis-shows.teatroshow")).unwrap(), "no tocar");
        // Y el chequeo completo de la aplicación acepta la licencia renovada.
        let st = qlab_license::check_with(&target.join("license.key"), later + 60, &stores(&target), &[], Some(&key.verifying_key()));
        if MachineId::local().ok().as_ref() == Some(&machine) {
            assert!(st.result.is_ok());
        }
    }

    #[test]
    fn renewing_only_the_license_does_not_need_or_copy_the_programs() {
        let (root, key) = usb("only", "p", &FULL);
        let target = scratch("only-target").join("TeatroCue");
        let mut c = Script::new(vec!["p"]);
        install(&opts(&root, &target, &key), &mut c).unwrap();
        fs::write(target.join("teatrocue-ui.exe"), "versión instalada").unwrap();
        // Pendrive sin payload (o con otro): igual se puede renovar solo la licencia.
        fs::remove_dir_all(root.join(PAYLOAD_DIR)).unwrap();
        let mut c = Script::new(vec!["p"]);
        let mut o = opts(&root, &target, &key);
        o.license_only = Some(true);
        o.now = t0() + DAY;
        let r = install(&o, &mut c).unwrap();
        assert!(r.license_only && r.renewed);
        assert_eq!(r.copied, vec!["license.key".to_string()]);
        assert_eq!(fs::read_to_string(target.join("teatrocue-ui.exe")).unwrap(), "versión instalada", "los programas no se tocan");
        // En una instalación NUEVA ese modo no tiene sentido y exige el paquete completo.
        let fresh = scratch("only-fresh").join("TeatroCue");
        let mut c = Script::new(vec!["p"]);
        let mut o = opts(&root, &fresh, &key);
        o.license_only = Some(true);
        assert!(install(&o, &mut c).unwrap_err().contains("payload"));
    }

    #[test]
    fn a_rolled_back_pc_clock_is_refused_before_issuing_a_license() {
        let (root, key) = usb("clock", "p", &FULL);
        let target = scratch("clock-target").join("TeatroCue");
        let mut c = Script::new(vec!["p"]);
        install(&opts(&root, &target, &key), &mut c).unwrap();
        // Después de usarla hasta +40 días, alguien retrasa el reloj y pide «renovar» para ganar tiempo.
        let machine = MachineId::from_guid("pc-del-teatro");
        let l = verify_with(&fs::read_to_string(target.join("license.key")).unwrap(), &key.verifying_key(), &machine).unwrap();
        clock::record(t0() + 40 * DAY, &l, &stores(&target));
        let mut c = Script::new(vec!["p"]);
        c.confirm = false; // el desarrollador no confirma la fecha
        let mut o = opts(&root, &target, &key);
        o.now = t0() + 2 * DAY;
        let err = install(&o, &mut c).unwrap_err();
        assert!(err.contains("reloj de esta PC") && err.contains("atrasado"), "{err}");
        assert_eq!(c.asked_term, 0, "ni siquiera se pregunta la duración");
        // Un ancla del sistema en el futuro (archivo de Windows) tampoco deja emitir.
        let mut c = Script::new(vec!["p"]);
        c.confirm = false;
        let mut o = opts(&root, &scratch("clock-fresh").join("TeatroCue"), &key);
        o.anchors = vec![t0() + 30 * DAY];
        assert!(install(&o, &mut c).unwrap_err().contains("atrasado"));
        // Con `--reset-reloj` (o confirmando la fecha) el desarrollador sí puede reponerlo: las marcas se reinician.
        let mut c = Script::new(vec!["p"]);
        let mut o = opts(&root, &target, &key);
        o.now = t0() + 2 * DAY;
        o.clock_override = Some(true);
        install(&o, &mut c).unwrap();
        assert!(c.said.iter().any(|l| l.contains("Fecha confirmada")));
        let l2 = verify_with(&fs::read_to_string(target.join("license.key")).unwrap(), &key.verifying_key(), &machine).unwrap();
        assert_eq!(clock::parse_mark(&stores(&target)[0].read().unwrap(), &l2), Ok(t0() + 2 * DAY), "la marca vuelve a la fecha confirmada");
    }

    #[test]
    fn menu_answers_are_parsed() {
        assert_eq!(parse_menu_choice("1"), Ok(Some(LicenseTerm::Days(30))));
        assert_eq!(parse_menu_choice(" 4 \n"), Ok(Some(LicenseTerm::Days(365))));
        assert_eq!(parse_menu_choice("5"), Ok(Some(LicenseTerm::Perpetual)));
        assert_eq!(parse_menu_choice("6"), Ok(None), "pide los días");
        assert!(parse_menu_choice("7").is_err() && parse_menu_choice("").is_err());
        assert_eq!(parse_days("45"), Ok(LicenseTerm::Days(45)));
        for bad in ["0", "-3", "abc", "3651", ""] {
            assert!(parse_days(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_wrong_password_is_retried_three_times_and_nothing_is_installed() {
        let (root, key) = usb("pw", "la-buena-contraseña", &FULL);
        let target = scratch("pw-target").join("TeatroCue");
        let mut c = Script::new(vec!["mala", "peor", "tampoco"]);
        let err = install(&opts(&root, &target, &key), &mut c).unwrap_err();
        assert!(err.contains("intentos"), "{err}");
        assert_eq!(c.asked, MAX_PASSWORD_ATTEMPTS);
        assert!(!target.exists(), "sin contraseña no se toca el destino");
        let mut c = Script::new(vec!["mala", "la-buena-contraseña"]);
        assert!(install(&opts(&root, &target, &key), &mut c).is_ok());
        assert!(c.said.iter().any(|l| l.contains("incorrecta (1/3)")));
    }

    #[test]
    fn an_incomplete_package_is_rejected_before_asking_for_the_password() {
        let (root, key) = usb("incomplete", "p", &["teatrocue-ui.exe", "teatrocue-daemon.exe"]);
        let target = scratch("incomplete-target").join("TeatroCue");
        let mut c = Script::new(vec![]);
        let err = install(&opts(&root, &target, &key), &mut c).unwrap_err();
        assert!(err.contains("faltan ffmpeg.exe"), "{err}");
        assert_eq!(c.asked, 0);
        assert!(!target.exists());
        assert!(check_package(&scratch("no-payload")).unwrap_err().contains("payload"));
    }

    #[test]
    fn a_master_key_from_another_build_is_refused() {
        let (root, _) = usb("mix", "p", &FULL);
        let other = SigningKey::generate(&mut OsRng);
        let target = scratch("mix-target").join("TeatroCue");
        let mut c = Script::new(vec!["p"]);
        let err = install(&opts(&root, &target, &other), &mut c).unwrap_err();
        assert!(err.contains("no corresponde a la clave pública"), "{err}");
        assert!(!target.exists());
    }

    #[test]
    fn a_missing_ffprobe_is_only_a_warning() {
        let (root, key) = usb("noprobe", "p", &["teatrocue-ui.exe", "teatrocue-daemon.exe", "ffmpeg.exe"]);
        let target = scratch("noprobe-target").join("TeatroCue");
        let mut c = Script::new(vec!["p"]);
        let r = install(&opts(&root, &target, &key), &mut c).unwrap();
        assert!(r.warnings.iter().any(|w| w.contains("ffprobe")));
    }

    #[test]
    fn files_in_use_ignores_missing_and_writable_files() {
        let dir = scratch("busy-target");
        fs::create_dir_all(dir.join("payload")).unwrap();
        fs::write(dir.join("teatrocue-ui.exe"), b"x").unwrap();
        let files = vec![PathBuf::from("teatrocue-ui.exe"), PathBuf::from("teatrocue-daemon.exe"), PathBuf::from("licencias/LICENSE.txt")];
        assert!(files_in_use(&dir, &files).is_empty(), "lo que no existe o se puede escribir no cuenta como en uso");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(dir.join("teatrocue-ui.exe"), fs::Permissions::from_mode(0o444)).unwrap();
            // Como usuario normal, abrir un archivo de solo lectura para escribir falla: se informa como ocupado
            // (igual que un programa en marcha en Windows). Si se corre como administrador, no falla y no hay nada que comprobar.
            if fs::OpenOptions::new().write(true).open(dir.join("teatrocue-ui.exe")).is_err() {
                assert_eq!(files_in_use(&dir, &files), vec!["teatrocue-ui.exe".to_string()]);
            }
        }
    }

    /// Consola de guion que además responde a la pregunta del puesto.
    struct WithPuesto {
        inner: Script,
        answer: &'static str,
        proposed: Vec<String>,
    }

    impl Console for WithPuesto {
        fn say(&mut self, t: &str) {
            self.inner.say(t);
        }
        fn ask_password(&mut self, a: usize) -> Result<Zeroizing<String>, String> {
            self.inner.ask_password(a)
        }
        fn choose_term(&mut self, c: Option<&str>) -> Result<LicenseTerm, String> {
            self.inner.choose_term(c)
        }
        fn confirm(&mut self, q: &str) -> bool {
            self.inner.confirm(q)
        }
        fn ask_puesto(&mut self, default: &str) -> String {
            self.proposed.push(default.to_string());
            self.answer.to_string()
        }
    }

    #[test]
    fn an_install_and_a_renewal_are_registered_in_licencias_db_and_readable_by_the_admin_layer() {
        let (root, key) = usb("reg", "pw-larga-1", &FULL);
        let target = scratch("reg-target").join("TeatroCue");
        let db_path = root.join("admin").join(REGISTRY_FILE);
        let machine = MachineId::from_guid("pc-del-teatro");
        let mut o = opts(&root, &target, &key);
        o.registry = Registry::Auto; // se ubica solo: admin/licencias.db del pendrive (y la crea)
        assert_eq!(locate_registry(&root), db_path);
        let mut c = WithPuesto { inner: Script::new(vec!["pw-larga-1", "pw-larga-1"]), answer: "Cabina Acer", proposed: vec![] };
        let r = install(&o, &mut c).unwrap();
        assert_eq!(r.registry.as_deref(), Some(db_path.as_path()));
        assert!(r.warnings.is_empty(), "{:?}", r.warnings);
        assert_eq!(c.proposed, vec![DEFAULT_PUESTO.to_string()], "propone «Cabina Principal» si el equipo es nuevo");

        // Lo que lee teatrocue-admin (misma capa `licencias-db`): equipo con su puesto y una emisión completa.
        let db = licencias_db::Db::open(&db_path).unwrap();
        let e = db.equipo_por_machine(machine.as_str()).unwrap().expect("equipo registrado");
        assert_eq!((e.institucion.as_str(), e.puesto_sala.as_str()), (LICENSEE, "Cabina Acer"));
        let h = db.historial(e.id).unwrap();
        assert_eq!(h.len(), 1);
        let license_text = fs::read_to_string(target.join("license.key")).unwrap();
        assert_eq!((h[0].tipo_emision.as_str(), h[0].fecha_emision, h[0].fecha_vencimiento, h[0].dias_otorgados, h[0].activation_epoch), (licencias_db::TIPO_PENDRIVE, t0() as i64, (t0() + 90 * DAY) as i64, 90, 0));
        assert_eq!(h[0].token_generado, license_text, "guarda el contenido completo de license.key");
        assert!(h[0].notas.starts_with("Instalación nueva en"));
        let inv = db.inventario(t0() as i64 + DAY as i64).unwrap();
        assert_eq!((inv.len(), inv[0].estado), (1, licencias_db::Estado::Activa));

        // Renovación sin vencimiento: misma PC, el puesto conocido es la propuesta, no se duplica el equipo y queda otra fila.
        let mut o2 = opts(&root, &target, &key);
        o2.registry = Registry::Auto;
        o2.license_only = Some(true);
        o2.term = Some(LicenseTerm::Perpetual);
        o2.now = t0() + 10 * DAY;
        let mut c2 = WithPuesto { inner: Script::new(vec!["pw-larga-1"]), answer: "", proposed: vec![] };
        let r2 = install(&o2, &mut c2).unwrap();
        assert!(r2.registry.is_some() && r2.renewed && r2.license_only);
        assert_eq!(c2.proposed, vec!["Cabina Acer".to_string()], "propone el puesto que ya figura");
        let db = licencias_db::Db::open(&db_path).unwrap();
        assert_eq!(db.todos_los_equipos().unwrap().len(), 1);
        let h = db.historial(e.id).unwrap();
        assert_eq!((h.len(), h[0].fecha_vencimiento, h[0].dias_otorgados, h[0].notas.as_str()), (2, 0, 0, "Renovación de licencia (solo licencia)"));
        assert_eq!(db.equipo_por_machine(machine.as_str()).unwrap().unwrap().puesto_sala, "Cabina Acer", "responder vacío conserva el puesto");
        assert_eq!(db.inventario(t0() as i64).unwrap()[0].vencimiento, Some(0));
    }

    #[test]
    fn the_puesto_flag_skips_the_question_and_a_blank_answer_uses_the_default() {
        let (root, key) = usb("puesto", "pw-larga-2", &FULL);
        let target = scratch("puesto-target").join("TeatroCue");
        let db_path = root.join("licencias.db");
        let mut o = opts(&root, &target, &key);
        o.registry = Registry::At(db_path.clone());
        o.puesto = Some("  Sala 2  ".into());
        let mut c = WithPuesto { inner: Script::new(vec!["pw-larga-2"]), answer: "no debería preguntarse", proposed: vec![] };
        install(&o, &mut c).unwrap();
        assert!(c.proposed.is_empty());
        let db = licencias_db::Db::open(&db_path).unwrap();
        assert_eq!(db.todos_los_equipos().unwrap()[0].puesto_sala, "Sala 2");
        // Una consola que no responde nada útil cae en la propuesta.
        let target2 = scratch("puesto-target2").join("TeatroCue");
        let mut o2 = opts(&root, &target2, &key);
        o2.registry = Registry::At(root.join("otra.db"));
        install(&o2, &mut Script::new(vec!["pw-larga-2"])).unwrap();
        assert_eq!(licencias_db::Db::open(&root.join("otra.db")).unwrap().todos_los_equipos().unwrap()[0].puesto_sala, DEFAULT_PUESTO);
    }

    #[test]
    fn a_registry_that_cannot_be_written_only_warns_and_the_install_still_succeeds() {
        let (root, key) = usb("ro", "pw-larga-3", &FULL);
        let target = scratch("ro-target").join("TeatroCue");
        // La «carpeta» admin es un archivo: la base no se puede crear (como un USB de solo lectura).
        fs::write(root.join("admin"), "no soy una carpeta").unwrap();
        let mut o = opts(&root, &target, &key);
        o.registry = Registry::Auto;
        let r = install(&o, &mut Script::new(vec!["pw-larga-3"])).expect("la instalación no se aborta");
        assert!(r.registry.is_none());
        assert!(r.warnings.iter().any(|w| w.starts_with("No se pudo actualizar licencias.db en el USB, pero TeatroCue se instaló correctamente")), "{:?}", r.warnings);
        // La licencia y los programas quedaron igual.
        assert!(target.join("license.key").is_file() && target.join("teatrocue-ui.exe").is_file());
        verify_with(&fs::read_to_string(target.join("license.key")).unwrap(), &key.verifying_key(), &MachineId::from_guid("pc-del-teatro")).unwrap();
    }

    #[test]
    fn the_registry_is_searched_in_admin_next_to_the_installer_then_one_level_up_then_beside_it() {
        let base = scratch("locate");
        let media = base.join("usb");
        fs::create_dir_all(media.join("admin")).unwrap();
        fs::create_dir_all(base.join("admin")).unwrap();
        assert_eq!(locate_registry(&media), media.join("admin").join(REGISTRY_FILE), "si no hay ninguna, se creará en admin/");
        fs::write(media.join(REGISTRY_FILE), "").unwrap();
        assert_eq!(locate_registry(&media), media.join(REGISTRY_FILE));
        fs::write(base.join("admin").join(REGISTRY_FILE), "").unwrap();
        assert_eq!(locate_registry(&media), media.join("..").join("admin").join(REGISTRY_FILE));
        fs::write(media.join("admin").join(REGISTRY_FILE), "").unwrap();
        assert_eq!(locate_registry(&media), media.join("admin").join(REGISTRY_FILE), "admin/ junto al instalador manda");
    }
}
