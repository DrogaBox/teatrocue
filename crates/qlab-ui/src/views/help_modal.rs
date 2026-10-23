//! Manual de uso (F1) y «Acerca de TeatroCue».
//!
//! Tres pestañas: atajos de teclado, operación de escena y autorización legal con el estado de la licencia.

use eframe::egui::{self, Color32, RichText, Ui};

use crate::license_gate::{LicenseInfo, LicenseState};
use crate::prefs::Keymap;
use crate::theme;
use crate::views::reactivation::ReactivationState;

/// Texto de autorización de uso exclusivo (se muestra tal cual en la pestaña legal).
pub const AUTHORIZATION_TITLE: &str = "AUTORIZACIÓN DE USO EXCLUSIVO:";
pub const AUTHORIZATION_TEXT: &str = "Este software ('TeatroCue') ha sido desarrollado y licenciado exclusivamente para las funciones, ensayos y actividades artísticas del BALLET OFICIAL DE CÓRDOBA (Teatro del Libertador San Martín).\nQueda estrictamente prohibida su copia, distribución, transferencia a terceros, modificación o uso en cualquier equipamiento ajeno a la institución sin autorización expresa del desarrollador.";
pub const DEFAULT_LICENSEE: &str = "Ballet Oficial de Córdoba";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum HelpTab {
    #[default]
    Shortcuts,
    Scene,
    Legal,
}

impl HelpTab {
    pub const ALL: [HelpTab; 3] = [HelpTab::Shortcuts, HelpTab::Scene, HelpTab::Legal];

    pub fn title(self) -> &'static str {
        match self {
            Self::Shortcuts => "Atajos de Teclado",
            Self::Scene => "Operación de Escena",
            Self::Legal => "Autorización Legal y Licencia",
        }
    }
}

#[derive(Default)]
pub struct HelpState {
    pub open: bool,
    pub tab: HelpTab,
    pub about_open: bool,
    /// «Novedades de esta versión» (sale sola al abrir una versión nueva y desde Ayuda).
    pub news_open: bool,
}

impl HelpState {
    pub fn toggle_manual(&mut self) {
        self.open = !self.open;
        self.about_open = false;
    }

    pub fn open_about(&mut self) {
        self.about_open = true;
        self.open = false;
    }

    /// ¿Hay un diálogo de ayuda abierto? (los atajos de la aplicación se suspenden)
    pub fn any_open(&self) -> bool {
        self.open || self.about_open || self.news_open
    }

    pub fn open_news(&mut self) {
        self.news_open = true;
        self.open = false;
        self.about_open = false;
    }
}

/// Atajos: tecla y qué hace.
pub const SHORTCUTS: [(&str, &str); 19] = [
    ("Espacio", "GO: dispara la cue en espera y avanza el playhead"),
    ("Esc", "Panic suave (fade corto de todo lo que suena)"),
    ("Esc, Esc", "Doble pulsación en menos de 0,7 s: parada dura inmediata"),
    ("F", "En la ventana de proyección: pantalla completa (Esc sale de ella)"),
    ("Supr / Retroceso", "Borra la cue seleccionada"),
    ("P", "Pausa / reanuda lo que suena: el cabezal queda quieto en ese frame (con M se puede marcar ahí) y al volver sigue desde el mismo punto"),
    ("M", "Marca una memoria donde suena la cue o donde está el cursor, y pide su nombre"),
    ("Ctrl+D", "Duplica la cue seleccionada (no los grupos)"),
    ("Ctrl+Z", "Deshace la última edición de una cue (los cambios seguidos cuentan como uno)"),
    ("Ctrl+Mayús+Z / Ctrl+Y", "Rehace lo deshecho"),
    ("Flecha arriba / abajo", "Cambia la selección (y el playhead en las cues de nivel superior)"),
    ("Doble clic en una cue", "La dispara en ese momento"),
    ("Ctrl+N", "Show nuevo (pide confirmar si hay cambios sin guardar)"),
    ("Ctrl+O", "Abrir show…"),
    ("Ctrl+S", "Guardar"),
    ("Ctrl+Mayús+S", "Guardar como…"),
    ("Ctrl+Q", "Salir"),
    ("F1", "Este manual"),
    ("Arrastrar archivos", "Soltar audio o vídeo sobre la ventana crea cues; sobre el Inspector cambia el archivo de la cue"),
];

