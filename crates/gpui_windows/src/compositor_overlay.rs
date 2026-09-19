//! A small compositor-owned child visual under the window's existing
//! DirectComposition root.
//!
//! The overlay is presentation-only platform content: the OS compositor can
//! reposition it, recolor it and run its opacity animation without the app
//! rendering or presenting a frame. It receives no input and has no access to
//! the window's scene. The app owns all policy — when to show it, where, and
//! which animation to run — through the [`PlatformCompositorOverlay`] trait.

use std::cell::RefCell;
use std::rc::Rc;

use anyhow::{Context as _, Result};
use gpui::{OpacityEase, OpacitySegment, PlatformCompositorOverlay};
use windows::{
    Win32::Foundation::{POINT, RECT},
    Win32::Graphics::{
        Direct3D11::{ID3D11Device, ID3D11DeviceContext, ID3D11RenderTargetView, ID3D11Texture2D},
        DirectComposition::{IDCompositionDevice, IDCompositionSurface, IDCompositionVisual},
        Dxgi::{Common::DXGI_FORMAT_B8G8R8A8_UNORM, IDXGISurface1, DXGI_ALPHA_MODE_PREMULTIPLIED},
    },
};

use crate::directx_renderer::{DirectComposition, DirectXRendererDevices};

pub(crate) struct CompositorOverlay {
    inner: RefCell<OverlayInner>,
}

struct OverlayInner {
    comp_device: IDCompositionDevice,
    device: ID3D11Device,
    device_context: ID3D11DeviceContext,

    visual: IDCompositionVisual,
    surface: Option<IDCompositionSurface>,

    x: i32,
    y: i32,
    width: u32,
    height: u32,
    color: [f32; 4],
    visible: bool,
    opacity: f32,
}

impl CompositorOverlay {
    /// Create the overlay visual as the topmost child of the composition
    /// root. It starts hidden and contentless until the app places it.
    pub(crate) fn new(
        devices: &DirectXRendererDevices,
        composition: &DirectComposition,
    ) -> Result<Rc<Self>> {
        let comp_device = composition.comp_device().clone();
        let visual = unsafe { comp_device.CreateVisual() }?;
        unsafe {
            visual.SetContent(None::<&IDCompositionSurface>)?;
            composition
                .root_visual()
                .AddVisual(&visual, true, None)?;
            visual.SetOffsetX2(0.0)?;
            visual.SetOffsetY2(0.0)?;
            visual.SetOpacity2(0.0)?;
            comp_device.Commit()?;
        }

        Ok(Rc::new(Self {
            inner: RefCell::new(OverlayInner {
                comp_device,
                device: devices.device.clone(),
                device_context: devices.device_context.clone(),
                visual,
                surface: None,
                x: 0,
                y: 0,
                width: 0,
                height: 0,
                color: [0.0, 0.0, 0.0, 1.0],
                visible: false,
                opacity: 1.0,
            }),
        }))
    }

    /// Recreate the overlay's composition resources on a fresh DirectComposition
    /// device after device loss, preserving the app's geometry, color,
    /// visibility and opacity state. The `Rc` the app holds stays valid.
    pub(crate) fn rebuild(
        &self,
        devices: &DirectXRendererDevices,
        composition: &DirectComposition,
    ) -> Result<()> {
        {
            let mut inner = self.inner.borrow_mut();
            let visual = unsafe { composition.comp_device().CreateVisual() }?;
            inner.comp_device = composition.comp_device().clone();
            inner.device = devices.device.clone();
            inner.device_context = devices.device_context.clone();
            inner.visual = visual.clone();
            inner.surface = None;
            // Reset the size so the next geometry call recreates the content.
            inner.width = 0;
            inner.height = 0;
            unsafe {
                visual.SetContent(None::<&IDCompositionSurface>)?;
                composition.root_visual().AddVisual(&visual, true, None)?;
            }
        }
        let (x, y, width, height, visible, opacity) = {
            let inner = self.inner.borrow();
            (inner.x, inner.y, inner.width, inner.height, inner.visible, inner.opacity)
        };
        if visible && width > 0 && height > 0 {
            self.set_geometry_impl(x, y, width, height)?;
        }
        self.set_visible_impl(visible)?;
        self.set_static_opacity_impl(opacity)
    }

    fn commit(inner: &OverlayInner) -> Result<()> {
        unsafe { inner.comp_device.Commit()? };
        Ok(())
    }

    /// Draw the solid content color into the composition surface.
    fn fill_surface(
        device: &ID3D11Device,
        device_context: &ID3D11DeviceContext,
        surface: &IDCompositionSurface,
        width: u32,
        height: u32,
        color: [f32; 4],
    ) -> Result<()> {
        let rect = RECT {
            left: 0,
            top: 0,
            right: width as i32,
            bottom: height as i32,
        };
        let mut offset = POINT::default();
        let update: IDXGISurface1 = unsafe { surface.BeginDraw(Some(&rect), &mut offset)? };
        let texture: ID3D11Texture2D = update.cast()?;
        let mut render_target_view: Option<ID3D11RenderTargetView> = None;
        unsafe { device.CreateRenderTargetView(&texture, None, Some(&mut render_target_view))? };
        let render_target_view = render_target_view.context("no render target view")?;
        let [r, g, b, a] = color;
        // Premultiplied alpha: the surface's alpha mode is premultiplied.
        unsafe {
            device_context.ClearRenderTargetView(&render_target_view, &[r * a, g * a, b * a, a])
        };
        unsafe { surface.EndDraw()? };
        Ok(())
    }

