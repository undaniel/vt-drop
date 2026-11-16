pub mod animation;
pub mod composition;
pub mod menu;
pub mod render;
pub mod styles;
pub mod tooltip;
pub mod window;

use crate::theme::Rgb;

/// Which drawing backend the widget uses.
///
/// `Composition` is the native visual layer: DWM animates and clips the pill,
/// and `CreateHostBackdropBrush` gives real acrylic inside the shape. The
/// original Direct2D/DirectComposition renderer stays as a fallback for older
/// builds and for `VTDROP_RENDERER=legacy`.
// `App` owns exactly one renderer for its whole life and the enum is never
// moved in a hot path, so boxing the larger variant would only add a pointer
// hop to every frame.
#[allow(clippy::large_enum_variant)]
pub enum Renderer {
    Legacy(render::Renderer),
    Composition(composition::CompositionRenderer),
}

/// True when this OS can host a `Windows.UI.Composition` desktop target.
pub fn composition_supported(theme: &crate::theme::Theme) -> bool {
    if std::env::var_os("VTDROP_RENDERER")
        .map(|value| value.to_string_lossy().eq_ignore_ascii_case("legacy"))
        .unwrap_or(false)
    {
        return false;
    }
    // CompositionGeometricClip (the circle/capsule mask) needs 1903+; 2004 is
    // the oldest build where the Win32 composition host is reliable.
    theme.build >= 19_041
}

impl Renderer {
    pub fn new(
        hwnd: windows::Win32::Foundation::HWND,
        dpi: u32,
        theme: crate::theme::Theme,
        prefer_composition: bool,
        acrylic: bool,
    ) -> windows::core::Result<Self> {
        if prefer_composition {
            match composition::CompositionRenderer::new(hwnd, dpi, theme, acrylic) {
                Ok(renderer) => return Ok(Self::Composition(renderer)),
                Err(_) => {
                    // Fall back to the battle-tested swap chain backend.
                }
            }
        }
        Ok(Self::Legacy(render::Renderer::new(hwnd, dpi, theme)?))
    }

    pub fn is_composition(&self) -> bool {
        matches!(self, Self::Composition(_))
    }

    pub fn set_theme(&mut self, theme: crate::theme::Theme) -> windows::core::Result<()> {
        match self {
            Self::Legacy(renderer) => renderer.set_theme(theme),
            Self::Composition(renderer) => renderer.set_theme(theme),
        }
    }

    pub fn set_layout(
        &mut self,
        width_dip: f32,
        height_dip: f32,
        anchor_x: f32,
        anchor_y: f32,
    ) -> windows::core::Result<()> {
        match self {
            Self::Legacy(renderer) => {
                renderer.set_layout(width_dip, height_dip, anchor_x, anchor_y)
            }
            Self::Composition(renderer) => {
                renderer.set_layout(width_dip, height_dip, anchor_x, anchor_y)
            }
        }
    }

    pub fn render(&mut self, state: &DrawState<'_>) -> windows::core::Result<()> {
        match self {
            Self::Legacy(renderer) => renderer.render(state),
            Self::Composition(renderer) => renderer.render(state),
        }
    }

    pub fn pixel_size(&self) -> (u32, u32) {
        match self {
            Self::Legacy(renderer) => renderer.pixel_size(),
            Self::Composition(renderer) => renderer.pixel_size(),
        }
    }

    pub fn dpi(&self) -> f32 {
        match self {
            Self::Legacy(renderer) => renderer.dpi(),
            Self::Composition(renderer) => renderer.dpi(),
        }
    }

    /// The expand value the compositor is currently showing, for hit testing.
    pub fn animated_expand(&self) -> Option<f32> {
        match self {
            Self::Legacy(_) => None,
            Self::Composition(renderer) => Some(renderer.animated_expand()),
        }
    }

    pub fn is_animating(&self) -> bool {
        match self {
            Self::Legacy(_) => false,
            Self::Composition(renderer) => renderer.is_animating(),
        }
    }
}

/// The visual representation of the widget. This is intentionally independent
/// from the worker protocol so rendering cannot block on network state.
#[derive(Debug, Clone)]
pub enum Scene {
    /// A single shield (idle, drag-over, hashing, querying, uploading, polling).
    Compact,
    /// Expanded ratio pill.
    Result {
        ratio: String,
        color: Rgb,
        /// Present only for a multi-file summary ("peor de 5").
        caption: Option<String>,
    },
    /// 404 confirmation pill.
    AskUpload {
        filename: String,
        accept_hover: bool,
        reject_hover: bool,
    },
    /// Grey error pill. The full explanation lives in a native tooltip.
    Error { title: String },
}

impl Scene {
    /// Target window size in DIPs.
    pub fn target_size(&self) -> (f32, f32) {
        match self {
            Scene::Compact => (64.0, 64.0),
            Scene::Result { caption, .. } => {
                if caption.is_some() {
                    (208.0, 58.0)
                } else {
                    (176.0, 52.0)
                }
            }
            Scene::AskUpload { .. } => (368.0, 116.0),
            Scene::Error { .. } => (232.0, 52.0),
        }
    }

    pub fn is_expanded(&self) -> bool {
        !matches!(self, Scene::Compact)
    }
}

/// Everything the renderer needs for one frame.
pub struct DrawState<'a> {
    pub scene: &'a Scene,
    pub theme: crate::theme::Theme,
    /// Current pill size in DIPs (animated).
    pub window_w: f32,
    pub window_h: f32,
    /// 0 = compact shield, 1 = fully expanded pill.
    pub expand: f32,
    /// Compact shield fade during a collapse (0 normally).
    pub compact_alpha: f32,
    /// Global fade multiplier (used by the collapse fade).
    pub alpha: f32,
    /// Shield scale while a file is dragged over the widget.
    pub hover_scale: f32,
    /// Accent glow strength, 0..1.
    pub glow: f32,
    /// Progress ring `(start, sweep)` in radians, when a worker operation is active.
    pub ring: Option<(f32, f32)>,
    /// Short text replacing the shield while busy ("42%", "1:05").
    pub label: Option<String>,
    /// 1-based file counter, only while a batch is active.
    pub badge: Option<(usize, usize)>,
    /// Mouse is over the widget: the outline takes the accent colour.
    pub hovered: bool,
    /// Busy and hovered: a ✕ replaces the centre to say "click to cancel".
    pub cancel_hint: bool,
    /// A scan is running: the progress ring is visible.
    pub busy: bool,
    /// Determinate progress 0..1 for hashing/uploading, when known.
    pub progress: Option<f32>,
}