fn heading(ui: &mut Ui, text: &str) {
    ui.add_space(6.0);
    ui.label(RichText::new(text).strong().size(15.0).color(theme::AMBER));
}

fn para(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).color(theme::TEXT));
    ui.add_space(2.0);
}

/// La tecla de cada fila del manual con las teclas que eligió el operador (Opciones > Preferencias).
pub fn shortcut_key(row_key: &str, keys: &Keymap) -> String {
    use crate::prefs::Command as C;
    let cmd = match row_key {
        "Espacio" => C::Go,
        "Esc" => C::Panic,
        "Esc, Esc" => return format!("{0}, {0}", keys.primary(C::Panic)),
        "Supr / Retroceso" => C::Delete,
        "P" => C::Pause,
        "M" => C::Memory,
        "Ctrl+D" => C::Duplicate,
        "Ctrl+Z" => C::Undo,
        "Ctrl+Mayús+Z / Ctrl+Y" => C::Redo,
        "Flecha arriba / abajo" => return format!("{} / {}", keys.primary(C::Up), keys.primary(C::Down)),
        "Ctrl+N" => C::New,
        "Ctrl+O" => C::Open,
        "Ctrl+S" => C::Save,
        "Ctrl+Mayús+S" => C::SaveAs,
        "Ctrl+Q" => C::Quit,
        "F1" => C::Help,
        other => return other.to_string(),
    };
    keys.all_display(cmd)
}

fn shortcuts_tab(ui: &mut Ui, keys: &Keymap) {
    ui.label(RichText::new("Las teclas se pueden cambiar en Opciones > Preferencias y atajos de teclado.").small().color(theme::TEXT_DIM));
    egui::Grid::new("help_shortcuts").num_columns(2).spacing([22.0, 8.0]).striped(true).show(ui, |ui| {
        for (key, what) in SHORTCUTS {
            ui.label(RichText::new(shortcut_key(key, keys)).monospace().strong().color(theme::GREEN));
            ui.label(what);
            ui.end_row();
        }
    });
}

