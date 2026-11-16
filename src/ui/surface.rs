//! Shared DirectComposition + Direct2D drawing surface.
//!
//! Both the custom context menu and the API key dialog render into a
//! transparent premultiplied swap chain. Clearing each frame is what allows
//! translucent acrylic backgrounds, rounded hover pills and smooth text.

use std::mem::ManuallyDrop;

use windows::core::{IUnknown, Interface, Result as WinResult, BOOL};
use windows::Win32::Foundation::{E_FAIL, HMODULE, HWND};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_PIXEL_FORMAT, D2D_RECT_F,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, ID2D1DeviceContext, ID2D1Factory1, ID2D1SolidColorBrush, ID2D1StrokeStyle,
    D2D1_ANTIALIAS_MODE_PER_PRIMITIVE, D2D1_BITMAP_OPTIONS_CANNOT_DRAW, D2D1_BITMAP_OPTIONS_TARGET,
    D2D1_BITMAP_PROPERTIES1, D2D1_DEVICE_CONTEXT_OPTIONS_NONE, D2D1_DRAW_TEXT_OPTIONS_CLIP,
    D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_ROUNDED_RECT, D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE,
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
    IDWriteTextLayout, DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_STRETCH_NORMAL,
    DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT, DWRITE_MEASURING_MODE_NATURAL,
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
use windows_numerics::Vector2;

pub type Color = (f32, f32, f32, f32);

pub struct Surface {
    _d3d_device: ID3D11Device,
    _d3d_context: ID3D11DeviceContext,
    _d2d_factory: ID2D1Factory1,
    dwrite: IDWriteFactory,
    _dcomp: IDCompositionDevice,
    _target: IDCompositionTarget,
    _visual: IDCompositionVisual,
    swapchain: IDXGISwapChain1,
    context: ID2D1DeviceContext,
    brush: ID2D1SolidColorBrush,
    pub width: f32,
    pub height: f32,
    pub dpi: f32,
}

