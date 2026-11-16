//! Renderer for the custom context menu.
//!
//! The transparent DirectComposition surface lets the DWM acrylic backdrop
//! show through and, unlike GDI, each frame is cleared before drawing. That
//! fixes stale hover pills and gives true premultiplied hover alpha.

use windows::core::Result as WinResult;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F;
use windows::Win32::Graphics::Direct2D::D2D1_ROUNDED_RECT;
use windows::Win32::Graphics::DirectWrite::{IDWriteTextFormat, DWRITE_FONT_WEIGHT_NORMAL};
use windows_numerics::Vector2;

use crate::theme::Theme;
use crate::ui::menu::MenuItem;
use crate::ui::surface::Surface;

const ITEM_HEIGHT: f32 = 32.0;
const SEPARATOR_HEIGHT: f32 = 9.0;
const PADDING: f32 = 6.0;
const TEXT_LEFT: f32 = 40.0;
const CHECK_CENTER: f32 = 18.0;

pub fn menu_height(items: &[MenuItem]) -> f32 {
    PADDING * 2.0
        + items
            .iter()
            .map(|item| {
                if item.is_separator() {
                    SEPARATOR_HEIGHT
                } else {
                    ITEM_HEIGHT
                }
            })
            .sum::<f32>()
}

pub struct MenuRenderer {
    surface: Surface,
    text: IDWriteTextFormat,
    theme: Theme,
}

impl MenuRenderer {
    pub fn new(
        hwnd: HWND,
        dpi: u32,
        width_dip: f32,
        height_dip: f32,
        theme: Theme,
    ) -> WinResult<Self> {
        let surface = Surface::new(hwnd, dpi, width_dip, height_dip)?;
        let text = surface.text_format(
            "Segoe UI Variable Text",
            12.5,
            DWRITE_FONT_WEIGHT_NORMAL,
            false,
            false,
        )?;
        Ok(Self {
            surface,
            text,
            theme,
        })
    }

    pub fn animate_in(&self) {
        let _ = self.surface.animate_in(6.0, crate::ui::animation::FAST_MS);
    }

    pub fn render(
        &mut self,
        items: &[MenuItem],
        selected: Option<usize>,
        transparent: bool,
    ) -> WinResult<()> {
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
        }

        let pad = PADDING;
        let item_h = ITEM_HEIGHT;
        let sep_h = SEPARATOR_HEIGHT;
        let text_left = TEXT_LEFT;
        let mut y = pad;

        for (index, item) in items.iter().enumerate() {
            if item.is_separator() {
                let line_y = y + sep_h * 0.5;
                self.surface.line(
                    Vector2 {
                        X: pad + 8.0,
                        Y: line_y,
                    },
                    Vector2 {
                        X: width - pad - 8.0,
                        Y: line_y,
                    },
                    if theme.dark {
                        (1.0, 1.0, 1.0, 0.16)
                    } else {
                        (0.0, 0.0, 0.0, 0.12)
                    },
                    1.0,
                );
                y += sep_h;
                continue;
            }

            let item_rect = item_rect(y, width, item_h);
            if selected == Some(index) {
                self.surface.fill_rounded(
                    item_rect.rect.left,
                    item_rect.rect.top,
                    item_rect.rect.right - item_rect.rect.left,
                    item_h,
                    6.0,
                    if theme.dark {
                        (1.0, 1.0, 1.0, 0.10)
                    } else {
                        (0.0, 0.0, 0.0, 0.06)
                    },
                );
            }

            if item.is_checked() {
                let cx = pad + CHECK_CENTER;
                let cy = (item_rect.rect.top + item_rect.rect.bottom) * 0.5;
                let color = theme.foreground().with_alpha(1.0);
                self.surface.line(
                    Vector2 { X: cx - 5.0, Y: cy },
                    Vector2 {
                        X: cx - 1.0,
                        Y: cy + 4.0,
                    },
                    color,
                    1.8,
                );
                self.surface.line(
                    Vector2 {
                        X: cx - 1.0,
                        Y: cy + 4.0,
                    },
                    Vector2 {
                        X: cx + 6.0,
                        Y: cy - 5.0,
                    },
                    color,
                    1.8,
                );
            }

            let color = if item.is_enabled() {
                theme.foreground()
            } else {
                theme.muted()
            };
            self.surface.text(
                &self.text,
                item.label(),
                text_left,
                item_rect.rect.top,
                width - 10.0 - text_left,
                item_h,
                color.with_alpha(1.0),
            );
            y += item_h;
        }

        self.surface.end()
    }
}

fn item_rect(top: f32, width: f32, height: f32) -> D2D1_ROUNDED_RECT {
    D2D1_ROUNDED_RECT {
        rect: D2D_RECT_F {
            left: PADDING,
            top,
            right: width - PADDING,
            bottom: top + height,
        },
        radiusX: 6.0,
        radiusY: 6.0,
    }
}
