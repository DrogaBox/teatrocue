//! Reloj gigante de escenario: cuenta regresiva del tiempo que le queda a la pista que suena.
//!
//! Verde mientras hay tiempo, ámbar parpadeante suave por debajo de 10 s y rojo parpadeante por debajo de 5 s (remate
//! de acto). Si suenan varias cues con fin conocido, manda la que más tiempo le queda: un efecto corto no tapa a la
//! pista principal.

use eframe::egui::{self, vec2, Color32, RichText, Ui};

use super::ViewCtx;
use crate::model::{CueUiPhase, Telemetry, UiModel};
use crate::theme;

pub const WARN_SECS: f64 = 10.0;
pub const CRITICAL_SECS: f64 = 5.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Countdown {
    pub cue_id: u64,
    pub remaining: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Alert {
    Normal,
    Warn,
    Critical,
}

pub fn alert_of(remaining: f64) -> Alert {
    if remaining < CRITICAL_SECS {
        Alert::Critical
    } else if remaining < WARN_SECS {
        Alert::Warn
    } else {
        Alert::Normal
    }
}

/// La cue de la que mostrar el tiempo restante, si hay alguna sonando con duración conocida.
pub fn countdown(model: &UiModel, telemetry: &Telemetry) -> Option<Countdown> {
    telemetry
        .running
        .iter()
        .filter(|(_, i)| i.phase == Some(CueUiPhase::Action))
        .filter_map(|(&id, i)| {
            let d = model.summary(id)?.duration.filter(|d| d.is_finite() && *d > 0.0)?;
            Some(Countdown { cue_id: id, remaining: (d - f64::from(i.action_elapsed)).max(0.0) })
        })
        .max_by(|a, b| a.remaining.total_cmp(&b.remaining).then(b.cue_id.cmp(&a.cue_id)))
}

/// `-01:24.3` (décimas, truncadas para que el cero llegue exacto al final).
pub fn fmt_countdown(remaining: f64) -> String {
    let tenths = (remaining.max(0.0) * 10.0).floor() as u64;
    let (m, s, t) = (tenths / 600, (tenths / 10) % 60, tenths % 10);
    format!("-{m:02}:{s:02}.{t}")
}

/// Intensidad del parpadeo `0.0..=1.0` a la hora `t`: suave en ámbar, marcado y más rápido en rojo.
pub fn blink(alert: Alert, t: f64) -> f32 {
    let (hz, depth) = match alert {
        Alert::Normal => return 1.0,
        Alert::Warn => (1.0, 0.35),
        Alert::Critical => (2.5, 0.7),
    };
    let wave = 0.5 + 0.5 * (t * std::f64::consts::TAU * hz).sin();
    (1.0 - depth * (1.0 - wave)) as f32
}

pub fn show(ui: &mut Ui, v: &ViewCtx, width: f32, font: f32) {
    let cd = countdown(v.model, v.telemetry);
    let alert = cd.map_or(Alert::Normal, |c| alert_of(c.remaining));
    let t = ui.ctx().input(|i| i.time);
    let k = blink(alert, t);
    let base = match (cd, alert) {
        (None, _) => theme::TEXT_DIM,
        (Some(_), Alert::Normal) => theme::GREEN,
        (Some(_), Alert::Warn) => theme::AMBER,
        (Some(_), Alert::Critical) => theme::RED,
    };
    let color = Color32::from_rgb((f32::from(base.r()) * k) as u8, (f32::from(base.g()) * k) as u8, (f32::from(base.b()) * k) as u8);
    let stroke = if alert == Alert::Normal { theme::GRID } else { color };
    let text = cd.map_or("--:--.-".to_string(), |c| fmt_countdown(c.remaining));
    egui::Frame::new().fill(theme::BG).stroke(egui::Stroke::new(if alert == Alert::Critical { 2.5 } else { 1.0 }, stroke)).inner_margin(egui::Margin::symmetric(10, 4)).show(ui, |ui| {
        ui.set_min_size(vec2(width, 46.0));
        ui.set_max_width(width);
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            let who = cd.and_then(|c| v.model.summary(c.cue_id)).map_or_else(|| "RESTANTE".to_string(), |c| format!("RESTANTE · {}", crate::format::ellipsize(&c.number, 8)));
            ui.label(RichText::new(who).small().color(theme::TEXT_DIM));
            ui.add(egui::Label::new(RichText::new(text).monospace().size(font).strong().color(color)).extend());
        });
    });
    if alert != Alert::Normal {
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(60));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::RunningInfo;
    use qlab_protocol::ipc_msg::CueSummary;

    fn summary(id: u64, duration: Option<f64>) -> CueSummary {
        CueSummary {
            id,
            number: id.to_string(),
            name: format!("c{id}"),
            kind: "audio".into(),
            state: "Running".into(),
            armed: true,
            parent: None,
            target: String::new(),
            pre_wait: 0.0,
            post_wait: 0.0,
            duration,
            continue_mode: Default::default(),
            flagged: false,
            color: Default::default(),
            dead: false,
            memories: Vec::new(),
            file_sample_rate: 48_000,
            start_override_secs: None,
        }
    }

    fn running(elapsed: f32) -> RunningInfo {
        RunningInfo { phase: Some(CueUiPhase::Action), action_elapsed: elapsed, ..Default::default() }
    }

    #[test]
    fn the_light_changes_at_ten_and_five_seconds() {
        assert_eq!(alert_of(60.0), Alert::Normal);
        assert_eq!(alert_of(10.0), Alert::Normal);
        assert_eq!(alert_of(9.99), Alert::Warn);
        assert_eq!(alert_of(5.0), Alert::Warn);
        assert_eq!(alert_of(4.99), Alert::Critical);
        assert_eq!(alert_of(0.0), Alert::Critical);
    }

    #[test]
    fn the_countdown_is_formatted_as_minus_minutes_seconds_tenths() {
        assert_eq!(fmt_countdown(84.3), "-01:24.3");
        assert_eq!(fmt_countdown(0.0), "-00:00.0");
        assert_eq!(fmt_countdown(0.99), "-00:00.9");
        assert_eq!(fmt_countdown(3725.0), "-62:05.0");
        assert_eq!(fmt_countdown(-3.0), "-00:00.0");
    }

    #[test]
    fn the_blink_is_steady_when_normal_and_stays_inside_its_depth_otherwise() {
        assert_eq!(blink(Alert::Normal, 0.37), 1.0);
        for i in 0..200 {
            let t = f64::from(i) * 0.013;
            let w = blink(Alert::Warn, t);
            let c = blink(Alert::Critical, t);
            assert!((0.65..=1.0).contains(&w) && (0.3..=1.0).contains(&c), "{w} {c}");
        }
        let min_c = (0..400).map(|i| blink(Alert::Critical, f64::from(i) * 0.005)).fold(1.0f32, f32::min);
        let min_w = (0..400).map(|i| blink(Alert::Warn, f64::from(i) * 0.005)).fold(1.0f32, f32::min);
        assert!(min_c < min_w, "el rojo parpadea más fuerte que el ámbar");
    }

    #[test]
    fn the_cue_with_most_time_left_wins_and_unknown_or_waiting_cues_are_ignored() {
        let mut model = UiModel::default();
        model.summaries = vec![summary(1, Some(120.0)), summary(2, Some(8.0)), summary(3, None), summary(4, Some(60.0))];
        let mut tel = Telemetry::default();
        assert_eq!(countdown(&model, &tel), None, "nada suena");
        tel.running.insert(1, running(35.7)); // quedan 84,3 s
        tel.running.insert(2, running(1.0)); // efecto corto: quedan 7 s
        tel.running.insert(3, running(5.0)); // sin fin conocido
        tel.running.insert(4, RunningInfo { phase: Some(CueUiPhase::PreWait), ..Default::default() }); // aún no suena
        let c = countdown(&model, &tel).unwrap();
        assert_eq!(c.cue_id, 1);
        assert!((c.remaining - 84.3).abs() < 1e-3, "{c:?}");
        tel.running.remove(&1);
        assert_eq!(countdown(&model, &tel).unwrap().cue_id, 2);
    }
}