fn scene_tab(ui: &mut Ui) {
    heading(ui, "Slices con etiquetas coreográficas");
    para(ui, "En Time & Loops, «Add Slice» pone un corte en el cursor (el de reproducción si suena, o el que dejaste con un clic en la onda). Cada corte abre un tramo hasta el siguiente.");
    para(ui, "Dale nombre a cada tramo («Entrada coro», «Danza en puntas»): el nombre flota sobre el corte en la onda. Un clic sobre el marcador alterna entre 1x, 2x e Infinite; Infinite repite el tramo hasta que lo liberes.");
    heading(ui, "Salida limpia con Devamp");
    para(ui, "Una cue atrapada en un tramo Infinite se marca VAMP (ámbar, parpadea) en la tabla. «Devamp» —el botón de Time & Loops, o GO con el playhead sobre esa cue— la deja terminar la pasada en curso y seguir con el tramo siguiente sin saltos ni clics. Mientras acaba se marca DEVAMP.");
    heading(ui, "Audio Levels: matriz, Gangs y Mute/Solo");
    para(ui, "La matriz cruza los canales del archivo (filas) con las salidas (columnas), de -120 a +12 dB. Arrastrar un fader suena en vivo. Los Gangs mueven varias celdas a la vez; Mute y Solo se aplican por salida y por entrada.");
    heading(ui, "Cues de control");
    para(ui, "«+ Control Cue» crea una cue que actúa sobre otra al dispararse: Stop (con fundido de salida), Pause, Resume, Devamp, Go to (mueve el playhead a otra cue: útil para repetir una sección), Load (precarga el audio), Arm o Disarm (arma o desarma otra cue). Elige la cue destino en su pestaña Control. Si el destino no está sonando, no pasa nada.");
    heading(ui, "Memo / Espera");
    para(ui, "«+ Memo/Espera» crea una cue que no suena: dura el tiempo que indiques. Con «Continuar: Auto-continue» sirve para esperar entre dos cues o para dejar notas en la lista.");
    heading(ui, "Volumen maestro");
    para(ui, "El deslizador MASTER de la barra superior baja o sube toda la salida a la vez, de -60 dB (silencio) a +6 dB; doble clic vuelve a 0 dB. No cambia los niveles de las cues. Al abrir TeatroCue siempre empieza en 0 dB, y también se puede mover por OSC con /master <dB>.");
    heading(ui, "Ecualizador (EQ) por cue");
    para(ui, "Pestaña EQ de una cue de audio o vídeo: 4 bandas (Low Cut, Low Shelf, Peaking, High Shelf, High Cut) de 20 Hz a 20 kHz, de -24 a +12 dB, con Q de 0,1 a 10. Active «Ecualizador activo» y arrastre los puntos de la curva (horizontal = frecuencia, vertical = ganancia); la rueda sobre un punto cambia la Q y el doble clic activa o desactiva esa banda. Debajo, la tabla permite ajustar frecuencia, ganancia, Q y tipo con el teclado. Los cambios suenan en vivo, sin chasquidos, y con el EQ apagado no cuesta nada de CPU.");
    heading(ui, "Memorias (puntos con nombre y nota)");
    para(ui, "Para marcar un punto: pulse M (o Enter, también el del teclado numérico, cuando ningún campo de texto tiene el foco) mientras suena la cue (captura ese instante), o con el cursor de edición sobre la onda (clic en la onda). Aparece un cuadro «Nombre de la memoria:» ya enfocado: escriba el nombre y pulse Enter (vacío = «Memoria N»); Esc cancela. La memoria aparece enseguida como un rombo ámbar con su nombre sobre la regla de la onda (un clic en el rombo salta a ese punto) y en el panel de memorias.");
    para(ui, "Ver > Memorias… abre un panel lateral (izquierda o derecha, con «A la derecha» / «A la izquierda») con la lista numerada: #, tiempo MM:SS.mmm, nombre, cue y nota. El campo «Buscar memoria…» filtra por nombre, nota o cue. Un clic en una fila lleva a esa cue y la deja lista: el playhead queda en ella y Espacio la dispara desde esa memoria; si la cue ya suena, salta a ese punto y sigue sonando. «Arrancar aquí» (en la lista de la cue) hace lo mismo y «Desde el inicio» lo quita; la región y los slices no se tocan.");
    heading(ui, "Cue OSC (enviar a otro equipo)");
    para(ui, "«+ OSC Cue» envía un mensaje OSC por UDP al dispararse: indique el destino (equipo:puerto, p. ej. la consola de luces) y el mensaje (/direccion 3 0.5 \"texto\"). Si el destino no responde o es inválido, avisa pero no detiene el show.");
    heading(ui, "Control remoto OSC");
    para(ui, "En Ver > Configuración de audio se puede activar un puerto UDP (53000 por defecto) para controlar TeatroCue desde otro equipo con OSC (TouchOSC, Companion, un Stream Deck…): /go, /panic, /stop, /pause, /resume, /next, /prev y /cue/<número>/start, stop, pause, resume, devamp, load, arm o disarm. Está apagado salvo que lo active, porque cualquiera en la red que conozca el puerto puede disparar la función.");
    heading(ui, "Configuración de audio");
    para(ui, "Ver > Configuración de audio…: elige la interfaz de sonido, los canales de salida, el buffer y la frecuencia. Se guarda en esta PC y se usa en cada arranque. Al aplicar, el motor se reinicia (se corta lo que suene) y el show abierto se recarga solo; con un buffer menor baja la latencia pero hay más riesgo de cortes.");
    heading(ui, "Vídeo esclavo del audio");
    para(ui, "Una cue de vídeo suena por su pista de audio y la imagen sigue al audio, no al revés: si la pantalla se atrasa, se descartan cuadros para no desfasar la música. «Proyección» abre la ventana para el proyector: arrástrala al monitor correcto y pulsa F.");
}

