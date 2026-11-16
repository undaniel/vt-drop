//! `Windows.UI.Composition` backend for the widget.
//!
//! The pill is a real composition visual tree: DWM owns the shape, the clip,
//! the backdrop and the animations. Only content that changes (text, verdict,
//! percentage) is redrawn, into `CompositionDrawingSurface`s, so idle CPU stays
//! at zero and expansion runs on the compositor thread at the monitor's
//! refresh rate.
//!
//! Coordinates are physical pixels: Windows.UI.Composition on Win32 works in
//! pixels, so every DIP is multiplied by `dpi / 96` on the way in.

use std::cell::RefCell;
use std::ffi::c_void;
use std::mem::size_of;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use windows::core::{w, Interface, Result as WinResult, HSTRING};
use windows::Foundation::{Size, TimeSpan, TypedEventHandler};
use windows::Graphics::DirectX::{DirectXAlphaMode, DirectXPixelFormat};
use windows::Win32::Foundation::{E_FAIL, HMODULE, HWND, POINT};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_BEZIER_SEGMENT, D2D1_COLOR_F, D2D1_FIGURE_BEGIN_FILLED, D2D1_FIGURE_BEGIN_HOLLOW,
    D2D1_FIGURE_END_CLOSED, D2D1_FIGURE_END_OPEN, D2D1_GRADIENT_STOP, D2D_RECT_F, D2D_SIZE_F,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, ID2D1Brush, ID2D1DeviceContext, ID2D1Factory, ID2D1Factory1,
    ID2D1GeometrySink, ID2D1PathGeometry, ID2D1RadialGradientBrush, ID2D1RenderTarget,
    ID2D1SolidColorBrush, ID2D1StrokeStyle, D2D1_ANTIALIAS_MODE_PER_PRIMITIVE, D2D1_ARC_SEGMENT,
    D2D1_ARC_SIZE_LARGE, D2D1_ARC_SIZE_SMALL, D2D1_BRUSH_PROPERTIES, D2D1_CAP_STYLE_FLAT,
    D2D1_CAP_STYLE_ROUND, D2D1_DASH_STYLE_SOLID, D2D1_DRAW_TEXT_OPTIONS_CLIP, D2D1_ELLIPSE,
    D2D1_EXTEND_MODE_CLAMP, D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_GAMMA_2_2,
    D2D1_LINE_JOIN_ROUND, D2D1_RADIAL_GRADIENT_BRUSH_PROPERTIES, D2D1_ROUNDED_RECT,
    D2D1_STROKE_STYLE_PROPERTIES, D2D1_SWEEP_DIRECTION_CLOCKWISE,
    D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE,
};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::DirectWrite::{
    DWriteCreateFactory, IDWriteFactory, IDWriteFontCollection, IDWriteTextFormat,
    DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL,
    DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_WEIGHT_SEMI_BOLD, DWRITE_MEASURING_MODE_NATURAL,
    DWRITE_PARAGRAPH_ALIGNMENT_CENTER, DWRITE_PARAGRAPH_ALIGNMENT_NEAR,
    DWRITE_TEXT_ALIGNMENT_CENTER, DWRITE_TEXT_ALIGNMENT_LEADING, DWRITE_WORD_WRAPPING_NO_WRAP,
    DWRITE_WORD_WRAPPING_WRAP,
};
use windows::Win32::Graphics::Dxgi::IDXGIAdapter;
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::System::WinRT::Composition::{
    ICompositionDrawingSurfaceInterop, ICompositorDesktopInterop, ICompositorInterop,
};
use windows::Win32::System::WinRT::{
    CreateDispatcherQueueController, DispatcherQueueOptions, DQTAT_COM_NONE, DQTYPE_THREAD_CURRENT,
};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;
use windows::UI::Color;
use windows::UI::Composition::Desktop::DesktopWindowTarget;
use windows::UI::Composition::{
    AnimationIterationBehavior, CompositionBackdropBrush, CompositionBatchCompletedEventArgs,
    CompositionBatchTypes, CompositionBrush, CompositionClip, CompositionColorBrush,
    CompositionDrawingSurface, CompositionEllipseGeometry, CompositionGeometricClip,
    CompositionGraphicsDevice, CompositionRoundedRectangleGeometry, CompositionScopedBatch,
    CompositionShape, CompositionStretch, CompositionStrokeCap, CompositionStrokeLineJoin,
    CompositionSurfaceBrush, Compositor, ContainerVisual, CubicBezierEasingFunction, ShapeVisual,
    SpriteVisual,
};
use windows_numerics::{Matrix3x2, Vector2, Vector3};

use crate::theme::{Rgb, Theme};
use crate::ui::animation::lerp;
use crate::ui::window;
use crate::ui::{DrawState, Scene};

/// Message posted when a scoped batch finishes. `WM_APP + 3`.
pub const WM_APP_ANIM_DONE: u32 = 0x8000 + 3;

/// WinUI `ControlNormalAnimationDuration`.
const NORMAL_MS: u64 = 250;
/// WinUI `ControlFastAnimationDuration`.
const FAST_MS: u64 = 167;

const MAX_W: f32 = 368.0;
const MAX_H: f32 = 116.0;
const COMPACT: f32 = 64.0;

const SHIELD_W: f32 = 34.0;
const SHIELD_H: f32 = 42.0;
/// Shield path centre inside its own surface, in DIPs.
const SHIELD_CX: f32 = 17.0;
const SHIELD_CY: f32 = 20.0;

const GLOW_SIZE: f32 = 54.0;
const RING_SIZE: f32 = 64.0;
const RING_RADIUS: f32 = 27.5;
const LABEL_W: f32 = 52.0;
const LABEL_H: f32 = 24.0;
const BADGE_W: f32 = 28.0;
const BADGE_H: f32 = 16.0;

thread_local! {
    /// One dispatcher queue per thread; creating a second one fails.
    // The initializer is already `const`; the lint cannot see through the WinRT
    // type, which has no `const` constructor.
    #[allow(clippy::missing_const_for_thread_local)]
    static DISPATCHER: RefCell<Option<windows::System::DispatcherQueueController>> =
        const { RefCell::new(None) };
}

fn ensure_dispatcher_queue() -> WinResult<()> {
    DISPATCHER.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            let options = DispatcherQueueOptions {
                dwSize: size_of::<DispatcherQueueOptions>() as u32,
                threadType: DQTYPE_THREAD_CURRENT,
                apartmentType: DQTAT_COM_NONE,
            };
            let controller = unsafe { CreateDispatcherQueueController(options)? };
            *slot = Some(controller);
        }
        Ok(())
    })
}

/// A premultiplied BGRA composition drawing surface with a D2D context.
struct Layer {
    surface: CompositionDrawingSurface,
    pixel_w: u32,
    pixel_h: u32,
    dpi: f32,
}

impl Layer {
    fn new(
        graphics: &CompositionGraphicsDevice,
        dpi: f32,
        width_dip: f32,
        height_dip: f32,
    ) -> WinResult<Self> {
        let scale = dpi / 96.0;
        let pixel_w = (width_dip * scale).ceil().max(1.0) as u32;
        let pixel_h = (height_dip * scale).ceil().max(1.0) as u32;
        let surface = graphics.CreateDrawingSurface(
            Size {
                Width: pixel_w as f32,
                Height: pixel_h as f32,
            },
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            DirectXAlphaMode::Premultiplied,
        )?;
        Ok(Self {
            surface,
            pixel_w,
            pixel_h,
            dpi,
        })
    }

    fn draw(
        &self,
        paint: impl FnOnce(&ID2D1DeviceContext, Matrix3x2) -> WinResult<()>,
    ) -> WinResult<()> {
        let interop: ICompositionDrawingSurfaceInterop = self.surface.cast()?;
        let mut offset = POINT::default();
        let update = windows::Win32::Foundation::RECT {
            left: 0,
            top: 0,
            right: self.pixel_w as i32,
            bottom: self.pixel_h as i32,
        };
        let context: ID2D1DeviceContext = unsafe { interop.BeginDraw(Some(&update), &mut offset)? };
        // The update lands at an atlas offset; draw through it so the content
        // stays at the surface's top-left.
        let scale = self.dpi / 96.0;
        let base = Matrix3x2 {
            M11: 1.0,
            M12: 0.0,
            M21: 0.0,
            M22: 1.0,
            M31: offset.x as f32 / scale,
            M32: offset.y as f32 / scale,
        };
        unsafe {
            context.SetDpi(self.dpi, self.dpi);
            context.SetTransform(&base);
            context.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
            context.SetAntialiasMode(D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
            context.Clear(Some(&transparent()));
        }
        let result = paint(&context, base);
        unsafe {
            // The composition interop already called BeginDraw on this context;
            // only its own EndDraw may be used.
            let _ = interop.EndDraw();
        }
        result
    }

    fn brush(&self, compositor: &Compositor) -> WinResult<CompositionBrush> {
        let brush: CompositionSurfaceBrush =
            compositor.CreateSurfaceBrushWithSurface(&self.surface)?;
        brush.SetStretch(CompositionStretch::Fill)?;
        brush.cast()
    }
}

#[derive(Debug, Clone, PartialEq)]
enum PanelKind {
    Result {
        ratio: String,
        color: Rgb,
        caption: Option<String>,
    },
    AskUpload {
        filename: String,
        accept_hover: bool,
        reject_hover: bool,
    },
    Error {
        title: String,
    },
}

pub struct CompositionRenderer {
    compositor: Compositor,
    _target: DesktopWindowTarget,
    _graphics: CompositionGraphicsDevice,
    _d3d_device: ID3D11Device,
    _d3d_context: ID3D11DeviceContext,
    _d2d_factory: ID2D1Factory1,
    _dwrite: IDWriteFactory,

