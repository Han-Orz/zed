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
use std::time::Instant;

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

/// Drop the cached overlay when it reported a failure, so the caller's next
/// step builds a fresh one. Returns whether one was dropped. This is the one
/// place the dead-handle decision lives: an overlay that failed keeps a
/// visual the compositor no longer shows, and serving it again would keep
/// re-asserting state onto nothing.
pub(crate) fn drop_unhealthy_overlay<T: PlatformCompositorOverlay + ?Sized>(
    overlay: &mut Option<Rc<RefCell<T>>>,
) -> bool {
    let unhealthy = overlay
        .as_ref()
        .is_some_and(|overlay| !overlay.borrow().is_healthy());
    if unhealthy {
        *overlay = None;
    }
    unhealthy
}


use crate::directx_renderer::{DirectComposition, DirectXRendererDevices};

pub(crate) struct CompositorOverlay {
    comp_device: IDCompositionDevice,
    device: ID3D11Device,
    device_context: ID3D11DeviceContext,

    visual: IDCompositionVisual,
    /// The same visual as `visual`, typed for opacity control (Windows 10+).
    opacity_visual: IDCompositionVisual3,
    surface: Option<IDCompositionSurface>,

    x: f32,
    y: f32,
    width: u32,
    height: u32,
    color: [f32; 4],
    /// What the compositor was last told to play. It is also the overlay's
    /// remembered opacity: a rebuild restores the value this motion holds or
    /// is passing through, so the overlay never comes back at a stale one.
    motion: Motion,
    /// Cleared when a compositor operation fails. The remembered state
    /// (geometry, color, motion) can then no longer be trusted to describe
    /// what the compositor shows, so the renderer drops the handle on its
    /// next request and builds a fresh overlay instead of reporting success
    /// on a visual that is not there.
    healthy: bool,
}

/// What the compositor is playing on the overlay's opacity. DirectComposition
/// exposes no getter for an animated value, so the overlay remembers the motion
/// it scheduled and derives the value on screen when the next motion has to
/// start from it instead of from a stale target.
enum Motion {
    /// A constant value, held.
    Hold(f32),
    /// A one-shot fade in flight.
    Fade {
        from: f32,
        to: f32,
        started: Instant,
        duration_s: f64,
    },
    /// The breathing cycle, repeating from `started`.
    Breathe {
        started: Instant,
        segments: Vec<OpacitySegment>,
    },
}

impl Motion {
    /// The opacity the visual shows at `now`.
    fn value_at(&self, now: Instant) -> f32 {
        match self {
            Motion::Hold(value) => *value,
            Motion::Fade {
                from,
                to,
                started,
                duration_s,
            } => {
                let elapsed = now.saturating_duration_since(*started).as_secs_f64();
                if *duration_s <= 0.0 || elapsed >= *duration_s {
                    return *to;
                }
                from + (to - from) * smoothstep(elapsed / duration_s)
            }
            Motion::Breathe { started, segments } => {
                let Some(last) = segments.last() else {
                    return 0.0;
                };
                let cycle = last.end_s;
                if cycle <= 0.0 {
                    return last.end_value;
                }
                let t = now.saturating_duration_since(*started).as_secs_f64() % cycle;
                let segment = segments
                    .iter()
                    .find(|segment| t < segment.end_s)
                    .unwrap_or(last);
                let duration = segment.end_s - segment.start_s;
                match segment.ease {
                    OpacityEase::Hold => segment.end_value,
                    OpacityEase::Smooth if duration > 0.0 => {
                        let u = (t - segment.start_s) / duration;
                        segment.start_value
                            + (segment.end_value - segment.start_value) * smoothstep(u)
                    }
                    OpacityEase::Smooth => segment.end_value,
                }
            }
        }
    }
}

