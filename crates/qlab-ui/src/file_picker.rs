//! Selector de archivos propio, dentro de la ventana. Evita los diálogos nativos (que dependen del
//! portal del escritorio y pueden bloquear el hilo de la UI) y lee los directorios en un hilo aparte.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver};

use eframe::egui::{self, Color32, RichText};

use crate::theme;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Filter {
    Audio,
    Show,
    Any,
}

impl Filter {
    pub fn matches(self, path: &Path) -> bool {
        let ext = path.extension().and_then(|e| e.to_str()).map(str::to_lowercase);
        match (self, ext.as_deref()) {
            (Filter::Any, _) => true,
            (Filter::Audio, Some("wav" | "mp3" | "flac" | "aif" | "aiff" | "ogg" | "oga" | "m4a" | "mp4" | "mov" | "mkv")) => true,
            (Filter::Show, Some("teatroshow" | "json" | "qlabshow" | "qlabrs")) => true,
            _ => false,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Filter::Audio => "Audio y vídeo (wav, flac, mp3, ogg, mp4, mov, mkv…)",
            Filter::Show => "Shows de TeatroCue (.teatroshow, .json)",
            Filter::Any => "Todos",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}

/// Contenido de un directorio: carpetas primero, luego archivos que cumplen el filtro; por nombre.
pub fn list_dir(dir: &Path, filter: Filter, show_hidden: bool) -> Result<Vec<Entry>, String> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))? {
        let Ok(e) = e else { continue };
        let name = e.file_name().to_string_lossy().into_owned();
        if !show_hidden && name.starts_with('.') {
            continue;
        }
        // `metadata` sigue enlaces simbólicos; uno roto simplemente se omite.
        let Ok(meta) = std::fs::metadata(e.path()) else { continue };
        if meta.is_dir() || filter.matches(&e.path()) {
            out.push(Entry { name, is_dir: meta.is_dir(), size: meta.len() });
        }
    }
    out.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    Ok(out)
}

pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let (mut v, mut u) = (bytes as f64, 0);
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 { format!("{bytes} B") } else { format!("{v:.1} {}", UNITS[u]) }
}

#[derive(Debug, PartialEq, Eq)]
pub enum PickerOutcome {
    Picked(PathBuf),
    Cancelled,
}

pub struct FilePicker {
    title: String,
    dir: PathBuf,
    filter: Filter,
    show_hidden: bool,
    listing: Option<Result<Vec<Entry>, String>>,
    loading: Option<Receiver<Result<Vec<Entry>, String>>>,
    selected: Option<String>,
    path_text: String,
}

impl FilePicker {
    pub fn new(title: impl Into<String>, start: PathBuf, filter: Filter) -> Self {
        let dir = if start.is_dir() { start } else { start.parent().map(Path::to_path_buf).unwrap_or_else(home_dir) };
        let mut p = Self { title: title.into(), path_text: dir.display().to_string(), dir, filter, show_hidden: false, listing: None, loading: None, selected: None };
        p.reload(&egui::Context::default());
        p
    }

    fn reload(&mut self, ctx: &egui::Context) {
        let (tx, rx) = channel();
        let (dir, filter, hidden) = (self.dir.clone(), self.filter, self.show_hidden);
        let ctx = ctx.clone();
        let _ = std::thread::Builder::new().name("qlab-ui-listdir".into()).spawn(move || {
            let _ = tx.send(list_dir(&dir, filter, hidden));
            ctx.request_repaint();
        });
        self.loading = Some(rx);
        self.listing = None;
        self.selected = None;
        self.path_text = self.dir.display().to_string();
    }

    fn go(&mut self, dir: PathBuf, ctx: &egui::Context) {
        self.dir = dir;
        self.reload(ctx);
    }

    pub fn current_dir(&self) -> &Path {
        &self.dir
    }

