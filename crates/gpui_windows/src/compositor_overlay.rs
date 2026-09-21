//! Small compositor-owned child visuals under the window's existing
//! DirectComposition root.
//!
//! An overlay is presentation-only platform content: the OS compositor can
//! reposition it, redraw it and run its opacity animation without the app
//! rendering or presenting a frame. It receives no input and has no access to
//! the window's scene. The app owns all policy through the
//! [`PlatformCompositorOverlay`] trait: whether to have one at all, when to
//! show it, where to place it, what it shows and which animation to run. The
//! handle the app holds owns the visual; the overlay releases it on drop.

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

use crate::directx_renderer::{DirectComposition, DirectXRendererDevices};

pub(crate) struct CompositorOverlay {
    comp_device: IDCompositionDevice,
    device: ID3D11Device,
    device_context: ID3D11DeviceContext,

    /// The composition root `visual` is attached to. Held so the overlay can
    /// detach its own visual on drop and on device loss.
    root_visual: IDCompositionVisual,
    visual: IDCompositionVisual,
    /// The same visual as `visual`, typed for opacity control (Windows 10+).
    opacity_visual: IDCompositionVisual3,
    surface: Option<IDCompositionSurface>,

    x: f32,
    y: f32,
    width: u32,
    height: u32,
    content: Content,
    /// What the compositor was last told to play. It is also the overlay's
    /// remembered opacity: a rebuild restores the value this motion holds or
    /// is passing through, so the overlay never comes back at a stale one.
    motion: Motion,
}

/// What the overlay shows inside its surface. The overlay remembers the last
/// content committed to it so a device-loss rebuild can draw it again without
/// the app uploading: this is what the visual is showing, not a texture cache.
enum Content {
    /// A solid non-premultiplied color.
    Solid([f32; 4]),
    /// A small tightly packed premultiplied RGBA8 image.
    Rgba {
        width: u32,
        height: u32,
        pixels: Vec<u8>,
    },
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
        let root_visual = composition.root_visual().clone();
        unsafe {
            visual.SetContent(None::<&windows::core::IUnknown>)?;
            root_visual.AddVisual(&visual, true, None)?;
            visual.SetOffsetX2(0.0)?;
            visual.SetOffsetY2(0.0)?;
            opacity_visual.SetOpacity2(0.0)?;
            comp_device.Commit()?;
        }

