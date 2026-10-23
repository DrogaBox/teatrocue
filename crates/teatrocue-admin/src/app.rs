//! Interfaz de `teatrocue-admin`: inicio de sesión con la contraseña maestra y cuatro pestañas.

use std::path::{Path, PathBuf};

use eframe::egui::{self, Color32, RichText};
use qlab_license::activation::ChallengeCode;
use qlab_license::{fmt_date, now_unix, MachineId};

use crate::auth::{find_master_key, Session, Vigencia};
use crate::db::{dias_restantes, Db, Equipo, Estado, Fila, SyncSummary, TIPO_EMERGENCIA, TIPO_PENDRIVE};
use crate::usb::UsbDb;
use std::time::{Duration, Instant};

/// Cada cuánto se vuelve a mirar si hay un pendrive conectado.
const USB_POLL: Duration = Duration::from_secs(2);
/// Cuánto se queda a la vista el aviso de sincronización.
const BANNER_LIFETIME: Duration = Duration::from_secs(12);

const GREEN: Color32 = Color32::from_rgb(0x4c, 0xc2, 0x74);
const AMBER: Color32 = Color32::from_rgb(0xf0, 0xa0, 0x3c);
const RED: Color32 = Color32::from_rgb(0xe5, 0x5b, 0x5b);
const DIM: Color32 = Color32::from_rgb(0x9a, 0x9c, 0xa6);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    Dashboard,
    Emitir,
    Emergencia,
    Respaldo,
}

impl Tab {
    const ALL: [Self; 4] = [Self::Dashboard, Self::Emitir, Self::Emergencia, Self::Respaldo];

    fn title(self) -> &'static str {
        match self {
            Self::Dashboard => "Inventario de Salas",
            Self::Emitir => "Emitir Licencia Fija",
            Self::Emergencia => "Resolutor de Emergencias",
            Self::Respaldo => "Mantenimiento y Respaldo",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Filtro {
    Todas,
    Activas,
    PorVencer,
    Vencidas,
}

impl Filtro {
    fn label(self) -> &'static str {
        match self {
            Self::Todas => "Todas",
            Self::Activas => "Activas",
            Self::PorVencer => "Por vencer en < 15 días",
            Self::Vencidas => "Vencidas",
        }
    }

    fn color(self) -> Color32 {
        match self {
            Self::Todas => DIM,
            Self::Activas => GREEN,
            Self::PorVencer => AMBER,
            Self::Vencidas => RED,
        }
    }

    fn admits(self, e: Estado) -> bool {
        match self {
            Self::Todas => true,
            Self::Activas => e == Estado::Activa,
            Self::PorVencer => e == Estado::PorVencer,
            Self::Vencidas => e == Estado::Vencida,
        }
    }
}

/// Filtra el inventario por texto (institución, puesto o Machine ID) y por semáforo.
pub fn filtrar<'a>(filas: &'a [Fila], texto: &str, filtro: Filtro) -> Vec<&'a Fila> {
    let needle = texto.trim().to_lowercase();
    filas
        .iter()
        .filter(|f| filtro.admits(f.estado))
        .filter(|f| needle.is_empty() || format!("{} {} {}", f.equipo.institucion, f.equipo.puesto_sala, f.equipo.machine_id).to_lowercase().contains(&needle))
        .collect()
}