    root: ContainerVisual,
    content: ContainerVisual,
    backdrop: SpriteVisual,
    tint: SpriteVisual,
    status: SpriteVisual,
    glow: SpriteVisual,
    ring_track: ShapeVisual,
    ring_arc: SpriteVisual,
    shield: SpriteVisual,
    label: SpriteVisual,
    panel: SpriteVisual,
    badge: SpriteVisual,
    border: ShapeVisual,

    pill_geometry: CompositionRoundedRectangleGeometry,
    border_geometry: CompositionRoundedRectangleGeometry,
    ring_geometry: CompositionEllipseGeometry,
    _pill_clip: CompositionClip,

    _backdrop_brush: CompositionBrush,
    tint_brush: CompositionColorBrush,
    status_brush: CompositionColorBrush,
    border_brush: CompositionColorBrush,
    ring_accent_brush: CompositionColorBrush,
    ring_track_brush: CompositionColorBrush,

    shield_layer: Layer,
    label_layer: Layer,
    panel_layer: Layer,
    badge_layer: Layer,
    glow_layer: Layer,
    arc_layer: Layer,

    shield_geometry: ID2D1PathGeometry,
    stroke_round: ID2D1StrokeStyle,
    text_small: IDWriteTextFormat,
    text_body: IDWriteTextFormat,
    text_title: IDWriteTextFormat,
    text_ratio: IDWriteTextFormat,
    text_badge: IDWriteTextFormat,
    text_percent: IDWriteTextFormat,
    text_warning: IDWriteTextFormat,

    theme: Theme,
    acrylic: bool,
    dpi: f32,
    scale: f32,
    hwnd: isize,
    // Layout is kept in DIPs; visuals are set in pixels.
    window_dip: (f32, f32),
    anchor: (f32, f32),
    layout: (f32, f32, f32, f32),
    scene_cache: Scene,
    expand: f32,
    animating: bool,
    animation_flag: Arc<AtomicBool>,
    batch: Option<CompositionScopedBatch>,
    batch_handler:
        Option<TypedEventHandler<windows::core::IInspectable, CompositionBatchCompletedEventArgs>>,

    // Content fingerprints: redraw only when they change.
    glyph_key: Option<(bool, Rgb)>,
    panel_key: Option<PanelKind>,
    label_key: Option<(Option<String>, bool)>,
    badge_key: Option<(usize, usize)>,
    hover_key: Option<bool>,

