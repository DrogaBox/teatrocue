//! Archivo > Importar audio (convertir / copiar de CD): ventana con el convertidor de audio y la copia de pistas de un CD.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use eframe::egui::{self, RichText};

use crate::audio_tools::{self, Channels, ConvertOptions, Event, Format, Job, RipItem, Track};
use crate::theme;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Tab {
    #[default]
    Convert,
    Cd,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ItemState {
    Waiting,
    Running(f32),
    Done(PathBuf),
    Failed(String),
}

#[derive(Clone, Debug)]
pub struct ItemRow {
    pub label: String,
    pub state: ItemState,
}

/// Una lectora con lo que se leyó de su disco.
pub struct DriveRow {
    pub name: String,
    pub tracks: Result<Vec<Track>, String>,
    /// Por pista: ¿se copia? y con qué nombre.
    pub pick: Vec<(bool, String)>,
}

#[derive(Default)]
pub struct AudioToolsUi {
    pub open: bool,
    pub tab: Tab,
    pub files: Vec<PathBuf>,
    pub opts: ConvertOptions,
    pub dest: Option<PathBuf>,
    pub cd_dest: Option<PathBuf>,
    /// Agregar al show, como cues, lo que se termine de convertir o copiar.
    pub add_as_cues: bool,
    /// Carpeta base para los destinos por defecto (Música del usuario).
    pub base_dir: PathBuf,
    pub drives: Vec<DriveRow>,
    pub drive_index: usize,
    scanned: bool,
    /// Para qué pestaña se está eligiendo la carpeta.
    folder_for: Tab,
    job: Option<Job>,
    rows: Vec<ItemRow>,
    damaged: Option<Arc<Mutex<Vec<(usize, u32)>>>>,
    /// Archivos terminados en este trabajo, en orden.
    finished: Vec<PathBuf>,
    pub message: Option<(String, bool)>,
}

/// Lo que la ventana le pide a la aplicación.
#[derive(Default, Debug, PartialEq, Eq)]
pub struct Requests {
    pub pick_files: bool,
    pub pick_folder: bool,
}

impl AudioToolsUi {
    pub fn new(base_dir: PathBuf) -> Self {
        Self { base_dir, ..Default::default() }
    }

    pub fn add_files(&mut self, files: Vec<PathBuf>) {
        for f in files {
            if !self.files.contains(&f) {
                self.files.push(f);
            }
        }
        self.tab = Tab::Convert;
        self.open = true;
    }

    pub fn set_folder(&mut self, p: PathBuf) {
        match self.folder_for {
            Tab::Convert => self.dest = Some(p),
            Tab::Cd => self.cd_dest = Some(p),
        }
    }

    pub fn busy(&self) -> bool {
        self.job.is_some()
    }

    /// Carpeta de destino vigente (la elegida o la de por defecto).
    pub fn convert_dest(&self) -> PathBuf {
        self.dest.clone().unwrap_or_else(|| self.files.first().and_then(|f| f.parent()).map_or_else(|| self.base_dir.clone(), PathBuf::from).join("TeatroCue convertidos"))
    }

    pub fn cd_dest_dir(&self) -> PathBuf {
        self.cd_dest.clone().unwrap_or_else(|| self.base_dir.join("TeatroCue").join("CD"))
    }

    pub fn scan_drives(&mut self) {
        self.scanned = true;
        self.drives = audio_tools::list_drives()
            .into_iter()
            .map(|mut d| {
                let tracks = d.tracks();
                let pick = tracks.as_ref().map(|t| t.iter().map(|t| (t.audio, format!("Pista {:02}", t.number))).collect()).unwrap_or_default();
                DriveRow { name: d.name(), tracks, pick }
            })
            .collect();
        self.drive_index = self.drive_index.min(self.drives.len().saturating_sub(1));
    }

    fn start_convert(&mut self, ctx: &egui::Context) {
        let files = self.files.clone();
        self.rows = files.iter().map(|f| ItemRow { label: f.file_name().map_or_else(|| f.display().to_string(), |n| n.to_string_lossy().into_owned()), state: ItemState::Waiting }).collect();
        self.finished.clear();
        self.damaged = None;
        let c = ctx.clone();
        self.job = Some(audio_tools::convert(files, self.convert_dest(), self.opts.clone(), move || c.request_repaint()));
        self.message = None;
    }

    fn start_rip(&mut self, ctx: &egui::Context) {
        let Some(row) = self.drives.get(self.drive_index) else { return };
        let Ok(tracks) = &row.tracks else { return };
        let items: Vec<RipItem> = tracks.iter().zip(&row.pick).filter(|(t, (on, _))| *on && t.audio).map(|(t, (_, name))| RipItem { track: t.clone(), name: name.clone() }).collect();
        if items.is_empty() {
            self.message = Some(("Marque al menos una pista".into(), true));
            return;
        }
        // La lectora se vuelve a abrir para el hilo de copia (el trabajo se queda con ella).
        let name = row.name.clone();
        let Some(drive) = audio_tools::list_drives().into_iter().find(|d| d.name() == name) else {
            self.message = Some(("No se encontró la lectora; ¿se sacó el disco?".into(), true));
            return;
        };
        self.rows = items.iter().map(|i| ItemRow { label: format!("{} ({})", i.name, crate::format::fmt_time(i.track.secs())), state: ItemState::Waiting }).collect();
        self.finished.clear();
        let c = ctx.clone();
        let (job, damaged) = audio_tools::rip(drive, items, self.cd_dest_dir(), self.opts.clone(), move || c.request_repaint());
        self.job = Some(job);
        self.damaged = Some(damaged);
        self.message = None;
    }

    /// Recoge las novedades del trabajo en marcha. Devuelve los archivos a agregar como cues cuando termina.
    pub fn tick(&mut self) -> Vec<PathBuf> {
        let Some(job) = &self.job else { return Vec::new() };
        let mut finished_job = None;
        for e in job.poll() {
            match e {
                Event::Started(i) => {
                    if let Some(r) = self.rows.get_mut(i) {
                        r.state = ItemState::Running(0.0);
                    }
                }
                Event::Progress(i, f) => {
                    if let Some(r) = self.rows.get_mut(i) {
                        r.state = ItemState::Running(f);
                    }
                }
                Event::Done(i, p) => {
                    self.finished.push(p.clone());
                    if let Some(r) = self.rows.get_mut(i) {
                        r.state = ItemState::Done(p);
                    }
                }
                Event::Failed(i, why) => {
                    if let Some(r) = self.rows.get_mut(i) {
                        r.state = ItemState::Failed(why);
                    }
                }
                Event::Finished { cancelled } => finished_job = Some(cancelled),
            }
        }
        let Some(cancelled) = finished_job else { return Vec::new() };
        self.job = None;
        let ok = self.finished.len();
        let failed = self.rows.iter().filter(|r| matches!(r.state, ItemState::Failed(_))).count();
        let bad: u32 = self.damaged.as_ref().and_then(|d| d.lock().ok().map(|g| g.iter().map(|x| x.1).sum())).unwrap_or(0);
        let mut msg = if cancelled { format!("Cancelado: {ok} listo(s)") } else if failed == 0 { format!("Listo: {ok} archivo(s)") } else { format!("Terminó con {failed} error(es): {ok} listo(s)") };
        if bad > 0 {
            msg.push_str(&format!(". Ojo: la lectora no pudo leer {bad} sector(es) del disco (quedan en silencio, de ~13 ms cada uno): revise esa pista"));
        }
        self.message = Some((msg, failed > 0 || bad > 0));
        if self.add_as_cues && !self.finished.is_empty() {
            return std::mem::take(&mut self.finished);
        }
        Vec::new()
    }
}

fn format_options(ui: &mut egui::Ui, o: &mut ConvertOptions) {
    egui::ComboBox::from_id_salt("tools_format").selected_text(o.format.label()).width(260.0).show_ui(ui, |ui| {
        for f in Format::ALL {
            ui.selectable_value(&mut o.format, f, f.label());
        }
    });
    if o.format.lossy() {
        ui.horizontal(|ui| {
            ui.label("Calidad");
            for k in [128, 192, 256, 320] {
                ui.selectable_value(&mut o.kbps, k, format!("{k} kbps"));
            }
        });
    }
}

fn advanced_options(ui: &mut egui::Ui, o: &mut ConvertOptions) {
    ui.horizontal(|ui| {
        ui.label("Frecuencia");
        ui.selectable_value(&mut o.sample_rate, None, "Igual al original");
        ui.selectable_value(&mut o.sample_rate, Some(44_100), "44,1 kHz");
        ui.selectable_value(&mut o.sample_rate, Some(48_000), "48 kHz");
    });
    ui.horizontal(|ui| {
        ui.label("Canales");
        ui.selectable_value(&mut o.channels, Channels::Keep, "Igual al original");
        ui.selectable_value(&mut o.channels, Channels::Stereo, "Estéreo");
        ui.selectable_value(&mut o.channels, Channels::Mono, "Mono");
    });
    ui.checkbox(&mut o.normalize, "Igualar el volumen (EBU R128, -16 LUFS)").on_hover_text("Todas las pistas quedan con un volumen parecido. Tarda más y cambia el volumen original");
}

fn progress_rows(ui: &mut egui::Ui, rows: &[ItemRow]) {
    for r in rows {
        ui.horizontal(|ui| {
            match &r.state {
                ItemState::Waiting => ui.label(RichText::new("en espera").small().color(theme::TEXT_DIM)),
                ItemState::Running(f) => ui.label(RichText::new(format!("{:.0} %", f * 100.0)).small().color(theme::AMBER)),
                ItemState::Done(_) => ui.label(RichText::new("listo").small().color(theme::GREEN)),
                ItemState::Failed(_) => ui.label(RichText::new("error").small().color(theme::RED)),
            };
            ui.label(&r.label);
        });
        if let ItemState::Failed(why) = &r.state {
            ui.label(RichText::new(why).small().color(theme::RED));
        }
        if let ItemState::Running(f) = &r.state {
            ui.add(egui::ProgressBar::new(*f).desired_height(6.0));
        }
    }
}

/// Dibuja la ventana.
pub fn show(ctx: &egui::Context, st: &mut AudioToolsUi) -> Requests {
    let mut req = Requests::default();
    if !st.open {
        return req;
    }
    let mut open = st.open;
    egui::Window::new("Importar audio: convertir y copiar de CD").id(egui::Id::NULL.with("audio_tools")).open(&mut open).default_width(640.0).collapsible(false).show(ctx, |ui| {
        ui.horizontal(|ui| {
            for (tab, label) in [(Tab::Convert, "Convertir archivos"), (Tab::Cd, "Copiar de un CD")] {
                if ui.selectable_label(st.tab == tab, RichText::new(label).strong().color(if st.tab == tab { theme::AMBER } else { theme::TEXT })).clicked() {
                    st.tab = tab;
                }
            }
        });
        ui.separator();
        let busy = st.busy();
        if !audio_tools::ffmpeg_available() {
            ui.label(RichText::new("No se encontró ffmpeg: viene en el paquete de instalación (carpeta payload).").color(theme::RED));
        }
        match st.tab {
            Tab::Convert => {
                ui.add_enabled_ui(!busy, |ui| {
                    ui.horizontal(|ui| {
                        if ui.button("Agregar archivos…").clicked() {
                            req.pick_files = true;
                        }
                        if ui.add_enabled(!st.files.is_empty(), egui::Button::new("Vaciar lista")).clicked() {
                            st.files.clear();
                            st.rows.clear();
                        }
                    });
                    ui.label(RichText::new("Sirve para audio y también para sacar el audio de un video (mp4, mov, mkv). Los originales no se tocan.").small().color(theme::TEXT_DIM));
                    let mut remove = None;
                    egui::ScrollArea::vertical().id_salt("tools_files").max_height(150.0).auto_shrink([false, true]).show(ui, |ui| {
                        if st.files.is_empty() {
                            ui.label(RichText::new("Todavía no hay archivos.").color(theme::TEXT_DIM));
                        }
                        for (i, f) in st.files.iter().enumerate() {
                            ui.horizontal(|ui| {
                                if ui.small_button("x").on_hover_text("Sacar de la lista").clicked() {
                                    remove = Some(i);
                                }
                                ui.label(f.file_name().map_or_else(|| f.display().to_string(), |n| n.to_string_lossy().into_owned()));
                            });
                        }
                    });
                    if let Some(i) = remove {
                        st.files.remove(i);
                    }
                    ui.separator();
                    ui.label(RichText::new("Convertir a").strong().color(theme::AMBER));
                    format_options(ui, &mut st.opts);
                    advanced_options(ui, &mut st.opts);
                    ui.horizontal(|ui| {
                        ui.label("Guardar en");
                        ui.label(RichText::new(st.convert_dest().display().to_string()).monospace());
                        if ui.small_button("Cambiar…").clicked() {
                            st.folder_for = Tab::Convert;
                            req.pick_folder = true;
                        }
                    });
                    ui.checkbox(&mut st.add_as_cues, "Agregar al show como cues cuando termine");
                });
                ui.horizontal(|ui| {
                    if busy {
                        if ui.button("Cancelar").clicked() {
                            if let Some(j) = &st.job {
                                j.cancel();
                            }
                        }
                    } else if ui.add_enabled(!st.files.is_empty() && audio_tools::ffmpeg_available(), egui::Button::new(RichText::new("Convertir").strong())).clicked() {
                        st.start_convert(ctx);
                    }
                });
            }
            Tab::Cd => {
                if !audio_tools::CD_SUPPORTED {
                    ui.label(RichText::new("Copiar de un CD solo está disponible en Windows.").color(theme::TEXT_DIM));
                } else {
                    if !st.scanned {
                        st.scan_drives();
                    }
                    ui.add_enabled_ui(!busy, |ui| {
                        ui.horizontal(|ui| {
                            if ui.button("Leer el disco").on_hover_text("Vuelve a leer la lectora (después de cambiar de disco)").clicked() {
                                st.scan_drives();
                            }
                            if st.drives.len() > 1 {
                                egui::ComboBox::from_id_salt("tools_drive").selected_text(st.drives.get(st.drive_index).map_or("—".to_string(), |d| d.name.clone())).show_ui(ui, |ui| {
                                    for (i, d) in st.drives.iter().enumerate() {
                                        ui.selectable_value(&mut st.drive_index, i, &d.name);
                                    }
                                });
                            }
                        });
                        match st.drives.get_mut(st.drive_index) {
                            None => {
                                ui.label(RichText::new("No se encontró ninguna lectora de CD.").color(theme::TEXT_DIM));
                            }
                            Some(d) => {
                                ui.label(RichText::new(&d.name).strong());
                                match &d.tracks {
                                    Err(e) => {
                                        ui.label(RichText::new(e).color(theme::TEXT_DIM));
                                    }
                                    Ok(tracks) => {
                                        let tracks = tracks.clone();
                                        ui.horizontal(|ui| {
                                            if ui.small_button("Marcar todas").clicked() {
                                                for (t, p) in tracks.iter().zip(d.pick.iter_mut()) {
                                                    p.0 = t.audio;
                                                }
                                            }
                                            if ui.small_button("Ninguna").clicked() {
                                                for p in d.pick.iter_mut() {
                                                    p.0 = false;
                                                }
                                            }
                                        });
                                        egui::ScrollArea::vertical().id_salt("tools_tracks").max_height(220.0).auto_shrink([false, true]).show(ui, |ui| {
                                            for (t, p) in tracks.iter().zip(d.pick.iter_mut()) {
                                                ui.horizontal(|ui| {
                                                    ui.add_enabled(t.audio, egui::Checkbox::without_text(&mut p.0));
                                                    ui.label(RichText::new(format!("{:02}", t.number)).monospace().color(theme::TEXT_DIM));
                                                    ui.add_enabled(t.audio, egui::TextEdit::singleline(&mut p.1).desired_width(260.0));
                                                    ui.label(RichText::new(if t.audio { crate::format::fmt_time(t.secs()) } else { "datos (no se copia)".into() }).small().color(theme::TEXT_DIM));
                                                });
                                            }
                                        });
                                    }
                                }
                            }
                        }
                        ui.separator();
                        ui.label(RichText::new("Guardar como").strong().color(theme::AMBER));
                        format_options(ui, &mut st.opts);
                        advanced_options(ui, &mut st.opts);
                        ui.horizontal(|ui| {
                            ui.label("Guardar en");
                            ui.label(RichText::new(st.cd_dest_dir().display().to_string()).monospace());
                            if ui.small_button("Cambiar…").clicked() {
                                st.folder_for = Tab::Cd;
                                req.pick_folder = true;
                            }
                        });
                        ui.checkbox(&mut st.add_as_cues, "Agregar al show como cues cuando termine");
                        ui.label(RichText::new("Copia directa de las pistas, sin internet: los nombres son «Pista 01…», edítelos a gusto. No se corrigen discos rayados: si un sector no se puede leer, queda en silencio y se avisa.").small().color(theme::TEXT_DIM));
                    });
                    ui.horizontal(|ui| {
                        if busy {
                            if ui.button("Cancelar").clicked() {
                                if let Some(j) = &st.job {
                                    j.cancel();
                                }
                            }
                        } else {
                            let ready = st.drives.get(st.drive_index).is_some_and(|d| d.tracks.is_ok() && d.pick.iter().any(|p| p.0));
                            if ui.add_enabled(ready, egui::Button::new(RichText::new("Copiar pistas").strong())).clicked() {
                                st.start_rip(ctx);
                            }
                        }
                    });
                }
            }
        }
        if !st.rows.is_empty() {
            ui.separator();
            egui::ScrollArea::vertical().id_salt("tools_progress").max_height(160.0).auto_shrink([false, true]).show(ui, |ui| progress_rows(ui, &st.rows));
        }
        if let Some((m, bad)) = &st.message {
            ui.label(RichText::new(m).color(if *bad { theme::AMBER } else { theme::GREEN }));
        }
    });
    st.open = open;
    req
}