fn estado_color(e: Estado) -> Color32 {
    match e {
        Estado::Activa => GREEN,
        Estado::PorVencer => AMBER,
        Estado::Vencida => RED,
        Estado::SinLicencia => DIM,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum VigChoice {
    Fija(Vigencia),
    Personalizada,
}

/// Mensaje de estado: `true` = éxito.
type Notice = Option<(String, bool)>;

pub struct AdminApp {
    db: Db,
    session: Session,
    key_path: Option<PathBuf>,
    password: String,
    login_error: Option<String>,
    /// El operador eligió seguir sin desbloquear (solo consulta).
    read_only: bool,
    tab: Tab,
    // Dashboard
    search: String,
    filtro: Filtro,
    selected: Option<i64>,
    notes_draft: String,
    notes_for: Option<i64>,
    // Emitir
    f_inst: String,
    f_puesto: String,
    f_machine: String,
    f_vig: VigChoice,
    f_dias: u32,
    emit_notice: Notice,
    // Emergencia
    req: String,
    e_inst: String,
    e_puesto: String,
    e_days: u32,
    e_result: Option<String>,
    e_notice: Notice,
    // Respaldo
    backup_notice: Notice,
    // Pendrive
    usb_detector: Box<dyn Fn() -> Option<UsbDb>>,
    usb: Option<UsbDb>,
    usb_polled: Option<Instant>,
    /// Sincronizar solo al detectar un pendrive nuevo (o al abrir con uno ya conectado).
    auto_sync: bool,
    /// Unidad con la que ya se sincronizó automáticamente (para no repetirlo en cada vistazo).
    synced_for: Option<std::path::PathBuf>,
    /// Aviso grande tras sincronizar: texto, ¿éxito?, cuándo.
    banner: Option<(String, bool, Instant)>,
}

impl AdminApp {
    pub fn new(db: Db) -> Self {
        Self {
            db,
            session: Session::default(),
            key_path: find_master_key(),
            password: String::new(),
            login_error: None,
            read_only: false,
            tab: Tab::Dashboard,
            search: String::new(),
            filtro: Filtro::Todas,
            selected: None,
            notes_draft: String::new(),
            notes_for: None,
            f_inst: "Ballet Oficial de Córdoba".into(),
            f_puesto: String::new(),
            f_machine: String::new(),
            f_vig: VigChoice::Fija(Vigencia::Anio1),
            f_dias: 30,
            emit_notice: None,
            req: String::new(),
            e_inst: String::new(),
            e_puesto: String::new(),
            e_days: 7,
            e_result: None,
            e_notice: None,
            backup_notice: None,
            // Las pruebas no deben tocar un pendrive real que esté conectado.
            usb_detector: if cfg!(test) { Box::new(|| None) } else { Box::new(crate::usb::detect) },
            usb: None,
            usb_polled: None,
            auto_sync: true,
            synced_for: None,
            banner: None,
        }
    }

    /// Cambia cómo se busca el pendrive (pruebas).
    pub fn set_usb_detector(&mut self, f: impl Fn() -> Option<UsbDb> + 'static) {
        self.usb_detector = Box::new(f);
        self.usb_polled = None;
    }

    pub fn usb(&self) -> Option<&UsbDb> {
        self.usb.as_ref()
    }

    /// Texto del aviso de sincronización vigente.
    pub fn banner_text(&self) -> Option<&str> {
        self.banner.as_ref().map(|(t, _, _)| t.as_str())
    }

    /// Mira si hay un pendrive y, si acaba de aparecer, sincroniza solo.
    pub fn poll_usb(&mut self, force: bool) {
        if !force && self.usb_polled.is_some_and(|t| t.elapsed() < USB_POLL) {
            return;
        }
        self.usb_polled = Some(Instant::now());
        self.usb = (self.usb_detector)();
        match &self.usb {
            None => self.synced_for = None,
            Some(u) if self.auto_sync && self.synced_for.as_deref() != Some(u.root.as_path()) => {
                self.synced_for = Some(u.root.clone());
                self.sync_now();
            }
            Some(_) => {}
        }
    }

    /// Fusiona la base local con la del pendrive y muestra el resultado.
    pub fn sync_now(&mut self) {
        let Some(usb) = self.usb.clone() else {
            self.banner = Some(("No hay pendrive conectado con licencias.db".into(), false, Instant::now()));
            return;
        };
        self.banner = Some(match licencias_db::sync_with_file(&self.db, &usb.db) {
            Ok(s) => (sync_message(&s), true, Instant::now()),
            Err(e) => (format!("No se pudo sincronizar con el pendrive: {e}"), false, Instant::now()),
        });
        // Las listas se leen de la base en cada fotograma; las notas del equipo abierto se vuelven a cargar.
        self.notes_for = None;
    }

    pub fn is_unlocked(&self) -> bool {
        self.session.is_unlocked()
    }

    /// Intenta desbloquear con la contraseña escrita; la contraseña se borra de la memoria de la interfaz en cualquier caso.
    pub fn try_unlock(&mut self) -> Result<(), String> {
        let pw = std::mem::take(&mut self.password);
        let path = self.key_path.clone().ok_or("no se encontró master.key.enc (junto al programa o en ~/.config/teatrocue-keys)")?;
        let r = self.session.unlock(&path, &pw);
        self.login_error = r.as_ref().err().cloned();
        r
    }

    // ---------------------------------------------------------------------------------------
    // Operaciones (separadas de los diálogos para poder probarlas)
    // ---------------------------------------------------------------------------------------

    fn vigencia(&self) -> Vigencia {
        match self.f_vig {
            VigChoice::Fija(v) => v,
            VigChoice::Personalizada => Vigencia::Dias(self.f_dias.clamp(1, 3650)),
        }
    }

    /// Firma el `license.key` del formulario, lo escribe en `path` y registra equipo y emisión.
    pub fn issue_license_to(&mut self, path: &Path) -> Result<String, String> {
        let machine: MachineId = self.f_machine.parse()?;
        let now = now_unix();
        let vig = self.vigencia();
        let text = self.session.license_file(&self.f_inst, &machine, vig, now)?;
        std::fs::write(path, &text).map_err(|e| format!("{}: {e}", path.display()))?;
        let id = self.db.upsert_equipo(&self.f_inst, &self.f_puesto, machine.as_str(), now as i64)?;
        let venc = vig.vence(now);
        self.db.add_emision(id, TIPO_PENDRIVE, now as i64, venc as i64, i64::from(vig.dias()), 0, &text, "")?;
        Ok(format!("license.key guardado en {} (vence: {})", path.display(), if venc == 0 { "nunca".to_string() } else { fmt_date(venc) }))
    }

    /// Equipo ya registrado para el código pegado, si el código es válido.
    pub fn equipo_del_codigo(&self) -> Option<Equipo> {
        let c: ChallengeCode = self.req.parse().ok()?;
        self.db.equipo_por_machine(c.machine.as_str()).ok().flatten()
    }

    /// Firma el código pegado, guarda el evento (y el equipo, si era nuevo) y devuelve la clave `RESP-…`.
    pub fn resolve_emergency(&mut self) -> Result<String, String> {
        let now = now_unix();
        let known = self.equipo_del_codigo();
        let (challenge, act, resp) = self.session.sign_emergency(&self.req, self.e_days, now)?;
        let (inst, puesto) = match &known {
            Some(e) => (e.institucion.clone(), e.puesto_sala.clone()),
            None => (self.e_inst.clone(), self.e_puesto.clone()),
        };
        if known.is_none() && inst.trim().is_empty() {
            return Err("el equipo no está registrado: escriba la institución para guardarlo".into());
        }
        let id = self.db.upsert_equipo(&inst, &puesto, challenge.machine.as_str(), now as i64)?;
        self.db.add_emision(id, TIPO_EMERGENCIA, now as i64, act.valid_until as i64, i64::from(self.e_days), i64::from(challenge.epoch), &resp, "")?;
        Ok(resp)
    }

    pub fn export_backup_to(&self, path: &Path) -> Result<(), String> {
        let text = if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("csv")) { self.db.exportar_csv()? } else { self.db.exportar_json()? };
        std::fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()))
    }

    // ---------------------------------------------------------------------------------------
    // Interfaz
    // ---------------------------------------------------------------------------------------

    pub fn show(&mut self, ui: &mut egui::Ui) {
        self.poll_usb(false);
        ui.ctx().request_repaint_after(USB_POLL);
        if self.banner.as_ref().is_some_and(|(_, _, t)| t.elapsed() > BANNER_LIFETIME) {
            self.banner = None;
        }
        self.banner_ui(ui);
        if !self.session.is_unlocked() && !self.read_only {
            self.login_ui(ui);
            return;
        }
        egui::Panel::top("tabs").show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                for t in Tab::ALL {
                    let sel = self.tab == t;
                    if ui.selectable_label(sel, RichText::new(t.title()).strong().color(if sel { AMBER } else { Color32::WHITE })).clicked() {
                        self.tab = t;
                    }
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    self.usb_indicator(ui);
                    ui.separator();
                    if self.session.is_unlocked() {
                        if ui.button("Bloquear").clicked() {
                            self.session.lock();
                            self.read_only = false;
                        }
                        ui.label(RichText::new("Sesión desbloqueada").color(GREEN));
                    } else {
                        ui.label(RichText::new("Solo consulta: firmas bloqueadas").color(AMBER));
                        if ui.button("Desbloquear").clicked() {
                            self.read_only = false;
                        }
                    }
                });
            });
            ui.add_space(4.0);
        });
        match self.tab {
            Tab::Dashboard => self.dashboard_ui(ui),
            Tab::Emitir => {
                egui::CentralPanel::default().show(ui, |ui| self.emitir_ui(ui));
            }
            Tab::Emergencia => {
                egui::CentralPanel::default().show(ui, |ui| self.emergencia_ui(ui));
            }
            Tab::Respaldo => {
                egui::CentralPanel::default().show(ui, |ui| self.respaldo_ui(ui));
            }
        }
    }

    /// Cabecera: «Pendrive no detectado», o «✓ Pendrive detectado [ruta]» con el botón «Sincronizar ahora».
    fn usb_indicator(&mut self, ui: &mut egui::Ui) {
        match self.usb.clone() {
            None => {
                ui.label(RichText::new("Pendrive no detectado").color(DIM));
            }
            Some(u) => {
                if ui.button(RichText::new("Sincronizar ahora").strong().color(Color32::BLACK)).on_hover_text("Fusiona las licencias del pendrive con la base de esta PC (en los dos sentidos)").highlight().clicked() {
                    self.sync_now();
                }
                ui.label(RichText::new(format!("Pendrive detectado [{}]", u.label())).strong().color(GREEN));
                // La tilde se dibuja (la tipografía incluida no trae el carácter ✓).
                let (rect, _) = ui.allocate_exact_size(egui::vec2(16.0, 16.0), egui::Sense::hover());
                let (a, b, c) = (rect.left_top() + egui::vec2(2.0, 9.0), rect.left_top() + egui::vec2(6.5, 13.5), rect.left_top() + egui::vec2(14.0, 3.0));
                ui.painter().add(egui::Shape::line(vec![a, b, c], egui::Stroke::new(2.5, GREEN)));
            }
        }
    }

    /// Aviso grande y claro del resultado de la sincronización.
    fn banner_ui(&mut self, ui: &mut egui::Ui) {
        let Some((text, ok, _)) = self.banner.clone() else { return };
        let mut close = false;
        egui::Panel::top("banner").show(ui, |ui| {
            egui::Frame::new().fill(if ok { Color32::from_rgb(0x14, 0x3a, 0x24) } else { Color32::from_rgb(0x4a, 0x18, 0x18) }).inner_margin(egui::Margin::symmetric(14, 10)).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(text).size(20.0).strong().color(if ok { GREEN } else { RED }));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        close = ui.button("Cerrar").clicked();
                    });
                });
            });
        });
        if close {
            self.banner = None;
        }
    }

    fn login_ui(&mut self, ui: &mut egui::Ui) {
        egui::CentralPanel::default().show(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(80.0);
                ui.heading("TeatroCue · Administración de licencias");
                ui.add_space(6.0);
                ui.label(RichText::new("Funciona sin conexión. La clave privada se descifra solo en memoria durante la sesión.").color(DIM));
                ui.add_space(20.0);
                match &self.key_path {
                    Some(p) => ui.label(RichText::new(format!("Clave: {}", p.display())).small().color(DIM)),
                    None => ui.label(RichText::new("No se encontró master.key.enc (junto al programa o en ~/.config/teatrocue-keys).").color(RED)),
                };
                ui.add_space(8.0);
                let l = ui.label("Contraseña maestra");
                let r = ui.add(egui::TextEdit::singleline(&mut self.password).password(true).hint_text("Contraseña maestra").desired_width(300.0)).labelled_by(l.id);
                let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                ui.add_space(8.0);
                if (ui.button(RichText::new("Desbloquear").strong()).clicked() || enter) && self.key_path.is_some() {
                    let _ = self.try_unlock();
                }
                if let Some(e) = &self.login_error {
                    ui.add_space(6.0);
                    ui.label(RichText::new(e).color(RED));
                }
                ui.add_space(16.0);
                ui.horizontal(|ui| {
                    ui.add_space((ui.available_width() / 2.0 - 160.0).max(0.0));
                    self.usb_indicator(ui);
                });
                ui.add_space(8.0);
                if ui.small_button("Continuar sin firmar (solo consulta)").clicked() {
                    self.read_only = true;
                    self.password.clear();
                }
            });
        });
    }

    fn notice(ui: &mut egui::Ui, n: &Notice) {
        if let Some((msg, ok)) = n {
            ui.label(RichText::new(msg).color(if *ok { GREEN } else { RED }));
        }
    }

    fn locked_banner(&self, ui: &mut egui::Ui) -> bool {
        if self.session.is_unlocked() {
            return false;
        }
        ui.label(RichText::new("Las firmas están bloqueadas: desbloquee con la contraseña maestra.").color(AMBER));
        true
    }

    fn dashboard_ui(&mut self, ui: &mut egui::Ui) {
        let now = now_unix() as i64;
        let filas = self.db.inventario(now).unwrap_or_default();
        // El panel lateral solo existe con una fila elegida.
        if let Some(id) = self.selected {
            if self.notes_for != Some(id) {
                self.notes_for = Some(id);
                self.notes_draft = filas.iter().find(|f| f.equipo.id == id).map(|f| f.equipo.notas.clone()).unwrap_or_default();
            }
            let fila = filas.iter().find(|f| f.equipo.id == id).cloned();
            egui::Panel::right("detalle").min_size(320.0).show(ui, |ui| {
                let Some(f) = fila else {
                    ui.label("El equipo ya no existe.");
                    return;
                };
                ui.add_space(6.0);
                ui.heading(if f.equipo.institucion.is_empty() { "(sin institución)" } else { &f.equipo.institucion });
                ui.label(&f.equipo.puesto_sala);
                ui.label(RichText::new(&f.equipo.machine_id).monospace().color(DIM));
                ui.separator();
                ui.label(RichText::new("Historial de licencias").strong());
                egui::ScrollArea::vertical().max_height(260.0).id_salt("hist").show(ui, |ui| match self.db.historial(id) {
                    Ok(h) if h.is_empty() => {
                        ui.label(RichText::new("Todavía no se emitió ninguna.").color(DIM));
                    }
                    Ok(h) => {
                        for m in h {
                            let vence = if m.fecha_vencimiento == 0 { "perpetua".to_string() } else { format!("vence {}", fmt_date(m.fecha_vencimiento as u64)) };
                            ui.label(RichText::new(format!("{} · {}", fmt_date(m.fecha_emision as u64), m.tipo_emision)).strong());
                            ui.label(RichText::new(format!("{vence} · {} días · época {}", m.dias_otorgados, m.activation_epoch)).small().color(DIM));
                            ui.add_space(4.0);
                        }
                    }
                    Err(e) => {
                        ui.label(RichText::new(e).color(RED));
                    }
                });
                ui.separator();
                let l = ui.label(RichText::new("Notas").strong());
                let r = ui.add(egui::TextEdit::multiline(&mut self.notes_draft).desired_rows(5).desired_width(f32::INFINITY).hint_text("Notas del equipo (se guardan solas)")).labelled_by(l.id);
                if r.changed() {
                    let _ = self.db.set_notas(id, &self.notes_draft);
                }
            });
        }
        egui::CentralPanel::default().show(ui, |ui| {
            ui.horizontal(|ui| {
                let l = ui.label("Buscar");
                ui.add(egui::TextEdit::singleline(&mut self.search).hint_text("Buscar por teatro, puesto o Machine ID…").desired_width(320.0)).labelled_by(l.id);
                for f in [Filtro::Todas, Filtro::Activas, Filtro::PorVencer, Filtro::Vencidas] {
                    let sel = self.filtro == f;
                    if ui.selectable_label(sel, RichText::new(f.label()).color(f.color()).strong()).clicked() {
                        self.filtro = f;
                    }
                }
            });
            ui.add_space(6.0);
            let vistas = filtrar(&filas, &self.search, self.filtro);
            ui.label(RichText::new(format!("{} de {} equipos", vistas.len(), filas.len())).small().color(DIM));
            if filas.is_empty() {
                ui.add_space(20.0);
                ui.label(RichText::new("Todavía no hay equipos. Emita una licencia o resuelva una emergencia para registrar el primero.").color(DIM));
                return;
            }
            egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
                egui::Grid::new("inventario").num_columns(6).striped(true).spacing([18.0, 6.0]).show(ui, |ui| {
                    for h in ["Institución", "Sala / Puesto", "Machine ID", "Vencimiento", "Días restantes", "Estado"] {
                        ui.label(RichText::new(h).small().strong().color(DIM));
                    }
                    ui.end_row();
                    for f in vistas {
                        let sel = self.selected == Some(f.equipo.id);
                        let (venc, dias) = match f.vencimiento {
                            None => ("—".to_string(), "—".to_string()),
                            Some(0) => ("Perpetua".to_string(), "∞".to_string()),
                            Some(v) => (fmt_date(v as u64), dias_restantes(v, now).map_or("—".into(), |d| d.to_string())),
                        };
                        let cells = [
                            f.equipo.institucion.clone(),
                            f.equipo.puesto_sala.clone(),
                            f.equipo.machine_id.clone(),
                            venc,
                            dias.replace('∞', "sin límite"),
                        ];
                        let mut clicked = false;
                        for c in cells {
                            clicked |= ui.selectable_label(sel, c).clicked();
                        }
                        clicked |= ui.selectable_label(sel, RichText::new(f.estado.label()).strong().color(estado_color(f.estado))).clicked();
                        if clicked {
                            self.selected = Some(f.equipo.id);
                        }
                        ui.end_row();
                    }
                });
            });
        });
    }

    fn emitir_ui(&mut self, ui: &mut egui::Ui) {
        ui.heading("Emitir licencia fija (archivo license.key)");
        ui.add_space(8.0);
        let locked = self.locked_banner(ui);
        egui::Grid::new("form_emitir").num_columns(2).spacing([14.0, 8.0]).show(ui, |ui| {
            let l = ui.label("Institución");
            ui.add(egui::TextEdit::singleline(&mut self.f_inst).desired_width(360.0)).labelled_by(l.id);
            ui.end_row();
            let l = ui.label("Puesto / Ubicación");
            ui.add(egui::TextEdit::singleline(&mut self.f_puesto).hint_text("Cabina Principal Acer").desired_width(360.0)).labelled_by(l.id);
            ui.end_row();
            let l = ui.label("Machine ID");
            ui.vertical(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.f_machine).hint_text("XXXX-XXXX-XXXX-XXXX").desired_width(260.0)).labelled_by(l.id);
                if !self.f_machine.trim().is_empty() {
                    match self.f_machine.trim().parse::<MachineId>() {
                        Ok(_) => ui.label(RichText::new("Formato válido").small().color(GREEN)),
                        Err(_) => ui.label(RichText::new("Formato inválido: son 4 grupos de 4 dígitos hexadecimales").small().color(RED)),
                    };
                }
            });
            ui.end_row();
            ui.label("Vigencia");
            ui.horizontal_wrapped(|ui| {
                for v in Vigencia::FIJAS {
                    ui.selectable_value(&mut self.f_vig, VigChoice::Fija(v), v.label());
                }
                ui.selectable_value(&mut self.f_vig, VigChoice::Personalizada, "Días personalizados");
                if self.f_vig == VigChoice::Personalizada {
                    ui.add(egui::DragValue::new(&mut self.f_dias).range(1..=3650).suffix(" días"));
                }
            });
            ui.end_row();
        });
        ui.add_space(10.0);
        let valid = self.f_machine.trim().parse::<MachineId>().is_ok() && !self.f_inst.trim().is_empty();
        if ui.add_enabled(!locked && valid, egui::Button::new(RichText::new("Generar y Guardar license.key").strong())).clicked() {
            let dest = rfd::FileDialog::new().set_title("Guardar license.key").set_file_name("license.key").save_file();
            if let Some(path) = dest {
                self.emit_notice = Some(match self.issue_license_to(&path) {
                    Ok(m) => (m, true),
                    Err(e) => (e, false),
                });
            }
        }
        ui.add_space(8.0);
        Self::notice(ui, &self.emit_notice);
    }

    fn emergencia_ui(&mut self, ui: &mut egui::Ui) {
        ui.heading("Resolutor de emergencias (WhatsApp)");
        ui.add_space(8.0);
        let locked = self.locked_banner(ui);
        let l = ui.label("Pegue el código de solicitud recibido:");
        let r = ui.add(egui::TextEdit::multiline(&mut self.req).font(egui::TextStyle::Monospace).desired_rows(3).desired_width(f32::INFINITY).hint_text("REQ-XXXX-XXXX-XXXX-XXXX-EE")).labelled_by(l.id);
        if r.changed() {
            self.e_result = None;
            self.e_notice = None;
        }
        let parsed = if self.req.trim().is_empty() { None } else { Some(self.req.parse::<ChallengeCode>()) };
        let mut can_sign = false;
        match &parsed {
            None => {}
            Some(Err(e)) => {
                ui.label(RichText::new(e.to_string()).color(RED));
            }
            Some(Ok(c)) => {
                ui.label(RichText::new(format!("PC {} · época {}", c.machine, c.epoch)).monospace().color(DIM));
                match self.equipo_del_codigo() {
                    Some(e) => {
                        ui.label(RichText::new(format!("Equipo identificado: {} — {}", e.institucion, e.puesto_sala)).strong().color(GREEN));
                        can_sign = true;
                    }
                    None => {
                        ui.label(RichText::new("Equipo no registrado: guárdelo con un nombre.").color(AMBER));
                        egui::Grid::new("nuevo_equipo").num_columns(2).show(ui, |ui| {
                            let l = ui.label("Institución");
                            ui.add(egui::TextEdit::singleline(&mut self.e_inst)).labelled_by(l.id);
                            ui.end_row();
                            let l = ui.label("Puesto");
                            ui.add(egui::TextEdit::singleline(&mut self.e_puesto)).labelled_by(l.id);
                            ui.end_row();
                        });
                        can_sign = !self.e_inst.trim().is_empty();
                    }
                }
            }
        }
        ui.add_space(8.0);
        ui.horizontal_wrapped(|ui| {
            ui.label("Extensión:");
            for (d, l) in [(7, "7 días (emergencia)"), (15, "15 días"), (30, "30 días"), (90, "90 días"), (365, "1 año")] {
                ui.selectable_value(&mut self.e_days, d, l);
            }
            ui.add(egui::DragValue::new(&mut self.e_days).range(1..=365).suffix(" días"));
        });
        ui.add_space(8.0);
        if ui.add_enabled(!locked && can_sign, egui::Button::new(RichText::new("Firmar y Generar Clave").strong())).clicked() {
            match self.resolve_emergency() {
                Ok(resp) => {
                    ui.ctx().copy_text(whatsapp_text(&resp));
                    self.e_notice = Some(("Clave generada y copiada al portapapeles.".into(), true));
                    self.e_result = Some(resp);
                }
                Err(e) => {
                    self.e_result = None;
                    self.e_notice = Some((e, false));
                }
            }
        }
        ui.add_space(8.0);
        if let Some(resp) = self.e_result.clone() {
            ui.group(|ui| {
                ui.label(RichText::new(&resp).monospace().strong().color(AMBER));
            });
            ui.add_space(6.0);
            if ui.add_sized([280.0, 44.0], egui::Button::new(RichText::new("Copiar para WhatsApp").strong())).clicked() {
                ui.ctx().copy_text(whatsapp_text(&resp));
                self.e_notice = Some(("Copiado. Péguelo en WhatsApp.".into(), true));
            }
        }
        Self::notice(ui, &self.e_notice);
    }

    fn respaldo_ui(&mut self, ui: &mut egui::Ui) {
        ui.heading("Mantenimiento y respaldo");
        ui.add_space(8.0);
        let (equipos, emisiones) = (self.db.todos_los_equipos().map_or(0, |v| v.len()), self.db.todas_las_emisiones().map_or(0, |v| v.len()));
        ui.label(format!("{equipos} equipos · {emisiones} licencias emitidas"));
        ui.label(RichText::new(format!("Base de datos: {}", crate::db::default_db_path().display())).small().color(DIM));
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            for (label, ext) in [("Exportar respaldo (JSON)", "json"), ("Exportar respaldo (CSV)", "csv")] {
                if ui.button(label).clicked() {
                    let dest = rfd::FileDialog::new().set_title("Exportar respaldo").set_file_name(format!("licencias-respaldo.{ext}")).save_file();
                    if let Some(path) = dest {
                        self.backup_notice = Some(match self.export_backup_to(&path) {
                            Ok(()) => (format!("Respaldo guardado en {}", path.display()), true),
                            Err(e) => (e, false),
                        });
                    }
                }
            }
        });
        ui.add_space(8.0);
        Self::notice(ui, &self.backup_notice);
    }
}

