//! DirectComposition renderer for the API key dialog.
//!
//! Everything is drawn by Direct2D/DirectWrite so the dialog matches the
//! Windows 11 Fluent look in both light and dark mode: acrylic surface,
//! Segoe UI Variable, rounded text field, accent primary button, focus rings
//! and translucent hover states. There are no classic Win32 child controls.

use windows::core::{Result as WinResult, BOOL};
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::DirectWrite::{
    IDWriteTextFormat, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_WEIGHT_SEMI_BOLD,
    DWRITE_HIT_TEST_METRICS, DWRITE_TEXT_METRICS,
};
use windows_numerics::Vector2;

use crate::config::is_valid_api_key;
use crate::theme::Theme;
use crate::ui::surface::{Color, Surface};

pub const WIDTH: f32 = 500.0;
pub const HEIGHT: f32 = 250.0;

const MARGIN: f32 = 24.0;
const TITLE_Y: f32 = 16.0;
const TITLE_H: f32 = 32.0;
const BODY_Y: f32 = 50.0;
const BODY_H: f32 = 48.0;
const INPUT_Y: f32 = 108.0;
const INPUT_H: f32 = 34.0;
const HINT_Y: f32 = 147.0;
const HINT_H: f32 = 18.0;
const BUTTON_Y: f32 = 192.0;
const BUTTON_W: f32 = 96.0;
const BUTTON_H: f32 = 32.0;
const BUTTON_GAP: f32 = 8.0;
const INNER_PAD: f32 = 10.0;
const FIELD_RADIUS: f32 = 4.0;
const BUTTON_RADIUS: f32 = 4.0;

const SAVE_X: f32 = WIDTH - MARGIN - BUTTON_W;
const CANCEL_X: f32 = SAVE_X - BUTTON_GAP - BUTTON_W;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    Input,
    Cancel,
    Save,
}

pub fn control_at(x: f32, y: f32) -> Option<Control> {
    if x >= MARGIN && x <= WIDTH - MARGIN && y >= INPUT_Y && y <= INPUT_Y + INPUT_H {
        return Some(Control::Input);
    }
    let in_button =
        |left: f32| x >= left && x <= left + BUTTON_W && y >= BUTTON_Y && y <= BUTTON_Y + BUTTON_H;
    if in_button(SAVE_X) {
        return Some(Control::Save);
    }
    if in_button(CANCEL_X) {
        return Some(Control::Cancel);
    }
    None
}

pub struct DialogView<'a> {
    pub title: &'a str,
    pub description: &'a str,
    pub existing: bool,
    pub key: &'a str,
    pub cursor: usize,
    pub select_all: bool,
    pub focus: Control,
    pub hovered: Option<Control>,
    pub pressed: Option<Control>,
    pub error: &'a str,
    pub caret_on: bool,
}

pub struct DialogRenderer {
    surface: Surface,
    title: IDWriteTextFormat,
    body: IDWriteTextFormat,
    input: IDWriteTextFormat,
    hint: IDWriteTextFormat,
    button: IDWriteTextFormat,
    scroll: f32,
    theme: Theme,
}

impl DialogRenderer {
    pub fn new(hwnd: HWND, dpi: u32, theme: Theme) -> WinResult<Self> {
        let surface = Surface::new(hwnd, dpi, WIDTH, HEIGHT)?;
        let title = surface.text_format(
            "Segoe UI Variable Display",
            20.0,
            DWRITE_FONT_WEIGHT_SEMI_BOLD,
            false,
            false,
        )?;
        let body = surface.text_format(
            "Segoe UI Variable Text",
            12.5,
            DWRITE_FONT_WEIGHT_NORMAL,
            false,
            true,
        )?;
        let input = surface.text_format(
            "Segoe UI Variable Text",
            13.0,
            DWRITE_FONT_WEIGHT_NORMAL,
            false,
            false,
        )?;
        let hint = surface.text_format(
            "Segoe UI Variable Text",
            12.0,
            DWRITE_FONT_WEIGHT_NORMAL,
            false,
            false,
        )?;
        let button = surface.text_format(
            "Segoe UI Variable Text",
            12.5,
            DWRITE_FONT_WEIGHT_SEMI_BOLD,
            true,
            false,
        )?;
        Ok(Self {
            surface,
            title,
            body,
            input,
            hint,
            button,
            scroll: 0.0,
            theme,
        })
    }

