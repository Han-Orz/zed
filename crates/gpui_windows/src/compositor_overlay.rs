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
        Direct3D11::{D3D11_BOX, ID3D11DeviceContext, ID3D11Texture2D},
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

/// One color channel scaled by alpha and quantized to 8 bits, which is the
/// premultiplied form the composition surface stores.
fn premultiplied_channel(channel: f32, alpha: f32) -> u8 {
    (channel * alpha * 255.0).round().clamp(0.0, 255.0) as u8
}

/// The content as premultiplied RGBA8 covering `width`x`height`.
///
/// A solid color expands to a buffer of that size, which is what lets both
/// content kinds reach the surface through one region-limited upload.
fn content_pixels(content: &Content, width: u32, height: u32) -> Vec<u8> {
    match content {
        Content::Solid(color) => {
            let [r, g, b, a] = *color;
            let pixel = [
                premultiplied_channel(r, a),
                premultiplied_channel(g, a),
                premultiplied_channel(b, a),
                (a * 255.0).round().clamp(0.0, 255.0) as u8,
            ];
            let mut pixels = Vec::with_capacity(width as usize * height as usize * 4);
            for _ in 0..(width as usize * height as usize) {
                pixels.extend_from_slice(&pixel);
            }
            pixels
        }
        Content::Rgba { pixels, .. } => pixels.clone(),
    }
}