    // Indeterminate ring state.
    arc_progress: Option<f32>,
    arc_animating: bool,
}

impl CompositionRenderer {
    pub fn new(hwnd: HWND, dpi: u32, theme: Theme, acrylic: bool) -> WinResult<Self> {
        ensure_dispatcher_queue()?;
        let dpi = dpi.max(96) as f32;
        let scale = dpi / 96.0;

        let (d3d_device, d3d_context) = create_d3d_device()?;
        let dxgi_device: IDXGIDevice = d3d_device.cast()?;
        let d2d_factory: ID2D1Factory1 =
            unsafe { D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)? };
        let d2d_device = unsafe { d2d_factory.CreateDevice(&dxgi_device)? };
        let dwrite: IDWriteFactory = unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)? };

        let compositor = Compositor::new()?;
        let interop: ICompositorInterop = compositor.cast()?;
        // A Direct2D device (not the raw D3D one) makes BeginDraw return an
        // ID2D1DeviceContext instead of E_INVALIDARG.
        let graphics = unsafe { interop.CreateGraphicsDevice(&d2d_device)? };
        let desktop: ICompositorDesktopInterop = compositor.cast()?;
        let target: DesktopWindowTarget =
            unsafe { desktop.CreateDesktopWindowTarget(hwnd, false)? };

        let root = compositor.CreateContainerVisual()?;
        let content = compositor.CreateContainerVisual()?;
        root.Children()?.InsertAtTop(&content)?;
        target.SetRoot(&root)?;

        let backdrop = compositor.CreateSpriteVisual()?;
        let tint = compositor.CreateSpriteVisual()?;
        let status = compositor.CreateSpriteVisual()?;
        let glow = compositor.CreateSpriteVisual()?;
        let ring_track = compositor.CreateShapeVisual()?;
        let ring_arc = compositor.CreateSpriteVisual()?;
        let shield = compositor.CreateSpriteVisual()?;
        let label = compositor.CreateSpriteVisual()?;
        let panel = compositor.CreateSpriteVisual()?;
        let badge = compositor.CreateSpriteVisual()?;
        let border = compositor.CreateShapeVisual()?;

        let pill_geometry = compositor.CreateRoundedRectangleGeometry()?;
        let border_geometry = compositor.CreateRoundedRectangleGeometry()?;
        let ring_geometry = compositor.CreateEllipseGeometry()?;
        let geometric_clip: CompositionGeometricClip =
            compositor.CreateGeometricClipWithGeometry(&pill_geometry)?;
        let pill_clip: CompositionClip = geometric_clip.cast()?;
        content.SetClip(&pill_clip)?;

        // Backdrop: host backdrop for acrylic, a solid colour otherwise.
        let backdrop_brush: CompositionBrush = if acrylic {
            let host: CompositionBackdropBrush = compositor.CreateHostBackdropBrush()?;
            host.cast()?
        } else {
            let solid =
                compositor.CreateColorBrushWithColor(ui_color(theme.solid_background(), 1.0))?;
            solid.cast()?
        };
        backdrop.SetBrush(&backdrop_brush)?;

        let tint_brush = compositor.CreateColorBrushWithColor(ui_color(
            theme.solid_background(),
            if acrylic { 0.62 } else { 0.0 },
        ))?;
        tint.SetBrush(&tint_brush)?;

        let status_brush = compositor.CreateColorBrushWithColor(ui_color(theme.green(), 0.14))?;
        status.SetBrush(&status_brush)?;
        status.SetOpacity(0.0)?;

        let border_brush = compositor.CreateColorBrushWithColor(ui_rgba(theme.border_tint()))?;
        let ring_accent_brush =
            compositor.CreateColorBrushWithColor(ui_color(theme.accent, 0.95))?;
        let ring_track_brush =
            compositor.CreateColorBrushWithColor(ui_color(theme.muted(), 0.20))?;

        // Backdrop visuals fill the whole window and are clipped by the pill
        // geometry, so animating the clip is enough to morph circle <-> capsule.
        content.Children()?.InsertAtTop(&backdrop)?;
        content.Children()?.InsertAtTop(&tint)?;
        content.Children()?.InsertAtTop(&status)?;
        content.Children()?.InsertAtTop(&glow)?;
        content.Children()?.InsertAtTop(&ring_track)?;
        content.Children()?.InsertAtTop(&ring_arc)?;
        content.Children()?.InsertAtTop(&shield)?;
        content.Children()?.InsertAtTop(&label)?;
        content.Children()?.InsertAtTop(&panel)?;
        content.Children()?.InsertAtTop(&badge)?;
        // The outline sits outside the clip so its stroke is not cut in half.
        root.Children()?.InsertAtTop(&border)?;

        let border_shape = compositor.CreateSpriteShape()?;
        border_shape.SetGeometry(&border_geometry)?;
        let border_base: CompositionBrush = border_brush.cast()?;
        border_shape.SetStrokeBrush(&border_base)?;
        border_shape.SetStrokeThickness(scale)?;
        border_shape.SetStrokeStartCap(CompositionStrokeCap::Round)?;
        border_shape.SetStrokeEndCap(CompositionStrokeCap::Round)?;
        border_shape.SetStrokeLineJoin(CompositionStrokeLineJoin::Round)?;
        border_shape.SetFillBrush(None::<&CompositionBrush>)?;
        let shape: CompositionShape = border_shape.cast()?;
        border.Shapes()?.Append(&shape)?;

        let track_shape = compositor.CreateSpriteShape()?;
        track_shape.SetGeometry(&ring_geometry)?;
        let track_base: CompositionBrush = ring_track_brush.cast()?;
        track_shape.SetStrokeBrush(&track_base)?;
        track_shape.SetStrokeThickness(1.2 * scale)?;
        track_shape.SetStrokeStartCap(CompositionStrokeCap::Round)?;
        track_shape.SetStrokeEndCap(CompositionStrokeCap::Round)?;
        track_shape.SetFillBrush(None::<&CompositionBrush>)?;
        let track: CompositionShape = track_shape.cast()?;
        ring_track.Shapes()?.Append(&track)?;

        let shield_layer = Layer::new(&graphics, dpi, SHIELD_W, SHIELD_H)?;
        let label_layer = Layer::new(&graphics, dpi, LABEL_W, LABEL_H)?;
        let panel_layer = Layer::new(&graphics, dpi, MAX_W, MAX_H)?;
        let badge_layer = Layer::new(&graphics, dpi, BADGE_W, BADGE_H)?;
        let glow_layer = Layer::new(&graphics, dpi, GLOW_SIZE, GLOW_SIZE)?;
        let arc_layer = Layer::new(&graphics, dpi, RING_SIZE, RING_SIZE)?;

        shield.SetBrush(&shield_layer.brush(&compositor)?)?;
        label.SetBrush(&label_layer.brush(&compositor)?)?;
        panel.SetBrush(&panel_layer.brush(&compositor)?)?;
        badge.SetBrush(&badge_layer.brush(&compositor)?)?;
        glow.SetBrush(&glow_layer.brush(&compositor)?)?;
        ring_arc.SetBrush(&arc_layer.brush(&compositor)?)?;

        let shield_geometry = create_shield_geometry(&d2d_factory)?;
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

        let text_small = make_text_format(&dwrite, "Segoe UI Variable Text", 10.0, false, false)?;
        let text_body = make_text_format(&dwrite, "Segoe UI Variable Text", 11.5, false, false)?;
        let text_title = make_text_format(&dwrite, "Segoe UI Variable Text", 13.0, true, false)?;
        let text_ratio =
            make_centered_text_format(&dwrite, "Segoe UI Variable Display", 20.0, true)?;
        let text_badge = make_badge_format(&dwrite, 9.0)?;
        let text_percent = make_badge_format(&dwrite, 13.0)?;
        let text_warning = make_text_format(&dwrite, "Segoe UI Variable Text", 10.0, false, true)?;

        let mut renderer = Self {
            compositor,
            _target: target,
            _graphics: graphics,
            _d3d_device: d3d_device,
            _d3d_context: d3d_context,
            _d2d_factory: d2d_factory,
            _dwrite: dwrite,
            root,
            content,
            backdrop,
            tint,
            status,
            glow,
            ring_track,
            ring_arc,
            shield,
            label,
            panel,
            badge,
            border,
            pill_geometry,
            border_geometry,
            ring_geometry,
            _pill_clip: pill_clip,
            _backdrop_brush: backdrop_brush,
            tint_brush,
            status_brush,
            border_brush,
            ring_accent_brush,
            ring_track_brush,
            shield_layer,
            label_layer,
            panel_layer,
            badge_layer,
            glow_layer,
            arc_layer,
            shield_geometry,
            stroke_round,
            text_small,
            text_body,
            text_title,
            text_ratio,
            text_badge,
            text_percent,
            text_warning,
            theme,
            acrylic,
            dpi,
            scale,
            hwnd: hwnd.0 as isize,
            window_dip: (COMPACT, COMPACT),
            anchor: (0.0, 0.0),
            layout: (f32::NAN, 0.0, 0.0, 0.0),
            scene_cache: Scene::Compact,
            expand: 0.0,
            animating: false,
            animation_flag: Arc::new(AtomicBool::new(false)),
            batch: None,
            batch_handler: None,
            glyph_key: None,
            panel_key: None,
            label_key: None,
            badge_key: None,
            hover_key: None,
            arc_progress: None,
            arc_animating: false,
        };
        renderer.sync_window()?;
        renderer.apply_frame(0.0)?;
        renderer.draw_glyph(false, theme.accent)?;
        renderer.draw_glow()?;
        renderer.draw_panel(None)?;
        renderer.draw_label(None, false)?;
        renderer.draw_badge(None)?;
        renderer.set_visibility(false, false, false, None, false, None)?;
        window::set_host_backdrop(hwnd, acrylic);
        Ok(renderer)
    }

    pub fn dpi(&self) -> f32 {
        self.dpi
    }

    pub fn pixel_size(&self) -> (u32, u32) {
        (
            (self.window_dip.0 * self.scale).ceil() as u32,
            (self.window_dip.1 * self.scale).ceil() as u32,
        )
    }

    /// Logical expand target (0 or 1). Hit testing uses it directly.
    pub fn animated_expand(&self) -> f32 {
        self.expand
    }

    /// True while a composition batch is still running.
    pub fn is_animating(&self) -> bool {
        self.animating && !self.animation_flag.load(Ordering::Acquire)
    }

    pub fn set_theme(&mut self, theme: Theme) -> WinResult<()> {
        let accent_changed = theme.accent != self.theme.accent;
        let background_changed = theme.dark != self.theme.dark;
        self.theme = theme;
        self.tint_brush.SetColor(ui_color(
            theme.solid_background(),
            if self.acrylic { 0.62 } else { 0.0 },
        ))?;
        self.ring_accent_brush
            .SetColor(ui_color(theme.accent, 0.95))?;
        self.ring_track_brush
            .SetColor(ui_color(theme.muted(), 0.20))?;
        self.border_brush.SetColor(ui_rgba(theme.border_tint()))?;
        self.glyph_key = None;
        self.panel_key = None;
        self.label_key = None;
        self.badge_key = None;
        if accent_changed || background_changed {
            self.draw_glow()?;
            self.arc_progress = None;
        }
        Ok(())
    }

    /// Anchor is the compact centre's offset from the window centre, in DIPs,
    /// *without* the animation factor: the compositor scales it by `1 - e`.
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
        self.window_dip = (width_dip.max(1.0), height_dip.max(1.0));
        self.anchor = (anchor_x, anchor_y);
        self.sync_window()?;
        if !self.animating {
            self.apply_frame(self.expand)?;
        }
        Ok(())
    }

    pub fn render(&mut self, state: &DrawState<'_>) -> WinResult<()> {
        self.scene_cache = state.scene.clone();
        let target = state.expand.clamp(0.0, 1.0);
        let animations = state.theme.animations_enabled;

        if self.animation_flag.load(Ordering::Acquire) && self.animating {
            self.animation_flag.store(false, Ordering::Release);
            self.animating = false;
            self.batch = None;
            self.batch_handler = None;
            self.apply_frame(self.expand)?;
        }

        if (target - self.expand).abs() > 0.001 {
            if animations {
                if self.animate_to(target).is_err() {
                    let _ = self.stop_animations();
                    self.apply_frame(target)?;
                }
            } else {
                self.stop_animations()?;
                self.apply_frame(target)?;
            }
            self.expand = target;
        } else if !self.animating {
            self.apply_frame(self.expand)?;
        }

        self.update_content(state)?;
        self.update_ring(state.busy, state.progress)?;
        self.set_visibility(
            state.scene.is_expanded(),
            matches!(state.scene, Scene::Result { .. }),
            state.busy,
            state.label.as_deref(),
            state.cancel_hint,
            state.badge,
        )?;
        self.update_hover(state.hovered)?;
        self.update_status(state.scene)?;
        Ok(())
    }

    fn sync_window(&mut self) -> WinResult<()> {
        let width = self.window_dip.0 * self.scale;
        let height = self.window_dip.1 * self.scale;
        let size = Vector2 {
            X: width,
            Y: height,
        };
        self.root.SetSize(size)?;
        self.content.SetSize(size)?;
        self.backdrop.SetSize(size)?;
        self.tint.SetSize(size)?;
        self.status.SetSize(size)?;
        self.border.SetSize(size)?;
        self.ring_track.SetSize(size)?;
        self.panel.SetSize(Vector2 {
            X: MAX_W * self.scale,
            Y: MAX_H * self.scale,
        })?;
        Ok(())
    }

    /// Geometry for the pill when `e` goes 0 (compact circle) -> 1 (scene pill).
    fn frame(&self, e: f32, scene: &Scene) -> Frame {
        frame_at(self.window_dip, self.anchor, e, scene)
    }

    /// The pill and border geometry at `frame`. Kept separate so an animation
    /// failure can snap just the shape without disturbing the content visuals.
    fn apply_geometry(&mut self, frame: &Frame) -> WinResult<()> {
        let s = self.scale;
        let w = frame.w * s;
        let h = frame.h * s;
        let x = frame.x * s;
        let y = frame.y * s;
        let radius = (w.min(h) * 0.5).max(0.0);
        self.pill_geometry.SetSize(Vector2 {
            X: w.max(1.0),
            Y: h.max(1.0),
        })?;
        self.pill_geometry.SetOffset(Vector2 { X: x, Y: y })?;
        self.pill_geometry.SetCornerRadius(Vector2 {
            X: radius,
            Y: radius,
        })?;

        let inset = 0.5 * s;
        self.border_geometry.SetSize(Vector2 {
            X: (w - inset * 2.0).max(1.0),
            Y: (h - inset * 2.0).max(1.0),
        })?;
        self.border_geometry.SetOffset(Vector2 {
            X: x + inset,
            Y: y + inset,
        })?;
        self.border_geometry.SetCornerRadius(Vector2 {
            X: (radius - inset).max(0.0),
            Y: (radius - inset).max(0.0),
        })?;
        Ok(())
    }

    fn apply_frame(&mut self, e: f32) -> WinResult<()> {
        let scene = self.scene_cache.clone();
        let frame = self.frame(e, &scene);
        self.apply_geometry(&frame)?;
        let s = self.scale;
        let w = frame.w * s;
        let h = frame.h * s;
        let x = frame.x * s;
        let y = frame.y * s;

        let cx = frame.cx * s;
        let cy = frame.cy * s;
        let ring_radius = RING_RADIUS * s;
        self.ring_geometry.SetCenter(Vector2 { X: cx, Y: cy })?;
        self.ring_geometry.SetRadius(Vector2 {
            X: ring_radius,
            Y: ring_radius,
        })?;

        let glow = GLOW_SIZE * s;
        self.glow.SetSize(Vector2 { X: glow, Y: glow })?;
        self.glow.SetOffset(Vector3 {
            X: cx - glow * 0.5,
            Y: cy - glow * 0.5,
            Z: 0.0,
        })?;
        self.glow.SetCenterPoint(Vector3 {
            X: cx,
            Y: cy,
            Z: 0.0,
        })?;

        let arc = RING_SIZE * s;
        self.ring_arc.SetSize(Vector2 { X: arc, Y: arc })?;
        self.ring_arc.SetOffset(Vector3 {
            X: cx - arc * 0.5,
            Y: cy - arc * 0.5,
            Z: 0.0,
        })?;
        self.ring_arc.SetCenterPoint(Vector3 {
            X: cx,
            Y: cy,
            Z: 0.0,
        })?;

        // Glyph: compact shield in the middle, smaller and left when expanded.
        let (glyph_cx, glyph_cy, glyph_scale) = self.glyph_layout(e, &scene, &frame);
        let shield_w = SHIELD_W * s;
        let shield_h = SHIELD_H * s;
        self.shield.SetSize(Vector2 {
            X: shield_w,
            Y: shield_h,
        })?;
        self.shield.SetOffset(Vector3 {
            X: glyph_cx * s - SHIELD_CX * s,
            Y: glyph_cy * s - SHIELD_CY * s,
            Z: 0.0,
        })?;
        self.shield.SetCenterPoint(Vector3 {
            X: glyph_cx * s,
            Y: glyph_cy * s,
            Z: 0.0,
        })?;
        self.shield.SetScale(Vector3 {
            X: glyph_scale,
            Y: glyph_scale,
            Z: 1.0,
        })?;

        self.label.SetSize(Vector2 {
            X: LABEL_W * s,
            Y: LABEL_H * s,
        })?;
        self.label.SetOffset(Vector3 {
            X: cx - LABEL_W * s * 0.5,
            Y: cy - LABEL_H * s * 0.5,
            Z: 0.0,
        })?;

        self.panel.SetOffset(Vector3 { X: x, Y: y, Z: 0.0 })?;

        let badge_w = BADGE_W * s;
        let badge_h = BADGE_H * s;
        self.badge.SetSize(Vector2 {
            X: badge_w,
            Y: badge_h,
        })?;
        self.badge.SetOffset(Vector3 {
            X: x + w - badge_w - 6.0 * s,
            Y: y + h - badge_h - 5.0 * s,
            Z: 0.0,
        })?;
        Ok(())
    }

    fn glyph_layout(&self, e: f32, scene: &Scene, frame: &Frame) -> (f32, f32, f32) {
        glyph_at(e, scene, frame)
    }

    fn animate_to(&mut self, target: f32) -> WinResult<()> {
        let scene = self.scene_cache.clone();
        let duration = if target > self.expand {
            NORMAL_MS
        } else {
            FAST_MS
        };
        let from = self.frame(self.expand, &scene);
        let to = self.frame(target, &scene);
        let s = self.scale;
        let easing = self.compositor.CreateCubicBezierEasingFunction(
            Vector2 { X: 0.0, Y: 0.0 },
            Vector2 { X: 0.0, Y: 1.0 },
        )?;
        let span = TimeSpan {
            Duration: (duration * 10_000) as i64,
        };

        let batch = match self
            .compositor
            .CreateScopedBatch(CompositionBatchTypes::Animation)
        {
            Ok(batch) => batch,
            Err(_) => {
                self.apply_frame(target)?;
                return Ok(());
            }
        };
        self.animation_flag.store(false, Ordering::Release);
        self.animating = true;

        let from_radius = (from.w.min(from.h) * s * 0.5).max(0.0);
        let to_radius = (to.w.min(to.h) * s * 0.5).max(0.0);
        let from_size = Vector2 {
            X: (from.w * s).max(1.0),
            Y: (from.h * s).max(1.0),
        };
        let to_size = Vector2 {
            X: (to.w * s).max(1.0),
            Y: (to.h * s).max(1.0),
        };
        let from_offset = Vector2 {
            X: from.x * s,
            Y: from.y * s,
        };
        let to_offset = Vector2 {
            X: to.x * s,
            Y: to.y * s,
        };

        let geometry = (|| -> WinResult<()> {
            animate_vector2(
                &self.pill_geometry,
                "Size",
                from_size,
                to_size,
                span,
                &easing,
            )?;
            animate_vector2(
                &self.pill_geometry,
                "Offset",
                from_offset,
                to_offset,
                span,
                &easing,
            )?;
            animate_vector2(
                &self.pill_geometry,
                "CornerRadius",
                Vector2 {
                    X: from_radius,
                    Y: from_radius,
                },
                Vector2 {
                    X: to_radius,
                    Y: to_radius,
                },
                span,
                &easing,
            )?;

            let inset = 0.5 * s;
            animate_vector2(
                &self.border_geometry,
                "Size",
                Vector2 {
                    X: (from_size.X - inset * 2.0).max(1.0),
                    Y: (from_size.Y - inset * 2.0).max(1.0),
                },
                Vector2 {
                    X: (to_size.X - inset * 2.0).max(1.0),
                    Y: (to_size.Y - inset * 2.0).max(1.0),
                },
                span,
                &easing,
            )?;
            animate_vector2(
                &self.border_geometry,
                "Offset",
                Vector2 {
                    X: from_offset.X + inset,
                    Y: from_offset.Y + inset,
                },
                Vector2 {
                    X: to_offset.X + inset,
                    Y: to_offset.Y + inset,
                },
                span,
                &easing,
            )?;
            animate_vector2(
                &self.border_geometry,
                "CornerRadius",
                Vector2 {
                    X: (from_radius - inset).max(0.0),
                    Y: (from_radius - inset).max(0.0),
                },
                Vector2 {
                    X: (to_radius - inset).max(0.0),
                    Y: (to_radius - inset).max(0.0),
                },
                span,
                &easing,
            )?;
            Ok(())
        })();
        if geometry.is_err() {
            // Some builds do not expose animatable geometry properties; snap
            // the shape and let the content animations carry the transition.
            let _ = self.apply_geometry(&to);
        }

        // Ring, glow, shield, label and badge follow the moving centre.
        animate_centered(&self.glow, &from, &to, s, span, &easing)?;
        animate_centered(&self.ring_arc, &from, &to, s, span, &easing)?;
        animate_centered(&self.label, &from, &to, s, span, &easing)?;

        let (from_gx, from_gy, from_gs) = self.glyph_layout(self.expand, &scene, &from);
        let (to_gx, to_gy, to_gs) = self.glyph_layout(target, &scene, &to);
        animate_vector3(
            &self.shield,
            "Offset",
            Vector3 {
                X: from_gx * s - SHIELD_CX * s,
                Y: from_gy * s - SHIELD_CY * s,
                Z: 0.0,
            },
            Vector3 {
                X: to_gx * s - SHIELD_CX * s,
                Y: to_gy * s - SHIELD_CY * s,
                Z: 0.0,
            },
            span,
            &easing,
        )?;
        animate_vector3(
            &self.shield,
            "Scale",
            Vector3 {
                X: from_gs,
                Y: from_gs,
                Z: 1.0,
            },
            Vector3 {
                X: to_gs,
                Y: to_gs,
                Z: 1.0,
            },
            span,
            &easing,
        )?;

        animate_vector3(
            &self.panel,
            "Offset",
            Vector3 {
                X: from.x * s + 8.0 * s,
                Y: from.y * s,
                Z: 0.0,
            },
            Vector3 {
                X: to.x * s,
                Y: to.y * s,
                Z: 0.0,
            },
            span,
            &easing,
        )?;
        let (from_panel, to_panel) = if target > self.expand {
            (0.0, 1.0)
        } else {
            (1.0, 0.0)
        };
        animate_scalar(&self.panel, "Opacity", from_panel, to_panel, span, &easing)?;
        animate_scalar(&self.status, "Opacity", from_panel, to_panel, span, &easing)?;

        batch.End()?;
        self.batch = Some(batch);
        self.attach_batch_handler()?;
        Ok(())
    }

    fn attach_batch_handler(&mut self) -> WinResult<()> {
        let Some(batch) = &self.batch else {
            return Ok(());
        };
        let flag = self.animation_flag.clone();
        let hwnd = self.hwnd;
        let handler = TypedEventHandler::new(move |_, _| {
            flag.store(true, Ordering::Release);
            if hwnd != 0 {
                unsafe {
                    let _ = PostMessageW(
                        Some(HWND(hwnd as *mut c_void)),
                        WM_APP_ANIM_DONE,
                        windows::Win32::Foundation::WPARAM(0),
                        windows::Win32::Foundation::LPARAM(0),
                    );
                }
            }
            Ok(())
        });
        batch.Completed(&handler)?;
        self.batch_handler = Some(handler);
        Ok(())
    }

    fn stop_animations(&mut self) -> WinResult<()> {
        self.animation_flag.store(false, Ordering::Release);
        self.animating = false;
        self.batch = None;
        self.batch_handler = None;
        for visual in [
            &self.panel,
            &self.status,
            &self.glow,
            &self.ring_arc,
            &self.label,
            &self.badge,
        ] {
            for property in ["Offset", "Opacity", "Scale"] {
                let _ = visual.StopAnimation(&HSTRING::from(property));
            }
        }
        let _ = self.shield.StopAnimation(&HSTRING::from("Offset"));
        let _ = self.shield.StopAnimation(&HSTRING::from("Scale"));
        for geometry in [&self.pill_geometry, &self.border_geometry] {
            for property in ["Size", "Offset", "CornerRadius"] {
                let _ = geometry.StopAnimation(&HSTRING::from(property));
            }
        }
        Ok(())
    }

    fn update_hover(&mut self, hovered: bool) -> WinResult<()> {
        if self.hover_key == Some(hovered) {
            return Ok(());
        }
        self.hover_key = Some(hovered);
        let target = if hovered { 1.0 } else { 0.0 };
        if !self.theme.animations_enabled {
            self.glow.SetOpacity(target)?;
            return Ok(());
        }
        let easing = self.compositor.CreateCubicBezierEasingFunction(
            Vector2 { X: 0.0, Y: 0.0 },
            Vector2 { X: 0.0, Y: 1.0 },
        )?;
        let span = TimeSpan {
            Duration: (FAST_MS * 10_000) as i64,
        };
        let from = self.glow.Opacity().unwrap_or(0.0);
        animate_scalar(&self.glow, "Opacity", from, target, span, &easing)?;
        Ok(())
    }

    fn update_status(&mut self, scene: &Scene) -> WinResult<()> {
        if let Scene::Result { color, .. } = scene {
            self.status_brush.SetColor(ui_color(*color, 0.14))?;
            self.border_brush.SetColor(ui_color(*color, 0.50))?;
        } else {
            self.border_brush
                .SetColor(ui_rgba(self.theme.border_tint()))?;
        }
        Ok(())
    }

    fn set_visibility(
        &mut self,
        expanded: bool,
        status_visible: bool,
        busy: bool,
        label: Option<&str>,
        cancel_hint: bool,
        badge: Option<(usize, usize)>,
    ) -> WinResult<()> {
        self.panel.SetIsVisible(expanded)?;
        self.status.SetIsVisible(expanded && status_visible)?;
        let show_ring = busy && !expanded;
        self.ring_track.SetIsVisible(show_ring)?;
        self.ring_arc.SetIsVisible(show_ring)?;
        let show_label = busy && !expanded && (label.is_some() || cancel_hint);
        self.label.SetIsVisible(show_label)?;
        self.shield.SetIsVisible(!show_label)?;
        let show_badge = badge.map(|(_, total)| total > 1).unwrap_or(false);
        self.badge.SetIsVisible(show_badge)?;
        Ok(())
    }

    fn update_content(&mut self, state: &DrawState<'_>) -> WinResult<()> {
        let glyph = match state.scene {
            Scene::Compact => {
                if state.busy && (state.cancel_hint || state.label.is_some()) {
                    None
                } else {
                    Some((false, state.theme.accent))
                }
            }
            Scene::Result { color, .. } => Some((false, *color)),
            Scene::AskUpload { .. } => Some((false, state.theme.accent)),
            Scene::Error { .. } => Some((true, state.theme.grey())),
        };
        if self.glyph_key != glyph {
            if let Some((alert, color)) = glyph {
                self.draw_glyph(alert, color)?;
            }
            self.glyph_key = glyph;
        }

        let panel = match state.scene {
            Scene::Compact => None,
            Scene::Result {
                ratio,
                color,
                caption,
            } => Some(PanelKind::Result {
                ratio: ratio.clone(),
                color: *color,
                caption: caption.clone(),
            }),
            Scene::AskUpload {
                filename,
                accept_hover,
                reject_hover,
            } => Some(PanelKind::AskUpload {
                filename: filename.clone(),
                accept_hover: *accept_hover,
                reject_hover: *reject_hover,
            }),
            Scene::Error { title } => Some(PanelKind::Error {
                title: title.clone(),
            }),
        };
        if self.panel_key != panel {
            self.draw_panel(panel.as_ref())?;
            self.panel_key = panel;
        }

        let label_key = (state.label.clone(), state.cancel_hint);
        if self.label_key != Some(label_key.clone()) {
            self.draw_label(state.label.as_deref(), state.cancel_hint)?;
            self.label_key = Some(label_key);
        }

        if self.badge_key != state.badge {
            self.draw_badge(state.badge)?;
            self.badge_key = state.badge;
        }
        Ok(())
    }

    fn update_ring(&mut self, busy: bool, progress: Option<f32>) -> WinResult<()> {
        if !busy {
            if self.arc_animating {
                let _ = self
                    .ring_arc
                    .StopAnimation(&HSTRING::from("RotationAngleInDegrees"));
                self.arc_animating = false;
            }
            return Ok(());
        }
        match progress {
            Some(value) => {
                if self.arc_animating {
                    let _ = self
                        .ring_arc
                        .StopAnimation(&HSTRING::from("RotationAngleInDegrees"));
                    self.ring_arc.SetRotationAngleInDegrees(0.0)?;
                    self.arc_animating = false;
                }
                let value = value.clamp(0.0, 1.0);
                if self
                    .arc_progress
                    .map(|old| (old - value).abs() > 0.01)
                    .unwrap_or(true)
                {
                    self.draw_arc(value)?;
                    self.arc_progress = Some(value);
                }
            }
            None => {
                if !self.arc_animating {
                    self.draw_arc(0.28)?;
                    self.arc_progress = Some(0.28);
                    self.ring_arc.SetRotationAngleInDegrees(0.0)?;
                    if self.theme.animations_enabled {
                        let animation = self.compositor.CreateScalarKeyFrameAnimation()?;
                        animation.InsertKeyFrame(0.0, 0.0)?;
                        animation.InsertKeyFrame(1.0, 360.0)?;
                        animation.SetDuration(TimeSpan {
                            Duration: (1_500 * 10_000) as i64,
                        })?;
                        animation.SetIterationBehavior(AnimationIterationBehavior::Forever)?;
                        self.ring_arc
                            .StartAnimation(&HSTRING::from("RotationAngleInDegrees"), &animation)?;
                        self.arc_animating = true;
                    }
                }
            }
        }
        Ok(())
    }

    fn draw_glyph(&self, alert: bool, color: Rgb) -> WinResult<()> {
        let geometry = &self.shield_geometry;
        let stroke = &self.stroke_round;
        self.shield_layer.draw(|context, base| {
            let brush = solid_brush(context, (1.0, 1.0, 1.0, 1.0))?;
            let canvas = Canvas {
                context,
                brush: &brush,
                stroke,
                base,
            };
            if alert {
                canvas.alert_icon(
                    Vector2 {
                        X: SHIELD_CX,
                        Y: SHIELD_CY,
                    },
                    14.0,
                    color,
                    1.0,
                );
            } else {
                canvas.shield(
                    geometry,
                    Vector2 {
                        X: SHIELD_CX,
                        Y: SHIELD_CY,
                    },
                    color,
                    1.0,
                );
            }
            Ok(())
        })
    }

    fn draw_glow(&self) -> WinResult<()> {
        let accent = self.theme.accent;
        self.glow_layer.draw(|context, _base| {
            let render_target: &ID2D1RenderTarget = context;
            let stops = [
                D2D1_GRADIENT_STOP {
                    position: 0.0,
                    color: d2d_color(accent, 0.55),
                },
                D2D1_GRADIENT_STOP {
                    position: 1.0,
                    color: d2d_color(accent, 0.0),
                },
            ];
            let collection = unsafe {
                render_target.CreateGradientStopCollection(
                    &stops,
                    D2D1_GAMMA_2_2,
                    D2D1_EXTEND_MODE_CLAMP,
                )?
            };
            let radius = GLOW_SIZE * 0.5;
            let brush: ID2D1RadialGradientBrush = unsafe {
                context.CreateRadialGradientBrush(
                    &D2D1_RADIAL_GRADIENT_BRUSH_PROPERTIES {
                        center: Vector2 {
                            X: radius,
                            Y: radius,
                        },
                        gradientOriginOffset: Vector2 { X: 0.0, Y: 0.0 },
                        radiusX: radius,
                        radiusY: radius,
                    },
                    Some(&D2D1_BRUSH_PROPERTIES::default()),
                    &collection,
                )?
            };
            let ellipse = D2D1_ELLIPSE {
                point: Vector2 {
                    X: radius,
                    Y: radius,
                },
                radiusX: radius,
                radiusY: radius,
            };
            unsafe {
                context.FillEllipse(&ellipse, &brush);
            }
            Ok(())
        })
    }

    fn draw_panel(&self, panel: Option<&PanelKind>) -> WinResult<()> {
        let theme = self.theme;
        let (w, h) = match panel {
            Some(PanelKind::Result { caption, .. }) => {
                if caption.is_some() {
                    (208.0, 58.0)
                } else {
                    (176.0, 52.0)
                }
            }
            Some(PanelKind::AskUpload { .. }) => (368.0, 116.0),
            Some(PanelKind::Error { .. }) => (232.0, 52.0),
            None => (MAX_W, MAX_H),
        };
        let stroke = &self.stroke_round;
        let text_small = &self.text_small;
        let text_body = &self.text_body;
        let text_title = &self.text_title;
        let text_ratio = &self.text_ratio;
        let text_warning = &self.text_warning;
        self.panel_layer.draw(|context, base| {
            let brush = solid_brush(context, (1.0, 1.0, 1.0, 1.0))?;
            let canvas = Canvas {
                context,
                brush: &brush,
                stroke,
                base,
            };
            match panel {
                None => {}
                Some(PanelKind::Result {
                    ratio,
                    color,
                    caption,
                }) => {
                    let _ = color;
                    let text_rect = if caption.is_some() {
                        D2D_RECT_F {
                            left: 44.0,
                            top: 3.0,
                            right: w - 12.0,
                            bottom: h - 18.0,
                        }
                    } else {
                        D2D_RECT_F {
                            left: 44.0,
                            top: 0.0,
                            right: w - 12.0,
                            bottom: h,
                        }
                    };
                    canvas.text(ratio, text_ratio, text_rect, theme.foreground(), 1.0);
                    if let Some(caption) = caption {
                        let rect = D2D_RECT_F {
                            left: 46.0,
                            top: h - 20.0,
                            right: w - 12.0,
                            bottom: h - 5.0,
                        };
                        canvas.text(caption, text_small, rect, theme.secondary(), 1.0);
                    }
                }
                Some(PanelKind::AskUpload {
                    filename,
                    accept_hover,
                    reject_hover,
                }) => {
                    canvas.text(
                        "No está en VirusTotal",
                        text_title,
                        D2D_RECT_F {
                            left: 52.0,
                            top: 14.0,
                            right: w - 14.0,
                            bottom: 32.0,
                        },
                        theme.foreground(),
                        1.0,
                    );
                    let name = ellipsize(filename, 46);
                    canvas.text(
                        &name,
                        text_body,
                        D2D_RECT_F {
                            left: 52.0,
                            top: 33.0,
                            right: w - 14.0,
                            bottom: 50.0,
                        },
                        theme.secondary(),
                        1.0,
                    );
                    canvas.text(
                        "El archivo será visible para usuarios de VirusTotal.",
                        text_warning,
                        D2D_RECT_F {
                            left: 52.0,
                            top: 56.0,
                            right: w - 86.0,
                            bottom: 86.0,
                        },
                        theme.muted(),
                        1.0,
                    );
                    canvas.button(
                        Vector2 {
                            X: w - 30.0,
                            Y: h - 30.0,
                        },
                        true,
                        *accept_hover,
                        theme,
                    );
                    canvas.button(
                        Vector2 {
                            X: w - 66.0,
                            Y: h - 30.0,
                        },
                        false,
                        *reject_hover,
                        theme,
                    );
                }
                Some(PanelKind::Error { title }) => {
                    canvas.text(
                        title,
                        text_title,
                        D2D_RECT_F {
                            left: 46.0,
                            top: 15.0,
                            right: w - 12.0,
                            bottom: h,
                        },
                        theme.foreground(),
                        1.0,
                    );
                }
            }
            Ok(())
        })
    }

    fn draw_label(&self, label: Option<&str>, cancel_hint: bool) -> WinResult<()> {
        let theme = self.theme;
        let text_percent = &self.text_percent;
        let stroke = &self.stroke_round;
        self.label_layer.draw(|context, base| {
            let brush = solid_brush(context, (1.0, 1.0, 1.0, 1.0))?;
            let canvas = Canvas {
                context,
                brush: &brush,
                stroke,
                base,
            };
            let center = Vector2 {
                X: LABEL_W * 0.5,
                Y: LABEL_H * 0.5,
            };
            if cancel_hint {
                let arm = 6.0;
                let color = theme.foreground().with_alpha(0.9);
                canvas.line(
                    Vector2 {
                        X: center.X - arm,
                        Y: center.Y - arm,
                    },
                    Vector2 {
                        X: center.X + arm,
                        Y: center.Y + arm,
                    },
                    color,
                    2.2,
                );
                canvas.line(
                    Vector2 {
                        X: center.X + arm,
                        Y: center.Y - arm,
                    },
                    Vector2 {
                        X: center.X - arm,
                        Y: center.Y + arm,
                    },
                    color,
                    2.2,
                );
            } else if let Some(label) = label {
                let rect = D2D_RECT_F {
                    left: 0.0,
                    top: 0.0,
                    right: LABEL_W,
                    bottom: LABEL_H,
                };
                canvas.text(label, text_percent, rect, theme.foreground(), 1.0);
            }
            Ok(())
        })
    }

    fn draw_badge(&self, badge: Option<(usize, usize)>) -> WinResult<()> {
        let Some((index, total)) = badge else {
            return Ok(());
        };
        let theme = self.theme;
        let text_badge = &self.text_badge;
        let stroke = &self.stroke_round;
        let label = format!("{index}/{total}");
        self.badge_layer.draw(|context, base| {
            let brush = solid_brush(context, (1.0, 1.0, 1.0, 1.0))?;
            let canvas = Canvas {
                context,
                brush: &brush,
                stroke,
                base,
            };
            let rect = D2D1_ROUNDED_RECT {
                rect: D2D_RECT_F {
                    left: 0.5,
                    top: 0.5,
                    right: BADGE_W - 0.5,
                    bottom: BADGE_H - 0.5,
                },
                radiusX: 7.0,
                radiusY: 7.0,
            };
            let background = if theme.dark {
                (0.06, 0.06, 0.06, 0.86)
            } else {
                (0.98, 0.98, 0.98, 0.92)
            };
            canvas.fill_rounded_rect(&rect, background);
            canvas.stroke_rounded_rect(&rect, theme.accent.with_alpha(0.75), 1.0);
            canvas.text(
                &label,
                text_badge,
                D2D_RECT_F {
                    left: 0.0,
                    top: 0.0,
                    right: BADGE_W,
                    bottom: BADGE_H,
                },
                theme.foreground(),
                1.0,
            );
            Ok(())
        })
    }

    fn draw_arc(&self, fraction: f32) -> WinResult<()> {
        let accent = self.theme.accent;
        self.arc_layer.draw(|context, _base| {
            let Some(geometry) = create_arc_geometry(&self._d2d_factory, fraction.clamp(0.02, 1.0))
            else {
                return Ok(());
            };
            let brush = solid_brush(context, accent.with_alpha(0.95))?;
            unsafe {
                context.DrawGeometry(&geometry, &brush, 2.2, &self.stroke_round);
            }
            Ok(())
        })
    }
}