fn legal_tab(ui: &mut Ui, license: &LicenseInfo, can_reactivate: bool, react: &mut ReactivationState) -> Option<String> {
    egui::Frame::new().fill(Color32::from_rgb(0x2a, 0x1f, 0x08)).stroke(egui::Stroke::new(1.5, theme::AMBER)).corner_radius(4).inner_margin(egui::Margin::same(12)).show(ui, |ui| {
        ui.label(RichText::new(AUTHORIZATION_TITLE).strong().size(17.0).color(theme::AMBER));
        ui.add_space(4.0);
        ui.label(RichText::new(AUTHORIZATION_TEXT).strong().size(14.5).color(theme::TEXT));
    });
    ui.add_space(12.0);
    heading(ui, "Licencia de esta PC");
    let (licensee, crypto, color) = license_lines(license);
    egui::Grid::new("help_license").num_columns(2).spacing([16.0, 6.0]).show(ui, |ui| {
        ui.label(RichText::new("Licenciado a").color(theme::TEXT_DIM));
        ui.label(RichText::new(licensee).strong());
        ui.end_row();
        ui.label(RichText::new("Machine ID").color(theme::TEXT_DIM));
        ui.label(RichText::new(license.machine_id.as_deref().unwrap_or("(no disponible)")).monospace().strong().color(theme::GREEN));
        ui.end_row();
        ui.label(RichText::new("Estado criptográfico").color(theme::TEXT_DIM));
        ui.label(RichText::new(crypto).color(color));
        ui.end_row();
    });
    // Una licencia sin vencimiento no necesita reactivarse.
    let perpetual = matches!(license.state, LicenseState::Valid { days_left: None, .. });
    if can_reactivate && !perpetual {
        ui.add_space(14.0);
        return crate::views::reactivation::show(ui, react, "help");
    }
    None
}

/// Líneas que describen la licencia: «Licenciado a: …», estado criptográfico y su color.
pub fn license_lines(license: &LicenseInfo) -> (String, String, Color32) {
    match &license.state {
        LicenseState::Valid { licensee, issued_unix, expires_unix, days_left } => {
            let term = match days_left {
                None => "sin vencimiento".to_string(),
                Some(d) => format!("vence el {} (quedan {d} días)", fmt_date_utc(*expires_unix)),
            };
            let color = if days_left.is_some_and(|d| d <= crate::license_gate::EXPIRY_WARNING_DAYS) { theme::AMBER } else { theme::GREEN };
            (licensee.clone(), format!("Firma Ed25519 válida para esta PC · emitida el {} · {term}", fmt_date_utc(*issued_unix)), color)
        }
        LicenseState::Invalid { reason } => ("— sin licencia válida —".to_string(), format!("Rechazada: {reason}"), theme::RED),
        LicenseState::NotEnforced => (DEFAULT_LICENSEE.to_string(), "Compilación de desarrollo: la licencia no se exige".to_string(), theme::AMBER),
    }
}

