//! Ventana «Configuración de audio»: elegir interfaz de sonido, canales, buffer y frecuencia.

use std::sync::mpsc::{channel, Receiver};

use eframe::egui::{self, RichText};

use crate::settings::{AudioSettings, BUFFER_CHOICES, MAX_CHANNELS, RATE_CHOICES};
use crate::theme;

pub const TITLE: &str = "Configuración de audio";
/// Primera opción del selector de auriculares.
pub const NO_PHONES: &str = "Sin salida de auriculares";
pub const DEFAULT_DEVICE: &str = "Dispositivo predeterminado del sistema";

#[derive(Default)]
pub struct AudioSettingsUi {
    pub open: bool,
    pub draft: AudioSettings,
    /// `(host, dispositivo)` de las salidas disponibles; `None` mientras se enumeran.
    pub devices: Option<Vec<(String, String)>>,
    scan: Option<Receiver<Vec<(String, String)>>>,
}

impl AudioSettingsUi {
    /// Abre la ventana con una copia de los ajustes actuales y enumera los dispositivos en un hilo (puede tardar).
    pub fn open_with(&mut self, current: &AudioSettings) {
        self.open = true;
        self.draft = current.clone();
        self.devices = None;
        let (tx, rx) = channel();
        self.scan = Some(rx);
        let _ = std::thread::Builder::new().name("qlab-device-scan".into()).spawn(move || {
            let _ = tx.send(qlab_daemon::backend_audio::list_output_devices());
        });
    }

    fn poll(&mut self) {
        if let Some(rx) = &self.scan {
            if let Ok(list) = rx.try_recv() {
                self.devices = Some(list);
                self.scan = None;
            }
        }
    }

    pub fn set_devices_for_test(&mut self, list: Vec<(String, String)>) {
        self.devices = Some(list);
        self.scan = None;
    }
}

/// Qué pidió el operador.
pub enum Outcome {
    Apply(AudioSettings),
    Close,
}

