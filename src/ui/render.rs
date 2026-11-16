//! Direct3D 11 + Direct2D + DirectComposition renderer for a transparent,
//! DPI-aware floating window.
//!
//! The swap chain is allocated once at the maximum widget size. The window is
//! resized around its centre while the artwork stays centred in the surface;
//! DirectComposition clips it to the window. This keeps every animation frame
//! cheap and avoids recreating GPU resources while dragging or expanding.

use std::mem::ManuallyDrop;

use windows::core::{w, IUnknown, Interface, Result as WinResult, BOOL};
use windows::Win32::Foundation::{E_FAIL, HMODULE, HWND};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_BEZIER_SEGMENT, D2D1_COLOR_F, D2D1_FIGURE_BEGIN_FILLED, D2D1_FIGURE_BEGIN_HOLLOW, D2D_SIZE_F,
    D2D1_FIGURE_END_CLOSED, D2D1_FIGURE_END_OPEN, D2D1_GRADIENT_STOP, D2D1_PIXEL_FORMAT,
    D2D_RECT_F,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, D2D1_ARC_SEGMENT, D2D1_ARC_SIZE_LARGE, D2D1_ARC_SIZE_SMALL,
    D2D1_SWEEP_DIRECTION_CLOCKWISE, ID2D1Brush, ID2D1DeviceContext, ID2D1Factory, ID2D1Factory1,
    ID2D1GeometrySink, ID2D1GradientStopCollection, ID2D1PathGeometry, ID2D1RadialGradientBrush,
    ID2D1RenderTarget, ID2D1SolidColorBrush, ID2D1StrokeStyle, D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
    D2D1_BITMAP_OPTIONS_CANNOT_DRAW, D2D1_BITMAP_OPTIONS_TARGET, D2D1_BITMAP_PROPERTIES1,
    D2D1_BRUSH_PROPERTIES, D2D1_CAP_STYLE_FLAT, D2D1_CAP_STYLE_ROUND, D2D1_DASH_STYLE_SOLID,
    D2D1_DEVICE_CONTEXT_OPTIONS_NONE, D2D1_DRAW_TEXT_OPTIONS_CLIP, D2D1_ELLIPSE,
    D2D1_EXTEND_MODE_CLAMP, D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_GAMMA_2_2,
    D2D1_LINE_JOIN_ROUND, D2D1_RADIAL_GRADIENT_BRUSH_PROPERTIES, D2D1_ROUNDED_RECT,
    D2D1_STROKE_STYLE_PROPERTIES, D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE,
};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::DirectComposition::{
    DCompositionCreateDevice, IDCompositionDevice, IDCompositionTarget, IDCompositionVisual,
};
use windows::Win32::Graphics::DirectWrite::{
    DWriteCreateFactory, IDWriteFactory, IDWriteFontCollection, IDWriteTextFormat,
    DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL,
    DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_WEIGHT_SEMI_BOLD, DWRITE_MEASURING_MODE_NATURAL,
    DWRITE_PARAGRAPH_ALIGNMENT_CENTER, DWRITE_PARAGRAPH_ALIGNMENT_NEAR,
    DWRITE_TEXT_ALIGNMENT_CENTER, DWRITE_TEXT_ALIGNMENT_LEADING, DWRITE_WORD_WRAPPING_NO_WRAP,
    DWRITE_WORD_WRAPPING_WRAP,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_PREMULTIPLIED, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGIAdapter, IDXGIDevice, IDXGIFactory2, IDXGISurface, IDXGISwapChain1, DXGI_PRESENT,
    DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
    DXGI_USAGE_RENDER_TARGET_OUTPUT,
};
use windows_numerics::{Matrix3x2, Vector2};

use crate::theme::{Rgb, Theme};
use crate::ui::{DrawState, Scene};

/// The largest surface the widget ever needs, in DIPs.
pub const MAX_W: f32 = 368.0;
pub const MAX_H: f32 = 116.0;
/// Corner radius larger than any widget: `rounded` clamps it to a full pill.
const PILL: f32 = 999.0;

pub struct Renderer {
    _d3d_device: ID3D11Device,
    _d3d_context: ID3D11DeviceContext,
    _d2d_factory: ID2D1Factory1,
    _dwrite: IDWriteFactory,
    dcomp: IDCompositionDevice,
    _target: IDCompositionTarget,
    visual: IDCompositionVisual,
    swapchain: IDXGISwapChain1,
    context: ID2D1DeviceContext,
    dpi: f32,
    pixel_w: u32,
    pixel_h: u32,
    theme: Theme,

    shield: ID2D1PathGeometry,
    layout: (f32, f32, f32, f32),
    stroke_round: ID2D1StrokeStyle,
    brush: ID2D1SolidColorBrush,
    glow: ID2D1RadialGradientBrush,
    _glow_stops: ID2D1GradientStopCollection,

    text_small: IDWriteTextFormat,
    text_body: IDWriteTextFormat,
    text_title: IDWriteTextFormat,
    text_ratio: IDWriteTextFormat,
    text_badge: IDWriteTextFormat,
    text_percent: IDWriteTextFormat,
    text_warning: IDWriteTextFormat,
}