struct Frame {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    cx: f32,
    cy: f32,
}

/// Pill geometry while expanding. `anchor` is the compact centre's offset from
/// the window centre, so the pill grows out of the circle even when the window
/// was clamped to the work area.
fn frame_at(window: (f32, f32), anchor: (f32, f32), e: f32, scene: &Scene) -> Frame {
    let (final_w, final_h) = scene.target_size();
    let e = e.clamp(0.0, 1.0);
    let w = lerp(COMPACT, final_w, e);
    let h = lerp(COMPACT, final_h, e);
    let cx = window.0 * 0.5 + anchor.0 * (1.0 - e);
    let cy = window.1 * 0.5 + anchor.1 * (1.0 - e);
    Frame {
        x: cx - w * 0.5,
        y: cy - h * 0.5,
        w,
        h,
        cx,
        cy,
    }
}

/// Where the shield/alert glyph sits, and its scale, for a given expand value.
fn glyph_at(e: f32, scene: &Scene, frame: &Frame) -> (f32, f32, f32) {
    if e <= 0.001 {
        return (frame.cx, frame.cy, 1.0);
    }
    let (tx, ty, scale) = match scene {
        Scene::Compact => (frame.cx, frame.cy, 1.0),
        Scene::Result { .. } => (frame.x + 24.0, frame.y + frame.h * 0.5, 0.62),
        Scene::AskUpload { .. } => (frame.x + 26.0, frame.y + 40.0, 0.70),
        Scene::Error { .. } => (frame.x + 25.0, frame.y + frame.h * 0.5, 12.0 / 15.0),
    };
    (
        lerp(frame.cx, tx, e),
        lerp(frame.cy, ty, e),
        lerp(1.0, scale, e),
    )
}

