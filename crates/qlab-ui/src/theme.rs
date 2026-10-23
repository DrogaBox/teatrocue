//! Paleta de teatro: fondo oscuro de alto contraste, texto ámbar/verde/cian según el estado.

use eframe::egui::{self, Color32, CornerRadius, Stroke};

use crate::model::CueUiState;

pub const BG: Color32 = Color32::from_rgb(0x12, 0x12, 0x14);
pub const PANEL: Color32 = Color32::from_rgb(0x1a, 0x1a, 0x1e);
pub const PANEL_RAISED: Color32 = Color32::from_rgb(0x24, 0x24, 0x2a);
pub const ROW_ALT: Color32 = Color32::from_rgb(0x17, 0x17, 0x1b);
pub const GRID: Color32 = Color32::from_rgb(0x34, 0x34, 0x3c);

pub const TEXT: Color32 = Color32::from_rgb(0xd8, 0xd8, 0xdc);
pub const TEXT_DIM: Color32 = Color32::from_rgb(0x82, 0x82, 0x8a);
pub const AMBER: Color32 = Color32::from_rgb(0xff, 0xb3, 0x1a);
pub const AMBER_DIM: Color32 = Color32::from_rgb(0x8a, 0x62, 0x14);
pub const GREEN: Color32 = Color32::from_rgb(0x3d, 0xe0, 0x6a);
pub const GREEN_DARK: Color32 = Color32::from_rgb(0x0f, 0x5a, 0x28);
pub const CYAN: Color32 = Color32::from_rgb(0x2e, 0xd6, 0xe8);
pub const RED: Color32 = Color32::from_rgb(0xff, 0x4a, 0x4a);
pub const RED_DARK: Color32 = Color32::from_rgb(0x6a, 0x16, 0x16);

/// Fondo de la fila del playhead y de la fila seleccionada.
pub const PLAYHEAD_BG: Color32 = Color32::from_rgb(0x3a, 0x2a, 0x08);
pub const SELECTED_BG: Color32 = Color32::from_rgb(0x1e, 0x2c, 0x4a);
pub const RUNNING_BG: Color32 = Color32::from_rgb(0x0e, 0x2e, 0x1a);
pub const WAITING_BG: Color32 = Color32::from_rgb(0x0c, 0x2a, 0x30);

/// Colores de las cues etiquetadas (columna de color de QLab).
pub fn cue_color(c: qlab_protocol::cue_dto::CueColor) -> Option<Color32> {
    use qlab_protocol::cue_dto::CueColor::*;
    Some(match c {
        None => return Option::None,
        Red => Color32::from_rgb(0xc0, 0x3a, 0x3a),
        Orange => Color32::from_rgb(0xd0, 0x7a, 0x1e),
        Green => Color32::from_rgb(0x3a, 0xa0, 0x58),
        Blue => Color32::from_rgb(0x3a, 0x6a, 0xd0),
        Purple => Color32::from_rgb(0x8a, 0x4a, 0xc0),
    })
}

/// Color del texto de una cue según su estado y si es el playhead.
pub fn text_color(state: CueUiState, is_playhead: bool, armed: bool) -> Color32 {
    if !armed {
        return TEXT_DIM;
    }
    match state {
        CueUiState::Running => GREEN,
        CueUiState::PreWaiting | CueUiState::PostWaiting => CYAN,
        CueUiState::Paused => AMBER_DIM,
        CueUiState::Completed => TEXT_DIM,
        CueUiState::Idle | CueUiState::Armed => {
            if is_playhead {
                AMBER
            } else {
                TEXT
            }
        }
    }
}

pub fn row_background(state: CueUiState, is_playhead: bool, is_selected: bool) -> Option<Color32> {
    match (state, is_playhead, is_selected) {
        (CueUiState::Running, _, _) => Some(RUNNING_BG),
        (CueUiState::PreWaiting | CueUiState::PostWaiting, _, _) => Some(WAITING_BG),
        (_, true, _) => Some(PLAYHEAD_BG),
        (_, _, true) => Some(SELECTED_BG),
        _ => None,
    }
}