    pub fn render(&mut self, view: &DialogView, transparent: bool) -> WinResult<()> {
        let theme = self.theme;
        let width = self.surface.width;
        let height = self.surface.height;
        self.surface.begin();

        if !transparent {
            self.surface.fill_rounded(
                0.0,
                0.0,
                width,
                height,
                8.0,
                theme.solid_background().with_alpha(1.0),
            );
            self.surface
                .stroke_rounded(0.0, 0.0, width, height, 8.0, theme.border_tint(), 1.0);
        } else {
            // Layer a subtle Fluent tint over the DWM acrylic so a saturated
            // wallpaper cannot wash out the dialog content.
            self.surface.fill_rounded(
                0.0,
                0.0,
                width,
                height,
                8.0,
                if theme.dark {
                    (0.06, 0.06, 0.06, 0.30)
                } else {
                    (1.0, 1.0, 1.0, 0.34)
                },
            );
            self.surface.stroke_rounded(
                0.5,
                0.5,
                width - 1.0,
                height - 1.0,
                8.0,
                if theme.dark {
                    (1.0, 1.0, 1.0, 0.08)
                } else {
                    (0.0, 0.0, 0.0, 0.08)
                },
                1.0,
            );
        }

        let fg = theme.foreground().with_alpha(1.0);
        let secondary = theme.secondary().with_alpha(1.0);

        self.surface.text(
            &self.title,
            view.title,
            MARGIN,
            TITLE_Y,
            width - MARGIN * 2.0,
            TITLE_H,
            fg,
        );
        self.surface.text(
            &self.body,
            view.description,
            MARGIN,
            BODY_Y,
            width - MARGIN * 2.0,
            BODY_H,
            secondary,
        );

        let field_width = width - MARGIN * 2.0;
        let focused = view.focus == Control::Input;
        let hovered = view.hovered == Some(Control::Input);
        let fill = match (theme.dark, hovered) {
            (true, false) => (1.0, 1.0, 1.0, 0.055),
            (true, true) => (1.0, 1.0, 1.0, 0.075),
            (false, false) => (1.0, 1.0, 1.0, 0.62),
            (false, true) => (1.0, 1.0, 1.0, 0.78),
        };
        self.surface
            .fill_rounded(MARGIN, INPUT_Y, field_width, INPUT_H, FIELD_RADIUS, fill);
        let border = if focused {
            theme.accent.with_alpha(1.0)
        } else if theme.dark {
            (1.0, 1.0, 1.0, 0.10)
        } else {
            (0.0, 0.0, 0.0, 0.13)
        };
        self.surface.stroke_rounded(
            MARGIN,
            INPUT_Y,
            field_width,
            INPUT_H,
            FIELD_RADIUS,
            border,
            if focused { 1.5 } else { 1.0 },
        );

        let mut caret_x = MARGIN + INNER_PAD;
        if view.key.is_empty() {
            self.scroll = 0.0;
            self.surface.text(
                &self.input,
                "Pega la API key aquí",
                MARGIN + INNER_PAD,
                INPUT_Y,
                field_width - INNER_PAD * 2.0,
                INPUT_H,
                theme.muted().with_alpha(0.85),
            );
        } else {
            let masked = "•".repeat(view.key.chars().count());
            let utf16: Vec<u16> = masked.encode_utf16().collect();
            if let Ok(layout) = unsafe {
                self.surface
                    .dwrite()
                    .CreateTextLayout(&utf16, &self.input, 2000.0, INPUT_H)
            } {
                let mut position_x = 0.0;
                let mut position_y = 0.0;
                let mut hit = DWRITE_HIT_TEST_METRICS::default();
                let _ = unsafe {
                    layout.HitTestTextPosition(
                        view.cursor as u32,
                        false,
                        &mut position_x,
                        &mut position_y,
                        &mut hit,
                    )
                };
                let mut text_metrics = DWRITE_TEXT_METRICS::default();
                let _ = unsafe { layout.GetMetrics(&mut text_metrics) };

                let inner_width = field_width - INNER_PAD * 2.0;
                let scroll = (position_x - inner_width + 2.0).max(0.0);
                self.scroll = scroll;
                let origin_x = MARGIN + INNER_PAD - scroll;
                let text_width = text_metrics.widthIncludingTrailingWhitespace;

                if view.select_all {
                    let selection = (
                        origin_x.max(MARGIN + INNER_PAD),
                        (origin_x + text_width).min(MARGIN + field_width - INNER_PAD),
                    );
                    if selection.1 > selection.0 {
                        self.surface.fill_rounded(
                            selection.0,
                            INPUT_Y + 8.0,
                            selection.1 - selection.0,
                            INPUT_H - 16.0,
                            2.0,
                            theme
                                .accent
                                .with_alpha(if theme.dark { 0.38 } else { 0.30 }),
                        );
                    }
                }

                self.surface.draw_layout(&layout, origin_x, INPUT_Y, fg);
                caret_x = origin_x + position_x;
            }
        }

        if focused && view.caret_on {
            self.surface.line(
                Vector2 {
                    X: caret_x,
                    Y: INPUT_Y + 9.0,
                },
                Vector2 {
                    X: caret_x,
                    Y: INPUT_Y + INPUT_H - 9.0,
                },
                fg,
                1.2,
            );
        }

        let hint_text;
        let hint_color;
        if !view.error.is_empty() {
            hint_text = view.error.to_owned();
            hint_color = theme.red().with_alpha(1.0);
        } else if view.key.is_empty() {
            if view.existing {
                hint_text = "Deja el campo vacío para conservar la clave actual.".to_owned();
            } else {
                hint_text = "La clave tiene 64 caracteres hexadecimales.".to_owned();
            }
            hint_color = theme.muted().with_alpha(0.95);
        } else {
            let valid = is_valid_api_key(view.key);
            hint_text = format!("{}/64 caracteres hexadecimales", view.key.chars().count());
            hint_color = if valid {
                theme.green().with_alpha(1.0)
            } else {
                theme.muted().with_alpha(0.95)
            };
        }
        self.surface.text(
            &self.hint,
            &hint_text,
            MARGIN,
            HINT_Y,
            width - MARGIN * 2.0,
            HINT_H,
            hint_color,
        );

        self.draw_cancel(view, secondary);
        self.draw_save(view);
        self.surface.end()
    }