struct Canvas<'a> {
    context: &'a ID2D1DeviceContext,
    brush: &'a ID2D1SolidColorBrush,
    stroke: &'a ID2D1StrokeStyle,
    /// Atlas translation returned by BeginDraw.
    base: Matrix3x2,
}

impl Canvas<'_> {
    fn set(&self, color: D2D1_COLOR_F) {
        unsafe {
            self.brush.SetColor(&color);
            self.brush.SetOpacity(1.0);
        }
    }

    fn fill_rounded_rect(&self, rect: &D2D1_ROUNDED_RECT, color: (f32, f32, f32, f32)) {
        self.set(d2d_rgba(color));
        unsafe {
            self.context.FillRoundedRectangle(rect, self.brush);
        }
    }

    fn stroke_rounded_rect(
        &self,
        rect: &D2D1_ROUNDED_RECT,
        color: (f32, f32, f32, f32),
        width: f32,
    ) {
        self.set(d2d_rgba(color));
        unsafe {
            self.context
                .DrawRoundedRectangle(rect, self.brush, width, self.stroke);
        }
    }

    fn line(&self, from: Vector2, to: Vector2, color: (f32, f32, f32, f32), width: f32) {
        self.set(d2d_rgba(color));
        unsafe {
            self.context
                .DrawLine(from, to, self.brush, width, self.stroke);
        }
    }

    fn shield(&self, geometry: &ID2D1PathGeometry, center: Vector2, color: Rgb, alpha: f32) {
        let transform = Matrix3x2 {
            M11: 1.0,
            M12: 0.0,
            M21: 0.0,
            M22: 1.0,
            M31: self.base.M31 + center.X - 16.0,
            M32: self.base.M32 + center.Y - 19.0,
        };
        unsafe {
            self.context.SetTransform(&transform);
            self.set(d2d_color(color, 0.16 * alpha));
            self.context
                .FillGeometry(geometry, self.brush, None::<&ID2D1Brush>);
            self.set(d2d_color(color, alpha));
            self.context
                .DrawGeometry(geometry, self.brush, 2.1, self.stroke);
            self.context.SetTransform(&self.base);
        }
    }

    fn alert_icon(&self, center: Vector2, radius: f32, color: Rgb, alpha: f32) {
        unsafe {
            self.set(d2d_color(color, 0.16 * alpha));
            self.context.FillEllipse(
                &D2D1_ELLIPSE {
                    point: center,
                    radiusX: radius,
                    radiusY: radius,
                },
                self.brush,
            );
            self.set(d2d_color(color, 0.75 * alpha));
            self.context.DrawEllipse(
                &D2D1_ELLIPSE {
                    point: center,
                    radiusX: radius,
                    radiusY: radius,
                },
                self.brush,
                1.3,
                self.stroke,
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
                self.brush,
                2.4,
                self.stroke,
            );
            self.set(d2d_color(color, alpha));
            self.context.FillEllipse(
                &D2D1_ELLIPSE {
                    point: Vector2 {
                        X: center.X,
                        Y: center.Y + radius * 0.46,
                    },
                    radiusX: 1.4,
                    radiusY: 1.4,
                },
                self.brush,
            );
        }
    }

    fn button(&self, center: Vector2, accept: bool, hover: bool, theme: Theme) {
        let color = if accept { theme.green() } else { theme.red() };
        let fill = if hover { 0.34 } else { 0.16 };
        unsafe {
            self.set(d2d_color(color, fill));
            self.context.FillEllipse(
                &D2D1_ELLIPSE {
                    point: center,
                    radiusX: 14.0,
                    radiusY: 14.0,
                },
                self.brush,
            );
            self.set(d2d_color(color, if hover { 0.95 } else { 0.65 }));
            self.context.DrawEllipse(
                &D2D1_ELLIPSE {
                    point: center,
                    radiusX: 14.0,
                    radiusY: 14.0,
                },
                self.brush,
                1.2,
                self.stroke,
            );
            self.set(d2d_color(color, 1.0));
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
                    self.brush,
                    width,
                    self.stroke,
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
                    self.brush,
                    width,
                    self.stroke,
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
                    self.brush,
                    width,
                    self.stroke,
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
                    self.brush,
                    width,
                    self.stroke,
                );
            }
        }
    }

    fn text(
        &self,
        text: &str,
        format: &IDWriteTextFormat,
        rect: D2D_RECT_F,
        color: Rgb,
        alpha: f32,
    ) {
        let utf16: Vec<u16> = text.encode_utf16().collect();
        self.set(d2d_color(color, alpha));
        unsafe {
            self.context.DrawText(
                &utf16,
                format,
                &rect,
                self.brush,
                D2D1_DRAW_TEXT_OPTIONS_CLIP,
                DWRITE_MEASURING_MODE_NATURAL,
            );
        }
    }
}