/// The symmetric ease-in-out curve the fade and the breathing fade segments
/// both use, kept in one place so the overlay's estimate of an in-flight
/// animation matches what the compositor is executing.
fn smoothstep(u: f64) -> f32 {
    let u = u.clamp(0.0, 1.0);
    (u * u * (3.0 - 2.0 * u)) as f32
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
            x: 0.0,
            y: 0.0,
            width: 0,
            height: 0,
            color: [0.0, 0.0, 0.0, 1.0],
            // Created invisible: the app's first fade brings it in.
            motion: Motion::Hold(0.0),
            healthy: true,
        })))
    }

    /// Recreate the overlay's composition resources on a fresh DirectComposition
    /// device after device loss, preserving the app's geometry, color and
    /// opacity state. The `Rc` the app holds stays valid.
    ///
    /// A failure here leaves the overlay dead — its visual belongs to a
    /// destroyed device — so the overlay marks itself unhealthy. The renderer
    /// then drops the handle on its next request and the following one builds
    /// a fresh overlay, which is the recovery path: an app that keeps its
    /// handle is told to refresh it and re-asserts its full state.
    pub(crate) fn rebuild(
        &mut self,
        devices: &DirectXRendererDevices,
        composition: &DirectComposition,
    ) -> Result<()> {
        if let Err(error) = self.rebuild_impl(devices, composition) {
            self.healthy = false;
            return Err(error);
        }
        Ok(())
    }

    fn rebuild_impl(
        &mut self,
        devices: &DirectXRendererDevices,
        composition: &DirectComposition,
    ) -> Result<()> {
        let (x, y, width, height) = (self.x, self.y, self.width, self.height);
        let visual = unsafe { composition.comp_device().CreateVisual() }?;
        let opacity_visual: IDCompositionVisual3 = visual.cast()?;
        self.comp_device = composition.comp_device().clone();
        self.device = devices.device.clone();
        self.device_context = devices.device_context.clone();
        self.visual = visual.clone();
        self.opacity_visual = opacity_visual;
        // The content surface belonged to the lost device; the geometry the app
        // committed did not. Clearing the remembered size makes the geometry
        // call below recreate the content at that committed size instead of
        // leaving the overlay contentless.
        self.surface = None;
        self.width = 0;
        self.height = 0;
        unsafe {
            visual.SetContent(None::<&windows::core::IUnknown>)?;
            composition.root_visual().AddVisual(&visual, true, None)?;
        }
        if width > 0 && height > 0 {
            self.set_geometry_impl(x, y, width, height)?;
        }
        // The animation died with the lost device, so it is replayed from the
        // value the visual was showing: a breathing cycle starts again, a fade
        // finishes its remaining time toward the same target, anything else
        // holds. Holding a fade here would strand the overlay half faded.
        let now = Instant::now();
        match &self.motion {
            Motion::Breathe { segments, .. } => {
                let segments = segments.clone();
                self.breathe_impl(&segments)
            }
            Motion::Fade {
                to,
                started,
                duration_s,
                ..
            } => {
                let remaining_s =
                    duration_s - now.saturating_duration_since(*started).as_secs_f64();
                let to = *to;
                self.fade_impl(to, remaining_s)
            }
            Motion::Hold(_) => self.hold_impl(self.motion.value_at(now)),
        }
    }

    /// Stop any compositor animation and hold `opacity`.
    fn hold_impl(&mut self, opacity: f32) -> Result<()> {
        self.motion = Motion::Hold(opacity);
        unsafe { self.opacity_visual.SetOpacity2(opacity)? };
        self.commit()
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
        unsafe {
            self.device
                .CreateRenderTargetView(&texture, None, Some(&mut render_target_view))?
        };
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

    fn set_geometry_impl(&mut self, x: f32, y: f32, width: u32, height: u32) -> Result<()> {
        if self.width != width || self.height != height {
            self.recreate_surface(width, height)?;
        }
        self.x = x;
        self.y = y;
        // The compositor places the visual at a sub-pixel offset: an animated
        // position is not rounded to whole pixels here.
        unsafe { self.visual.SetOffsetX2(x)? };
        unsafe { self.visual.SetOffsetY2(y)? };
        self.commit()
    }

    fn set_color_impl(&mut self, color: [f32; 4]) -> Result<()> {
        self.color = color;
        if let Some(surface) = self.surface.clone() {
            self.fill_surface(&surface, self.width, self.height, self.color)?;
        }
        self.commit()
    }

    /// Fade to `target` over `duration_s`, starting from the opacity on screen
    /// right now — the tail of a previous fade or the phase of a running
    /// breathing cycle — so no transition ever snaps.
    fn fade_impl(&mut self, target: f32, duration_s: f64) -> Result<()> {
        let now = Instant::now();
        let from = self.motion.value_at(now);
        if duration_s <= 0.0 || from == target {
            return self.hold_impl(target);
        }
        let animation = unsafe { self.comp_device.CreateAnimation()? };
        // The same smoothstep the breathing fade segments use.
        let delta = f64::from(target - from);
        unsafe {
            animation.AddCubic(
                0.0,
                from,
                0.0,
                (3.0 * delta / (duration_s * duration_s)) as f32,
                (-2.0 * delta / (duration_s * duration_s * duration_s)) as f32,
            )?
        };
        unsafe { self.opacity_visual.SetOpacity(&animation)? };
        self.motion = Motion::Fade {
            from,
            to: target,
            started: now,
            duration_s,
        };
        self.commit()
    }

    fn breathe_impl(&mut self, segments: &[OpacitySegment]) -> Result<()> {
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
        self.motion = Motion::Breathe {
            started: Instant::now(),
            segments: segments.to_vec(),
        };
        self.commit()
    }
}