    /// Maps a click inside the text field to a byte cursor position.
    pub fn caret_from_point(&self, key: &str, x_dip: f32) -> usize {
        if key.is_empty() {
            return 0;
        }
        let masked = "•".repeat(key.chars().count());
        let utf16: Vec<u16> = masked.encode_utf16().collect();
        let Ok(layout) = (unsafe {
            self.surface
                .dwrite()
                .CreateTextLayout(&utf16, &self.input, 2000.0, INPUT_H)
        }) else {
            return key.len();
        };
        let mut trailing = BOOL(0);
        let mut inside = BOOL(0);
        let mut metrics = DWRITE_HIT_TEST_METRICS::default();
        let relative = (x_dip - MARGIN - INNER_PAD + self.scroll).max(0.0);
        if unsafe {
            layout.HitTestPoint(
                relative,
                INPUT_H * 0.5,
                &mut trailing,
                &mut inside,
                &mut metrics,
            )
        }
        .is_err()
        {
            return key.len();
        }
        (metrics.textPosition as usize + usize::from(trailing.as_bool())).min(key.len())
    }

    fn draw_cancel(&mut self, view: &DialogView, text_color: Color) {
        let hovered = view.hovered == Some(Control::Cancel);
        let pressed = view.pressed == Some(Control::Cancel) && hovered;
        let base = if self.theme.dark {
            (1.0, 1.0, 1.0, 0.06)
        } else {
            (1.0, 1.0, 1.0, 0.55)
        };
        self.surface
            .fill_rounded(CANCEL_X, BUTTON_Y, BUTTON_W, BUTTON_H, BUTTON_RADIUS, base);
        if hovered {
            let overlay = if self.theme.dark {
                (1.0, 1.0, 1.0, if pressed { 0.01 } else { 0.07 })
            } else {
                (0.0, 0.0, 0.0, if pressed { 0.10 } else { 0.06 })
            };
            self.surface.fill_rounded(
                CANCEL_X,
                BUTTON_Y,
                BUTTON_W,
                BUTTON_H,
                BUTTON_RADIUS,
                overlay,
            );
        }
        let border = if self.theme.dark {
            (1.0, 1.0, 1.0, 0.10)
        } else {
            (0.0, 0.0, 0.0, 0.12)
        };
        self.surface.stroke_rounded(
            CANCEL_X,
            BUTTON_Y,
            BUTTON_W,
            BUTTON_H,
            BUTTON_RADIUS,
            border,
            1.0,
        );
        self.surface.text(
            &self.button,
            "Cancelar",
            CANCEL_X,
            BUTTON_Y,
            BUTTON_W,
            BUTTON_H,
            text_color,
        );
        self.focus_ring(Control::Cancel, view);
    }