    /// Dibuja la ventana. Devuelve el resultado cuando el usuario elige o cancela.
    pub fn show(&mut self, ctx: &egui::Context) -> Option<PickerOutcome> {
        if let Some(rx) = &self.loading {
            if let Ok(r) = rx.try_recv() {
                self.listing = Some(r);
                self.loading = None;
            }
        }
        let mut outcome = None;
        let mut navigate: Option<PathBuf> = None;
        let mut open = true;
        egui::Window::new(self.title.clone())
            .collapsible(false)
            .resizable(true)
            .default_size([560.0, 420.0])
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .open(&mut open)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if ui.button("Subir").clicked() {
                        if let Some(parent) = self.dir.parent() {
                            navigate = Some(parent.to_path_buf());
                        }
                    }
                    if ui.button("Inicio").clicked() {
                        navigate = Some(home_dir());
                    }
                    if ui.button("/").clicked() {
                        navigate = Some(PathBuf::from(std::path::MAIN_SEPARATOR_STR));
                    }
                    let r = ui.add(egui::TextEdit::singleline(&mut self.path_text).desired_width(f32::INFINITY));
                    if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        let p = PathBuf::from(self.path_text.trim());
                        if p.is_dir() {
                            navigate = Some(p);
                        } else if p.is_file() {
                            outcome = Some(PickerOutcome::Picked(p));
                        }
                    }
                });
                ui.horizontal(|ui| {
                    ui.label(RichText::new(self.filter.label()).color(theme::TEXT_DIM));
                    if ui.checkbox(&mut self.show_hidden, "ocultos").changed() {
                        navigate = Some(self.dir.clone());
                    }
                });
                ui.separator();
                egui::ScrollArea::vertical().auto_shrink([false, false]).max_height(ui.available_height() - 40.0).show(ui, |ui| match &self.listing {
                    None => {
                        ui.label(RichText::new("leyendo…").color(theme::TEXT_DIM));
                    }
                    Some(Err(e)) => {
                        ui.label(RichText::new(e).color(theme::RED));
                    }
                    Some(Ok(entries)) if entries.is_empty() => {
                        ui.label(RichText::new("(vacío)").color(theme::TEXT_DIM));
                    }
                    Some(Ok(entries)) => {
                        for e in entries {
                            let selected = self.selected.as_deref() == Some(e.name.as_str());
                            let label = if e.is_dir { format!("[{}]", e.name) } else { format!("{}   {}", e.name, human_size(e.size)) };
                            let color: Color32 = if e.is_dir { theme::AMBER } else { theme::TEXT };
                            let r = ui.selectable_label(selected, RichText::new(label).color(color));
                            if r.clicked() {
                                self.selected = Some(e.name.clone());
                            }
                            if r.double_clicked() {
                                let p = self.dir.join(&e.name);
                                if e.is_dir {
                                    navigate = Some(p);
                                } else {
                                    outcome = Some(PickerOutcome::Picked(p));
                                }
                            }
                        }
                    }
                });
                ui.separator();
                ui.horizontal(|ui| {
                    let chosen = self.selected.as_ref().map(|n| self.dir.join(n));
                    let can_pick = chosen.as_ref().is_some_and(|p| p.is_file());
                    if ui.add_enabled(can_pick, egui::Button::new("Elegir")).clicked() {
                        outcome = chosen.clone().map(PickerOutcome::Picked);
                    }
                    if ui.button("Cancelar").clicked() {
                        outcome = Some(PickerOutcome::Cancelled);
                    }
                });
            });
        if !open {
            outcome = Some(PickerOutcome::Cancelled);
        }
        if let Some(dir) = navigate {
            self.go(dir, ctx);
        }
        outcome
    }
}

pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("qlab-ui-picker-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("Zeta")).unwrap();
        std::fs::create_dir_all(root.join("alfa")).unwrap();
        std::fs::create_dir_all(root.join(".oculta")).unwrap();
        for f in ["b.wav", "A.MP3", "nota.txt", "show.json", ".secreto.wav"] {
            std::fs::write(root.join(f), vec![0u8; 2048]).unwrap();
        }
        root
    }

    #[test]
    fn filters_by_extension_ignoring_case() {
        assert!(Filter::Audio.matches(Path::new("x/A.MP3")) && Filter::Audio.matches(Path::new("b.flac")));
        assert!(!Filter::Audio.matches(Path::new("nota.txt")) && !Filter::Audio.matches(Path::new("sin_extension")));
        assert!(Filter::Show.matches(Path::new("s.JSON")) && !Filter::Show.matches(Path::new("s.wav")));
        assert!(Filter::Any.matches(Path::new("lo_que_sea")));
        assert!(Filter::Audio.label().contains("Audio"));
    }

    #[test]
    fn listing_puts_folders_first_and_hides_dotfiles() {
        let root = tree("list");
        let names = |hidden| list_dir(&root, Filter::Audio, hidden).unwrap().into_iter().map(|e| (e.name, e.is_dir)).collect::<Vec<_>>();
        assert_eq!(names(false), vec![("alfa".into(), true), ("Zeta".into(), true), ("A.MP3".into(), false), ("b.wav".into(), false)]);
        let all = names(true);
        assert!(all.contains(&(".oculta".to_string(), true)) && all.contains(&(".secreto.wav".to_string(), false)));
        let shows = list_dir(&root, Filter::Show, false).unwrap();
        assert_eq!(shows.iter().filter(|e| !e.is_dir).map(|e| e.name.as_str()).collect::<Vec<_>>(), vec!["show.json"]);
        assert_eq!(list_dir(&root, Filter::Any, false).unwrap().iter().find(|e| e.name == "b.wav").unwrap().size, 2048);
        assert!(list_dir(&root.join("no_existe"), Filter::Any, false).is_err());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn sizes_read_well() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1023), "1023 B");
        assert_eq!(human_size(1536), "1.5 KB");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0 MB");
        assert_eq!(human_size(3 * 1024 * 1024 * 1024), "3.0 GB");
    }

    #[test]
    fn the_window_lists_in_the_background_and_picks_a_file() {
        let root = tree("window");
        let ctx = egui::Context::default();
        let mut picker = FilePicker::new("Elegir audio", root.clone(), Filter::Audio);
        assert_eq!(picker.current_dir(), root);
        // La lectura es asíncrona: se pinta hasta que llegue el listado.
        let t0 = std::time::Instant::now();
        loop {
            let out = ctx.run_ui(Default::default(), |ui| {
                assert!(picker.show(ui.ctx()).is_none());
            });
            out.drop_without_applying_deltas();
            if picker.listing.is_some() {
                break;
            }
            assert!(t0.elapsed() < std::time::Duration::from_secs(5), "el listado no llegó");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(picker.listing.as_ref().unwrap().as_ref().unwrap().len(), 4);
        // Elegir, navegar y cancelar se modelan sobre el estado interno.
        picker.go(root.join("alfa"), &ctx);
        assert_eq!(picker.current_dir(), root.join("alfa"));
        std::fs::remove_dir_all(root).ok();
    }
}
