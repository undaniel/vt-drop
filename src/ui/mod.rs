pub mod animation;
pub mod dialog_render;
pub mod menu;
pub mod menu_render;
pub mod render;
pub mod surface;
pub mod tooltip;
pub mod window;

use crate::theme::Rgb;

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
}
