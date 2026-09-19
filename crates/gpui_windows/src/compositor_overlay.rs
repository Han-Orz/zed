//! A small compositor-owned child visual under the window's existing
//! DirectComposition root.
//!
//! The overlay is presentation-only platform content: the OS compositor can
//! reposition it, recolor it and run its opacity animation without the app
//! rendering or presenting a frame. It receives no input and has no access to
//! the window's scene. The app owns all policy through the
//! [`PlatformCompositorOverlay`] trait: when to show it, where to place it,
//! and which animation to run.

use std::cell::RefCell;
use std::rc::Rc;

use anyhow::{Context as _, Result};
use gpui::{OpacityEase, OpacitySegment, PlatformCompositorOverlay};
use windows::core::Interface;
use windows::{
    Win32::Foundation::{POINT, RECT},
    Win32::Graphics::{
        Direct3D11::{ID3D11Device, ID3D11DeviceContext, ID3D11RenderTargetView, ID3D11Texture2D},
        DirectComposition::{
            IDCompositionDevice, IDCompositionSurface, IDCompositionVisual, IDCompositionVisual3,
        },
        Dxgi::{
            Common::{DXGI_ALPHA_MODE_PREMULTIPLIED, DXGI_FORMAT_B8G8R8A8_UNORM},
            IDXGISurface1,
        },
    },
};

use crate::directx_renderer::{DirectComposition, DirectXRendererDevices};

pub(crate) struct CompositorOverlay {
    comp_device: IDCompositionDevice,
    device: ID3D11Device,
    device_context: ID3D11DeviceContext,

    visual: IDCompositionVisual,
    /// The same visual as `visual`, typed for opacity control (Windows 10+).
    opacity_visual: IDCompositionVisual3,
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
    ) -> Result<Rc<RefCell<Self>>> {
        let comp_device = composition.comp_device().clone();
        let visual = unsafe { comp_device.CreateVisual() }?;
        let opacity_visual: IDCompositionVisual3 = visual.cast()?;
        unsafe {
            visual.SetContent(None::<&windows::core::IUnknown>)?;
            composition.root_visual().AddVisual(&visual, true, None)?;
            visual.SetOffsetX2(0.0)?;
            visual.SetOffsetY2(0.0)?;
            opacity_visual.SetOpacity2(0.0)?;
            comp_device.Commit()?;
        }

        Ok(Rc::new(RefCell::new(Self {
            comp_device,
            device: devices.device.clone(),
            device_context: devices.device_context.clone(),
            opacity_visual,
            visual,
            surface: None,
            x: 0,
            y: 0,
            width: 0,
            height: 0,
            color: [0.0, 0.0, 0.0, 1.0],
            visible: false,
            opacity: 1.0,
        })))
    }

    /// Recreate the overlay's composition resources on a fresh DirectComposition
    /// device after device loss, preserving the app's geometry, color,
    /// visibility and opacity state. The `Rc` the app holds stays valid.
    pub(crate) fn rebuild(
        &mut self,
        devices: &DirectXRendererDevices,
        composition: &DirectComposition,
    ) -> Result<()> {
        let visual = unsafe { composition.comp_device().CreateVisual() }?;
        let opacity_visual: IDCompositionVisual3 = visual.cast()?;
        self.comp_device = composition.comp_device().clone();
        self.device = devices.device.clone();
        self.device_context = devices.device_context.clone();
        self.visual = visual.clone();
        self.opacity_visual = opacity_visual;
        self.surface = None;
        // Reset the size so the next geometry call recreates the content.
        self.width = 0;
        self.height = 0;
        unsafe {
            visual.SetContent(None::<&windows::core::IUnknown>)?;
            composition.root_visual().AddVisual(&visual, true, None)?;
        }
        if self.visible && self.width > 0 && self.height > 0 {
            self.set_geometry_impl(self.x, self.y, self.width, self.height)?;
        }
        self.set_visible_impl(self.visible)?;
        self.set_static_opacity_impl(self.opacity)
    }

    fn commit(&self) -> Result<()> {
        unsafe { self.comp_device.Commit()? };
        Ok(())
    }

    /// Draw the solid content color into the composition surface.
    fn fill_surface(
        &self,
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
        unsafe { self.device.CreateRenderTargetView(&texture, None, Some(&mut render_target_view))? };
        let render_target_view = render_target_view.context("no render target view")?;
        let [r, g, b, a] = color;
        // Premultiplied alpha: the surface's alpha mode is premultiplied.
        unsafe {
            self.device_context
                .ClearRenderTargetView(&render_target_view, &[r * a, g * a, b * a, a])
        };
        unsafe { surface.EndDraw()? };
        Ok(())
    }

    fn recreate_surface(&mut self, width: u32, height: u32) -> Result<()> {
        let surface = unsafe {
            self.comp_device.CreateSurface(
                width,
                height,
                DXGI_FORMAT_B8G8R8A8_UNORM,
                DXGI_ALPHA_MODE_PREMULTIPLIED,
            )?
        };
        self.fill_surface(&surface, width, height, self.color)?;
        unsafe { self.visual.SetContent(&surface)? };
        self.surface = Some(surface);
        self.width = width;
        self.height = height;
        Ok(())
    }

    fn set_geometry_impl(&mut self, x: i32, y: i32, width: u32, height: u32) -> Result<()> {
        if self.width != width || self.height != height {
            self.recreate_surface(width, height)?;
        }
        self.x = x;
        self.y = y;
        unsafe { self.visual.SetOffsetX2(x as f32)? };
        unsafe { self.visual.SetOffsetY2(y as f32)? };
        self.commit()
    }

    fn set_color_impl(&mut self, color: [f32; 4]) -> Result<()> {
        self.color = color;
        if let Some(surface) = self.surface.clone() {
            self.fill_surface(&surface, self.width, self.height, self.color)?;
        }
        self.commit()
    }

    fn set_visible_impl(&mut self, visible: bool) -> Result<()> {
        self.visible = visible;
        unsafe {
            self.opacity_visual
                .SetOpacity2(if visible { self.opacity } else { 0.0 })?
        };
        self.commit()
    }

    fn animate_opacity_cycle_impl(&mut self, segments: &[OpacitySegment]) -> Result<()> {
        let Some(last) = segments.last() else {
            return Ok(());
        };
        let animation = unsafe { self.comp_device.CreateAnimation()? };
        for segment in segments {
            let duration = (segment.end_s - segment.start_s).max(0.0);
            match segment.ease {
                OpacityEase::Hold => unsafe {
                    animation.AddCubic(segment.start_s, segment.end_value, 0.0, 0.0, 0.0)?;
                },
                // Smoothstep: v(t) = a + (b - a) * (3u^2 - 2u^3), u = (t - s) / T.
                OpacityEase::Smooth if duration > 0.0 => unsafe {
                    let delta = f64::from(segment.end_value - segment.start_value);
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
        unsafe { self.opacity_visual.SetOpacity(&animation)? };
        self.commit()
    }

    fn set_static_opacity_impl(&mut self, opacity: f32) -> Result<()> {
        self.opacity = opacity;
        unsafe {
            self.opacity_visual
                .SetOpacity2(if self.visible { opacity } else { 0.0 })?
        };
        self.commit()
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