fn animate_vector2(
    target: &CompositionRoundedRectangleGeometry,
    property: &str,
    from: Vector2,
    to: Vector2,
    duration: TimeSpan,
    easing: &CubicBezierEasingFunction,
) -> WinResult<()> {
    let compositor = target.Compositor()?;
    let animation = compositor.CreateVector2KeyFrameAnimation()?;
    animation.InsertKeyFrame(0.0, from)?;
    animation.InsertKeyFrameWithEasingFunction(1.0, to, easing)?;
    animation.SetDuration(duration)?;
    target.StartAnimation(&HSTRING::from(property), &animation)?;
    Ok(())
}

fn animate_centered(
    visual: &SpriteVisual,
    from: &Frame,
    to: &Frame,
    scale: f32,
    duration: TimeSpan,
    easing: &CubicBezierEasingFunction,
) -> WinResult<()> {
    let size = visual.Size()?;
    animate_vector3(
        visual,
        "Offset",
        Vector3 {
            X: from.cx * scale - size.X * 0.5,
            Y: from.cy * scale - size.Y * 0.5,
            Z: 0.0,
        },
        Vector3 {
            X: to.cx * scale - size.X * 0.5,
            Y: to.cy * scale - size.Y * 0.5,
            Z: 0.0,
        },
        duration,
        easing,
    )
}