impl PlatformCompositorOverlay for CompositorOverlay {
    fn set_geometry(&mut self, x: f32, y: f32, width: u32, height: u32) {
        if let Err(error) = self.set_geometry_impl(x, y, width, height) {
            log::error!("compositor overlay set_geometry failed: {error}");
            self.healthy = false;
        }
    }

    fn set_color(&mut self, color: [f32; 4]) {
        if let Err(error) = self.set_color_impl(color) {
            log::error!("compositor overlay set_color failed: {error}");
            self.healthy = false;
        }
    }

    fn fade_opacity(&mut self, target: f32, duration_s: f64) {
        if let Err(error) = self.fade_impl(target, duration_s) {
            log::error!("compositor overlay fade_opacity failed: {error}");
            self.healthy = false;
        }
    }

    fn animate_opacity_cycle(&mut self, segments: &[OpacitySegment]) {
        if let Err(error) = self.breathe_impl(segments) {
            log::error!("compositor overlay animate_opacity_cycle failed: {error}");
            self.healthy = false;
        }
    }

    fn hold_opacity(&mut self, opacity: f32) {
        if let Err(error) = self.hold_impl(opacity) {
            log::error!("compositor overlay hold_opacity failed: {error}");
            self.healthy = false;
        }
    }

    fn is_healthy(&self) -> bool {
        self.healthy
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in for the platform overlay: no device, just the health flag
    /// the drop decision reads.
    struct FakeOverlay {
        healthy: bool,
    }

    impl PlatformCompositorOverlay for FakeOverlay {
        fn set_geometry(&mut self, _: f32, _: f32, _: u32, _: u32) {}
        fn set_color(&mut self, _: [f32; 4]) {}
        fn fade_opacity(&mut self, _: f32, _: f64) {}
        fn animate_opacity_cycle(&mut self, _: &[OpacitySegment]) {}
        fn hold_opacity(&mut self, _: f32) {}
        fn is_healthy(&self) -> bool {
            self.healthy
        }
    }

    fn fake(healthy: bool) -> Rc<RefCell<dyn PlatformCompositorOverlay>> {
        Rc::new(RefCell::new(FakeOverlay { healthy }))
    }

    #[test]
    fn an_unhealthy_overlay_is_dropped_so_a_fresh_one_can_be_built() {
        // BLOCKER (liveness): a failed overlay keeps a visual the compositor
        // does not show. Serving that handle again would re-assert state onto
        // nothing forever, so the drop decision must clear it — the caller
        // then builds a replacement on the same call.
        let mut slot = Some(fake(false));
        assert!(drop_unhealthy_overlay(&mut slot));
        assert!(slot.is_none(), "the dead handle must not stay cached");

        // A healthy overlay is kept: dropping it would needlessly rebuild the
        // visual and restart its opacity.
        let healthy = fake(true);
        let mut slot = Some(healthy.clone());
        assert!(!drop_unhealthy_overlay(&mut slot));
        assert!(slot.is_some_and(|current| Rc::ptr_eq(&current, &healthy)));

        // No overlay at all is not a drop.
        let mut slot: Option<Rc<RefCell<dyn PlatformCompositorOverlay>>> = None;
        assert!(!drop_unhealthy_overlay(&mut slot));
    }
}