/// The size the overlay shows `content` at when it is placed at `placed`.
///
/// This is the whole resize contract in one expression: a solid color fills
/// whatever size it is placed at, while an image is exactly its own raster size,
/// because this overlay never scales one. A placement that an image does not fill
/// is therefore not applied to the surface — the image, and the surface holding
/// it, stay complete until the image for that size arrives, which is one
/// `set_content_rgba` call away.
fn shown_size(content: &Content, placed: (u32, u32)) -> (u32, u32) {
    match content {
        Content::Solid(_) => placed,
        Content::Rgba { width, height, .. } => (*width, *height),
    }
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
    /// device after device loss, preserving the app's position, content and
    /// opacity state. The `Rc` the app holds stays valid.
    pub(crate) fn rebuild(
        &mut self,
        devices: &DirectXRendererDevices,
        composition: &DirectComposition,
    ) -> Result<()> {
        // The size the overlay was showing at, captured before the rebuild clears
        // it: the surface comes back from the remembered content, so its size is
        // the size that content is shown at rather than one a placement asked for.
        let placed = (self.width, self.height);
        let visual = unsafe { composition.comp_device().CreateVisual() }?;
        let opacity_visual: IDCompositionVisual3 = visual.cast()?;
        let root_visual = composition.root_visual().clone();
        // The old visual belongs to the lost device, so detaching it is a
        // best effort against a root that is already gone.
        self.detach_visual();
        self.comp_device = composition.comp_device().clone();
        self.device_context = devices.device_context.clone();
        self.root_visual = root_visual.clone();
        self.visual = visual.clone();
        self.opacity_visual = opacity_visual;
        // The content surface belonged to the lost device; the content the app
        // committed did not. Clearing the remembered size makes the draw below
        // recreate the surface at the size that content is shown at, drawing the
        // remembered content into it, instead of leaving it contentless.
        self.surface = None;
        self.width = 0;
        self.height = 0;
        unsafe {
            visual.SetContent(None::<&windows::core::IUnknown>)?;
            root_visual.AddVisual(&visual, true, None)?;
        }
        let (width, height) = shown_size(&self.content, placed);
        if width > 0 && height > 0 {
            self.recreate_surface(width, height)?;
        }
        self.apply_position()?;
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
        // `BeginDraw` returns an update surface that is generally a sub-rectangle
        // of the composition surface, located at this returned origin. Even when
        // the whole logical surface is requested the origin is not promised to be
        // `(0, 0)`, so the content has to be written at it rather than assumed to
        // start at the update surface's own origin.
        let mut offset = POINT::default();
        let update: IDXGISurface1 = unsafe { surface.BeginDraw(Some(&rect), &mut offset)? };
        let drawn = self.draw_content(&update, offset, width, height);
        let ended: Result<()> = unsafe { surface.EndDraw() }.map_err(Into::into);
        drawn.and(ended)
    }

    /// Draw the remembered content into the surface `BeginDraw` opened, at the
    /// `offset` that call reported for this update region.
    ///
    /// Both content kinds reach the surface the same way: one tightly packed
    /// premultiplied image written into the region `BeginDraw` allocated. A
    /// solid color is rasterized into that image rather than cleared through a
    /// render target view, because a clear covers the whole texture while the
    /// update surface is generally a sub-rectangle of the composition surface —
    /// clearing it would write pixels outside the requested region.
    fn draw_content(
        &self,
        update: &IDXGISurface1,
        offset: POINT,
        width: u32,
        height: u32,
    ) -> Result<()> {
        let texture: ID3D11Texture2D = update.cast()?;
        // The composition surface is B8G8R8A8 while this trait is explicitly
        // RGBA, so this one platform boundary swaps red and blue and nothing
        // above it knows the surface's format.
        let mut bgra = self.content_pixels(width, height);
        for pixel in bgra.chunks_exact_mut(4) {
            pixel.swap(0, 2);
        }
        let (update_width, update_height) = surface_dimensions(update)?;
        let destination = destination_box(width, height, offset)
            .context("compositor update offset is outside the update surface")?;
        validate_destination(&destination, update_width, update_height)?;
        unsafe {
            self.device_context.UpdateSubresource(
                &texture,
                0,
                Some(&destination),
                bgra.as_ptr() as _,
                width * 4,
                0,
            );
        }
        Ok(())
    }

    /// The remembered content as premultiplied RGBA8 covering `width`x`height`.
    ///
    /// A solid color expands to a buffer of that size, which is what lets both
    /// content kinds use the same region-limited upload.
    fn content_pixels(&self, width: u32, height: u32) -> Vec<u8> {
        content_pixels(&self.content, width, height)
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

    /// Place the overlay at `x`,`y` in the size the remembered content is shown
    /// at, recreating the surface when it does not already have that size.
    ///
    /// The position always applies. The size only applies where the content can
    /// fill it: a placement an image does not fill leaves the surface — and the
    /// complete image in it — alone, rather than resizing it onto content that
    /// cannot cover it. The image for the new size arrives in the same update and
    /// resizes the surface itself.
    fn set_geometry_impl(&mut self, x: f32, y: f32, width: u32, height: u32) -> Result<()> {
        self.x = x;
        self.y = y;
        let (shown_width, shown_height) = shown_size(&self.content, (width, height));
        if (self.width, self.height) != (shown_width, shown_height) {
            self.recreate_surface(shown_width, shown_height)?;
        }
        self.apply_position()
    }

    /// Move the visual to the overlay's committed position and commit.
    ///
    /// The compositor places the visual at a sub-pixel offset: an animated
    /// position is not rounded to whole pixels here.
    fn apply_position(&self) -> Result<()> {
        unsafe { self.visual.SetOffsetX2(self.x)? };
        unsafe { self.visual.SetOffsetY2(self.y)? };
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
        self.content = Content::Rgba {
            width,
            height,
            pixels: pixels.to_vec(),
        };
        if let Some(surface) = self.surface.clone() {
            // An image is shown at its own raster size, so an image of another size
            // resizes the surface with it — in one operation, drawn before it is
            // attached, which is what lets a placement that asked for the new size
            // and the image that fills it arrive in either order without a surface
            // holding content that does not cover it.
            let (shown_width, shown_height) = shown_size(&self.content, (self.width, self.height));
            if (shown_width, shown_height) != (self.width, self.height) {
                self.recreate_surface(shown_width, shown_height)?;
            } else {
                self.fill_surface(&surface, self.width, self.height)?;
            }
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

/// The destination box for content of `width`x`height` written at the origin
/// `BeginDraw` reported for this update.
///
/// Returns `None` when that origin cannot be expressed as a D3D11 box: an
/// update offset is a signed pixel coordinate, so a negative origin has no
/// destination inside the update surface and is a caller-visible failure
/// rather than something to wrap into the wrong place.
fn destination_box(width: u32, height: u32, offset: POINT) -> Option<D3D11_BOX> {
    let left = u32::try_from(offset.x).ok()?;
    let top = u32::try_from(offset.y).ok()?;
    let right = left.checked_add(width)?;
    let bottom = top.checked_add(height)?;
    Some(D3D11_BOX {
        left,
        top,
        front: 0,
        right,
        bottom,
        back: 1,
    })
}

/// Reject a destination box that leaves the update surface. A box that does not
/// fit would be silently clipped by D3D11, drawing a cropped image at a wrong
/// scale instead of failing loudly.
fn validate_destination(
    destination: &D3D11_BOX,
    update_width: u32,
    update_height: u32,
) -> Result<()> {
    anyhow::ensure!(
        destination.right <= update_width && destination.bottom <= update_height,
        "compositor update destination {}x{} leaves the {update_width}x{update_height} update surface",
        destination.right,
        destination.bottom
    );
    Ok(())
}

/// The pixel size of the update surface `BeginDraw` returned, which bounds the
/// destination the content may be written to.
fn surface_dimensions(update: &IDXGISurface1) -> Result<(u32, u32)> {
    let description = unsafe { update.GetDesc()? };
    Ok((description.Width, description.Height))
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
    use super::{
        Content, content_pixels, destination_box, shown_size, validate_destination, validate_rgba,
        validate_rgba_fits,
    };
    use windows::Win32::Foundation::POINT;

    /// An image of `width`x`height` whose pixel values are not under test.
    fn image(width: u32, height: u32) -> Content {
        Content::Rgba {
            width,
            height,
            pixels: vec![0; width as usize * height as usize * 4],
        }
    }

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

    /// The update surface's returned origin is where the content must be
    /// written; zero is only one of the origins `BeginDraw` may report.
    #[test]
    fn content_destination_starts_at_the_reported_update_offset() {
        let zero = destination_box(8, 4, POINT { x: 0, y: 0 }).expect("zero offset");
        assert_eq!((zero.left, zero.top, zero.right, zero.bottom), (0, 0, 8, 4));
        assert_eq!((zero.front, zero.back), (0, 1), "one depth slice");

        let shifted_x = destination_box(8, 4, POINT { x: 3, y: 0 }).expect("positive x");
        assert_eq!(
            (
                shifted_x.left,
                shifted_x.top,
                shifted_x.right,
                shifted_x.bottom
            ),
            (3, 0, 11, 4)
        );

        let shifted_y = destination_box(8, 4, POINT { x: 0, y: 5 }).expect("positive y");
        assert_eq!(
            (
                shifted_y.left,
                shifted_y.top,
                shifted_y.right,
                shifted_y.bottom
            ),
            (0, 5, 8, 9)
        );

        let shifted = destination_box(8, 4, POINT { x: 2, y: 5 }).expect("x and y");
        assert_eq!(
            (shifted.left, shifted.top, shifted.right, shifted.bottom),
            (2, 5, 10, 9),
            "an offset moves the destination in both axes"
        );
    }

    #[test]
    fn a_negative_update_offset_has_no_destination() {
        assert!(destination_box(8, 4, POINT { x: -1, y: 0 }).is_none());
        assert!(destination_box(8, 4, POINT { x: 0, y: -1 }).is_none());
    }

    #[test]
    fn an_origin_that_would_overflow_has_no_destination() {
        assert!(destination_box(u32::MAX, 1, POINT { x: 1, y: 0 }).is_none());
        assert!(destination_box(1, u32::MAX, POINT { x: 0, y: 1 }).is_none());
    }

    /// A destination that does not fit is rejected instead of being silently
    /// clipped into a cropped draw at the wrong place.
    #[test]
    fn a_destination_must_stay_inside_the_update_surface() {
        let fits = destination_box(8, 4, POINT { x: 2, y: 1 }).expect("offset");
        assert!(validate_destination(&fits, 10, 5).is_ok(), "exactly fits");
        assert!(
            validate_destination(&fits, 9, 5).is_err(),
            "overflows width"
        );
        assert!(
            validate_destination(&fits, 10, 4).is_err(),
            "overflows height"
        );
    }

    /// A solid color becomes a premultiplied image covering exactly the
    /// requested region, so it reaches the surface through the same
    /// region-limited upload as an image instead of clearing a whole render
    /// target that may extend past the update rectangle.
    #[test]
    fn a_solid_color_rasterizes_to_a_premultiplied_image() {
        let overlay = Content::Solid([0.2, 0.4, 0.6, 0.5]);
        let pixels = content_pixels(&overlay, 2, 2);
        assert_eq!(pixels.len(), 16, "2x2 premultiplied RGBA");
        // Premultiplied: each channel scaled by alpha 0.5.
        assert_eq!(&pixels[0..4], &[26, 51, 77, 128]);
        assert!(
            pixels.chunks_exact(4).all(|pixel| pixel == &pixels[0..4]),
            "every pixel of a solid fill is the same"
        );
    }

    /// The image kind keeps its own pixels; only the byte order is swapped at
    /// the surface boundary, never inside this buffer.
    #[test]
    fn an_image_keeps_its_pixels() {
        let source = vec![1, 2, 3, 4, 5, 6, 7, 8];
        let content = Content::Rgba {
            width: 2,
            height: 1,
            pixels: source.clone(),
        };
        assert_eq!(content_pixels(&content, 2, 1), source);
    }
    // ---- the resize contract -----------------------------------------------
    //
    // These exercise the size rule the surface operations are built on, purely:
    // creating, drawing into and attaching a real DirectComposition surface needs
    // a device and stays a Windows integration concern.

    /// The update the app really performs — place the overlay, then upload the
    /// image for that placement — ends at the image's own size, in both
    /// directions, and the surface holds a complete image at every step.
    #[test]
    fn an_image_of_another_size_resizes_the_surface_with_it() {
        // Growing: the placement arrives while the previous image is remembered.
        assert_eq!(
            shown_size(&image(24, 24), (30, 30)),
            (24, 24),
            "an image is shown at its own size, never stretched to the placement"
        );
        // The image for the new size arrives in the same update and resizes it.
        assert_eq!(shown_size(&image(30, 30), (30, 30)), (30, 30), "larger");

        // Shrinking behaves the same way.
        assert_eq!(
            shown_size(&image(30, 30), (18, 18)),
            (30, 30),
            "the image on screen stays complete until its replacement arrives"
        );
        assert_eq!(shown_size(&image(18, 18), (18, 18)), (18, 18), "smaller");
    }

    /// An image that already fills the surface is drawn into the surface it has:
    /// neither a same-size update nor a plain move recreates anything.
    #[test]
    fn a_same_size_image_updates_the_surface_it_already_has() {
        let content = image(24, 24);
        let surface = (24, 24);
        assert_eq!(
            shown_size(&content, surface),
            surface,
            "a same-size image, and a move of one, leave the surface size alone"
        );
    }

    /// A solid color fills whatever size it is placed at, and the image that
    /// replaces it takes the size over from then on.
    #[test]
    fn a_solid_color_and_an_image_swap_the_size_authority() {
        let solid = Content::Solid([0.2, 0.4, 0.6, 1.0]);
        assert_eq!(shown_size(&solid, (30, 30)), (30, 30), "a solid fills it");
        assert_eq!(shown_size(&solid, (18, 18)), (18, 18), "at any size");
        assert_eq!(
            shown_size(&image(24, 24), (30, 30)),
            (24, 24),
            "an image owns the size, not the placement"
        );
        assert_eq!(
            shown_size(&solid, (24, 24)),
            (24, 24),
            "and a solid replacing one keeps the size the surface has"
        );
    }

    /// A device-loss rebuild restores what the overlay was showing, at the size
    /// that content is shown at: an image's own raster size, or the placement for
    /// a solid color. A surface the lost device had is never reused.
    #[test]
    fn a_rebuild_restores_the_remembered_content_at_the_size_it_is_shown_at() {
        assert_eq!(shown_size(&image(30, 30), (30, 30)), (30, 30), "an image");
        let solid = Content::Solid([0.2, 0.4, 0.6, 1.0]);
        assert_eq!(shown_size(&solid, (30, 30)), (30, 30), "a solid color");
        assert_eq!(
            shown_size(&solid, (0, 0)),
            (0, 0),
            "an overlay that was never placed has no surface to rebuild"
        );
    }
}