        Ok(Rc::new(RefCell::new(Self {
            comp_device,
            device: devices.device.clone(),
            device_context: devices.device_context.clone(),
            root_visual,
            opacity_visual,
            visual,
            surface: None,
            x: 0.0,
            y: 0.0,
            width: 0,
            height: 0,
            content: Content::Solid([0.0, 0.0, 0.0, 1.0]),
            // Created invisible: the app's first fade brings it in.
            motion: Motion::Hold(0.0),
        })))
    }

    /// Recreate the overlay's composition resources on a fresh DirectComposition
    /// device after device loss, preserving the app's geometry, color and
    /// opacity state. The `Rc` the app holds stays valid.
    pub(crate) fn rebuild(
        &mut self,
        devices: &DirectXRendererDevices,
        composition: &DirectComposition,
    ) -> Result<()> {
        let (x, y, width, height) = (self.x, self.y, self.width, self.height);
        let visual = unsafe { composition.comp_device().CreateVisual() }?;
        let opacity_visual: IDCompositionVisual3 = visual.cast()?;
        let root_visual = composition.root_visual().clone();
        // The old visual belongs to the lost device, so detaching it is a
        // best effort against a root that is already gone.
        self.detach_visual();
        self.comp_device = composition.comp_device().clone();
        self.device = devices.device.clone();
        self.device_context = devices.device_context.clone();
        self.root_visual = root_visual.clone();
        self.visual = visual.clone();
        self.opacity_visual = opacity_visual;
        // The content surface belonged to the lost device; the geometry the app
        // committed did not. Clearing the remembered size makes the geometry
        // call below recreate the surface at that committed size, drawing the
        // remembered content into it, instead of leaving it contentless.
        self.surface = None;
        self.width = 0;
        self.height = 0;
        unsafe {
            visual.SetContent(None::<&windows::core::IUnknown>)?;
            root_visual.AddVisual(&visual, true, None)?;
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

    /// Detach the overlay's visual from the composition root. Best effort: on
    /// device loss and at shutdown the device behind the root is already gone,
    /// and a visual that cannot be removed is not an app failure.
    fn detach_visual(&self) {
        let result = unsafe {
            self.root_visual
                .RemoveVisual(&self.visual)
                .and_then(|_| self.comp_device.Commit())
        };
        if let Err(error) = result {
            log::debug!("compositor overlay detach skipped: {error}");
        }
    }

    /// Draw the remembered content into the composition surface.
    ///
    /// Everything that can be rejected about the content is rejected before
    /// `BeginDraw`. Once the surface is open it is closed again on every path,
    /// so a rejected draw never leaves the surface mid-update.
    fn fill_surface(&self, surface: &IDCompositionSurface, width: u32, height: u32) -> Result<()> {
        if let Content::Rgba {
            width: content_width,
            height: content_height,
            pixels,
        } = &self.content
        {
            validate_rgba(*content_width, *content_height, pixels)?;
            validate_rgba_fits(*content_width, *content_height, width, height)?;
        }
        let rect = RECT {
            left: 0,
            top: 0,
            right: width as i32,
            bottom: height as i32,
        };
        let mut offset = POINT::default();
        let update: IDXGISurface1 = unsafe { surface.BeginDraw(Some(&rect), &mut offset)? };
        let drawn = self.draw_content(&update);
        let ended: Result<()> = unsafe { surface.EndDraw() }.map_err(Into::into);
        drawn.and(ended)
    }

    /// Draw the remembered content into the surface `BeginDraw` opened.
    fn draw_content(&self, update: &IDXGISurface1) -> Result<()> {
        let texture: ID3D11Texture2D = update.cast()?;
        match &self.content {
            Content::Solid(color) => {
                let mut render_target_view: Option<ID3D11RenderTargetView> = None;
                unsafe {
                    self.device.CreateRenderTargetView(
                        &texture,
                        None,
                        Some(&mut render_target_view),
                    )?
                };
                let render_target_view = render_target_view.context("no render target view")?;
                let [r, g, b, a] = *color;
                // Premultiplied alpha: the surface's alpha mode is premultiplied.
                unsafe {
                    self.device_context
                        .ClearRenderTargetView(&render_target_view, &[r * a, g * a, b * a, a])
                };
                Ok(())
            }
            Content::Rgba { width, pixels, .. } => {
                // The composition surface is B8G8R8A8 while this trait is
                // explicitly RGBA, so this one platform boundary swaps red and
                // blue and nothing above it knows the surface's format.
                let mut bgra = pixels.to_vec();
                for pixel in bgra.chunks_exact_mut(4) {
                    pixel.swap(0, 2);
                }
                unsafe {
                    self.device_context.UpdateSubresource(
                        &texture,
                        0,
                        None,
                        bgra.as_ptr() as _,
                        width * 4,
                        0,
                    );
                }
                Ok(())
            }
        }
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
        self.fill_surface(&surface, width, height)?;
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
        self.content = Content::Solid(color);
        if let Some(surface) = self.surface.clone() {
            self.fill_surface(&surface, self.width, self.height)?;
        }
        self.commit()
    }

    fn set_content_rgba_impl(&mut self, width: u32, height: u32, pixels: &[u8]) -> Result<()> {
        // The image is rejected here, before it becomes the remembered
        // content, so an invalid upload cannot poison a later draw or rebuild.
        validate_rgba(width, height, pixels)?;
        if self.surface.is_some() {
            // `set_geometry` owns how big the overlay is, so an image uploaded
            // into a placed overlay has to be the size of its surface.
            validate_rgba_fits(width, height, self.width, self.height)?;
        }
        self.content = Content::Rgba {
            width,
            height,
            pixels: pixels.to_vec(),
        };
        if let Some(surface) = self.surface.clone() {
            self.fill_surface(&surface, self.width, self.height)?;
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
            )?;
            // A one-shot DirectComposition animation needs an explicit finite
            // endpoint; without End, the final cubic segment keeps extending.
            animation.End(duration_s, target)?;
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

impl Drop for CompositorOverlay {
    fn drop(&mut self) {
        self.detach_visual();
    }
}

/// Reject an image that is not exactly the buffer it claims to be: dimensions
/// must be non-zero and `pixels` must hold exactly `width * height * 4` bytes.
fn validate_rgba(width: u32, height: u32, pixels: &[u8]) -> Result<()> {
    anyhow::ensure!(
        width > 0 && height > 0,
        "RGBA content dimensions must be non-zero"
    );
    let expected = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .context("RGBA content dimensions overflow")?;
    anyhow::ensure!(
        pixels.len() == expected,
        "RGBA content has {} bytes, expected {expected}",
        pixels.len()
    );
    Ok(())
}

/// Reject an image that does not cover the surface it is drawn into. The
/// surface is a fixed raster, so an image that does not fill it would be drawn
/// at the wrong scale.
fn validate_rgba_fits(
    width: u32,
    height: u32,
    surface_width: u32,
    surface_height: u32,
) -> Result<()> {
    anyhow::ensure!(
        width == surface_width && height == surface_height,
        "RGBA content is {width}x{height}, but the surface is {surface_width}x{surface_height}"
    );
    Ok(())
}

impl PlatformCompositorOverlay for CompositorOverlay {
    fn set_geometry(&mut self, x: f32, y: f32, width: u32, height: u32) {
        if let Err(error) = self.set_geometry_impl(x, y, width, height) {
            log::error!("compositor overlay set_geometry failed: {error}");
        }
    }

    fn set_content_rgba(&mut self, width: u32, height: u32, pixels: &[u8]) {
        if let Err(error) = self.set_content_rgba_impl(width, height, pixels) {
            log::error!("compositor overlay set_content_rgba failed: {error}");
        }
    }

    fn set_color(&mut self, color: [f32; 4]) {
        if let Err(error) = self.set_color_impl(color) {
            log::error!("compositor overlay set_color failed: {error}");
        }
    }

    fn fade_opacity(&mut self, target: f32, duration_s: f64) {
        if let Err(error) = self.fade_impl(target, duration_s) {
            log::error!("compositor overlay fade_opacity failed: {error}");
        }
    }

    fn animate_opacity_cycle(&mut self, segments: &[OpacitySegment]) {
        if let Err(error) = self.breathe_impl(segments) {
            log::error!("compositor overlay animate_opacity_cycle failed: {error}");
        }
    }

    fn hold_opacity(&mut self, opacity: f32) {
        if let Err(error) = self.hold_impl(opacity) {
            log::error!("compositor overlay hold_opacity failed: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{validate_rgba, validate_rgba_fits};

    #[test]
    fn rgba_content_must_be_a_complete_nonzero_buffer() {
        assert!(validate_rgba(2, 2, &[0; 16]).is_ok());
        assert!(validate_rgba(1, 1, &[0; 3]).is_err(), "short buffer");
        assert!(validate_rgba(1, 1, &[0; 5]).is_err(), "long buffer");
        assert!(validate_rgba(0, 1, &[]).is_err(), "zero width");
        assert!(validate_rgba(1, 0, &[]).is_err(), "zero height");
        assert!(
            validate_rgba(u32::MAX, u32::MAX, &[0; 4]).is_err(),
            "dimensions that overflow a byte count"
        );
    }

    #[test]
    fn rgba_content_must_cover_the_surface_it_is_drawn_into() {
        assert!(validate_rgba_fits(22, 22, 22, 22).is_ok());
        assert!(
            validate_rgba_fits(22, 22, 23, 22).is_err(),
            "narrow surface"
        );
        assert!(validate_rgba_fits(22, 22, 22, 21).is_err(), "short surface");
    }
}
