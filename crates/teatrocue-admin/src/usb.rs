//! Detección del pendrive con `licencias.db`: en Linux las unidades montadas bajo `/run/media` y `/media`; en Windows las
//! letras de unidad (`D:\`, `E:\`…). Solo cuenta una unidad que ya tenga `admin/licencias.db` (o `licencias.db` en la raíz).

use std::path::{Path, PathBuf};

use licencias_db::DB_FILE;

/// Un pendrive con base de datos.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsbDb {
    /// Raíz de la unidad (`/run/media/usuario/ETIQUETA`, `E:\`).
    pub root: PathBuf,
    /// El archivo `licencias.db` dentro de la unidad.
    pub db: PathBuf,
}

impl UsbDb {
    /// Texto corto para la cabecera: la letra de unidad o la ruta de montaje.
    pub fn label(&self) -> String {
        self.root.display().to_string()
    }
}

/// ¿Esta carpeta es la raíz de un pendrive con `licencias.db`? Prefiere `admin/licencias.db`.
pub fn probe(root: &Path) -> Option<UsbDb> {
    [root.join("admin").join(DB_FILE), root.join(DB_FILE)].into_iter().find(|p| p.is_file()).map(|db| UsbDb { root: root.to_path_buf(), db })
}

/// Busca el primer pendrive en las carpetas de montaje `mount_roots`. Cada una puede contener directamente las unidades
/// (`/media/ETIQUETA`) o una carpeta por usuario con las unidades dentro (`/run/media/usuario/ETIQUETA`).
pub fn find_in_mount_roots(mount_roots: &[PathBuf]) -> Option<UsbDb> {
    for base in mount_roots {
        let Ok(level1) = std::fs::read_dir(base) else { continue };
        let mut level1: Vec<PathBuf> = level1.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.is_dir()).collect();
        level1.sort();
        for p in level1 {
            if let Some(u) = probe(&p) {
                return Some(u);
            }
            let Ok(level2) = std::fs::read_dir(&p) else { continue };
            let mut level2: Vec<PathBuf> = level2.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.is_dir()).collect();
            level2.sort();
            if let Some(u) = level2.iter().find_map(|q| probe(q)) {
                return Some(u);
            }
        }
    }
    None
}

/// Primera letra de unidad (de `D:` a `Z:`) con `admin\licencias.db`. `letters` permite probar con otras raíces.
pub fn find_in_drive_roots(roots: &[PathBuf]) -> Option<UsbDb> {
    roots.iter().find_map(|r| probe(r))
}

/// Carpetas de montaje de esta plataforma.
pub fn default_mount_roots() -> Vec<PathBuf> {
    vec![PathBuf::from("/run/media"), PathBuf::from("/media")]
}

/// Letras de unidad de Windows que se sondean (la `C:` es el disco del sistema y se omite).
pub fn windows_drive_roots() -> Vec<PathBuf> {
    ('D'..='Z').map(|l| PathBuf::from(format!("{l}:\\"))).collect()
}

/// Detecta el pendrive en esta plataforma.
pub fn detect() -> Option<UsbDb> {
    if cfg!(windows) {
        find_in_drive_roots(&windows_drive_roots())
    } else {
        find_in_mount_roots(&default_mount_roots())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("teatrocue-usb-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_mounted_drive_with_admin_licencias_db_is_found_under_run_media_style_roots() {
        let base = scratch("run");
        let media = base.join("run-media");
        let (user_dir, mount) = (media.join("droga"), media.join("droga").join("9496-E32D"));
        std::fs::create_dir_all(mount.join("admin")).unwrap();
        std::fs::create_dir_all(user_dir.join("Gordo")).unwrap(); // otra unidad sin base de datos
        assert_eq!(find_in_mount_roots(&[media.clone()]), None, "sin licencias.db no cuenta");
        std::fs::write(mount.join("admin").join(DB_FILE), "").unwrap();
        let u = find_in_mount_roots(&[media.clone()]).expect("pendrive detectado");
        assert_eq!((u.root.clone(), u.db.clone()), (mount.clone(), mount.join("admin").join(DB_FILE)));
        assert!(u.label().ends_with("9496-E32D"));
        // `licencias.db` suelta en la raíz también sirve, pero `admin/` tiene prioridad.
        std::fs::write(mount.join(DB_FILE), "").unwrap();
        assert_eq!(probe(&mount).unwrap().db, mount.join("admin").join(DB_FILE));
        std::fs::remove_file(mount.join("admin").join(DB_FILE)).unwrap();
        assert_eq!(probe(&mount).unwrap().db, mount.join(DB_FILE));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn media_style_roots_with_drives_directly_inside_and_windows_style_drive_roots_work_too() {
        let base = scratch("media");
        let media = base.join("media");
        std::fs::create_dir_all(media.join("USBDISK").join("admin")).unwrap();
        std::fs::write(media.join("USBDISK").join("admin").join(DB_FILE), "").unwrap();
        assert_eq!(find_in_mount_roots(&[base.join("no-existe"), media.clone()]).unwrap().root, media.join("USBDISK"));
        // «Letras de unidad»: raíces explícitas, la primera que tenga la base.
        let (d, e) = (base.join("D"), base.join("E"));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::create_dir_all(e.join("admin")).unwrap();
        std::fs::write(e.join("admin").join(DB_FILE), "").unwrap();
        assert_eq!(find_in_drive_roots(&[d, e.clone()]).unwrap().root, e);
        assert_eq!(windows_drive_roots().first().unwrap(), &PathBuf::from("D:\\"));
        assert_eq!(windows_drive_roots().len(), 23);
        let _ = std::fs::remove_dir_all(&base);
    }
}
