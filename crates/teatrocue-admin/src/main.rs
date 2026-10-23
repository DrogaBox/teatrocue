// En Windows nunca abre una consola.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui;
use teatrocue_admin::app::{apply_theme, AdminApp};
use teatrocue_admin::db::{default_db_path, Db};

fn main() -> eframe::Result {
    let path = default_db_path();
    let db = match Db::open(&path) {
        Ok(db) => db,
        Err(e) => {
            let msg = format!("No se pudo abrir la base de datos de licencias\n{}\n\n{e}", path.display());
            // Sin consola en Windows: el error se muestra en un cuadro de diálogo.
            let _ = rfd::MessageDialog::new().set_title("TeatroCue Admin").set_description(&msg).set_level(rfd::MessageLevel::Error).show();
            eprintln!("{msg}");
            std::process::exit(1);
        }
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_title("TeatroCue · Administración de licencias").with_inner_size([1100.0, 700.0]).with_min_inner_size([800.0, 520.0]),
        ..Default::default()
    };
    eframe::run_native(
        "TeatroCue Admin",
        options,
        Box::new(|cc| {
            apply_theme(&cc.egui_ctx);
            Ok(Box::new(AdminApp::new(db)))
        }),
    )
}