    fn draw_save(&mut self, view: &DialogView) {
        let hovered = view.hovered == Some(Control::Save);
        let pressed = view.pressed == Some(Control::Save) && hovered;
        let can_submit = if view.key.is_empty() {
            view.existing
        } else {
            is_valid_api_key(view.key)
        };
        let accent = self.theme.accent;
        if can_submit {
            self.surface.fill_rounded(
                SAVE_X,
                BUTTON_Y,
                BUTTON_W,
                BUTTON_H,
                BUTTON_RADIUS,
                accent.with_alpha(1.0),
            );
            if hovered {
                self.surface.fill_rounded(
                    SAVE_X,
                    BUTTON_Y,
                    BUTTON_W,
                    BUTTON_H,
                    BUTTON_RADIUS,
                    if self.theme.dark {
                        (1.0, 1.0, 1.0, if pressed { 0.02 } else { 0.14 })
                    } else {
                        (0.0, 0.0, 0.0, if pressed { 0.14 } else { 0.08 })
                    },
                );
            }
        } else {
            self.surface.fill_rounded(
                SAVE_X,
                BUTTON_Y,
                BUTTON_W,
                BUTTON_H,
                BUTTON_RADIUS,
                if self.theme.dark {
                    (1.0, 1.0, 1.0, 0.07)
                } else {
                    (0.0, 0.0, 0.0, 0.045)
                },
            );
        }
        let luminance = 0.299 * accent.r as f32 + 0.587 * accent.g as f32 + 0.114 * accent.b as f32;
        let label = if !can_submit {
            self.theme.muted().with_alpha(0.85)
        } else if luminance > 150.0 {
            (0.05, 0.05, 0.05, 0.95)
        } else {
            (1.0, 1.0, 1.0, 0.98)
        };
        self.surface.text(
            &self.button,
            "Guardar",
            SAVE_X,
            BUTTON_Y,
            BUTTON_W,
            BUTTON_H,
            label,
        );
        self.focus_ring(Control::Save, view);
    }

    fn focus_ring(&mut self, control: Control, view: &DialogView) {
        if view.focus != control {
            return;
        }
        let x = if control == Control::Save {
            SAVE_X
        } else {
            CANCEL_X
        };
        let color = if self.theme.dark {
            (1.0, 1.0, 1.0, 0.80)
        } else {
            (0.0, 0.0, 0.0, 0.70)
        };
        self.surface.stroke_rounded(
            x - 3.0,
            BUTTON_Y - 3.0,
            BUTTON_W + 6.0,
            BUTTON_H + 6.0,
            BUTTON_RADIUS + 3.0,
            color,
            1.4,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_hit_testing_matches_layout() {
        assert_eq!(
            control_at(WIDTH / 2.0, INPUT_Y + INPUT_H / 2.0),
            Some(Control::Input)
        );
        assert_eq!(
            control_at(SAVE_X + BUTTON_W / 2.0, BUTTON_Y + BUTTON_H / 2.0),
            Some(Control::Save)
        );
        assert_eq!(
            control_at(CANCEL_X + BUTTON_W / 2.0, BUTTON_Y + BUTTON_H / 2.0),
            Some(Control::Cancel)
        );
        assert_eq!(control_at(1.0, 1.0), None);
        assert_eq!(control_at(WIDTH / 2.0, BUTTON_Y + BUTTON_H + 5.0), None);
    }
}