fn animate_vector3(
    visual: &SpriteVisual,
    property: &str,
    from: Vector3,
    to: Vector3,
    duration: TimeSpan,
    easing: &CubicBezierEasingFunction,
) -> WinResult<()> {
    let compositor = visual.Compositor()?;
    let animation = compositor.CreateVector3KeyFrameAnimation()?;
    animation.InsertKeyFrame(0.0, from)?;
    animation.InsertKeyFrameWithEasingFunction(1.0, to, easing)?;
    animation.SetDuration(duration)?;
    visual.StartAnimation(&HSTRING::from(property), &animation)?;
    Ok(())
}

fn animate_scalar(
    visual: &SpriteVisual,
    property: &str,
    from: f32,
    to: f32,
    duration: TimeSpan,
    easing: &CubicBezierEasingFunction,
) -> WinResult<()> {
    let compositor = visual.Compositor()?;
    let animation = compositor.CreateScalarKeyFrameAnimation()?;
    animation.InsertKeyFrame(0.0, from)?;
    animation.InsertKeyFrameWithEasingFunction(1.0, to, easing)?;
    animation.SetDuration(duration)?;
    visual.StartAnimation(&HSTRING::from(property), &animation)?;
    Ok(())
}

fn solid_brush(
    context: &ID2D1DeviceContext,
    color: (f32, f32, f32, f32),
) -> WinResult<ID2D1SolidColorBrush> {
    unsafe {
        context.CreateSolidColorBrush(
            &D2D1_COLOR_F {
                r: color.0,
                g: color.1,
                b: color.2,
                a: color.3,
            },
            None,
        )
    }
}

fn d2d_color(color: Rgb, alpha: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: color.r as f32 / 255.0,
        g: color.g as f32 / 255.0,
        b: color.b as f32 / 255.0,
        a: alpha,
    }
}

fn d2d_rgba(color: (f32, f32, f32, f32)) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: color.0,
        g: color.1,
        b: color.2,
        a: color.3,
    }
}

fn ui_color(color: Rgb, alpha: f32) -> Color {
    Color {
        A: (alpha.clamp(0.0, 1.0) * 255.0).round() as u8,
        R: color.r,
        G: color.g,
        B: color.b,
    }
}