/// «Sincronización completada: +N equipos nuevos, +M licencias importadas del pendrive».
pub fn sync_message(s: &SyncSummary) -> String {
    let mut t = format!("Sincronización completada: +{} equipos nuevos, +{} licencias importadas del pendrive", s.equipos_nuevos, s.licencias_importadas);
    if s.licencias_exportadas + s.equipos_exportados > 0 && s.remoto_escribible {
        t.push_str(&format!(" · enviados al pendrive: +{} equipos, +{} licencias", s.equipos_exportados, s.licencias_exportadas));
    }
    if !s.remoto_escribible {
        t.push_str(" (el pendrive es de solo lectura: no se le enviaron cambios)");
    }
    t
}

/// Texto listo para pegar en WhatsApp.
pub fn whatsapp_text(resp: &str) -> String {
    format!("Clave de activación TeatroCue: [{resp}]")
}

impl eframe::App for AdminApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.show(ui);
    }
}

pub fn apply_theme(ctx: &egui::Context) {
    ctx.set_visuals(egui::Visuals::dark());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Estado;
    use ed25519_dalek::SigningKey;
    use qlab_license::activation::challenge_for;
    use qlab_license::clock::MarkStore;
    use qlab_license::keyfile;

    fn app_unlocked(tag: &str) -> (AdminApp, SigningKey, PathBuf) {
        let dir = std::env::temp_dir().join(format!("teatrocue-admin-app-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sk = SigningKey::from_bytes(&[5u8; 32]);
        let kp = dir.join("master.key.enc");
        std::fs::write(&kp, keyfile::encrypt(&sk, "pw", 1_000)).unwrap();
        let mut app = AdminApp::new(Db::open_memory().unwrap());
        app.key_path = Some(kp);
        app.password = "pw".into();
        app.try_unlock().unwrap();
        (app, sk, dir)
    }

    fn request_for(guid: &str) -> (MachineId, String) {
        let m = MachineId::from_guid(guid);
        let t0 = qlab_license::clock::BUILD_UNIX + 10 * 86_400;
        let lic = qlab_license::License { licensee: "x".into(), machine: m.clone(), issued_unix: t0, expires_unix: t0 + 86_400 };
        let stores: Vec<Box<dyn MarkStore>> = Vec::new();
        (m, challenge_for(&lic, &stores, None).to_string())
    }

    #[test]
    fn unlocking_clears_the_password_and_a_wrong_one_keeps_signing_blocked() {
        let (mut app, _, _) = app_unlocked("login");
        assert!(app.is_unlocked() && app.password.is_empty());
        app.session.lock();
        app.password = "mal".into();
        assert!(app.try_unlock().is_err() && !app.is_unlocked());
        assert!(app.password.is_empty(), "la contraseña no queda en la interfaz");
        assert!(app.login_error.is_some());
        assert!(app.issue_license_to(Path::new("/tmp/x")).is_err());
    }

    #[test]
    fn issuing_a_license_writes_a_verifiable_file_and_registers_machine_and_emission() {
        let (mut app, sk, dir) = app_unlocked("issue");
        let (m, _) = request_for("cabina-acer");
        app.f_inst = "Ballet Oficial de Córdoba".into();
        app.f_puesto = "Cabina Principal Acer".into();
        app.f_machine = m.to_string().to_lowercase();
        app.f_vig = VigChoice::Fija(Vigencia::Meses6);
        let out = dir.join("license.key");
        app.issue_license_to(&out).unwrap();
        let l = qlab_license::verify_with(&std::fs::read_to_string(&out).unwrap(), &sk.verifying_key(), &m).unwrap();
        assert_eq!(l.licensee, "Ballet Oficial de Córdoba");
        let e = app.db.equipo_por_machine(m.as_str()).unwrap().unwrap();
        assert_eq!(e.puesto_sala, "Cabina Principal Acer");
        let h = app.db.historial(e.id).unwrap();
        assert_eq!((h.len(), h[0].tipo_emision.as_str(), h[0].dias_otorgados), (1, TIPO_PENDRIVE, 180));
        assert_eq!(h[0].fecha_vencimiento as u64, l.expires_unix);
        // Machine ID inválido: no escribe ni registra.
        app.f_machine = "1234".into();
        assert!(app.issue_license_to(&dir.join("otro.key")).is_err() && !dir.join("otro.key").exists());
        // Perpetua queda en 0.
        app.f_machine = m.to_string();
        app.f_vig = VigChoice::Fija(Vigencia::Perpetua);
        app.issue_license_to(&out).unwrap();
        assert_eq!(app.db.historial(e.id).unwrap()[0].fecha_vencimiento, 0);
        assert_eq!(app.db.inventario(now_unix() as i64).unwrap()[0].estado, Estado::Activa);
    }

    #[test]
    fn resolving_an_emergency_identifies_known_machines_registers_new_ones_and_logs_the_event() {
        let (mut app, sk, _) = app_unlocked("emerg");
        let (m, code) = request_for("sala-nueva");
        app.req = format!("Hola, este es el código: {code} gracias");
        // El texto de WhatsApp con ruido no es un código válido: hay que pegar solo el código.
        app.req = code.clone();
        assert!(app.equipo_del_codigo().is_none());
        // Equipo nuevo sin nombre: no firma.
        assert!(app.resolve_emergency().unwrap_err().contains("institución"));
        app.e_inst = "Teatro del Libertador".into();
        app.e_puesto = "Cabina".into();
        app.e_days = 15;
        let resp = app.resolve_emergency().unwrap();
        let key = qlab_license::activation::verify_activation(&resp, &sk.verifying_key()).unwrap();
        assert_eq!((key.machine, key.epoch), (m.clone(), 0));
        let e = app.equipo_del_codigo().expect("quedó registrado");
        assert_eq!((e.institucion.as_str(), e.puesto_sala.as_str()), ("Teatro del Libertador", "Cabina"));
        let h = app.db.historial(e.id).unwrap();
        assert_eq!((h[0].tipo_emision.as_str(), h[0].dias_otorgados, h[0].token_generado.as_str()), (TIPO_EMERGENCIA, 15, resp.as_str()));
        // Segunda vez: ya identificado, no hace falta nombre y se suma al historial.
        app.e_inst.clear();
        app.e_days = 7;
        app.resolve_emergency().unwrap();
        assert_eq!(app.db.historial(e.id).unwrap().len(), 2);
        assert_eq!(whatsapp_text("RESP-A"), "Clave de activación TeatroCue: [RESP-A]");
    }

    #[test]
    fn search_and_chips_filter_the_inventory() {
        let db = Db::open_memory().unwrap();
        let now = now_unix() as i64;
        let a = db.upsert_equipo("Ballet Oficial", "Cabina", "AAAA-AAAA-AAAA-AAAA", now).unwrap();
        let b = db.upsert_equipo("Teatro Real", "Sala 2", "BBBB-BBBB-BBBB-BBBB", now).unwrap();
        let c = db.upsert_equipo("Otro", "Foso", "CCCC-CCCC-CCCC-CCCC", now).unwrap();
        db.add_emision(a, TIPO_PENDRIVE, now, now + 100 * 86_400, 100, 0, "", "").unwrap();
        db.add_emision(b, TIPO_PENDRIVE, now, now + 3 * 86_400, 3, 0, "", "").unwrap();
        db.add_emision(c, TIPO_PENDRIVE, now - 9 * 86_400, now - 86_400, 8, 0, "", "").unwrap();
        let filas = db.inventario(now).unwrap();
        let names = |t: &str, f: Filtro| filtrar(&filas, t, f).iter().map(|x| x.equipo.institucion.clone()).collect::<Vec<_>>();
        assert_eq!(names("", Filtro::Todas).len(), 3);
        assert_eq!(names("", Filtro::Activas), vec!["Ballet Oficial"]);
        assert_eq!(names("", Filtro::PorVencer), vec!["Teatro Real"]);
        assert_eq!(names("", Filtro::Vencidas), vec!["Otro"]);
        assert_eq!(names("sala 2", Filtro::Todas), vec!["Teatro Real"]);
        assert_eq!(names("cccc-cccc", Filtro::Todas), vec!["Otro"]);
        assert_eq!(names("ballet", Filtro::Vencidas).len(), 0);
    }

    #[test]
    fn the_backup_is_written_as_json_or_csv_by_extension() {
        let (app, _, dir) = app_unlocked("backup");
        app.db.upsert_equipo("A", "x", "AAAA-AAAA-AAAA-AAAA", 1).unwrap();
        app.export_backup_to(&dir.join("r.json")).unwrap();
        app.export_backup_to(&dir.join("r.csv")).unwrap();
        assert!(std::fs::read_to_string(dir.join("r.json")).unwrap().contains("teatrocue-licencias/v1"));
        assert!(std::fs::read_to_string(dir.join("r.csv")).unwrap().starts_with("institucion,"));
    }

    #[test]
    fn the_ui_shows_the_login_then_the_four_tabs() {
        use egui_kittest::kittest::Queryable;
        let (app, _, _) = app_unlocked("ui");
        let mut locked = AdminApp::new(Db::open_memory().unwrap());
        locked.key_path = app.key_path.clone();
        let mut h = egui_kittest::Harness::new_ui_state(|ui, a: &mut AdminApp| a.show(ui), locked);
        h.run();
        assert!(h.query_by_label("Continuar sin firmar (solo consulta)").is_some(), "pantalla de inicio de sesión");
        assert!(h.query_by_label("Inventario de Salas").is_none());
        let mut h = egui_kittest::Harness::new_ui_state(|ui, a: &mut AdminApp| a.show(ui), app);
        h.run();
        for t in ["Inventario de Salas", "Emitir Licencia Fija", "Resolutor de Emergencias", "Mantenimiento y Respaldo"] {
            assert!(h.query_by_label(t).is_some(), "falta la pestaña {t}");
        }
        h.get_by_label("Resolutor de Emergencias").click();
        h.run();
        assert!(h.query_by_label("Firmar y Generar Clave").is_some());
        h.get_by_label("Emitir Licencia Fija").click();
        h.run();
        assert!(h.query_by_label("Generar y Guardar license.key").is_some());
    }

    #[test]
    fn plugging_in_the_usb_syncs_automatically_shows_the_banner_and_the_manual_button_syncs_again() {
        use egui_kittest::kittest::Queryable;
        use std::cell::RefCell;
        use std::rc::Rc;
        let (mut app, _, dir) = app_unlocked("usbsync");
        // Oficina: sala A. Pendrive: sala B instalada en el teatro.
        let now = now_unix() as i64;
        app.db.upsert_equipo("Ballet Oficial de Córdoba", "Cabina Principal", "AAAA-AAAA-AAAA-AAAA", now).unwrap();
        let usb_dir = dir.join("usb");
        std::fs::create_dir_all(usb_dir.join("admin")).unwrap();
        let usb_db_path = usb_dir.join("admin").join(crate::db::DB_FILE);
        let _ = std::fs::remove_file(&usb_db_path);
        {
            let mut usb_db = Db::open(&usb_db_path).unwrap();
            usb_db
                .registrar_emision(&licencias_db::Registro {
                    institucion: "Ballet Oficial de Córdoba",
                    puesto: "Cabina Acer",
                    machine_id: "BBBB-BBBB-BBBB-BBBB",
                    notas_equipo: "",
                    tipo_emision: TIPO_PENDRIVE,
                    fecha_emision: now,
                    fecha_vencimiento: now + 30 * 86_400,
                    dias_otorgados: 30,
                    activation_epoch: 0,
                    token: "lic-B-1",
                    notas: "Instalación nueva",
                })
                .unwrap();
        }
        let plugged: Rc<RefCell<Option<UsbDb>>> = Rc::new(RefCell::new(None));
        let p2 = Rc::clone(&plugged);
        app.set_usb_detector(move || p2.borrow().clone());
        let mut h = egui_kittest::Harness::new_ui_state(|ui, a: &mut AdminApp| a.show(ui), app);
        h.run();
        assert!(h.query_by_label("Pendrive no detectado").is_some());
        assert!(h.query_by_label("Sincronizar ahora").is_none());
        assert_eq!(h.state().banner_text(), None);

        // Se conecta el pendrive: sincroniza solo, avisa con el texto pedido y las listas se actualizan.
        *plugged.borrow_mut() = Some(UsbDb { root: usb_dir.clone(), db: usb_db_path.clone() });
        h.state_mut().poll_usb(true);
        h.run();
        assert_eq!(h.state().banner_text(), Some("Sincronización completada: +1 equipos nuevos, +1 licencias importadas del pendrive · enviados al pendrive: +1 equipos, +0 licencias"));
        assert!(h.query_by_label_contains("Pendrive detectado").is_some() && h.query_by_label("Sincronizar ahora").is_some());
        let inv = h.state().db.inventario(now).unwrap();
        assert_eq!(inv.len(), 2, "la sala del pendrive ya está en el inventario");
        assert!(h.query_by_label_contains("Cabina Acer").is_some(), "y se ve en la tabla");
        // La sala de la oficina viajó al pendrive.
        let usb_db = Db::open(&usb_db_path).unwrap();
        assert!(usb_db.equipo_por_machine("AAAA-AAAA-AAAA-AAAA").unwrap().is_some());
        // No repite la sincronización automática en cada vistazo.
        h.state_mut().banner = None;
        h.state_mut().poll_usb(true);
        assert_eq!(h.state().banner_text(), None);

        // Llegan datos nuevos al pendrive y se pulsa «Sincronizar ahora».
        {
            let mut usb_db = Db::open(&usb_db_path).unwrap();
            usb_db
                .registrar_emision(&licencias_db::Registro { institucion: "Ballet Oficial de Córdoba", puesto: "Cabina Acer", machine_id: "BBBB-BBBB-BBBB-BBBB", notas_equipo: "", tipo_emision: TIPO_PENDRIVE, fecha_emision: now + 10, fecha_vencimiento: 0, dias_otorgados: 0, activation_epoch: 0, token: "lic-B-2", notas: "Renovación" })
                .unwrap();
        }
        h.run();
        h.get_by_label("Sincronizar ahora").click();
        h.run();
        assert!(h.state().banner_text().is_some_and(|t| t.starts_with("Sincronización completada: +0 equipos nuevos, +1 licencias importadas")), "{:?}", h.state().banner_text());
        let b = h.state().db.equipo_por_machine("BBBB-BBBB-BBBB-BBBB").unwrap().unwrap();
        assert_eq!(h.state().db.historial(b.id).unwrap().len(), 2);

        // Se retira el pendrive.
        *plugged.borrow_mut() = None;
        h.state_mut().poll_usb(true);
        h.run();
        assert!(h.query_by_label("Pendrive no detectado").is_some());
    }

    #[test]
    fn the_banner_messages_follow_the_requested_wording_and_warn_about_read_only_drives() {
        let s = SyncSummary { equipos_nuevos: 2, licencias_importadas: 5, remoto_escribible: true, ..Default::default() };
        assert_eq!(sync_message(&s), "Sincronización completada: +2 equipos nuevos, +5 licencias importadas del pendrive");
        let ro = SyncSummary { remoto_escribible: false, ..s };
        assert!(sync_message(&ro).ends_with("(el pendrive es de solo lectura: no se le enviaron cambios)"));
    }
}