impl Renderer {
    pub fn new(hwnd: HWND, dpi: u32, theme: Theme) -> WinResult<Self> {
        let dpi = if dpi == 0 { 96 } else { dpi };
        let scale = dpi as f32 / 96.0;
        let pixel_w = (MAX_W * scale).ceil() as u32;
        let pixel_h = (MAX_H * scale).ceil() as u32;

        let (d3d_device, d3d_context) = create_d3d_device()?;
        let dxgi_device: IDXGIDevice = d3d_device.cast()?;
        let adapter: IDXGIAdapter = unsafe { dxgi_device.GetAdapter()? };
        let dxgi_factory: IDXGIFactory2 = unsafe { adapter.GetParent()? };

        let d2d_factory: ID2D1Factory1 =
            unsafe { D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)? };
        let d2d_device = unsafe { d2d_factory.CreateDevice(&dxgi_device)? };
        let context = unsafe { d2d_device.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)? };
        let dwrite: IDWriteFactory = unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)? };

        let swapchain = unsafe {
            dxgi_factory.CreateSwapChainForComposition(
                &d3d_device,
                &DXGI_SWAP_CHAIN_DESC1 {
                    Width: pixel_w,
                    Height: pixel_h,
                    Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    Stereo: BOOL(0),
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                    BufferCount: 2,
                    Scaling: DXGI_SCALING_STRETCH,
                    SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
                    AlphaMode: DXGI_ALPHA_MODE_PREMULTIPLIED,
                    Flags: 0,
                },
                None,
            )?
        };

        let surface: IDXGISurface = unsafe { swapchain.GetBuffer(0)? };
        let bitmap_props = D2D1_BITMAP_PROPERTIES1 {
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: dpi as f32,
            dpiY: dpi as f32,
            bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
            colorContext: ManuallyDrop::new(None),
        };
        let target_bitmap =
            unsafe { context.CreateBitmapFromDxgiSurface(&surface, Some(&bitmap_props))? };
        unsafe {
            context.SetTarget(&target_bitmap);
            context.SetDpi(dpi as f32, dpi as f32);
            context.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
            context.SetAntialiasMode(D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
        }

        let dcomp: IDCompositionDevice = unsafe { DCompositionCreateDevice(&dxgi_device)? };
        let target = unsafe { dcomp.CreateTargetForHwnd(hwnd, true)? };
        let visual = unsafe { dcomp.CreateVisual()? };
        let content: IUnknown = swapchain.cast()?;
        unsafe {
            visual.SetContent(&content)?;
            target.SetRoot(&visual)?;
            dcomp.Commit()?;
        }

        let shield = create_shield_geometry(&d2d_factory)?;
        let factory_base: &ID2D1Factory = &d2d_factory;
        let stroke_round: ID2D1StrokeStyle = unsafe {
            factory_base.CreateStrokeStyle(
                &D2D1_STROKE_STYLE_PROPERTIES {
                    startCap: D2D1_CAP_STYLE_ROUND,
                    endCap: D2D1_CAP_STYLE_ROUND,
                    dashCap: D2D1_CAP_STYLE_FLAT,
                    lineJoin: D2D1_LINE_JOIN_ROUND,
                    miterLimit: 1.0,
                    dashStyle: D2D1_DASH_STYLE_SOLID,
                    dashOffset: 0.0,
                },
                None,
            )?
        };
        let brush = unsafe {
            context.CreateSolidColorBrush(
                &D2D1_COLOR_F {
                    r: 1.0,
                    g: 1.0,
                    b: 1.0,
                    a: 1.0,
                },
                None,
            )?
        };
        let (glow_stops, glow) = create_glow_brush(&context, theme.accent)?;

        let text_small = make_text_format(&dwrite, "Segoe UI Variable Text", 10.0, false, false)?;
        let text_body = make_text_format(&dwrite, "Segoe UI Variable Text", 11.5, false, false)?;
        let text_title = make_text_format(&dwrite, "Segoe UI Variable Text", 13.0, true, false)?;
        let text_ratio =
            make_centered_text_format(&dwrite, "Segoe UI Variable Display", 20.0, true)?;
        let text_badge = make_badge_format(&dwrite, 9.0)?;
        let text_percent = make_badge_format(&dwrite, 13.0)?;
        let text_warning = make_text_format(&dwrite, "Segoe UI Variable Text", 10.0, false, true)?;

        let mut renderer = Self {
            _d3d_device: d3d_device,
            _d3d_context: d3d_context,
            _d2d_factory: d2d_factory,
            _dwrite: dwrite,
            dcomp,
            _target: target,
            visual,
            swapchain,
            context,
            dpi: dpi as f32,
            pixel_w,
            pixel_h,
            theme,
            shield,
            layout: (f32::NAN, 0.0, 0.0, 0.0),
            stroke_round,
            brush,
            glow,
            _glow_stops: glow_stops,
            text_small,
            text_body,
            text_title,
            text_ratio,
            text_badge,
            text_percent,
            text_warning,
        };
        renderer.set_layout(64.0, 64.0, 0.0, 0.0)?;
        renderer.render(&DrawState {
            scene: &Scene::Compact,
            theme,
            window_w: 64.0,
            window_h: 64.0,
            expand: 0.0,
            compact_alpha: 0.0,
            alpha: 1.0,
            hover_scale: 1.0,
            glow: 0.0,
            ring: None,
            label: None,
            badge: None,
            hovered: false,
            cancel_hint: false,
        })?;
        Ok(renderer)
    }

    pub fn set_theme(&mut self, theme: Theme) -> WinResult<()> {
        if self.theme.accent != theme.accent {
            let (stops, glow) = create_glow_brush(&self.context, theme.accent)?;
            self._glow_stops = stops;
            self.glow = glow;
        }
        self.theme = theme;
        Ok(())
    }

    /// Places the maximum-size surface so the artwork is centred in the
    /// window, shifted by `anchor` (DIPs). Only commits when something changed.
    pub fn set_layout(
        &mut self,
        width_dip: f32,
        height_dip: f32,
        anchor_x: f32,
        anchor_y: f32,
    ) -> WinResult<()> {
        let layout = (width_dip, height_dip, anchor_x, anchor_y);
        if layout == self.layout {
            return Ok(());
        }
        self.layout = layout;
        let width = width_dip.clamp(1.0, MAX_W);
        let height = height_dip.clamp(1.0, MAX_H);
        // DirectComposition visual offsets are physical pixels, not DIPs.
        let scale = self.dpi / 96.0;
        let offset_x = ((width - MAX_W) * 0.5 + anchor_x) * scale;
        let offset_y = ((height - MAX_H) * 0.5 + anchor_y) * scale;
        unsafe {
            self.visual.SetOffsetX2(offset_x)?;
            self.visual.SetOffsetY2(offset_y)?;
            self.dcomp.Commit()?;
        }
        Ok(())
    }

    pub fn render(&mut self, state: &DrawState<'_>) -> WinResult<()> {
        let alpha = state.alpha.clamp(0.0, 1.0);
        let expand = state.expand.clamp(0.0, 1.08);
        let center = Vector2 {
            X: MAX_W * 0.5,
            Y: MAX_H * 0.5,
        };
        let (x, y, w, h) = centered_rect(state.window_w, state.window_h);

        unsafe {
            self.context.BeginDraw();
            self.context.Clear(Some(&D2D1_COLOR_F {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 0.0,
            }));

            // Circle when compact, capsule when expanded: `rounded` clamps the
            // radius to half the short side, so the shape morphs on its own.
            // Inset half a pixel so the outline is not cut by the window edge.
            let (sx, sy, sw, sh) = (x + 0.5, y + 0.5, w - 1.0, h - 1.0);
            self.fill_rounded(sx, sy, sw, sh, PILL, state.theme.solid_background(), alpha);
            let outline = if state.hovered {
                state.theme.accent.with_alpha(0.85)
            } else {
                state.theme.border_tint()
            };
            self.stroke_rounded(sx, sy, sw, sh, PILL, outline, alpha);

            if state.compact_alpha > 0.001 {
                self.draw_shield(
                    center,
                    1.0,
                    state.theme.accent,
                    1.0,
                    state.compact_alpha * alpha,
                );
            }

            match state.scene {
                Scene::Compact => {
                    self.draw_compact(state, center, alpha);
                }
                Scene::Result {
                    ratio,
                    color,
                    caption,
                } => {
                    self.draw_panel(x, y, w, h, expand, alpha, Some(*color));
                    self.draw_result(
                        state,
                        x,
                        y,
                        w,
                        h,
                        expand,
                        alpha,
                        ratio,
                        *color,
                        caption.as_deref(),
                    );
                }
                Scene::AskUpload {
                    filename,
                    accept_hover,
                    reject_hover,
                } => {
                    self.draw_panel(x, y, w, h, expand, alpha, None);
                    self.draw_ask(
                        state,
                        x,
                        y,
                        w,
                        h,
                        expand,
                        alpha,
                        filename,
                        *accept_hover,
                        *reject_hover,
                    );
                }
                Scene::Error { title } => {
                    self.draw_panel(x, y, w, h, expand, alpha, None);
                    self.draw_error(state, x, y, w, h, expand, alpha, title);
                }
            }

            self.context.SetTransform(&Matrix3x2 {
                M11: 1.0,
                M12: 0.0,
                M21: 0.0,
                M22: 1.0,
                M31: 0.0,
                M32: 0.0,
            });
            self.context.EndDraw(None, None)?;
            self.swapchain.Present(1, DXGI_PRESENT(0)).ok()?;
        }
        Ok(())
    }

    pub fn pixel_size(&self) -> (u32, u32) {
        (self.pixel_w, self.pixel_h)
    }

    pub fn dpi(&self) -> f32 {
        self.dpi
    }

    fn draw_compact(&self, state: &DrawState<'_>, center: Vector2, alpha: f32) {
        let theme = state.theme;
        let scale = state.hover_scale.clamp(1.0, 1.2);
        let glow_alpha = (state.glow * 0.55 * alpha).clamp(0.0, 1.0);
        if glow_alpha > 0.001 {
            unsafe {
                self.glow.SetCenter(center);
                self.glow
                    .SetGradientOriginOffset(Vector2 { X: 0.0, Y: 0.0 });
                // Stays inside the 32-DIP circle even at hover scale.
                self.glow.SetRadiusX(26.0 * scale);
                self.glow.SetRadiusY(26.0 * scale);
                self.glow.SetOpacity(glow_alpha);
                let ellipse = D2D1_ELLIPSE {
                    point: center,
                    radiusX: 26.0 * scale,
                    radiusY: 26.0 * scale,
                };
                self.context.FillEllipse(&ellipse, &self.glow);
            }
        }

        if let Some(angle) = state.ring {
            self.draw_ring(center, scale * 1.02, angle, alpha);
        }

        match &state.label {
            _ if state.cancel_hint => {
                let arm = 6.0;
                let (from_a, to_a) = (
                    Vector2 { X: center.X - arm, Y: center.Y - arm },
                    Vector2 { X: center.X + arm, Y: center.Y + arm },
                );
                let (from_b, to_b) = (
                    Vector2 { X: center.X + arm, Y: center.Y - arm },
                    Vector2 { X: center.X - arm, Y: center.Y + arm },
                );
                unsafe {
                    self.set_brush(theme.foreground().with_alpha(0.9 * alpha));
                    self.context.DrawLine(from_a, to_a, &self.brush, 2.2, &self.stroke_round);
                    self.context.DrawLine(from_b, to_b, &self.brush, 2.2, &self.stroke_round);
                }
            }
            Some(label) => {
                let rect = D2D_RECT_F {
                    left: center.X - 22.0,
                    top: center.Y - 10.0,
                    right: center.X + 22.0,
                    bottom: center.Y + 10.0,
                };
                self.draw_text(label, &self.text_percent, rect, theme.foreground(), alpha);
            }
            None => self.draw_shield(center, scale, theme.accent, 1.0, alpha),
        }

        if let Some((index, total)) = state.badge {
            if total > 1 {
                self.draw_badge(state, index, total, alpha);
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_result(
        &self,
        state: &DrawState<'_>,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        expand: f32,
        alpha: f32,
        ratio: &str,
        color: Rgb,
        caption: Option<&str>,
    ) {
        let panel_alpha = expand.min(1.0) * alpha;
        if panel_alpha <= 0.001 {
            return;
        }
        let (text_alpha, slide) = content_fade(expand, alpha);
        let theme = state.theme;
        let shield_center = Vector2 {
            X: lerp(MAX_W * 0.5, x + 24.0, expand),
            Y: lerp(MAX_H * 0.5, y + h * 0.5, expand),
        };
        let shield_scale = lerp(1.0, 0.62, expand);
        self.draw_shield(shield_center, shield_scale, color, 1.0, panel_alpha);

        let text_rect = if caption.is_some() {
            D2D_RECT_F {
                left: x + 44.0 + slide,
                top: y + 3.0,
                right: x + w - 12.0,
                bottom: y + h - 18.0,
            }
        } else {
            D2D_RECT_F {
                left: x + 44.0 + slide,
                top: y,
                right: x + w - 12.0,
                bottom: y + h,
            }
        };
        self.draw_text(
            ratio,
            &self.text_ratio,
            text_rect,
            theme.foreground(),
            text_alpha,
        );

        if let Some(caption) = caption {
            let rect = D2D_RECT_F {
                left: x + 46.0 + slide,
                top: y + h - 20.0,
                right: x + w - 12.0,
                bottom: y + h - 5.0,
            };
            self.draw_text(
                caption,
                &self.text_small,
                rect,
                theme.secondary(),
                text_alpha,
            );
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_ask(
        &self,
        state: &DrawState<'_>,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        expand: f32,
        alpha: f32,
        filename: &str,
        accept_hover: bool,
        reject_hover: bool,
    ) {
        let panel_alpha = expand.min(1.0) * alpha;
        if panel_alpha <= 0.001 {
            return;
        }
        let (text_alpha, slide) = content_fade(expand, alpha);
        let theme = state.theme;
        let shield_center = Vector2 {
            X: lerp(MAX_W * 0.5, x + 26.0, expand),
            Y: lerp(MAX_H * 0.5, y + 40.0, expand),
        };
        let shield_scale = lerp(1.0, 0.70, expand);
        self.draw_shield(shield_center, shield_scale, theme.accent, 1.0, panel_alpha);

        let title_rect = D2D_RECT_F {
            left: x + 52.0 + slide,
            top: y + 14.0,
            right: x + w - 14.0,
            bottom: y + 32.0,
        };
        self.draw_text(
            "No está en VirusTotal",
            &self.text_title,
            title_rect,
            theme.foreground(),
            text_alpha,
        );

        let name = ellipsize(filename, 46);
        let name_rect = D2D_RECT_F {
            left: x + 52.0 + slide,
            top: y + 33.0,
            right: x + w - 14.0,
            bottom: y + 50.0,
        };
        self.draw_text(
            &name,
            &self.text_body,
            name_rect,
            theme.secondary(),
            text_alpha,
        );

        let warning_rect = D2D_RECT_F {
            left: x + 52.0 + slide,
            top: y + 56.0,
            right: x + w - 86.0,
            bottom: y + 86.0,
        };
        self.draw_text(
            "El archivo será visible para usuarios de VirusTotal.",
            &self.text_warning,
            warning_rect,
            theme.muted(),
            text_alpha,
        );

        let accept = Vector2 {
            X: x + w - 30.0,
            Y: y + h - 30.0,
        };
        let reject = Vector2 {
            X: x + w - 66.0,
            Y: y + h - 30.0,
        };
        self.draw_button(accept, true, accept_hover, text_alpha, theme);
        self.draw_button(reject, false, reject_hover, text_alpha, theme);
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_error(
        &self,
        state: &DrawState<'_>,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        expand: f32,
        alpha: f32,
        title: &str,
    ) {
        let panel_alpha = expand.min(1.0) * alpha;
        if panel_alpha <= 0.001 {
            return;
        }
        let (text_alpha, slide) = content_fade(expand, alpha);
        let theme = state.theme;
        let icon_center = Vector2 {
            X: lerp(MAX_W * 0.5, x + 25.0, expand),
            Y: lerp(MAX_H * 0.5, y + h * 0.5, expand),
        };
        let radius = lerp(15.0, 12.0, expand);
        self.draw_alert_icon(icon_center, radius, theme.grey(), panel_alpha);

        let text_rect = D2D_RECT_F {
            left: x + 46.0 + slide,
            top: y + 15.0,
            right: x + w - 12.0,
            bottom: y + h,
        };
        self.draw_text(
            title,
            &self.text_title,
            text_rect,
            theme.foreground(),
            text_alpha,
        );
    }

    /// Verdict tint over the capsule.
    fn draw_panel(
        &self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        expand: f32,
        alpha: f32,
        status: Option<Rgb>,
    ) {
        let a = expand.min(1.0) * alpha;
        if a <= 0.001 {
            return;
        }
        if let Some(status) = status {
            unsafe {
                self.set_brush(status.with_alpha(0.14 * a));
                let rr = rounded(x + 0.5, y + 0.5, w - 1.0, h - 1.0, PILL);
                self.context.FillRoundedRectangle(&rr, &self.brush);
                self.set_brush(status.with_alpha(0.50 * a));
                self.context
                    .DrawRoundedRectangle(&rr, &self.brush, 1.0, &self.stroke_round);
            }
        }
    }

    fn draw_shield(&self, center: Vector2, scale: f32, color: Rgb, fill_alpha: f32, alpha: f32) {
        let transform = Matrix3x2 {
            M11: scale,
            M12: 0.0,
            M21: 0.0,
            M22: scale,
            M31: center.X - 16.0 * scale,
            M32: center.Y - 19.0 * scale,
        };
        unsafe {
            self.context.SetTransform(&transform);
            self.set_brush(color.with_alpha(fill_alpha * 0.16 * alpha));
            self.context
                .FillGeometry(&self.shield, &self.brush, None::<&ID2D1Brush>);
            self.set_brush(color.with_alpha(alpha));
            self.context
                .DrawGeometry(&self.shield, &self.brush, 2.1, &self.stroke_round);
            self.context.SetTransform(&identity());
        }
    }

    fn draw_ring(&self, center: Vector2, scale: f32, (start, sweep): (f32, f32), alpha: f32) {
        let theme = self.theme;
        let radius = 27.5 * scale;
        unsafe {
            self.set_brush(theme.muted().with_alpha(0.20 * alpha));
            self.context.DrawEllipse(
                &D2D1_ELLIPSE {
                    point: center,
                    radiusX: radius,
                    radiusY: radius,
                },
                &self.brush,
                1.2,
                &self.stroke_round,
            );
            // ponytail: one small path per frame, only while busy.
            if let Ok(arc) = create_arc_geometry(&self._d2d_factory, center, radius, start, sweep) {
                self.set_brush(theme.accent.with_alpha(0.95 * alpha));
                self.context
                    .DrawGeometry(&arc, &self.brush, 2.2, &self.stroke_round);
            }
        }
    }

    fn draw_badge(
        &self,
        state: &DrawState<'_>,
        index: usize,
        total: usize,
        alpha: f32,
    ) {
        let (x, y, w, h) = centered_rect(state.window_w, state.window_h);
        let badge = D2D_RECT_F {
            left: x + w - 32.0,
            top: y + h - 20.0,
            right: x + w - 6.0,
            bottom: y + h - 5.0,
        };
        unsafe {
            let bg = if state.theme.dark {
                (0.06, 0.06, 0.06, 0.86 * alpha)
            } else {
                (0.98, 0.98, 0.98, 0.92 * alpha)
            };
            self.set_brush(bg);
            let rr = D2D1_ROUNDED_RECT {
                rect: badge,
                radiusX: 7.0,
                radiusY: 7.0,
            };
            self.context.FillRoundedRectangle(&rr, &self.brush);
            self.set_brush(state.theme.accent.with_alpha(0.75 * alpha));
            self.context
                .DrawRoundedRectangle(&rr, &self.brush, 1.0, &self.stroke_round);
            self.set_brush(state.theme.foreground().with_alpha(alpha));
            self.brush.SetOpacity(alpha);
        }
        let label = format!("{index}/{total}");
        self.draw_text(
            &label,
            &self.text_badge,
            badge,
            state.theme.foreground(),
            alpha,
        );
    }

    fn draw_button(&self, center: Vector2, accept: bool, hover: bool, alpha: f32, theme: Theme) {
        let color = if accept { theme.green() } else { theme.red() };
        let fill = if hover { 0.34 } else { 0.16 };
        unsafe {
            self.set_brush(color.with_alpha(fill * alpha));
            self.context.FillEllipse(
                &D2D1_ELLIPSE {
                    point: center,
                    radiusX: 14.0,
                    radiusY: 14.0,
                },
                &self.brush,
            );
            self.set_brush(color.with_alpha((if hover { 0.95 } else { 0.65 }) * alpha));
            self.context.DrawEllipse(
                &D2D1_ELLIPSE {
                    point: center,
                    radiusX: 14.0,
                    radiusY: 14.0,
                },
                &self.brush,
                1.2,
                &self.stroke_round,
            );
            self.set_brush(color.with_alpha(alpha));
            let width = if hover { 2.6 } else { 2.2 };
            if accept {
                self.context.DrawLine(
                    Vector2 {
                        X: center.X - 5.0,
                        Y: center.Y,
                    },
                    Vector2 {
                        X: center.X - 1.5,
                        Y: center.Y + 3.8,
                    },
                    &self.brush,
                    width,
                    &self.stroke_round,
                );
                self.context.DrawLine(
                    Vector2 {
                        X: center.X - 1.5,
                        Y: center.Y + 3.8,
                    },
                    Vector2 {
                        X: center.X + 5.2,
                        Y: center.Y - 4.0,
                    },
                    &self.brush,
                    width,
                    &self.stroke_round,
                );
            } else {
                self.context.DrawLine(
                    Vector2 {
                        X: center.X - 4.2,
                        Y: center.Y - 4.2,
                    },
                    Vector2 {
                        X: center.X + 4.2,
                        Y: center.Y + 4.2,
                    },
                    &self.brush,
                    width,
                    &self.stroke_round,
                );
                self.context.DrawLine(
                    Vector2 {
                        X: center.X + 4.2,
                        Y: center.Y - 4.2,
                    },
                    Vector2 {
                        X: center.X - 4.2,
                        Y: center.Y + 4.2,
                    },
                    &self.brush,
                    width,
                    &self.stroke_round,
                );
            }
        }
    }

    fn draw_alert_icon(&self, center: Vector2, radius: f32, color: Rgb, alpha: f32) {
        unsafe {
            self.set_brush(color.with_alpha(0.16 * alpha));
            self.context.FillEllipse(
                &D2D1_ELLIPSE {
                    point: center,
                    radiusX: radius,
                    radiusY: radius,
                },
                &self.brush,
            );
            self.set_brush(color.with_alpha(0.75 * alpha));
            self.context.DrawEllipse(
                &D2D1_ELLIPSE {
                    point: center,
                    radiusX: radius,
                    radiusY: radius,
                },
                &self.brush,
                1.3,
                &self.stroke_round,
            );
            self.context.DrawLine(
                Vector2 {
                    X: center.X,
                    Y: center.Y - radius * 0.48,
                },
                Vector2 {
                    X: center.X,
                    Y: center.Y + radius * 0.16,
                },
                &self.brush,
                2.4,
                &self.stroke_round,
            );
            self.context.FillEllipse(
                &D2D1_ELLIPSE {
                    point: Vector2 {
                        X: center.X,
                        Y: center.Y + radius * 0.46,
                    },
                    radiusX: 1.4,
                    radiusY: 1.4,
                },
                &self.brush,
            );
        }
    }

    fn draw_text(
        &self,
        text: &str,
        format: &IDWriteTextFormat,
        rect: D2D_RECT_F,
        color: Rgb,
        alpha: f32,
    ) {
        let utf16: Vec<u16> = text.encode_utf16().collect();
        unsafe {
            self.set_brush(color.with_alpha(alpha));
            self.context.DrawText(
                &utf16,
                format,
                &rect,
                &self.brush,
                D2D1_DRAW_TEXT_OPTIONS_CLIP,
                DWRITE_MEASURING_MODE_NATURAL,
            );
        }
    }

    fn fill_rounded(&self, x: f32, y: f32, w: f32, h: f32, radius: f32, color: Rgb, alpha: f32) {
        if alpha <= 0.001 {
            return;
        }
        unsafe {
            self.set_brush(color.with_alpha(alpha));
            let rr = rounded(x, y, w, h, radius);
            self.context.FillRoundedRectangle(&rr, &self.brush);
        }
    }

    fn stroke_rounded(
        &self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        radius: f32,
        color: (f32, f32, f32, f32),
        alpha: f32,
    ) {
        if alpha <= 0.001 {
            return;
        }
        unsafe {
            self.set_brush((color.0, color.1, color.2, color.3 * alpha));
            let rr = rounded(x, y, w, h, radius);
            self.context
                .DrawRoundedRectangle(&rr, &self.brush, 1.0, &self.stroke_round);
        }
    }

    fn set_brush(&self, color: (f32, f32, f32, f32)) {
        unsafe {
            self.brush.SetColor(&D2D1_COLOR_F {
                r: color.0,
                g: color.1,
                b: color.2,
                a: color.3,
            });
            self.brush.SetOpacity(1.0);
        }
    }
}

fn create_d3d_device() -> WinResult<(ID3D11Device, ID3D11DeviceContext)> {
    match try_create(D3D_DRIVER_TYPE_WARP) {
        Ok(value) => Ok(value),
        Err(_) => try_create(D3D_DRIVER_TYPE_HARDWARE),
    }
}

fn try_create(driver: D3D_DRIVER_TYPE) -> WinResult<(ID3D11Device, ID3D11DeviceContext)> {
    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;
    unsafe {
        D3D11CreateDevice(
            None::<&IDXGIAdapter>,
            driver,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )?;
    }
    let device = device.ok_or_else(|| windows::core::Error::from_hresult(E_FAIL))?;
    let context = context.ok_or_else(|| windows::core::Error::from_hresult(E_FAIL))?;
    Ok((device, context))
}

fn create_shield_geometry(factory: &ID2D1Factory1) -> WinResult<ID2D1PathGeometry> {
    let factory_base: &ID2D1Factory = factory;
    let geometry: ID2D1PathGeometry = unsafe { factory_base.CreatePathGeometry()? };
    let sink: ID2D1GeometrySink = unsafe { geometry.Open()? };
    unsafe {
        sink.BeginFigure(Vector2 { X: 16.0, Y: 1.5 }, D2D1_FIGURE_BEGIN_FILLED);
        sink.AddLine(Vector2 { X: 30.0, Y: 7.0 });
        sink.AddLine(Vector2 { X: 30.0, Y: 17.0 });
        sink.AddBezier(&D2D1_BEZIER_SEGMENT {
            point1: Vector2 { X: 30.0, Y: 27.0 },
            point2: Vector2 { X: 24.0, Y: 34.0 },
            point3: Vector2 { X: 16.0, Y: 37.0 },
        });
        sink.AddBezier(&D2D1_BEZIER_SEGMENT {
            point1: Vector2 { X: 8.0, Y: 34.0 },
            point2: Vector2 { X: 2.0, Y: 27.0 },
            point3: Vector2 { X: 2.0, Y: 17.0 },
        });
        sink.AddLine(Vector2 { X: 2.0, Y: 7.0 });
        sink.EndFigure(D2D1_FIGURE_END_CLOSED);
        sink.Close()?;
    }
    Ok(geometry)
}

fn create_arc_geometry(
    factory: &ID2D1Factory1,
    center: Vector2,
    radius: f32,
    start: f32,
    sweep: f32,
) -> WinResult<ID2D1PathGeometry> {
    let factory_base: &ID2D1Factory = factory;
    let geometry: ID2D1PathGeometry = unsafe { factory_base.CreatePathGeometry()? };
    let sink: ID2D1GeometrySink = unsafe { geometry.Open()? };
    let point = |angle: f32| Vector2 {
        X: center.X + radius * angle.cos(),
        Y: center.Y + radius * angle.sin(),
    };
    unsafe {
        sink.BeginFigure(point(start), D2D1_FIGURE_BEGIN_HOLLOW);
        sink.AddArc(&D2D1_ARC_SEGMENT {
            point: point(start + sweep),
            size: D2D_SIZE_F {
                width: radius,
                height: radius,
            },
            rotationAngle: 0.0,
            sweepDirection: D2D1_SWEEP_DIRECTION_CLOCKWISE,
            arcSize: if sweep > std::f32::consts::PI {
                D2D1_ARC_SIZE_LARGE
            } else {
                D2D1_ARC_SIZE_SMALL
            },
        });
        sink.EndFigure(D2D1_FIGURE_END_OPEN);
        sink.Close()?;
    }
    Ok(geometry)
}

fn create_glow_brush(
    context: &ID2D1DeviceContext,
    accent: Rgb,
) -> WinResult<(ID2D1GradientStopCollection, ID2D1RadialGradientBrush)> {
    let stops = [
        D2D1_GRADIENT_STOP {
            position: 0.0,
            color: D2D1_COLOR_F {
                r: accent.r as f32 / 255.0,
                g: accent.g as f32 / 255.0,
                b: accent.b as f32 / 255.0,
                a: 0.55,
            },
        },
        D2D1_GRADIENT_STOP {
            position: 1.0,
            color: D2D1_COLOR_F {
                r: accent.r as f32 / 255.0,
                g: accent.g as f32 / 255.0,
                b: accent.b as f32 / 255.0,
                a: 0.0,
            },
        },
    ];
    let render_target: &ID2D1RenderTarget = context;
    let collection = unsafe {
        render_target.CreateGradientStopCollection(
            &stops,
            D2D1_GAMMA_2_2,
            D2D1_EXTEND_MODE_CLAMP,
        )?
    };
    let brush = unsafe {
        context.CreateRadialGradientBrush(
            &D2D1_RADIAL_GRADIENT_BRUSH_PROPERTIES {
                center: Vector2 { X: 0.0, Y: 0.0 },
                gradientOriginOffset: Vector2 { X: 0.0, Y: 0.0 },
                radiusX: 35.0,
                radiusY: 35.0,
            },
            Some(&D2D1_BRUSH_PROPERTIES::default()),
            &collection,
        )?
    };
    Ok((collection, brush))
}

fn make_text_format(
    dwrite: &IDWriteFactory,
    family: &str,
    size: f32,
    semibold: bool,
    wrap: bool,
) -> WinResult<IDWriteTextFormat> {
    let family = windows::core::HSTRING::from(family);
    let format = unsafe {
        dwrite.CreateTextFormat(
            &family,
            None::<&IDWriteFontCollection>,
            if semibold {
                DWRITE_FONT_WEIGHT_SEMI_BOLD
            } else {
                DWRITE_FONT_WEIGHT_NORMAL
            },
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            size,
            w!("es-ES"),
        )?
    };
    unsafe {
        format.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_LEADING)?;
        format.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_NEAR)?;
        format.SetWordWrapping(if wrap {
            DWRITE_WORD_WRAPPING_WRAP
        } else {
            DWRITE_WORD_WRAPPING_NO_WRAP
        })?;
    }
    Ok(format)
}

fn make_centered_text_format(
    dwrite: &IDWriteFactory,
    family: &str,
    size: f32,
    semibold: bool,
) -> WinResult<IDWriteTextFormat> {
    let family = windows::core::HSTRING::from(family);
    let format = unsafe {
        dwrite.CreateTextFormat(
            &family,
            None::<&IDWriteFontCollection>,
            if semibold {
                DWRITE_FONT_WEIGHT_SEMI_BOLD
            } else {
                DWRITE_FONT_WEIGHT_NORMAL
            },
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            size,
            w!("es-ES"),
        )?
    };
    unsafe {
        format.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_LEADING)?;
        format.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
        format.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
    }
    Ok(format)
}

fn make_badge_format(dwrite: &IDWriteFactory, size: f32) -> WinResult<IDWriteTextFormat> {
    let family = windows::core::HSTRING::from("Segoe UI Variable Text");
    let format = unsafe {
        dwrite.CreateTextFormat(
            &family,
            None::<&IDWriteFontCollection>,
            DWRITE_FONT_WEIGHT_SEMI_BOLD,
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            size,
            w!("es-ES"),
        )?
    };
    unsafe {
        format.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
        format.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
        format.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
    }
    Ok(format)
}

fn centered_rect(width: f32, height: f32) -> (f32, f32, f32, f32) {
    let width = width.clamp(1.0, MAX_W);
    let height = height.clamp(1.0, MAX_H);
    ((MAX_W - width) * 0.5, (MAX_H - height) * 0.5, width, height)
}

fn rounded(x: f32, y: f32, w: f32, h: f32, radius: f32) -> D2D1_ROUNDED_RECT {
    D2D1_ROUNDED_RECT {
        rect: D2D_RECT_F {
            left: x,
            top: y,
            right: x + w,
            bottom: y + h,
        },
        radiusX: radius.min(w * 0.5).min(h * 0.5),
        radiusY: radius.min(w * 0.5).min(h * 0.5),
    }
}

fn identity() -> Matrix3x2 {
    Matrix3x2 {
        M11: 1.0,
        M12: 0.0,
        M21: 0.0,
        M22: 1.0,
        M31: 0.0,
        M32: 0.0,
    }
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

/// Text follows the panel: it fades in over the last 70 % of the expansion
/// and glides 8 DIPs into place. Returns `(alpha, x offset)`.
fn content_fade(expand: f32, alpha: f32) -> (f32, f32) {
    let t = ((expand - 0.3) / 0.7).clamp(0.0, 1.0);
    (t * alpha, (1.0 - t) * 8.0)
}

fn ellipsize(value: &str, max_chars: usize) -> String {
    let count = value.chars().count();
    if count <= max_chars {
        return value.to_owned();
    }
    let mut result: String = value.chars().take(max_chars.saturating_sub(1)).collect();
    result.push('…');
    result
}