    fn recreate_surface(inner: &mut OverlayInner, width: u32, height: u32) -> Result<()> {
        let surface = unsafe {
            inner.comp_device.CreateSurface(
                width,
                height,
                DXGI_FORMAT_B8G8R8A8_UNORM,
                DXGI_ALPHA_MODE_PREMULTIPLIED,
            )?
        };
        Self::fill_surface(
            &inner.device,
            &inner.device_context,
            &surface,
            width,
            height,
            inner.color,
        )?;
        unsafe { inner.visual.SetContent(&surface)? };
        inner.surface = Some(surface);
        inner.width = width;
        inner.height = height;
        Ok(())
    }

    fn set_geometry_impl(&self, x: i32, y: i32, width: u32, height: u32) -> Result<()> {
        let mut inner = self.inner.borrow_mut();
        if inner.width != width || inner.height != height {
            Self::recreate_surface(&mut inner, width, height)?;
        }
        inner.x = x;
        inner.y = y;
        unsafe { inner.visual.SetOffsetX2(x as f32)? };
        unsafe { inner.visual.SetOffsetY2(y as f32)? };
        Self::commit(&inner)
    }

    fn set_color_impl(&self, color: [f32; 4]) -> Result<()> {
        let mut inner = self.inner.borrow_mut();
        inner.color = color;
        if let Some(surface) = inner.surface.clone() {
            Self::fill_surface(
                &inner.device,
                &inner.device_context,
                &surface,
                inner.width,
                inner.height,
                inner.color,
            )?;
        }
        Self::commit(&inner)
    }

    fn set_visible_impl(&self, visible: bool) -> Result<()> {
        let mut inner = self.inner.borrow_mut();
        inner.visible = visible;
        unsafe { inner.visual.SetOpacity2(if visible { inner.opacity } else { 0.0 })? };
        Self::commit(&inner)
    }

    fn animate_opacity_cycle_impl(&self, segments: &[OpacitySegment]) -> Result<()> {
        let inner = self.inner.borrow();
        let Some(last) = segments.last() else {
            return Ok(());
        };
        let animation = unsafe { inner.comp_device.CreateAnimation()? };
        for segment in segments {
            let duration = (segment.end_s - segment.start_s).max(0.0);
            match segment.ease {
                OpacityEase::Hold => unsafe {
                    animation.AddCubic(segment.start_s, segment.end_value, 0.0, 0.0, 0.0)?;
                },
                // Smoothstep: v(t) = a + (b - a) * (3u² - 2u³), u = (t - s) / T.
                OpacityEase::Smooth if duration > 0.0 => unsafe {
                    let delta = segment.end_value - segment.start_value;
                    animation.AddCubic(
                        segment.start_s,
                        segment.start_value,
                        0.0,
                        (3.0 * delta / (duration * duration)) as f32,
                        (-2.0 * delta / (duration * duration * duration)) as f32,
                    )?;
                },
                OpacityEase::Smooth => unsafe {
                    animation.AddCubic(segment.start_s, segment.end_value, 0.0, 0.0, 0.0)?;
                },
            }
        }
        // Repeat the full cycle indefinitely: the portion of the animation
        // immediately preceding the repeat point replays forever, and a
        // repeat as the last segment stays in effect with no end.
        unsafe { animation.AddRepeat(last.end_s, last.end_s)? };
        unsafe { inner.visual.SetOpacity(&animation)? };
        Self::commit(&inner)
    }

    fn set_static_opacity_impl(&self, opacity: f32) -> Result<()> {
        let mut inner = self.inner.borrow_mut();
        inner.opacity = opacity;
        unsafe {
            inner
                .visual
                .SetOpacity2(if inner.visible { opacity } else { 0.0 })?
        };
        Self::commit(&inner)
    }
}

impl PlatformCompositorOverlay for CompositorOverlay {
    fn set_geometry(&mut self, x: i32, y: i32, width: u32, height: u32) {
        if let Err(error) = self.set_geometry_impl(x, y, width, height) {
            log::error!("compositor overlay set_geometry failed: {error}");
        }
    }

    fn set_color(&mut self, color: [f32; 4]) {
        if let Err(error) = self.set_color_impl(color) {
            log::error!("compositor overlay set_color failed: {error}");
        }
    }

    fn set_visible(&mut self, visible: bool) {
        if let Err(error) = self.set_visible_impl(visible) {
            log::error!("compositor overlay set_visible failed: {error}");
        }
    }

    fn animate_opacity_cycle(&mut self, segments: &[OpacitySegment]) {
        if let Err(error) = self.animate_opacity_cycle_impl(segments) {
            log::error!("compositor overlay animate_opacity_cycle failed: {error}");
        }
    }

    fn set_static_opacity(&mut self, opacity: f32) {
        if let Err(error) = self.set_static_opacity_impl(opacity) {
            log::error!("compositor overlay set_static_opacity failed: {error}");
        }
    }
}