impl Surface {
    pub fn new(hwnd: HWND, dpi: u32, width_dip: f32, height_dip: f32) -> WinResult<Self> {
        let dpi = dpi.max(96);
        let scale = dpi as f32 / 96.0;
        let width_px = (width_dip * scale).ceil() as u32;
        let height_px = (height_dip * scale).ceil() as u32;

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
                    Width: width_px,
                    Height: height_px,
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
        let props = D2D1_BITMAP_PROPERTIES1 {
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: dpi as f32,
            dpiY: dpi as f32,
            bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
            colorContext: ManuallyDrop::new(None),
        };
        let bitmap = unsafe { context.CreateBitmapFromDxgiSurface(&surface, Some(&props))? };
        unsafe {
            context.SetTarget(&bitmap);
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

        Ok(Self {
            _d3d_device: d3d_device,
            _d3d_context: d3d_context,
            _d2d_factory: d2d_factory,
            dwrite,
            _dcomp: dcomp,
            _target: target,
            _visual: visual,
            swapchain,
            context,
            brush,
            width: width_dip,
            height: height_dip,
            dpi: dpi as f32,
        })
    }

    /// Fluent entrance (fade + slide down) run by the compositor itself, so it
    /// stays smooth at display refresh with no work on the UI thread.
    pub fn animate_in(&self, slide_dip: f32, duration_ms: u64) -> WinResult<()> {
        let seconds = duration_ms as f64 / 1000.0;
        let d = seconds as f32;
        // Ease-out cubic as a polynomial: from + Δ(3u − 3u² + u³), u = t/d.
        let ease = |from: f32, to: f32| -> WinResult<_> {
            let delta = to - from;
            let animation = unsafe { self._dcomp.CreateAnimation()? };
            unsafe {
                animation.AddCubic(0.0, from, 3.0 * delta / d, -3.0 * delta / (d * d), delta / (d * d * d))?;
                animation.End(seconds, to)?;
            }
            Ok(animation)
        };
        let offset = ease(-slide_dip * self.dpi / 96.0, 0.0)?;
        let opacity = ease(0.0, 1.0)?;
        unsafe {
            let effect = self._dcomp.CreateEffectGroup()?;
            effect.SetOpacity(&opacity)?;
            self._visual.SetEffect(&effect)?;
            self._visual.SetOffsetY(&offset)?;
            self._dcomp.Commit()
        }
    }

    /// Begins a frame and clears it to fully transparent so stale pixels from
    /// the previous hover state cannot survive.
    pub fn begin(&mut self) {
        unsafe {
            self.context.BeginDraw();
            self.context.Clear(Some(&D2D1_COLOR_F {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 0.0,
            }));
        }
    }

    pub fn end(&mut self) -> WinResult<()> {
        unsafe {
            self.context.EndDraw(None, None)?;
            self.swapchain.Present(1, DXGI_PRESENT(0)).ok()?;
        }
        Ok(())
    }

    pub fn fill_rounded(
        &mut self,
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        radius: f32,
        color: Color,
    ) {
        self.set_color(color);
        let rect = rounded(x, y, width, height, radius);
        unsafe {
            self.context.FillRoundedRectangle(&rect, &self.brush);
        }
    }

    pub fn stroke_rounded(
        &mut self,
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        radius: f32,
        color: Color,
        stroke_width: f32,
    ) {
        self.set_color(color);
        let rect = rounded(x, y, width, height, radius);
        unsafe {
            self.context.DrawRoundedRectangle(
                &rect,
                &self.brush,
                stroke_width,
                None::<&ID2D1StrokeStyle>,
            );
        }
    }

    pub fn line(&mut self, from: Vector2, to: Vector2, color: Color, width: f32) {
        self.set_color(color);
        unsafe {
            self.context
                .DrawLine(from, to, &self.brush, width, None::<&ID2D1StrokeStyle>);
        }
    }

    pub fn text(
        &mut self,
        format: &IDWriteTextFormat,
        text: &str,
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        color: Color,
    ) {
        let utf16: Vec<u16> = text.encode_utf16().collect();
        self.set_color(color);
        let rect = D2D_RECT_F {
            left: x,
            top: y,
            right: x + width,
            bottom: y + height,
        };
        unsafe {
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

    pub fn draw_layout(&mut self, layout: &IDWriteTextLayout, x: f32, y: f32, color: Color) {
        self.set_color(color);
        unsafe {
            self.context.DrawTextLayout(
                Vector2 { X: x, Y: y },
                layout,
                &self.brush,
                D2D1_DRAW_TEXT_OPTIONS_CLIP,
            );
        }
    }

    pub fn text_format(
        &self,
        family: &str,
        size: f32,
        weight: DWRITE_FONT_WEIGHT,
        centered: bool,
        wrap: bool,
    ) -> WinResult<IDWriteTextFormat> {
        let alignment = if centered {
            DWRITE_TEXT_ALIGNMENT_CENTER
        } else {
            DWRITE_TEXT_ALIGNMENT_LEADING
        };
        let paragraph = if wrap {
            DWRITE_PARAGRAPH_ALIGNMENT_NEAR
        } else {
            DWRITE_PARAGRAPH_ALIGNMENT_CENTER
        };
        let wrapping = if wrap {
            DWRITE_WORD_WRAPPING_WRAP
        } else {
            DWRITE_WORD_WRAPPING_NO_WRAP
        };
        let format = match self.make_format(family, size, weight) {
            Ok(format) => format,
            // Windows 10 has no Segoe UI Variable; fall back to Segoe UI.
            Err(_) => self.make_format("Segoe UI", size, weight)?,
        };
        unsafe {
            format.SetTextAlignment(alignment)?;
            format.SetParagraphAlignment(paragraph)?;
            format.SetWordWrapping(wrapping)?;
        }
        Ok(format)
    }

    pub fn dwrite(&self) -> &IDWriteFactory {
        &self.dwrite
    }

    fn make_format(
        &self,
        family: &str,
        size: f32,
        weight: DWRITE_FONT_WEIGHT,
    ) -> WinResult<IDWriteTextFormat> {
        let family = windows::core::HSTRING::from(family);
        unsafe {
            self.dwrite.CreateTextFormat(
                &family,
                None::<&IDWriteFontCollection>,
                weight,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                size,
                windows::core::w!("es-ES"),
            )
        }
    }

    fn set_color(&mut self, color: Color) {
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

fn rounded(x: f32, y: f32, w: f32, h: f32, radius: f32) -> D2D1_ROUNDED_RECT {
    D2D1_ROUNDED_RECT {
        rect: D2D_RECT_F {
            left: x,
            top: y,
            right: x + w,
            bottom: y + h,
        },
        radiusX: radius,
        radiusY: radius,
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