pub fn show(ctx: &egui::Context, st: &mut AudioSettingsUi, engine_note: &str) -> Option<Outcome> {
    if !st.open {
        return None;
    }
    st.poll();
    let mut out = None;
    let modal = egui::Modal::new(egui::Id::NULL.with("audio_settings_modal")).show(ctx, |ui| {
        ui.set_width(560.0);
        ui.heading(TITLE);
        ui.add_space(6.0);
        ui.label(RichText::new(engine_note).color(theme::TEXT_DIM));
        ui.add_space(10.0);
        egui::Grid::new("audio_settings_grid").num_columns(2).spacing([14.0, 8.0]).show(ui, |ui| {
            ui.label("Salida de audio");
            let selected = match (&st.draft.host, &st.draft.device) {
                (_, Some(d)) => d.clone(),
                _ => DEFAULT_DEVICE.to_string(),
            };
            egui::ComboBox::from_id_salt("audio_device").selected_text(selected).width(380.0).show_ui(ui, |ui| {
                if ui.selectable_label(st.draft.device.is_none(), DEFAULT_DEVICE).clicked() {
                    st.draft.device = None;
                    st.draft.host = None;
                }
                match &st.devices {
                    None => {
                        ui.label(RichText::new("Buscando dispositivos…").color(theme::TEXT_DIM));
                    }
                    Some(list) => {
                        for (host, name) in list.iter().filter(|(_, n)| !n.is_empty()) {
                            let on = st.draft.device.as_deref() == Some(name.as_str()) && st.draft.host.as_deref() == Some(host.as_str());
                            if ui.selectable_label(on, format!("{name}  ({host})")).clicked() {
                                st.draft.device = Some(name.clone());
                                st.draft.host = Some(host.clone());
                            }
                        }
                    }
                }
            });
            ui.end_row();
            ui.label("Canales de salida");
            ui.add(egui::DragValue::new(&mut st.draft.channels).range(1..=MAX_CHANNELS).suffix(" canales"));
            ui.end_row();
            ui.label("Buffer");
            egui::ComboBox::from_id_salt("audio_buffer").selected_text(format!("{} frames", st.draft.buffer_frames)).show_ui(ui, |ui| {
                for b in BUFFER_CHOICES {
                    ui.selectable_value(&mut st.draft.buffer_frames, b, format!("{b} frames"));
                }
            });
            ui.end_row();
            ui.label("Frecuencia");
            egui::ComboBox::from_id_salt("audio_rate").selected_text(format!("{} Hz", st.draft.sample_rate)).show_ui(ui, |ui| {
                for r in RATE_CHOICES {
                    ui.selectable_value(&mut st.draft.sample_rate, r, format!("{r} Hz"));
                }
            });
            ui.end_row();
            ui.label("Auriculares (PFL)");
            let pairs: Vec<u16> = (2..st.draft.channels.saturating_sub(1)).step_by(2).collect();
            let text = match (&st.draft.monitor_device, st.draft.monitor_first) {
                (Some(d), _) => d.clone(),
                (None, Some(m)) => format!("Canales {}-{} de la placa principal", m + 1, m + 2),
                _ => NO_PHONES.to_string(),
            };
            ui.vertical(|ui| {
                egui::ComboBox::from_id_salt("audio_monitor").selected_text(text).width(380.0).show_ui(ui, |ui| {
                    if ui.selectable_label(st.draft.monitor_device.is_none() && st.draft.monitor_first.is_none(), NO_PHONES).clicked() {
                        st.draft.monitor_device = None;
                        st.draft.monitor_first = None;
                    }
                    // Canales sobrantes de la placa principal (si tiene 4 o más).
                    for m in &pairs {
                        let on = st.draft.monitor_device.is_none() && st.draft.monitor_first == Some(*m);
                        if ui.selectable_label(on, format!("Canales {}-{} de la placa principal", m + 1, m + 2)).clicked() {
                            st.draft.monitor_device = None;
                            st.draft.monitor_first = Some(*m);
                        }
                    }
                    // Cualquier otra salida del sistema: no hace falta que la placa principal tenga 4 canales.
                    ui.separator();
                    ui.label(RichText::new("Otras salidas del sistema (otra placa)").small().color(theme::TEXT_DIM));
                    match &st.devices {
                        None => {
                            ui.label(RichText::new("Buscando dispositivos…").color(theme::TEXT_DIM));
                        }
                        Some(list) => {
                            let mut seen: Vec<&str> = Vec::new();
                            let others: Vec<&str> = list
                                .iter()
                                .map(|(_, n)| n.as_str())
                                .filter(|n| !n.is_empty() && st.draft.device.as_deref().is_none_or(|main| !main.eq_ignore_ascii_case(n)))
                                .filter(|n| {
                                    let new = !seen.contains(n);
                                    seen.push(n);
                                    new
                                })
                                .collect();
                            if others.is_empty() {
                                ui.label(RichText::new("No hay otras salidas de audio.").color(theme::TEXT_DIM));
                            }
                            for name in others {
                                let on = st.draft.monitor_device.as_deref() == Some(name);
                                if ui.selectable_label(on, name).clicked() {
                                    st.draft.monitor_device = Some(name.to_string());
                                    st.draft.monitor_first = None;
                                }
                            }
                        }
                    }
                });
                if st.draft.monitor_device.is_some() {
                    ui.label(RichText::new("Los auriculares suenan por su propia placa (con su reloj y frecuencia): el audio se adapta solo.").small().color(theme::TEXT_DIM));
                }
            });
            ui.end_row();
            ui.label("Control remoto OSC");
            ui.horizontal(|ui| {
                let mut on = st.draft.osc_port.is_some();
                if ui.checkbox(&mut on, "Activar en el puerto UDP").changed() {
                    st.draft.osc_port = on.then_some(53_000);
                }
                if let Some(p) = st.draft.osc_port.as_mut() {
                    ui.add(egui::DragValue::new(p).range(1024..=65_535));
                }
            });
            ui.end_row();
        });
        ui.label(RichText::new("OSC: /go /panic /stop /pause /resume /master <dB> /next /prev /cue/<n>/start|stop|pause|resume|devamp|load|arm|disarm. Cualquiera en la red con el puerto puede disparar la función: actívelo solo en una red de confianza.").small().color(theme::TEXT_DIM));
        ui.add_space(8.0);
        ui.label(RichText::new("Aplicar reinicia el motor de audio: se corta lo que esté sonando y el show abierto se vuelve a cargar solo.").small().color(theme::AMBER));
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            if ui.button(RichText::new("Aplicar y reiniciar motor").strong()).clicked() {
                out = Some(Outcome::Apply(st.draft.clone().sanitized()));
            }
            if ui.button("Cancelar").clicked() {
                out = Some(Outcome::Close);
            }
        });
    });
    if modal.should_close() && out.is_none() {
        out = Some(Outcome::Close);
    }
    if out.is_some() {
        st.open = false;
    }
    out
}