pub fn apply(ctx: &egui::Context) {
    let mut v = egui::Visuals::dark();
    v.panel_fill = PANEL;
    v.window_fill = PANEL_RAISED;
    v.extreme_bg_color = BG;
    v.faint_bg_color = ROW_ALT;
    v.override_text_color = Some(TEXT);
    v.selection.bg_fill = Color32::from_rgb(0x2a, 0x4a, 0x8a);
    v.selection.stroke = Stroke::new(1.0, AMBER);
    v.hyperlink_color = CYAN;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, GRID);
    v.widgets.inactive.bg_fill = PANEL_RAISED;
    v.widgets.inactive.weak_bg_fill = PANEL_RAISED;
    v.widgets.hovered.bg_fill = Color32::from_rgb(0x34, 0x34, 0x3e);
    v.widgets.hovered.weak_bg_fill = Color32::from_rgb(0x34, 0x34, 0x3e);
    v.widgets.active.bg_fill = Color32::from_rgb(0x4a, 0x4a, 0x58);
    v.window_corner_radius = CornerRadius::same(4);
    v.menu_corner_radius = CornerRadius::same(4);
    ctx.set_theme(egui::Theme::Dark);
    ctx.set_visuals_of(egui::Theme::Dark, v);
    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = egui::vec2(6.0, 4.0);
        style.spacing.button_padding = egui::vec2(8.0, 3.0);
        style.interaction.selectable_labels = false; // las etiquetas de la tabla no deben robar el clic de la fila
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_colors_follow_the_theatre_convention() {
        assert_eq!(text_color(CueUiState::Running, false, true), GREEN);
        assert_eq!(text_color(CueUiState::PreWaiting, false, true), CYAN);
        assert_eq!(text_color(CueUiState::PostWaiting, true, true), CYAN);
        assert_eq!(text_color(CueUiState::Idle, true, true), AMBER, "el playhead va en ámbar");
        assert_eq!(text_color(CueUiState::Idle, false, true), TEXT);
        assert_eq!(text_color(CueUiState::Running, true, false), TEXT_DIM, "una cue desarmada se apaga");
        assert_eq!(text_color(CueUiState::Completed, false, true), TEXT_DIM);
    }

    #[test]
    fn backgrounds_prioritise_activity_over_playhead_over_selection() {
        assert_eq!(row_background(CueUiState::Running, true, true), Some(RUNNING_BG));
        assert_eq!(row_background(CueUiState::Idle, true, true), Some(PLAYHEAD_BG));
        assert_eq!(row_background(CueUiState::Idle, false, true), Some(SELECTED_BG));
        assert_eq!(row_background(CueUiState::Idle, false, false), None);
        assert_eq!(row_background(CueUiState::PostWaiting, false, false), Some(WAITING_BG));
    }

    #[test]
    fn text_has_enough_contrast_on_the_dark_background() {
        fn lum(c: Color32) -> f32 {
            let f = |v: u8| {
                let x = v as f32 / 255.0;
                if x <= 0.03928 { x / 12.92 } else { ((x + 0.055) / 1.055).powf(2.4) }
            };
            0.2126 * f(c.r()) + 0.7152 * f(c.g()) + 0.0722 * f(c.b())
        }
        let ratio = |a: Color32, b: Color32| {
            let (la, lb) = (lum(a) + 0.05, lum(b) + 0.05);
            la.max(lb) / la.min(lb)
        };
        for fg in [TEXT, AMBER, GREEN, CYAN] {
            for bg in [BG, PLAYHEAD_BG, RUNNING_BG, WAITING_BG, SELECTED_BG] {
                assert!(ratio(fg, bg) >= 7.0, "{fg:?} sobre {bg:?}: {:.1}", ratio(fg, bg));
            }
        }
    }

    #[test]
    fn apply_does_not_panic_and_cue_colors_map() {
        let ctx = egui::Context::default();
        apply(&ctx);
        assert!(cue_color(qlab_protocol::cue_dto::CueColor::None).is_none());
        assert!(cue_color(qlab_protocol::cue_dto::CueColor::Purple).is_some());
    }
}