/// `AAAA-MM-DD` (UTC) de unos segundos Unix.
pub fn fmt_date_utc(unix: u64) -> String {
    // Algoritmo de días civiles (Howard Hinnant).
    let z = (unix / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    format!("{:04}-{:02}-{:02}", y + (m <= 2) as i64, m, d)
}

/// Dibuja el manual y el «Acerca de» si están abiertos.
/// Devuelve la clave de activación pegada cuando el operador pulsa «Activar».
pub fn show(ctx: &egui::Context, st: &mut HelpState, license: &LicenseInfo, can_reactivate: bool, react: &mut ReactivationState, keys: &Keymap) -> Option<String> {
    let mut key = None;
    if st.open {
        let modal = egui::Modal::new(egui::Id::NULL.with("help_modal")).show(ctx, |ui| {
            ui.set_width(720.0);
            ui.set_max_height((ctx.content_rect().height() - 140.0).max(240.0));
            ui.horizontal(|ui| {
                ui.heading("Manual de uso de TeatroCue");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Cerrar (Esc)").clicked() {
                        st.open = false;
                    }
                });
            });
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                for tab in HelpTab::ALL {
                    if ui.selectable_label(st.tab == tab, RichText::new(tab.title()).strong().color(if st.tab == tab { theme::AMBER } else { theme::TEXT })).clicked() {
                        st.tab = tab;
                    }
                }
            });
            ui.separator();
            egui::ScrollArea::vertical().id_salt("help_scroll").auto_shrink([false, false]).show(ui, |ui| match st.tab {
                HelpTab::Shortcuts => shortcuts_tab(ui, keys),
                HelpTab::Scene => scene_tab(ui),
                HelpTab::Legal => key = legal_tab(ui, license, can_reactivate, react),
            });
        });
        // F1 lo gestiona la aplicación (alterna); aquí se cierra con Esc, un clic fuera o el botón.
        if modal.should_close() {
            st.open = false;
        }
    }
    if st.about_open {
        let mut go_legal = false;
        let modal = egui::Modal::new(egui::Id::NULL.with("about_modal")).show(ctx, |ui| {
            ui.set_width(430.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("TeatroCue").size(34.0).strong().color(theme::AMBER));
                ui.label(RichText::new(format!("Versión {}", env!("CARGO_PKG_VERSION"))).small().color(theme::TEXT_DIM));
                ui.add_space(8.0);
                ui.label("Reproductor de audio y vídeo para escena.");
                ui.add_space(8.0);
                let (licensee, crypto, color) = license_lines(license);
                ui.label(RichText::new(format!("Licenciado a: {licensee}")).strong());
                ui.label(RichText::new(format!("Machine ID: {}", license.machine_id.as_deref().unwrap_or("(no disponible)"))).monospace());
                ui.label(RichText::new(crypto).small().color(color));
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui.button("Autorización legal").clicked() {
                        go_legal = true;
                    }
                    if ui.button("Cerrar").clicked() {
                        st.about_open = false;
                    }
                });
            });
        });
        if modal.should_close() {
            st.about_open = false;
        }
        if go_legal {
            st.about_open = false;
            st.open = true;
            st.tab = HelpTab::Legal;
        }
    }
    if st.news_open {
        let modal = egui::Modal::new(egui::Id::NULL.with("news_modal")).show(ctx, |ui| {
            ui.set_width(640.0_f32.min(ctx.content_rect().width() - 60.0));
            ui.set_max_height((ctx.content_rect().height() - 140.0).max(240.0));
            ui.horizontal(|ui| {
                ui.heading("Novedades de esta versión");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button(RichText::new("Entendido").strong()).clicked() {
                        st.news_open = false;
                    }
                });
            });
            ui.label(RichText::new("Sale una sola vez por versión. Para volver a verlas: Ayuda > Novedades de esta versión.").small().color(theme::TEXT_DIM));
            ui.separator();
            let text = crate::novedades::text();
            egui::ScrollArea::vertical().id_salt("news_scroll").auto_shrink([false, true]).show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 1.0;
                for line in crate::novedades::lines(&text) {
                    match line {
                        crate::novedades::Line::Heading(h) => heading(ui, h),
                        crate::novedades::Line::Item(t) => {
                            ui.add_space(2.0);
                            ui.label(RichText::new(t.trim()).strong().color(theme::TEXT));
                        }
                        crate::novedades::Line::Text(t) => {
                            // Se conserva la sangría de los ejemplos; el resto del párrafo va alineado con el punto.
                            let indent = t.len() - t.trim_start().len();
                            ui.horizontal(|ui| {
                                ui.add_space(if indent > 3 { 28.0 } else { 14.0 });
                                ui.label(RichText::new(t.trim()).color(if indent > 3 { theme::AMBER } else { theme::TEXT }));
                            });
                        }
                        crate::novedades::Line::Blank => ui.add_space(5.0),
                    }
                }
            });
        });
        if modal.should_close() {
            st.news_open = false;
        }
    }
    key
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_legal_text_is_exact_and_names_the_institution_and_the_theatre() {
        assert_eq!(AUTHORIZATION_TITLE, "AUTORIZACIÓN DE USO EXCLUSIVO:");
        for needle in ["('TeatroCue')", "BALLET OFICIAL DE CÓRDOBA (Teatro del Libertador San Martín)", "Queda estrictamente prohibida su copia, distribución, transferencia a terceros, modificación o uso en cualquier equipamiento ajeno a la institución sin autorización expresa del desarrollador."] {
            assert!(AUTHORIZATION_TEXT.contains(needle), "falta: {needle}");
        }
    }

    #[test]
    fn dates_are_formatted_in_utc() {
        assert_eq!(fmt_date_utc(0), "1970-01-01");
        assert_eq!(fmt_date_utc(951_782_400), "2000-02-29");
        assert_eq!(fmt_date_utc(1_798_761_600), "2027-01-01");
        assert_eq!(fmt_date_utc(1_798_761_599), "2026-12-31");
    }

    #[test]
    fn every_shortcut_the_task_names_is_documented() {
        let keys: Vec<&str> = SHORTCUTS.iter().map(|s| s.0).collect();
        for k in ["Espacio", "Esc", "Esc, Esc", "F", "Supr / Retroceso", "Ctrl+N", "Ctrl+O", "Ctrl+S", "F1"] {
            assert!(keys.contains(&k), "falta el atajo {k}");
        }
        assert_eq!(HelpTab::ALL.map(|t| t.title()), ["Atajos de Teclado", "Operación de Escena", "Autorización Legal y Licencia"]);
    }

    #[test]
    fn the_license_lines_follow_the_state() {
        let valid = LicenseInfo { state: LicenseState::Valid { licensee: "Ballet Oficial de Córdoba".into(), issued_unix: 1_798_761_600, expires_unix: 1_798_761_600 + 30 * 86_400, days_left: Some(30) }, machine_id: Some("AAAA-BBBB-CCCC-DDDD".into()), lock_text: None, reactivable: false };
        let (who, crypto, _) = license_lines(&valid);
        assert_eq!(who, "Ballet Oficial de Córdoba");
        assert!(crypto.contains("Ed25519") && crypto.contains("2027-01-01") && crypto.contains("vence el 2027-01-31 (quedan 30 días)"), "{crypto}");
        let forever = LicenseInfo { state: LicenseState::Valid { licensee: "x".into(), issued_unix: 0, expires_unix: 0, days_left: None }, machine_id: None, lock_text: None, reactivable: false };
        assert!(license_lines(&forever).1.contains("sin vencimiento"));
        let bad = LicenseInfo { state: LicenseState::Invalid { reason: "la firma no es válida".into() }, machine_id: None, lock_text: None, reactivable: false };
        assert!(license_lines(&bad).1.contains("Rechazada"));
        assert!(license_lines(&LicenseInfo::not_enforced()).1.contains("desarrollo"));
    }

    #[test]
    fn the_manual_shows_the_keys_the_operator_chose() {
        let mut k = Keymap::default();
        for (key, _) in SHORTCUTS {
            if !matches!(key, "Esc, Esc" | "Flecha arriba / abajo" | "Supr / Retroceso" | "Ctrl+Mayús+Z / Ctrl+Y" | "M") {
                assert_eq!(shortcut_key(key, &k), key, "de fábrica cada fila dice lo mismo que antes");
            }
        }
        assert_eq!(shortcut_key("Supr / Retroceso", &k), "Supr / Retroceso");
        k.assign(crate::prefs::Command::Go, 0, crate::prefs::Binding::plain(egui::Key::F12)).unwrap();
        k.assign(crate::prefs::Command::Panic, 0, crate::prefs::Binding::plain(egui::Key::F11)).unwrap();
        assert_eq!(shortcut_key("Espacio", &k), "F12");
        assert_eq!(shortcut_key("Esc, Esc", &k), "F11, F11");
    }

    #[test]
    fn opening_about_closes_the_manual_and_toggling_works() {
        let mut s = HelpState::default();
        s.toggle_manual();
        assert!(s.open && s.any_open());
        s.open_about();
        assert!(!s.open && s.about_open);
        s.toggle_manual();
        assert!(s.open && !s.about_open);
        s.toggle_manual();
        assert!(!s.any_open());
    }
}