fn ui_rgba(color: (f32, f32, f32, f32)) -> Color {
    Color {
        A: (color.3.clamp(0.0, 1.0) * 255.0).round() as u8,
        R: (color.0.clamp(0.0, 1.0) * 255.0).round() as u8,
        G: (color.1.clamp(0.0, 1.0) * 255.0).round() as u8,
        B: (color.2.clamp(0.0, 1.0) * 255.0).round() as u8,
    }
}

fn transparent() -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: 0.0,
        g: 0.0,
        b: 0.0,
        a: 0.0,
    }
}

fn create_d3d_device() -> WinResult<(ID3D11Device, ID3D11DeviceContext)> {
    // The composition graphics device draws its surfaces on the GPU; WARP
    // surfaces are not guaranteed to accept a D2D interop context.
    match try_create(D3D_DRIVER_TYPE_HARDWARE) {
        Ok(value) => Ok(value),
        Err(_) => try_create(D3D_DRIVER_TYPE_WARP),
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

fn create_arc_geometry(factory: &ID2D1Factory1, fraction: f32) -> Option<ID2D1PathGeometry> {
    let factory_base: &ID2D1Factory = factory;
    let geometry: ID2D1PathGeometry = unsafe { factory_base.CreatePathGeometry().ok()? };
    let sink: ID2D1GeometrySink = unsafe { geometry.Open().ok()? };
    let center = Vector2 {
        X: RING_SIZE * 0.5,
        Y: RING_SIZE * 0.5,
    };
    let radius = RING_RADIUS;
    let start = -std::f32::consts::FRAC_PI_2;
    let sweep = std::f32::consts::TAU * fraction.clamp(0.0, 1.0);
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
        let _ = sink.Close();
    }
    Some(geometry)
}

fn make_text_format(
    dwrite: &IDWriteFactory,
    family: &str,
    size: f32,
    semibold: bool,
    wrap: bool,
) -> WinResult<IDWriteTextFormat> {
    let family = HSTRING::from(family);
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
    let family = HSTRING::from(family);
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
    let family = HSTRING::from("Segoe UI Variable Text");
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

fn ellipsize(value: &str, max_chars: usize) -> String {
    let count = value.chars().count();
    if count <= max_chars {
        return value.to_owned();
    }
    let mut result: String = value.chars().take(max_chars.saturating_sub(1)).collect();
    result.push('…');
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result_scene() -> Scene {
        Scene::Result {
            ratio: "0/70".into(),
            color: Rgb::new(0x1E, 0x8E, 0x5A),
            caption: Some("Sin detecciones".into()),
        }
    }

    #[test]
    fn compact_frame_is_the_original_circle() {
        let frame = frame_at((64.0, 64.0), (0.0, 0.0), 0.0, &Scene::Compact);
        assert!((frame.w - 64.0).abs() < 0.001);
        assert!((frame.h - 64.0).abs() < 0.001);
        assert!((frame.cx - 32.0).abs() < 0.001);
        assert!((frame.cy - 32.0).abs() < 0.001);
    }

    #[test]
    fn expanded_frame_matches_the_scene_size() {
        let scene = result_scene();
        let (w, h) = scene.target_size();
        let frame = frame_at((w, h), (0.0, 0.0), 1.0, &scene);
        assert!((frame.w - w).abs() < 0.001);
        assert!((frame.h - h).abs() < 0.001);
        assert!((frame.x).abs() < 0.001);
        assert!((frame.y).abs() < 0.001);
    }

    #[test]
    fn anchor_only_offsets_the_compact_end() {
        let compact = frame_at((176.0, 64.0), (10.0, -6.0), 0.0, &Scene::Compact);
        assert!((compact.cx - (88.0 + 10.0)).abs() < 0.001);
        assert!((compact.cy - (32.0 - 6.0)).abs() < 0.001);
        let settled = frame_at((176.0, 64.0), (10.0, -6.0), 1.0, &Scene::Compact);
        assert!((settled.cx - 88.0).abs() < 0.001);
        assert!((settled.cy - 32.0).abs() < 0.001);
    }

    #[test]
    fn glyph_shrinks_and_moves_left_when_expanded() {
        let scene = result_scene();
        let (w, h) = scene.target_size();
        let frame = frame_at((w, h), (0.0, 0.0), 1.0, &scene);
        let (cx, cy, scale) = glyph_at(1.0, &scene, &frame);
        assert!((scale - 0.62).abs() < 0.001);
        assert!((cx - 24.0).abs() < 0.001);
        assert!((cy - h * 0.5).abs() < 0.001);
    }

    #[test]
    fn glyph_starts_at_the_compact_centre() {
        let frame = frame_at((64.0, 64.0), (0.0, 0.0), 0.0, &Scene::Compact);
        let (cx, cy, scale) = glyph_at(0.0, &Scene::Compact, &frame);
        assert!((cx - frame.cx).abs() < 0.001);
        assert!((cy - frame.cy).abs() < 0.001);
        assert!((scale - 1.0).abs() < 0.001);
    }

    #[test]
    fn color_bytes_keep_alpha() {
        let color = ui_color(Rgb::new(0x12, 0x34, 0x56), 0.5);
        assert_eq!(color.R, 0x12);
        assert_eq!(color.G, 0x34);
        assert_eq!(color.B, 0x56);
        assert_eq!(color.A, 128);
    }

    #[test]
    fn long_filenames_are_ellipsized() {
        assert_eq!(ellipsize("abc", 8), "abc");
        assert_eq!(ellipsize("abcdefghij", 8), "abcdefg…");
    }

    /// Boots a real composition target on a hidden window: catches bad API
    /// signatures, WARP graphics-device problems and surface creation errors
    /// without showing anything on screen.
    #[test]
    fn composition_target_renders_a_frame() {
        use windows::core::w;
        use windows::Win32::Foundation::HINSTANCE;
        use windows::Win32::System::Com::{
            CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED,
        };
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcW, DestroyWindow, RegisterClassW, CS_HREDRAW, CS_VREDRAW,
            WNDCLASSW, WS_EX_NOREDIRECTIONBITMAP, WS_EX_TOOLWINDOW, WS_POPUP,
        };

        let class_name = w!("VTDropCompositionTest");
        unsafe extern "system" fn proc(
            hwnd: HWND,
            msg: u32,
            wparam: windows::Win32::Foundation::WPARAM,
            lparam: windows::Win32::Foundation::LPARAM,
        ) -> windows::Win32::Foundation::LRESULT {
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }

        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        }
        let hinstance = unsafe { GetModuleHandleW(None) }.expect("module");
        let class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(proc),
            hInstance: HINSTANCE(hinstance.0),
            lpszClassName: class_name,
            ..Default::default()
        };
        unsafe {
            RegisterClassW(&class);
        }
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_NOREDIRECTIONBITMAP,
                class_name,
                w!("VTDrop test"),
                WS_POPUP,
                0,
                0,
                64,
                64,
                None,
                None,
                Some(HINSTANCE(hinstance.0)),
                None,
            )
        }
        .expect("window");

        let theme = Theme::detect();
        window::make_frameless(hwnd);
        window::set_host_backdrop(hwnd, theme.is_win11());
        let mut renderer =
            CompositionRenderer::new(hwnd, 96, theme, false).expect("composition renderer");

        // The morph relies on animated geometry properties.
        let easing = renderer
            .compositor
            .CreateCubicBezierEasingFunction(Vector2 { X: 0.0, Y: 0.0 }, Vector2 { X: 0.0, Y: 1.0 })
            .expect("easing");
        for property in ["Size", "Offset", "CornerRadius"] {
            animate_vector2(
                &renderer.pill_geometry,
                property,
                Vector2 { X: 64.0, Y: 64.0 },
                Vector2 { X: 176.0, Y: 52.0 },
                TimeSpan { Duration: 25_000 },
                &easing,
            )
            .unwrap_or_else(|error| panic!("{property} is not animatable: {error}"));
        }

        let compact = Scene::Compact;
        let compact_state = DrawState {
            scene: &compact,
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
            busy: false,
            progress: None,
        };
        renderer.set_layout(64.0, 64.0, 0.0, 0.0).expect("layout");
        renderer.render(&compact_state).expect("compact frame");

        let result = Scene::Result {
            ratio: "1/70".into(),
            color: theme.green(),
            caption: Some("Sin detecciones".into()),
        };
        let expanded = DrawState {
            scene: &result,
            theme,
            window_w: 208.0,
            window_h: 58.0,
            expand: 1.0,
            ..compact_state
        };
        renderer.set_layout(208.0, 58.0, 0.0, 0.0).expect("layout");
        renderer.render(&expanded).expect("expanded frame");

        let busy = DrawState {
            scene: &compact,
            theme,
            window_w: 64.0,
            window_h: 64.0,
            expand: 0.0,
            busy: true,
            label: Some("42%".into()),
            progress: Some(0.42),
            ..expanded
        };
        renderer.set_layout(64.0, 64.0, 0.0, 0.0).expect("layout");
        renderer.render(&busy).expect("busy frame");

        drop(renderer);
        unsafe {
            let _ = DestroyWindow(hwnd);
            CoUninitialize();
        }
    }
}
