use std::{
    slice,
    sync::{Arc, OnceLock},
    time::Instant,
};

use anyhow::{Context, Result};
use gpui_util::ResultExt;
use windows::{
    Win32::{
        Foundation::{HWND, POINT, RECT},
        Graphics::{
            Direct3D::*,
            Direct3D11::*,
            DirectComposition::*,
            DirectWrite::*,
            Dxgi::{Common::*, *},
        },
        System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency},
    },
    core::{HSTRING, Interface},
};

use crate::directx_renderer::shader_resources::{RawShaderBytes, ShaderModule, ShaderTarget};
use crate::*;
use gpui::*;

pub(crate) const DISABLE_DIRECT_COMPOSITION: &str = "GPUI_DISABLE_DIRECT_COMPOSITION";
const RENDER_TARGET_FORMAT: DXGI_FORMAT = DXGI_FORMAT_B8G8R8A8_UNORM;
// This configuration is used for MSAA rendering on paths only, and it's guaranteed to be supported by DirectX 11.
const PATH_MULTISAMPLE_COUNT: u32 = 4;
const MAX_INSTANCE_BUFFER_SIZE: usize = 256 * 1024 * 1024;

pub(crate) struct FontInfo {
    pub gamma_ratios: [f32; 4],
    pub grayscale_enhanced_contrast: f32,
    pub subpixel_enhanced_contrast: f32,
    pub is_bgr: bool,
}

pub(crate) struct DirectXRenderer {
    hwnd: HWND,
    atlas: Arc<DirectXAtlas>,
    devices: Option<DirectXRendererDevices>,
    resources: Option<DirectXResources>,
    globals: DirectXGlobalElements,
    pipelines: DirectXRenderPipelines,
    direct_composition: Option<DirectComposition>,
    #[cfg(feature = "diagnostics")]
    present_count: u64,
    font_info: &'static FontInfo,
    local_target: Option<LocalTarget>,
    timebase: Option<CompositionTimebase>,

    width: u32,
    height: u32,

    /// Whether we want to skip drwaing due to device lost events.
    ///
    /// In that case we want to discard the first frame that we draw as we got reset in the middle of a frame
    /// meaning we lost all the allocated gpu textures and scene resources.
    skip_draws: bool,
}

/// Direct3D objects
#[derive(Clone)]
pub(crate) struct DirectXRendererDevices {
    pub(crate) adapter: IDXGIAdapter1,
    pub(crate) dxgi_factory: IDXGIFactory6,
    pub(crate) device: ID3D11Device,
    pub(crate) device_context: ID3D11DeviceContext,
    dxgi_device: Option<IDXGIDevice>,
    annotation: Option<ID3DUserDefinedAnnotation>,
}

struct DirectXResources {
    // Direct3D rendering objects
    swap_chain: Option<IDXGISwapChain1>,
    render_target: Option<ID3D11Texture2D>,
    render_target_view: Option<ID3D11RenderTargetView>,

    // Path intermediate textures (with MSAA)
    path_intermediate_texture: ID3D11Texture2D,
    path_intermediate_srv: Option<ID3D11ShaderResourceView>,
    path_intermediate_msaa_texture: ID3D11Texture2D,
    path_intermediate_msaa_view: Option<ID3D11RenderTargetView>,
}

#[derive(Clone)]
struct PathAttachments {
    texture: ID3D11Texture2D,
    srv: Option<ID3D11ShaderResourceView>,
    msaa: ID3D11Texture2D,
    view: Option<ID3D11RenderTargetView>,
    width: u32,
    height: u32,
}

impl PathAttachments {
    fn new(device: &ID3D11Device, width: u32, height: u32) -> Result<Self> {
        let (texture, srv) = create_path_intermediate_texture(device, width, height)?;
        let (msaa, view) = create_path_intermediate_msaa_texture_and_view(device, width, height)?;
        Ok(Self {
            texture,
            srv,
            msaa,
            view,
            width,
            height,
        })
    }
}

struct LocalTarget {
    #[cfg(any(test, feature = "test-support"))]
    texture: ID3D11Texture2D,
    #[cfg(any(test, feature = "test-support"))]
    view: Option<ID3D11RenderTargetView>,
    paths: PathAttachments,
}

#[cfg(any(test, feature = "test-support"))]
struct PassMeasurement {
    disjoint: ID3D11Query,
    start: ID3D11Query,
    end: ID3D11Query,
    cpu: Instant,
    encoded_us: u128,
}

#[cfg(any(test, feature = "test-support"))]
impl PassMeasurement {
    fn begin(devices: &DirectXRendererDevices) -> Result<Self> {
        let query = |kind| -> Result<ID3D11Query> {
            let mut result = None;
            unsafe {
                devices.device.CreateQuery(
                    &D3D11_QUERY_DESC {
                        Query: kind,
                        MiscFlags: 0,
                    },
                    Some(&mut result),
                )?;
            }
            result.context("GPU query missing")
        };
        let result = Self {
            disjoint: query(D3D11_QUERY_TIMESTAMP_DISJOINT)?,
            start: query(D3D11_QUERY_TIMESTAMP)?,
            end: query(D3D11_QUERY_TIMESTAMP)?,
            cpu: Instant::now(),
            encoded_us: 0,
        };
        unsafe {
            devices.device_context.Begin(&result.disjoint);
            devices.device_context.End(&result.start);
        }
        Ok(result)
    }
    fn end(&mut self, context: &ID3D11DeviceContext) {
        self.encoded_us = self.cpu.elapsed().as_micros();
        unsafe {
            context.End(&self.end);
            context.End(&self.disjoint);
        }
    }
    fn report(&self, context: &ID3D11DeviceContext, label: &str, area: u32) -> Result<()> {
        let mut disjoint = D3D11_QUERY_DATA_TIMESTAMP_DISJOINT::default();
        let mut start = 0u64;
        let mut end = 0u64;
        unsafe {
            context.GetData(
                &self.disjoint,
                Some(&mut disjoint as *mut _ as _),
                std::mem::size_of_val(&disjoint) as u32,
                0,
            )?;
            context.GetData(&self.start, Some(&mut start as *mut _ as _), 8, 0)?;
            context.GetData(&self.end, Some(&mut end as *mut _ as _), 8, 0)?;
        }
        anyhow::ensure!(
            !disjoint.Disjoint.as_bool() && disjoint.Frequency > 0 && end >= start,
            "invalid GPU timing sample"
        );
        eprintln!(
            "COST pass={label} area={area} cpu_us={} gpu_us={:.3}",
            self.encoded_us,
            (end - start) as f64 * 1e6 / disjoint.Frequency as f64
        );
        Ok(())
    }
}

/// Source coordinates stay in the scene; only the destination viewport and
/// fragment position mapping change. Path attachments have their own origin.
struct DrawTarget {
    view: Option<ID3D11RenderTargetView>,
    region: RECT,
    offset: POINT,
    paths: PathAttachments,
}

#[derive(Clone, Copy)]
enum ScenePass {
    Base,
    LocalEndpoint,
    #[cfg(any(test, feature = "test-support"))]
    Reference(f32),
}

impl DrawTarget {
    fn viewport(&self) -> D3D11_VIEWPORT {
        D3D11_VIEWPORT {
            TopLeftX: self.offset.x as f32,
            TopLeftY: self.offset.y as f32,
            Width: (self.region.right - self.region.left) as f32,
            Height: (self.region.bottom - self.region.top) as f32,
            MinDepth: 0.0,
            MaxDepth: 1.0,
        }
    }

    fn bounds(&self) -> Bounds<ScaledPixels> {
        Bounds::from_corners(
            point(
                ScaledPixels(self.region.left as f32),
                ScaledPixels(self.region.top as f32),
            ),
            point(
                ScaledPixels(self.region.right as f32),
                ScaledPixels(self.region.bottom as f32),
            ),
        )
    }
}

struct DirectXRenderPipelines {
    shadow_pipeline: PipelineState<Shadow>,
    quad_pipeline: PipelineState<Quad>,
    path_rasterization_pipeline: PipelineState<PathRasterizationSprite>,
    path_sprite_pipeline: PipelineState<PathSprite>,
    underline_pipeline: PipelineState<Underline>,
    mono_sprites: PipelineState<MonochromeSprite>,
    subpixel_sprites: PipelineState<SubpixelSprite>,
    poly_sprites: PipelineState<PolychromeSprite>,
}

struct DirectXGlobalElements {
    global_params_buffer: Option<ID3D11Buffer>,
    batch_params_buffer: Option<ID3D11Buffer>,
    sampler: Option<ID3D11SamplerState>,
}

struct Annotation<'a>(&'a ID3DUserDefinedAnnotation);

impl<'a> Annotation<'a> {
    fn new(annotation: &'a ID3DUserDefinedAnnotation, label: HSTRING) -> Self {
        unsafe { annotation.BeginEvent(&label) };
        Self(annotation)
    }
}

impl Drop for Annotation<'_> {
    fn drop(&mut self) {
        unsafe { self.0.EndEvent() };
    }
}

pub(crate) struct DirectComposition {
    comp_device: IDCompositionDevice,
    _comp_target: IDCompositionTarget,
    _root_visual: IDCompositionVisual,
    main_visual: IDCompositionVisual,
    local_visual: IDCompositionVisual3,
    main_surface: Option<CompositionSurface>,
    local_surface: Option<CompositionSurface>,
    track: Option<Arc<OpacityTrack>>,
}

struct CompositionSurface {
    surface: IDCompositionSurface,
    width: u32,
    height: u32,
}

struct CompositionTimebase {
    app: Instant,
    qpc: i64,
    frequency: i64,
}

impl CompositionTimebase {
    fn new(app: Instant) -> Result<Self> {
        let mut qpc = 0;
        let mut frequency = 0;
        unsafe {
            QueryPerformanceCounter(&mut qpc)?;
            QueryPerformanceFrequency(&mut frequency)?;
        }
        Ok(Self {
            app,
            qpc,
            frequency,
        })
    }

    fn qpc_at(&self, at: Instant) -> i64 {
        let seconds = if at >= self.app {
            at.duration_since(self.app).as_secs_f64()
        } else {
            -self.app.duration_since(at).as_secs_f64()
        };
        self.qpc + (seconds * self.frequency as f64).round() as i64
    }
}

impl DirectXRendererDevices {
    pub(crate) fn new(
        directx_devices: &DirectXDevices,
        disable_direct_composition: bool,
    ) -> Result<Self> {
        let DirectXDevices {
            adapter,
            dxgi_factory,
            device,
            device_context,
        } = directx_devices;
        let dxgi_device = if disable_direct_composition {
            None
        } else {
            Some(device.cast().context("Creating DXGI device")?)
        };
        let annotation = device_context.cast().ok();

        Ok(Self {
            adapter: adapter.clone(),
            dxgi_factory: dxgi_factory.clone(),
            device: device.clone(),
            device_context: device_context.clone(),
            dxgi_device,
            annotation,
        })
    }
}

impl DirectXRenderer {
    pub(crate) fn new(
        hwnd: HWND,
        directx_devices: &DirectXDevices,
        disable_direct_composition: bool,
    ) -> Result<Self> {
        if disable_direct_composition {
            log::info!("Direct Composition is disabled.");
        }

        let devices = DirectXRendererDevices::new(directx_devices, disable_direct_composition)
            .context("Creating DirectX devices")?;
        let atlas = Arc::new(DirectXAtlas::new(&devices.device, &devices.device_context));

        let resources = DirectXResources::new(&devices, 1, 1, hwnd, disable_direct_composition)
            .context("Creating DirectX resources")?;
        let globals = DirectXGlobalElements::new(&devices.device)
            .context("Creating DirectX global elements")?;
        let pipelines = DirectXRenderPipelines::new(&devices.device)
            .context("Creating DirectX render pipelines")?;

        let direct_composition = if disable_direct_composition {
            None
        } else {
            let composition = DirectComposition::new(devices.dxgi_device.as_ref().unwrap(), hwnd)
                .context("Creating DirectComposition")?;
            Some(composition)
        };

        Ok(DirectXRenderer {
            hwnd,
            atlas,
            devices: Some(devices),
            resources: Some(resources),
            globals,
            pipelines,
            direct_composition,
            #[cfg(feature = "diagnostics")]
            present_count: 0,
            font_info: Self::get_font_info(),
            local_target: None,
            timebase: None,
            width: 1,
            height: 1,
            skip_draws: false,
        })
    }

    pub(crate) fn sprite_atlas(&self) -> Arc<dyn PlatformAtlas> {
        self.atlas.clone()
    }

    /// Monotonic renderer present count, maintained only in a build that keeps
    /// the platform's presentation evidence (Diagnostic Release).
    #[cfg(feature = "diagnostics")]
    pub(crate) fn present_count(&self) -> u64 {
        self.present_count
    }

    fn pre_draw(&self, target: &DrawTarget, clear_color: &[f32; 4]) -> Result<()> {
        let device_context = &self
            .devices
            .as_ref()
            .expect("devices missing")
            .device_context;
        self.bind_target(target)?;
        unsafe {
            let context: ID3D11DeviceContext1 = device_context.cast()?;
            let viewport = target.viewport();
            context.ClearView(
                target.view.as_ref().context("missing render target view")?,
                clear_color,
                Some(&[RECT {
                    left: target.offset.x,
                    top: target.offset.y,
                    right: target.offset.x + viewport.Width as i32,
                    bottom: target.offset.y + viewport.Height as i32,
                }]),
            );
        }
        Ok(())
    }

    fn bind_target(&self, target: &DrawTarget) -> Result<()> {
        let device_context = &self
            .devices
            .as_ref()
            .context("devices missing")?
            .device_context;
        let viewport = target.viewport();
        update_buffer(
            device_context,
            self.globals.global_params_buffer.as_ref().unwrap(),
            &[GlobalParams {
                gamma_ratios: self.font_info.gamma_ratios,
                viewport_size: [viewport.Width, viewport.Height],
                grayscale_enhanced_contrast: self.font_info.grayscale_enhanced_contrast,
                subpixel_enhanced_contrast: self.font_info.subpixel_enhanced_contrast,
                is_bgr: self.font_info.is_bgr as u32,
                _pad: [0; 3],
                source_origin: [target.region.left as f32, target.region.top as f32],
                target_offset: [target.offset.x as f32, target.offset.y as f32],
                path_texture_size: [target.paths.width as f32, target.paths.height as f32],
                _target_pad: [0; 2],
            }],
        )?;
        unsafe {
            device_context.OMSetRenderTargets(Some(slice::from_ref(&target.view)), None);
            device_context.RSSetViewports(Some(slice::from_ref(&viewport)));
            device_context.RSSetScissorRects(Some(&[RECT {
                left: target.offset.x,
                top: target.offset.y,
                right: target.offset.x + viewport.Width as i32,
                bottom: target.offset.y + viewport.Height as i32,
            }]));
            device_context
                .VSSetConstantBuffers(0, Some(slice::from_ref(&self.globals.global_params_buffer)));
            device_context
                .VSSetConstantBuffers(1, Some(slice::from_ref(&self.globals.batch_params_buffer)));
            device_context
                .PSSetConstantBuffers(0, Some(slice::from_ref(&self.globals.global_params_buffer)));
        }
        Ok(())
    }

    #[inline]
    fn present(&mut self) -> Result<()> {
        let result = unsafe {
            self.resources
                .as_ref()
                .expect("resources missing")
                .swap_chain
                .as_ref()
                .context("swap chain unavailable")?
                .Present(0, DXGI_PRESENT(0))
        };
        result.ok().context("Presenting swap chain failed")?;
        #[cfg(feature = "diagnostics")]
        {
            self.present_count += 1;
        }
        Ok(())
    }

    pub(crate) fn handle_device_lost(&mut self, directx_devices: &DirectXDevices) -> Result<()> {
        let disable_direct_composition = self
            .resources
            .as_ref()
            .context("resources missing")?
            .swap_chain
            .is_some();
        try_to_recover_from_device_lost(|| {
            self.handle_device_lost_impl(directx_devices, disable_direct_composition)
                .context("DirectXRenderer handling device lost")
        })
    }

    fn handle_device_lost_impl(
        &mut self,
        directx_devices: &DirectXDevices,
        disable_direct_composition: bool,
    ) -> Result<()> {
        unsafe {
            #[cfg(debug_assertions)]
            if let Some(devices) = &self.devices {
                report_live_objects(&devices.device)
                    .context("Failed to report live objects after device lost")
                    .log_err();
            }

            self.resources.take();
            if let Some(devices) = &self.devices {
                devices.device_context.OMSetRenderTargets(None, None);
                devices.device_context.ClearState();
                devices.device_context.Flush();
                #[cfg(debug_assertions)]
                report_live_objects(&devices.device)
                    .context("Failed to report live objects after device lost")
                    .log_err();
            }

            self.direct_composition.take();
            self.devices.take();
        }

        let devices = DirectXRendererDevices::new(directx_devices, disable_direct_composition)
            .context("Recreating DirectX devices")?;
        let resources = DirectXResources::new(
            &devices,
            self.width,
            self.height,
            self.hwnd,
            disable_direct_composition,
        )
        .context("Creating DirectX resources")?;
        let globals = DirectXGlobalElements::new(&devices.device)
            .context("Creating DirectXGlobalElements")?;
        let pipelines = DirectXRenderPipelines::new(&devices.device)
            .context("Creating DirectXRenderPipelines")?;

        let direct_composition = if disable_direct_composition {
            None
        } else {
            let composition =
                DirectComposition::new(devices.dxgi_device.as_ref().unwrap(), self.hwnd)?;
            Some(composition)
        };

        self.atlas
            .handle_device_lost(&devices.device, &devices.device_context);

        unsafe {
            devices
                .device_context
                .OMSetRenderTargets(Some(slice::from_ref(&resources.render_target_view)), None);
        }
        self.devices = Some(devices);
        self.resources = Some(resources);
        self.globals = globals;
        self.pipelines = pipelines;
        self.direct_composition = direct_composition;
        self.local_target = None;
        self.skip_draws = true;
        Ok(())
    }

    pub(crate) fn draw(
        &mut self,
        scene: &Scene,
        background_appearance: WindowBackgroundAppearance,
    ) -> Result<()> {
        if self.skip_draws {
            // skip drawing this frame, we just recovered from a device lost event
            // and so likely do not have the textures anymore that are required for drawing
            return Ok(());
        }
        if self
            .resources
            .as_ref()
            .context("resources missing")?
            .swap_chain
            .is_none()
        {
            self.draw_composition(scene, background_appearance)
        } else {
            anyhow::ensure!(
                scene.opacity_quad.is_none(),
                "opacity tracks require DirectComposition on Windows"
            );
            self.render(scene, background_appearance)?;
            self.present()
        }
    }

    fn draw_composition(
        &mut self,
        scene: &Scene,
        appearance: WindowBackgroundAppearance,
    ) -> Result<()> {
        if scene.opacity_quad.is_some() {
            anyhow::ensure!(
                appearance == WindowBackgroundAppearance::Opaque,
                "opacity endpoint evaluation requires an opaque window"
            );
        }
        let devices = self.devices.as_ref().context("devices missing")?.clone();
        if self.timebase.is_none() {
            self.timebase = Some(CompositionTimebase::new(
                scene
                    .presentation_time
                    .context("scene submission time missing")?,
            )?);
        }
        let mut composition = match self.direct_composition.take() {
            Some(composition) => composition,
            None => DirectComposition::new(
                devices
                    .dxgi_device
                    .as_ref()
                    .context("DXGI device missing")?,
                self.hwnd,
            )?,
        };
        let result = (|| {
            composition.ensure_main(self.width, self.height)?;
            let surface = &composition
                .main_surface
                .as_ref()
                .context("main surface missing")?
                .surface;
            let target = self.main_target()?;
            let requested = target.region;
            self.draw_surface(scene, appearance, surface, requested, target, false)?;
            #[cfg(any(test, feature = "test-support"))]
            opacity_probe_pause();

            if let Some(region) = self.opacity_region(scene) {
                let width = (region.right - region.left) as u32;
                let height = (region.bottom - region.top) as u32;
                composition.ensure_local(width, height)?;
                let target = self.local_target(region)?;
                let surface = &composition
                    .local_surface
                    .as_ref()
                    .context("local surface missing")?
                    .surface;
                let allocation = composition
                    .local_surface
                    .as_ref()
                    .context("local surface missing")?;
                let requested = RECT {
                    left: 0,
                    top: 0,
                    right: allocation.width as i32,
                    bottom: allocation.height as i32,
                };
                self.draw_surface(scene, appearance, surface, requested, target, true)?;
                unsafe {
                    composition.local_visual.SetContent(surface)?;
                    composition.local_visual.SetOffsetX2(region.left as f32)?;
                    composition.local_visual.SetOffsetY2(region.top as f32)?;
                    composition.local_visual.SetClip2(
                        &windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F {
                            left: 0.0,
                            top: 0.0,
                            right: width as f32,
                            bottom: height as f32,
                        },
                    )?;
                }
                let track = &scene
                    .opacity_quad
                    .as_ref()
                    .context("opacity quad missing")?
                    .track;
                if !composition
                    .track
                    .as_ref()
                    .is_some_and(|old| Arc::ptr_eq(old, track))
                {
                    composition
                        .bind_track(track, self.timebase.as_ref().context("timebase missing")?)?;
                    composition.track = Some(track.clone());
                }
            } else {
                unsafe {
                    composition
                        .local_visual
                        .SetContent(None::<&windows::core::IUnknown>)?;
                }
                composition.track = None;
            }
            unsafe {
                #[cfg(any(test, feature = "test-support"))]
                opacity_probe_pause();
                composition.main_visual.SetContent(
                    &composition
                        .main_surface
                        .as_ref()
                        .context("main surface missing")?
                        .surface,
                )?;
                composition.comp_device.Commit()?;
            }
            #[cfg(feature = "diagnostics")]
            {
                self.present_count += 1;
            }
            Ok(())
        })();
        // An unsuccessful preparation must never leak pending surface updates
        // into a later Commit. Recreate this derived native device next frame.
        if result.is_ok() {
            self.direct_composition = Some(composition);
        }
        result
    }

    fn draw_surface(
        &mut self,
        scene: &Scene,
        appearance: WindowBackgroundAppearance,
        surface: &IDCompositionSurface,
        requested: RECT,
        mut target: DrawTarget,
        local: bool,
    ) -> Result<()> {
        let mut offset = POINT::default();
        let update: IDXGISurface = unsafe { surface.BeginDraw(Some(&requested), &mut offset)? };
        let drawn = (|| {
            let texture: ID3D11Texture2D = update.cast()?;
            let mut description = D3D11_TEXTURE2D_DESC::default();
            unsafe {
                texture.GetDesc(&mut description);
            }
            #[cfg(any(test, feature = "test-support"))]
            if std::env::var_os("GPUI_ATOMIC_DELAY_MS").is_some() {
                eprintln!(
                    "SURFACE local={local} requested={}x{} offset={},{} texture={}x{}",
                    requested.right - requested.left,
                    requested.bottom - requested.top,
                    offset.x,
                    offset.y,
                    description.Width,
                    description.Height
                );
            }
            let width = (requested.right - requested.left) as u64;
            let height = (requested.bottom - requested.top) as u64;
            anyhow::ensure!(
                offset.x >= 0
                    && offset.y >= 0
                    && offset.x as u64 + width <= description.Width as u64
                    && offset.y as u64 + height <= description.Height as u64,
                "BeginDraw destination is outside the update texture"
            );
            target.view = None;
            unsafe {
                self.devices
                    .as_ref()
                    .context("devices missing")?
                    .device
                    .CreateRenderTargetView(&texture, None, Some(&mut target.view))?;
            }
            target.offset = offset;
            if local {
                let context: ID3D11DeviceContext1 = self
                    .devices
                    .as_ref()
                    .context("devices missing")?
                    .device_context
                    .cast()?;
                unsafe {
                    context.ClearView(
                        target.view.as_ref().context("render target view missing")?,
                        &[1.0; 4],
                        Some(&[RECT {
                            left: offset.x,
                            top: offset.y,
                            right: offset.x + width as i32,
                            bottom: offset.y + height as i32,
                        }]),
                    );
                }
            }
            self.render_target(
                scene,
                appearance,
                &target,
                if local {
                    ScenePass::LocalEndpoint
                } else {
                    ScenePass::Base
                },
            )
        })();
        if let Some(devices) = &self.devices {
            unsafe {
                devices.device_context.OMSetRenderTargets(None, None);
            }
        }
        let ended = unsafe { surface.EndDraw() }.map_err(anyhow::Error::from);
        drawn.and(ended)
    }

    /// Clear the render target for `background_appearance` and encode every
    /// primitive batch of `scene` into it, without presenting. Shared by
    /// [`draw`](Self::draw) (which then presents) and
    /// [`render_to_image`](Self::render_to_image) (which reads the target back
    /// instead), so the two cannot drift.
    fn render(
        &mut self,
        scene: &Scene,
        background_appearance: WindowBackgroundAppearance,
    ) -> Result<()> {
        let devices = self.devices.as_ref().context("devices missing")?;
        let resources = self.resources.as_mut().context("resources missing")?;
        if resources.render_target.is_none() {
            let (texture, _) =
                create_path_intermediate_texture(&devices.device, self.width, self.height)?;
            unsafe {
                devices.device.CreateRenderTargetView(
                    &texture,
                    None,
                    Some(&mut resources.render_target_view),
                )?;
            }
            resources.render_target = Some(texture);
        }
        let target = self.main_target()?;
        self.render_target(scene, background_appearance, &target, ScenePass::Base)
    }

    fn main_target(&self) -> Result<DrawTarget> {
        let resources = self.resources.as_ref().context("resources missing")?;
        Ok(DrawTarget {
            view: resources.render_target_view.clone(),
            region: RECT {
                left: 0,
                top: 0,
                right: self.width as i32,
                bottom: self.height as i32,
            },
            offset: POINT::default(),
            paths: PathAttachments {
                texture: resources.path_intermediate_texture.clone(),
                srv: resources.path_intermediate_srv.clone(),
                msaa: resources.path_intermediate_msaa_texture.clone(),
                view: resources.path_intermediate_msaa_view.clone(),
                width: self.width,
                height: self.height,
            },
        })
    }

    fn local_target(&mut self, region: RECT) -> Result<DrawTarget> {
        let width = (region.right - region.left) as u32;
        let height = (region.bottom - region.top) as u32;
        if !self
            .local_target
            .as_ref()
            .is_some_and(|target| target.paths.width >= width && target.paths.height >= height)
        {
            let device = &self.devices.as_ref().context("devices missing")?.device;
            let width = width.next_power_of_two();
            let height = height.next_power_of_two();
            #[cfg(any(test, feature = "test-support"))]
            let (texture, view) = {
                let (texture, _) = create_path_intermediate_texture(device, width, height)?;
                let mut view = None;
                unsafe {
                    device.CreateRenderTargetView(&texture, None, Some(&mut view))?;
                }
                (texture, view)
            };
            self.local_target = Some(LocalTarget {
                #[cfg(any(test, feature = "test-support"))]
                texture,
                #[cfg(any(test, feature = "test-support"))]
                view,
                paths: PathAttachments::new(device, width, height)?,
            });
        }
        let local = self.local_target.as_ref().context("local target missing")?;
        #[cfg(any(test, feature = "test-support"))]
        let view = local.view.clone();
        #[cfg(not(any(test, feature = "test-support")))]
        let view = None;
        Ok(DrawTarget {
            view,
            region,
            offset: POINT::default(),
            paths: local.paths.clone(),
        })
    }

    fn render_target(
        &mut self,
        scene: &Scene,
        background_appearance: WindowBackgroundAppearance,
        target: &DrawTarget,
        pass: ScenePass,
    ) -> Result<()> {
        let local = matches!(pass, ScenePass::LocalEndpoint);
        self.pre_draw(
            target,
            &match background_appearance {
                WindowBackgroundAppearance::Opaque => [1.0f32; 4],
                _ => [0.0f32; 4],
            },
        )?;
        if !local {
            self.upload_scene_buffers(scene)?;
        }
        #[cfg(any(test, feature = "test-support"))]
        if let ScenePass::Reference(alpha) = pass
            && let Some(opacity) = &scene.opacity_quad
        {
            let mut quads = scene.quads.clone();
            let quad = &mut quads[opacity.quad_index];
            quad.background = quad.background.opacity(alpha);
            quad.border_color = quad.border_color.opacity(alpha);
            let devices = self.devices.as_ref().context("devices missing")?;
            self.pipelines.quad_pipeline.update_buffer(
                &devices.device,
                &devices.device_context,
                &quads,
            )?;
        }

        let annotation = self
            .devices
            .as_ref()
            .and_then(|devices| devices.annotation.clone())
            .filter(|annotation| unsafe { annotation.GetStatus().as_bool() });
        for batch in scene.batches() {
            let _annotation = annotation
                .as_ref()
                .map(|annotation| Annotation::new(annotation, HSTRING::from(batch.label())));
            match batch {
                PrimitiveBatch::Shadows(range) => {
                    for range in visible_ranges(range, local, |index| intersects(scene.shadows[index].bounds, scene.shadows[index].content_mask, target.bounds())) {
                        self.draw_shadows(range.start, range.len())?;
                    }
                    Ok(())
                }
                PrimitiveBatch::Quads(range) => {
                    for range in visible_ranges(range, true, |index| {
                        (!matches!(pass, ScenePass::Base) || !scene.opacity_quad.as_ref().is_some_and(|quad| quad.quad_index == index))
                            && (!local || intersects(scene.quads[index].bounds, scene.quads[index].content_mask, target.bounds()))
                    }) { self.draw_quads(range.start, range.len())?; }
                    Ok(())
                }
                PrimitiveBatch::Paths(range) => {
                    let paths = if local {
                        std::borrow::Cow::Owned(scene.paths[range].iter().filter(|path| !path.clipped_bounds().intersect(&target.bounds()).is_empty()).cloned().collect::<Vec<_>>())
                    } else { std::borrow::Cow::Borrowed(&scene.paths[range]) };
                    self.draw_paths_to_intermediate(&paths, target)?;
                    self.draw_paths_from_intermediate(&paths, target)
                }
                PrimitiveBatch::Underlines(range) => {
                    for range in visible_ranges(range, local, |index| intersects(scene.underlines[index].bounds, scene.underlines[index].content_mask, target.bounds())) { self.draw_underlines(range.start, range.len())?; }
                    Ok(())
                }
                PrimitiveBatch::MonochromeSprites { texture_id, range } => {
                    for range in visible_ranges(range, local, |index| intersects(transformed_bounds(scene.monochrome_sprites[index].bounds, scene.monochrome_sprites[index].transformation), scene.monochrome_sprites[index].content_mask, target.bounds())) { self.draw_monochrome_sprites(texture_id, range.start, range.len())?; }
                    Ok(())
                }
                PrimitiveBatch::SubpixelSprites { texture_id, range } => {
                    for range in visible_ranges(range, local, |index| intersects(transformed_bounds(scene.subpixel_sprites[index].bounds, scene.subpixel_sprites[index].transformation), scene.subpixel_sprites[index].content_mask, target.bounds())) { self.draw_subpixel_sprites(texture_id, range.start, range.len())?; }
                    Ok(())
                }
                PrimitiveBatch::PolychromeSprites { texture_id, range } => {
                    for range in visible_ranges(range, local, |index| intersects(scene.polychrome_sprites[index].bounds, scene.polychrome_sprites[index].content_mask, target.bounds())) { self.draw_polychrome_sprites(texture_id, range.start, range.len())?; }
                    Ok(())
                }
                PrimitiveBatch::Surfaces(range) => self.draw_surfaces(&scene.surfaces[range]),
            }
            .with_context(|| {
                format!(
                    "scene too large:\
                    {} paths, {} shadows, {} quads, {} underlines, {} mono, {} subpixel, {} poly, {} surfaces",
                    scene.paths.len(),
                    scene.shadows.len(),
                    scene.quads.len(),
                    scene.underlines.len(),
                    scene.monochrome_sprites.len(),
                    scene.subpixel_sprites.len(),
                    scene.polychrome_sprites.len(),
                    scene.surfaces.len(),
                )
            })?;
        }
        Ok(())
    }

    /// Render `scene` to an offscreen CPU image **without presenting** so
    /// the window need never be shown or visible (the macOS headless path
    /// goes through MetalRenderer; this is the Windows analogue). Draws into
    /// the existing render target, copies it into a `D3D11_USAGE_STAGING`
    /// texture, maps it, and converts BGRA to RGBA.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn render_to_image(
        &mut self,
        scene: &Scene,
        background_appearance: WindowBackgroundAppearance,
    ) -> Result<image::RgbaImage> {
        // A pending device-lost recovery (`skip_draws`) leaves the atlas holding
        // tile references from the previous device; drawing before the forced
        // re-render rebuilds them panics in `DirectXAtlasState::texture`.
        anyhow::ensure!(
            !self.skip_draws,
            "render_to_image unavailable while recovering from a lost device"
        );
        let devices = self.devices.as_ref().context("devices missing")?.clone();
        let mut main_measurement = PassMeasurement::begin(&devices)?;
        self.render(scene, background_appearance)?;
        main_measurement.end(&devices.device_context);
        let render_target = self
            .resources
            .as_ref()
            .context("resources missing")?
            .render_target
            .as_ref()
            .context("render target missing")?
            .clone();
        let mut image = self.read_texture(&render_target)?;
        main_measurement.report(&devices.device_context, "main", self.width * self.height)?;
        if let Some(opacity) = &scene.opacity_quad
            && let Some(region) = self.opacity_region(scene)
        {
            let target = self.local_target(region)?;
            let mut local_measurement = PassMeasurement::begin(&devices)?;
            self.render_target(
                scene,
                background_appearance,
                &target,
                ScenePass::LocalEndpoint,
            )?;
            local_measurement.end(&devices.device_context);
            let local = self
                .local_target
                .as_ref()
                .context("local target missing")?
                .texture
                .clone();
            let endpoint = self.read_texture(&local)?;
            local_measurement.report(
                &devices.device_context,
                "local",
                (region.right - region.left) as u32 * (region.bottom - region.top) as u32,
            )?;
            let alpha = opacity.track.value_at(
                scene
                    .presentation_time
                    .unwrap_or(opacity.track.started_at()),
            );
            for y in 0..(region.bottom - region.top) as u32 {
                for x in 0..(region.right - region.left) as u32 {
                    let base = image.get_pixel_mut(region.left as u32 + x, region.top as u32 + y);
                    let endpoint = endpoint.get_pixel(x, y);
                    for channel in 0..4 {
                        base[channel] = (base[channel] as f32 * (1.0 - alpha)
                            + endpoint[channel] as f32 * alpha)
                            .round() as u8;
                    }
                }
            }
        }
        Ok(image)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn render_opacity_reference(
        &mut self,
        scene: &Scene,
        appearance: WindowBackgroundAppearance,
    ) -> Result<image::RgbaImage> {
        // Allocate the same test-only full target as render_to_image.
        self.render(scene, appearance)?;
        let alpha = scene.opacity_quad.as_ref().map_or(0.0, |opacity| {
            opacity.track.value_at(
                scene
                    .presentation_time
                    .unwrap_or(opacity.track.started_at()),
            )
        });
        self.render_target(
            scene,
            appearance,
            &self.main_target()?,
            ScenePass::Reference(alpha),
        )?;
        let texture = self
            .resources
            .as_ref()
            .context("resources missing")?
            .render_target
            .as_ref()
            .context("render target missing")?;
        self.read_texture(texture)
    }

    fn opacity_region(&self, scene: &Scene) -> Option<RECT> {
        let opacity = scene.opacity_quad.as_ref()?;
        let quad = &scene.quads[opacity.quad_index];
        let bounds = quad.bounds.intersect(&quad.content_mask.bounds);
        let region = RECT {
            left: (bounds.left().0.floor() as i32 - 1).max(0),
            top: (bounds.top().0.floor() as i32 - 1).max(0),
            right: (bounds.right().0.ceil() as i32 + 1).min(self.width as i32),
            bottom: (bounds.bottom().0.ceil() as i32 + 1).min(self.height as i32),
        };
        (region.left < region.right && region.top < region.bottom).then_some(region)
    }

    #[cfg(any(test, feature = "test-support"))]
    fn read_texture(&self, render_target: &ID3D11Texture2D) -> Result<image::RgbaImage> {
        let devices = self.devices.as_ref().context("devices missing")?;
        let device = &devices.device;
        let context = &devices.device_context;
        // A CPU-readable copy of the render target.
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { render_target.GetDesc(&mut desc) };
        let width = desc.Width;
        let height = desc.Height;
        let staging_desc = D3D11_TEXTURE2D_DESC {
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
            MipLevels: 1,
            ArraySize: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            ..desc
        };
        let mut staging: Option<ID3D11Texture2D> = None;
        unsafe { device.CreateTexture2D(&staging_desc, None, Some(&mut staging))? };
        let staging = staging.context("creating staging texture")?;
        unsafe { context.CopyResource(&staging, render_target) };

        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe { context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))? };
        let row_bytes = (width as usize) * 4;
        let mut pixels = vec![0u8; row_bytes * height as usize];
        // SAFETY: `Map` succeeded, so `pData` points at `RowPitch * height`
        // readable bytes for as long as the mapping is held, and `RowPitch >=
        // row_bytes` (it only ever adds trailing padding). `pixels` is sized
        // `row_bytes * height`, so every copy stays in bounds on both sides,
        // and the regions cannot overlap (`pixels` is a fresh allocation).
        unsafe {
            let src = mapped.pData as *const u8;
            for row in 0..height as usize {
                let s = src.add(row * mapped.RowPitch as usize);
                let d = pixels.as_mut_ptr().add(row * row_bytes);
                std::ptr::copy_nonoverlapping(s, d, row_bytes);
            }
            context.Unmap(&staging, 0);
        }
        // The render target is BGRA; image::RgbaImage expects RGBA.
        for px in pixels.chunks_exact_mut(4) {
            px.swap(0, 2);
        }
        image::RgbaImage::from_raw(width, height, pixels)
            .context("Failed to build RgbaImage from staging readback")
    }

    pub(crate) fn resize(&mut self, new_size: Size<DevicePixels>) -> Result<()> {
        let width = new_size.width.0.max(1) as u32;
        let height = new_size.height.0.max(1) as u32;
        if self.width == width && self.height == height {
            return Ok(());
        }
        self.width = width;
        self.height = height;

        // Clear the render target before resizing
        let devices = self.devices.as_ref().context("devices missing")?;
        unsafe { devices.device_context.OMSetRenderTargets(None, None) };
        let resources = self.resources.as_mut().context("resources missing")?;
        resources.render_target.take();
        resources.render_target_view.take();

        // Resizing the swap chain requires a call to the underlying DXGI adapter, which can return the device removed error.
        // The app might have moved to a monitor that's attached to a different graphics device.
        // When a graphics device is removed or reset, the desktop resolution often changes, resulting in a window size change.
        // But here we just return the error, because we are handling device lost scenarios elsewhere.
        if let Some(swap_chain) = &resources.swap_chain {
            unsafe {
                swap_chain
                    .ResizeBuffers(
                        BUFFER_COUNT as u32,
                        width,
                        height,
                        RENDER_TARGET_FORMAT,
                        DXGI_SWAP_CHAIN_FLAG(0),
                    )
                    .context("Failed to resize swap chain")?;
            }
        }

        resources.recreate_resources(devices, width, height)?;

        unsafe {
            devices
                .device_context
                .OMSetRenderTargets(Some(slice::from_ref(&resources.render_target_view)), None);
        }

        Ok(())
    }

    fn upload_scene_buffers(&mut self, scene: &Scene) -> Result<()> {
        let devices = self.devices.as_ref().context("devices missing")?;

        if !scene.shadows.is_empty() {
            self.pipelines.shadow_pipeline.update_buffer(
                &devices.device,
                &devices.device_context,
                &scene.shadows,
            )?;
        }

        if !scene.quads.is_empty() {
            self.pipelines.quad_pipeline.update_buffer(
                &devices.device,
                &devices.device_context,
                &scene.quads,
            )?;
        }

        if !scene.underlines.is_empty() {
            self.pipelines.underline_pipeline.update_buffer(
                &devices.device,
                &devices.device_context,
                &scene.underlines,
            )?;
        }

        if !scene.monochrome_sprites.is_empty() {
            self.pipelines.mono_sprites.update_buffer(
                &devices.device,
                &devices.device_context,
                &scene.monochrome_sprites,
            )?;
        }

        if !scene.subpixel_sprites.is_empty() {
            self.pipelines.subpixel_sprites.update_buffer(
                &devices.device,
                &devices.device_context,
                &scene.subpixel_sprites,
            )?;
        }

        if !scene.polychrome_sprites.is_empty() {
            self.pipelines.poly_sprites.update_buffer(
                &devices.device,
                &devices.device_context,
                &scene.polychrome_sprites,
            )?;
        }

        Ok(())
    }

    fn draw_shadows(&mut self, start: usize, len: usize) -> Result<()> {
        if len == 0 {
            return Ok(());
        }
        let devices = self.devices.as_ref().context("devices missing")?;
        self.pipelines.shadow_pipeline.draw_range(
            &devices.device_context,
            self.globals
                .batch_params_buffer
                .as_ref()
                .context("batch params buffer missing")?,
            start as u32,
            len as u32,
        )
    }

    fn draw_quads(&mut self, start: usize, len: usize) -> Result<()> {
        if len == 0 {
            return Ok(());
        }
        let devices = self.devices.as_ref().context("devices missing")?;
        self.pipelines.quad_pipeline.draw_range(
            &devices.device_context,
            self.globals
                .batch_params_buffer
                .as_ref()
                .context("batch params buffer missing")?,
            start as u32,
            len as u32,
        )
    }

    fn draw_paths_to_intermediate(
        &mut self,
        paths: &[Path<ScaledPixels>],
        target: &DrawTarget,
    ) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }

        let devices = self.devices.as_ref().context("devices missing")?;
        let internal = DrawTarget {
            view: target.paths.view.clone(),
            region: target.region,
            offset: POINT::default(),
            paths: target.paths.clone(),
        };
        self.bind_target(&internal)?;
        // Clear intermediate MSAA texture
        unsafe {
            devices
                .device_context
                .ClearRenderTargetView(target.paths.view.as_ref().unwrap(), &[0.0; 4]);
            // Set intermediate MSAA texture as render target
            devices
                .device_context
                .OMSetRenderTargets(Some(slice::from_ref(&target.paths.view)), None);
        }

        // Collect all vertices and sprites for a single draw call
        let mut vertices = Vec::new();

        for path in paths {
            vertices.extend(path.vertices.iter().map(|v| PathRasterizationSprite {
                xy_position: v.xy_position,
                st_position: v.st_position,
                color: path.color,
                bounds: path.clipped_bounds(),
            }));
        }

        self.pipelines.path_rasterization_pipeline.update_buffer(
            &devices.device,
            &devices.device_context,
            &vertices,
        )?;

        self.pipelines.path_rasterization_pipeline.draw(
            &devices.device_context,
            D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST,
            vertices.len() as u32,
            1,
        )?;

        // Resolve MSAA to non-MSAA intermediate texture
        unsafe {
            devices.device_context.ResolveSubresource(
                &target.paths.texture,
                0,
                &target.paths.msaa,
                0,
                RENDER_TARGET_FORMAT,
            );
        }
        self.bind_target(target)?;

        Ok(())
    }

    fn draw_paths_from_intermediate(
        &mut self,
        paths: &[Path<ScaledPixels>],
        target: &DrawTarget,
    ) -> Result<()> {
        let Some(first_path) = paths.first() else {
            return Ok(());
        };

        // When copying paths from the intermediate texture to the drawable,
        // each pixel must only be copied once, in case of transparent paths.
        //
        // If all paths have the same draw order, then their bounds are all
        // disjoint, so we can copy each path's bounds individually. If this
        // batch combines different draw orders, we perform a single copy
        // for a minimal spanning rect.
        let sprites = if paths.last().unwrap().order == first_path.order {
            paths
                .iter()
                .map(|path| PathSprite {
                    bounds: path.clipped_bounds(),
                })
                .collect::<Vec<_>>()
        } else {
            let mut bounds = first_path.clipped_bounds();
            for path in paths.iter().skip(1) {
                bounds = bounds.union(&path.clipped_bounds());
            }
            vec![PathSprite { bounds }]
        };

        let devices = self.devices.as_ref().context("devices missing")?;
        self.pipelines.path_sprite_pipeline.update_buffer(
            &devices.device,
            &devices.device_context,
            &sprites,
        )?;

        // Draw the sprites with the path texture
        self.pipelines.path_sprite_pipeline.draw_with_texture(
            &devices.device_context,
            slice::from_ref(&target.paths.srv),
            slice::from_ref(&self.globals.sampler),
            sprites.len() as u32,
        )
    }

    fn draw_underlines(&mut self, start: usize, len: usize) -> Result<()> {
        if len == 0 {
            return Ok(());
        }
        let devices = self.devices.as_ref().context("devices missing")?;
        self.pipelines.underline_pipeline.draw_range(
            &devices.device_context,
            self.globals
                .batch_params_buffer
                .as_ref()
                .context("batch params buffer missing")?,
            start as u32,
            len as u32,
        )
    }

    fn draw_monochrome_sprites(
        &mut self,
        texture_id: AtlasTextureId,
        start: usize,
        len: usize,
    ) -> Result<()> {
        if len == 0 {
            return Ok(());
        }
        let devices = self.devices.as_ref().context("devices missing")?;
        let texture_view = self.atlas.get_texture_view(texture_id);
        self.pipelines.mono_sprites.draw_range_with_texture(
            &devices.device_context,
            &texture_view,
            self.globals
                .batch_params_buffer
                .as_ref()
                .context("batch params buffer missing")?,
            slice::from_ref(&self.globals.sampler),
            start as u32,
            len as u32,
        )
    }

    fn draw_subpixel_sprites(
        &mut self,
        texture_id: AtlasTextureId,
        start: usize,
        len: usize,
    ) -> Result<()> {
        if len == 0 {
            return Ok(());
        }
        let devices = self.devices.as_ref().context("devices missing")?;
        let texture_view = self.atlas.get_texture_view(texture_id);
        self.pipelines.subpixel_sprites.draw_range_with_texture(
            &devices.device_context,
            &texture_view,
            self.globals
                .batch_params_buffer
                .as_ref()
                .context("batch params buffer missing")?,
            slice::from_ref(&self.globals.sampler),
            start as u32,
            len as u32,
        )
    }

    fn draw_polychrome_sprites(
        &mut self,
        texture_id: AtlasTextureId,
        start: usize,
        len: usize,
    ) -> Result<()> {
        if len == 0 {
            return Ok(());
        }
        let devices = self.devices.as_ref().context("devices missing")?;
        let texture_view = self.atlas.get_texture_view(texture_id);
        self.pipelines.poly_sprites.draw_range_with_texture(
            &devices.device_context,
            &texture_view,
            self.globals
                .batch_params_buffer
                .as_ref()
                .context("batch params buffer missing")?,
            slice::from_ref(&self.globals.sampler),
            start as u32,
            len as u32,
        )
    }

    fn draw_surfaces(&mut self, surfaces: &[PaintSurface]) -> Result<()> {
        if surfaces.is_empty() {
            return Ok(());
        }
        Ok(())
    }

    pub(crate) fn gpu_specs(&self) -> Result<GpuSpecs> {
        let devices = self.devices.as_ref().context("devices missing")?;
        let desc = unsafe { devices.adapter.GetDesc1() }?;
        let is_software_emulated = (desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32) != 0;
        let device_name = String::from_utf16_lossy(&desc.Description)
            .trim_matches(char::from(0))
            .to_string();
        let driver_name = match desc.VendorId {
            0x10DE => "NVIDIA Corporation".to_string(),
            0x1002 => "AMD Corporation".to_string(),
            0x8086 => "Intel Corporation".to_string(),
            id => format!("Unknown Vendor (ID: {:#X})", id),
        };
        let driver_version = match desc.VendorId {
            0x10DE => nvidia::get_driver_version(),
            0x1002 => amd::get_driver_version(),
            // For Intel and other vendors, we use the DXGI API to get the driver version.
            _ => dxgi::get_driver_version(&devices.adapter),
        }
        .context("Failed to get gpu driver info")
        .log_err()
        .unwrap_or("Unknown Driver".to_string());
        Ok(GpuSpecs {
            is_software_emulated,
            device_name,
            driver_name,
            driver_info: driver_version,
        })
    }

    pub(crate) fn get_font_info() -> &'static FontInfo {
        static CACHED_FONT_INFO: OnceLock<FontInfo> = OnceLock::new();
        CACHED_FONT_INFO.get_or_init(|| unsafe {
            let factory: IDWriteFactory5 = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED).unwrap();
            let render_params: IDWriteRenderingParams1 =
                factory.CreateRenderingParams().unwrap().cast().unwrap();
            FontInfo {
                gamma_ratios: gpui::get_gamma_correction_ratios(render_params.GetGamma()),
                grayscale_enhanced_contrast: render_params.GetGrayscaleEnhancedContrast(),
                subpixel_enhanced_contrast: render_params.GetEnhancedContrast(),
                is_bgr: render_params.GetPixelGeometry() == DWRITE_PIXEL_GEOMETRY_BGR,
            }
        })
    }

    pub(crate) fn mark_drawable(&mut self) {
        self.skip_draws = false;
    }
}

impl DirectXResources {
    pub fn new(
        devices: &DirectXRendererDevices,
        width: u32,
        height: u32,
        hwnd: HWND,
        disable_direct_composition: bool,
    ) -> Result<Self> {
        let swap_chain = if disable_direct_composition {
            Some(create_swap_chain(
                &devices.dxgi_factory,
                &devices.device,
                hwnd,
                width,
                height,
            )?)
        } else {
            None
        };

        let (
            render_target,
            render_target_view,
            path_intermediate_texture,
            path_intermediate_srv,
            path_intermediate_msaa_texture,
            path_intermediate_msaa_view,
        ) = create_resources(devices, swap_chain.as_ref(), width, height)?;
        set_rasterizer_state(&devices.device, &devices.device_context)?;

        Ok(Self {
            swap_chain,
            render_target,
            render_target_view,
            path_intermediate_texture,
            path_intermediate_msaa_texture,
            path_intermediate_msaa_view,
            path_intermediate_srv,
        })
    }

    #[inline]
    fn recreate_resources(
        &mut self,
        devices: &DirectXRendererDevices,
        width: u32,
        height: u32,
    ) -> Result<()> {
        let (
            render_target,
            render_target_view,
            path_intermediate_texture,
            path_intermediate_srv,
            path_intermediate_msaa_texture,
            path_intermediate_msaa_view,
        ) = create_resources(devices, self.swap_chain.as_ref(), width, height)?;
        self.render_target = render_target;
        self.render_target_view = render_target_view;
        self.path_intermediate_texture = path_intermediate_texture;
        self.path_intermediate_msaa_texture = path_intermediate_msaa_texture;
        self.path_intermediate_msaa_view = path_intermediate_msaa_view;
        self.path_intermediate_srv = path_intermediate_srv;
        Ok(())
    }
}

impl DirectXRenderPipelines {
    pub fn new(device: &ID3D11Device) -> Result<Self> {
        let shadow_pipeline = PipelineState::new(
            device,
            "shadow_pipeline",
            ShaderModule::Shadow,
            4,
            create_blend_state(device)?,
        )?;
        let quad_pipeline = PipelineState::new(
            device,
            "quad_pipeline",
            ShaderModule::Quad,
            64,
            create_blend_state(device)?,
        )?;
        let path_rasterization_pipeline = PipelineState::new(
            device,
            "path_rasterization_pipeline",
            ShaderModule::PathRasterization,
            32,
            create_blend_state_for_path_rasterization(device)?,
        )?;
        let path_sprite_pipeline = PipelineState::new(
            device,
            "path_sprite_pipeline",
            ShaderModule::PathSprite,
            4,
            create_blend_state_for_path_sprite(device)?,
        )?;
        let underline_pipeline = PipelineState::new(
            device,
            "underline_pipeline",
            ShaderModule::Underline,
            4,
            create_blend_state(device)?,
        )?;
        let mono_sprites = PipelineState::new(
            device,
            "monochrome_sprite_pipeline",
            ShaderModule::MonochromeSprite,
            512,
            create_blend_state(device)?,
        )?;
        let subpixel_sprites = PipelineState::new(
            device,
            "subpixel_sprite_pipeline",
            ShaderModule::SubpixelSprite,
            512,
            create_blend_state_for_subpixel_rendering(device)?,
        )?;
        let poly_sprites = PipelineState::new(
            device,
            "polychrome_sprite_pipeline",
            ShaderModule::PolychromeSprite,
            16,
            create_blend_state(device)?,
        )?;

        Ok(Self {
            shadow_pipeline,
            quad_pipeline,
            path_rasterization_pipeline,
            path_sprite_pipeline,
            underline_pipeline,
            mono_sprites,
            subpixel_sprites,
            poly_sprites,
        })
    }
}

impl DirectComposition {
    pub fn new(dxgi_device: &IDXGIDevice, hwnd: HWND) -> Result<Self> {
        let comp_device = get_comp_device(dxgi_device)?;
        let comp_target = unsafe { comp_device.CreateTargetForHwnd(hwnd, true) }?;
        let comp_visual = unsafe { comp_device.CreateVisual() }?;
        let main_visual = unsafe { comp_device.CreateVisual() }?;
        let local_visual: IDCompositionVisual3 = unsafe { comp_device.CreateVisual() }?.cast()?;
        unsafe {
            comp_visual.AddVisual(&main_visual, false, None)?;
            comp_visual.AddVisual(&local_visual, true, &main_visual)?;
            comp_target.SetRoot(&comp_visual)?;
        }

        Ok(Self {
            comp_device,
            _comp_target: comp_target,
            _root_visual: comp_visual,
            main_visual,
            local_visual,
            main_surface: None,
            local_surface: None,
            track: None,
        })
    }

    fn ensure_main(&mut self, width: u32, height: u32) -> Result<()> {
        ensure_composition_surface(&self.comp_device, &mut self.main_surface, width, height)
    }

    fn ensure_local(&mut self, width: u32, height: u32) -> Result<()> {
        if !self
            .local_surface
            .as_ref()
            .is_some_and(|surface| surface.width >= width && surface.height >= height)
        {
            ensure_composition_surface(
                &self.comp_device,
                &mut self.local_surface,
                width.next_power_of_two(),
                height.next_power_of_two(),
            )?;
        }
        Ok(())
    }

    fn bind_track(&self, track: &OpacityTrack, timebase: &CompositionTimebase) -> Result<()> {
        if let OpacityEnding::Hold { at_s: 0.0, value } = track.ending() {
            unsafe {
                self.local_visual.SetOpacity2(value)?;
            }
            return Ok(());
        }
        let animation = unsafe { self.comp_device.CreateAnimation()? };
        for segment in track.segments() {
            let [a, b, c, d] = segment.coefficients;
            unsafe {
                animation.AddCubic(segment.offset_s, a, b, c, d)?;
            }
        }
        unsafe {
            match track.ending() {
                OpacityEnding::Hold { at_s, value } => animation.End(at_s, value)?,
                OpacityEnding::Repeat { at_s, duration_s } => {
                    animation.AddRepeat(at_s, duration_s)?
                }
            }
            animation.SetAbsoluteBeginTime(timebase.qpc_at(track.started_at()))?;
            self.local_visual.SetOpacity(&animation)?;
        }
        Ok(())
    }
}

impl DirectXGlobalElements {
    pub fn new(device: &ID3D11Device) -> Result<Self> {
        let global_params_buffer = create_constant_buffer::<GlobalParams>(device)?;
        let batch_params_buffer = create_constant_buffer::<BatchParams>(device)?;

        let sampler = unsafe {
            let desc = D3D11_SAMPLER_DESC {
                Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
                AddressU: D3D11_TEXTURE_ADDRESS_WRAP,
                AddressV: D3D11_TEXTURE_ADDRESS_WRAP,
                AddressW: D3D11_TEXTURE_ADDRESS_WRAP,
                MipLODBias: 0.0,
                MaxAnisotropy: 1,
                ComparisonFunc: D3D11_COMPARISON_ALWAYS,
                BorderColor: [0.0; 4],
                MinLOD: 0.0,
                MaxLOD: D3D11_FLOAT32_MAX,
            };
            let mut output = None;
            device.CreateSamplerState(&desc, Some(&mut output))?;
            output
        };

        Ok(Self {
            global_params_buffer,
            batch_params_buffer,
            sampler,
        })
    }
}

#[derive(Debug, Default)]
#[repr(C)]
struct GlobalParams {
    gamma_ratios: [f32; 4],
    viewport_size: [f32; 2],
    grayscale_enhanced_contrast: f32,
    subpixel_enhanced_contrast: f32,
    is_bgr: u32,
    _pad: [u32; 3],
    source_origin: [f32; 2],
    target_offset: [f32; 2],
    path_texture_size: [f32; 2],
    _target_pad: [u32; 2],
}

#[derive(Clone, Copy, Debug, Default)]
#[repr(C, align(16))]
struct BatchParams {
    start_index: u32,
    _padding: [u32; 3],
}

const _: () = assert!(std::mem::size_of::<BatchParams>() == 16);

struct PipelineState<T> {
    label: &'static str,
    vertex: ID3D11VertexShader,
    fragment: ID3D11PixelShader,
    buffer: ID3D11Buffer,
    buffer_size: usize,
    view: Option<ID3D11ShaderResourceView>,
    blend_state: ID3D11BlendState,
    _marker: std::marker::PhantomData<T>,
}

impl<T> PipelineState<T> {
    fn new(
        device: &ID3D11Device,
        label: &'static str,
        shader_module: ShaderModule,
        buffer_size: usize,
        blend_state: ID3D11BlendState,
    ) -> Result<Self> {
        let vertex = {
            let raw_shader = RawShaderBytes::new(shader_module, ShaderTarget::Vertex)?;
            create_vertex_shader(device, raw_shader.as_bytes())?
        };
        let fragment = {
            let raw_shader = RawShaderBytes::new(shader_module, ShaderTarget::Fragment)?;
            create_fragment_shader(device, raw_shader.as_bytes())?
        };
        let buffer = create_buffer(device, std::mem::size_of::<T>(), buffer_size)?;
        let view = create_buffer_view(device, &buffer)?;

        Ok(PipelineState {
            label,
            vertex,
            fragment,
            buffer,
            buffer_size,
            view,
            blend_state,
            _marker: std::marker::PhantomData,
        })
    }

    fn update_buffer(
        &mut self,
        device: &ID3D11Device,
        device_context: &ID3D11DeviceContext,
        data: &[T],
    ) -> Result<()> {
        if self.buffer_size < data.len() {
            let element_size = std::mem::size_of::<T>();
            let required_size = std::mem::size_of_val(data);
            anyhow::ensure!(
                required_size <= MAX_INSTANCE_BUFFER_SIZE,
                "{} buffer needs {required_size} bytes, above the maximum of {MAX_INSTANCE_BUFFER_SIZE}",
                self.label
            );
            let new_buffer_size = data
                .len()
                .next_power_of_two()
                .min(MAX_INSTANCE_BUFFER_SIZE / element_size);
            log::debug!(
                "Updating {} buffer size from {} to {}",
                self.label,
                self.buffer_size,
                new_buffer_size
            );
            let buffer = create_buffer(device, std::mem::size_of::<T>(), new_buffer_size)?;
            let view = create_buffer_view(device, &buffer)?;
            self.buffer = buffer;
            self.view = view;
            self.buffer_size = new_buffer_size;
        }
        update_buffer(device_context, &self.buffer, data)
    }

    fn draw(
        &self,
        device_context: &ID3D11DeviceContext,
        topology: D3D_PRIMITIVE_TOPOLOGY,
        vertex_count: u32,
        instance_count: u32,
    ) -> Result<()> {
        set_pipeline_state(
            device_context,
            slice::from_ref(&self.view),
            topology,
            &self.vertex,
            &self.fragment,
            &self.blend_state,
        );
        unsafe {
            device_context.DrawInstanced(vertex_count, instance_count, 0, 0);
        }
        Ok(())
    }

    fn draw_with_texture(
        &self,
        device_context: &ID3D11DeviceContext,
        texture: &[Option<ID3D11ShaderResourceView>],
        sampler: &[Option<ID3D11SamplerState>],
        instance_count: u32,
    ) -> Result<()> {
        set_pipeline_state(
            device_context,
            slice::from_ref(&self.view),
            D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP,
            &self.vertex,
            &self.fragment,
            &self.blend_state,
        );
        unsafe {
            device_context.PSSetSamplers(0, Some(sampler));
            device_context.VSSetShaderResources(0, Some(texture));
            device_context.PSSetShaderResources(0, Some(texture));

            device_context.DrawInstanced(4, instance_count, 0, 0);
        }
        Ok(())
    }

    fn draw_range(
        &self,
        device_context: &ID3D11DeviceContext,
        batch_params_buffer: &ID3D11Buffer,
        first_instance: u32,
        instance_count: u32,
    ) -> Result<()> {
        anyhow::ensure!(
            first_instance as usize + instance_count as usize <= self.buffer_size,
            "DirectX instance range exceeds the {} buffer",
            self.label
        );
        update_batch_start(device_context, batch_params_buffer, first_instance)?;
        set_pipeline_state(
            device_context,
            slice::from_ref(&self.view),
            D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP,
            &self.vertex,
            &self.fragment,
            &self.blend_state,
        );
        unsafe {
            device_context.DrawInstanced(4, instance_count, 0, 0);
        }
        Ok(())
    }

    fn draw_range_with_texture(
        &self,
        device_context: &ID3D11DeviceContext,
        texture: &[Option<ID3D11ShaderResourceView>],
        batch_params_buffer: &ID3D11Buffer,
        sampler: &[Option<ID3D11SamplerState>],
        first_instance: u32,
        instance_count: u32,
    ) -> Result<()> {
        anyhow::ensure!(
            first_instance as usize + instance_count as usize <= self.buffer_size,
            "DirectX instance range exceeds the {} buffer",
            self.label
        );
        update_batch_start(device_context, batch_params_buffer, first_instance)?;
        set_pipeline_state(
            device_context,
            slice::from_ref(&self.view),
            D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP,
            &self.vertex,
            &self.fragment,
            &self.blend_state,
        );
        unsafe {
            device_context.PSSetSamplers(0, Some(sampler));
            device_context.VSSetShaderResources(0, Some(texture));
            device_context.PSSetShaderResources(0, Some(texture));
            device_context.DrawInstanced(4, instance_count, 0, 0);
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
#[repr(C)]
struct PathRasterizationSprite {
    xy_position: Point<ScaledPixels>,
    st_position: Point<f32>,
    color: Background,
    bounds: Bounds<ScaledPixels>,
}

#[derive(Clone, Copy)]
#[repr(C)]
struct PathSprite {
    bounds: Bounds<ScaledPixels>,
}

fn ensure_composition_surface(
    device: &IDCompositionDevice,
    slot: &mut Option<CompositionSurface>,
    width: u32,
    height: u32,
) -> Result<()> {
    if !slot
        .as_ref()
        .is_some_and(|surface| surface.width == width && surface.height == height)
    {
        let surface = unsafe {
            device.CreateSurface(
                width,
                height,
                RENDER_TARGET_FORMAT,
                DXGI_ALPHA_MODE_PREMULTIPLIED,
            )?
        };
        *slot = Some(CompositionSurface {
            surface,
            width,
            height,
        });
    }
    Ok(())
}

#[cfg(any(test, feature = "test-support"))]
fn opacity_probe_pause() {
    if let Some(milliseconds) = std::env::var_os("GPUI_ATOMIC_DELAY_MS") {
        std::thread::sleep(std::time::Duration::from_millis(
            milliseconds.to_string_lossy().parse().expect("probe delay"),
        ));
    }
}

fn intersects(
    bounds: Bounds<ScaledPixels>,
    mask: ContentMask<ScaledPixels>,
    region: Bounds<ScaledPixels>,
) -> bool {
    !bounds.intersect(&mask.bounds).intersect(&region).is_empty()
}

fn transformed_bounds(
    bounds: Bounds<ScaledPixels>,
    transform: TransformationMatrix,
) -> Bounds<ScaledPixels> {
    let points = [
        bounds.origin,
        bounds.top_right(),
        bounds.bottom_left(),
        bounds.bottom_right(),
    ]
    .map(|point| {
        let point = transform.apply(gpui::point(px(point.x.0), px(point.y.0)));
        gpui::point(
            ScaledPixels(point.x.as_f32()),
            ScaledPixels(point.y.as_f32()),
        )
    });
    let mut result = Bounds::new(points[0], Size::default());
    for point in points {
        result = result.union(&Bounds::new(point, Size::default()));
    }
    result
}

fn visible_ranges(
    mut range: std::ops::Range<usize>,
    filter: bool,
    mut visible: impl FnMut(usize) -> bool,
) -> impl Iterator<Item = std::ops::Range<usize>> {
    std::iter::from_fn(move || {
        if !filter {
            return (range.start < range.end).then(|| {
                let result = range.clone();
                range.start = range.end;
                result
            });
        }
        while range.start < range.end && !visible(range.start) {
            range.start += 1;
        }
        let start = range.start;
        while range.start < range.end && visible(range.start) {
            range.start += 1;
        }
        (start < range.start).then_some(start..range.start)
    })
}

impl Drop for DirectXRenderer {
    fn drop(&mut self) {
        #[cfg(debug_assertions)]
        if let Some(devices) = &self.devices {
            report_live_objects(&devices.device).ok();
        }
    }
}

#[inline]
fn get_comp_device(dxgi_device: &IDXGIDevice) -> Result<IDCompositionDevice> {
    // Create the newest device version that still speaks the v1 interface.
    // A legacy `DCompositionCreateDevice` device produces visuals that do not
    // implement `IDCompositionVisual3` (E_NOINTERFACE), which the compositor
    // local visual needs for compositor-run opacity animation. The v1 interface
    // itself (CreateTargetForHwnd, CreateVisual, Commit) is unchanged.
    Ok(unsafe {
        DCompositionCreateDevice3(dxgi_device).or_else(|_| DCompositionCreateDevice(dxgi_device))?
    })
}

fn create_swap_chain(
    dxgi_factory: &IDXGIFactory6,
    device: &ID3D11Device,
    hwnd: HWND,
    width: u32,
    height: u32,
) -> Result<IDXGISwapChain1> {
    use windows::Win32::Graphics::Dxgi::DXGI_MWA_NO_ALT_ENTER;

    let desc = DXGI_SWAP_CHAIN_DESC1 {
        Width: width,
        Height: height,
        Format: RENDER_TARGET_FORMAT,
        Stereo: false.into(),
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
        BufferCount: BUFFER_COUNT as u32,
        Scaling: DXGI_SCALING_NONE,
        SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
        AlphaMode: DXGI_ALPHA_MODE_IGNORE,
        Flags: 0,
    };
    let swap_chain =
        unsafe { dxgi_factory.CreateSwapChainForHwnd(device, hwnd, &desc, None, None) }?;
    unsafe { dxgi_factory.MakeWindowAssociation(hwnd, DXGI_MWA_NO_ALT_ENTER) }?;
    Ok(swap_chain)
}

#[inline]
fn create_resources(
    devices: &DirectXRendererDevices,
    swap_chain: Option<&IDXGISwapChain1>,
    width: u32,
    height: u32,
) -> Result<(
    Option<ID3D11Texture2D>,
    Option<ID3D11RenderTargetView>,
    ID3D11Texture2D,
    Option<ID3D11ShaderResourceView>,
    ID3D11Texture2D,
    Option<ID3D11RenderTargetView>,
)> {
    let (render_target, render_target_view) = if let Some(swap_chain) = swap_chain {
        let (texture, view) = create_render_target_and_its_view(swap_chain, &devices.device)?;
        (Some(texture), view)
    } else {
        (None, None)
    };
    let (path_intermediate_texture, path_intermediate_srv) =
        create_path_intermediate_texture(&devices.device, width, height)?;
    let (path_intermediate_msaa_texture, path_intermediate_msaa_view) =
        create_path_intermediate_msaa_texture_and_view(&devices.device, width, height)?;
    Ok((
        render_target,
        render_target_view,
        path_intermediate_texture,
        path_intermediate_srv,
        path_intermediate_msaa_texture,
        path_intermediate_msaa_view,
    ))
}

#[inline]
fn create_render_target_and_its_view(
    swap_chain: &IDXGISwapChain1,
    device: &ID3D11Device,
) -> Result<(ID3D11Texture2D, Option<ID3D11RenderTargetView>)> {
    let render_target: ID3D11Texture2D = unsafe { swap_chain.GetBuffer(0) }?;
    let mut render_target_view = None;
    unsafe { device.CreateRenderTargetView(&render_target, None, Some(&mut render_target_view))? };
    Ok((render_target, render_target_view))
}

#[inline]
fn create_path_intermediate_texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
) -> Result<(ID3D11Texture2D, Option<ID3D11ShaderResourceView>)> {
    let texture = unsafe {
        let mut output = None;
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: RENDER_TARGET_FORMAT,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        device.CreateTexture2D(&desc, None, Some(&mut output))?;
        output.unwrap()
    };

    let mut shader_resource_view = None;
    unsafe { device.CreateShaderResourceView(&texture, None, Some(&mut shader_resource_view))? };

    Ok((texture, Some(shader_resource_view.unwrap())))
}

#[inline]
fn create_path_intermediate_msaa_texture_and_view(
    device: &ID3D11Device,
    width: u32,
    height: u32,
) -> Result<(ID3D11Texture2D, Option<ID3D11RenderTargetView>)> {
    let msaa_texture = unsafe {
        let mut output = None;
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: RENDER_TARGET_FORMAT,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: PATH_MULTISAMPLE_COUNT,
                Quality: D3D11_STANDARD_MULTISAMPLE_PATTERN.0 as u32,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        device.CreateTexture2D(&desc, None, Some(&mut output))?;
        output.unwrap()
    };
    let mut msaa_view = None;
    unsafe { device.CreateRenderTargetView(&msaa_texture, None, Some(&mut msaa_view))? };
    Ok((msaa_texture, Some(msaa_view.unwrap())))
}

#[inline]
fn set_rasterizer_state(device: &ID3D11Device, device_context: &ID3D11DeviceContext) -> Result<()> {
    let desc = D3D11_RASTERIZER_DESC {
        FillMode: D3D11_FILL_SOLID,
        CullMode: D3D11_CULL_NONE,
        FrontCounterClockwise: false.into(),
        DepthBias: 0,
        DepthBiasClamp: 0.0,
        SlopeScaledDepthBias: 0.0,
        DepthClipEnable: true.into(),
        ScissorEnable: true.into(),
        MultisampleEnable: true.into(),
        AntialiasedLineEnable: false.into(),
    };
    let rasterizer_state = unsafe {
        let mut state = None;
        device.CreateRasterizerState(&desc, Some(&mut state))?;
        state.unwrap()
    };
    unsafe { device_context.RSSetState(&rasterizer_state) };
    Ok(())
}

// https://learn.microsoft.com/en-us/windows/win32/api/d3d11/ns-d3d11-d3d11_blend_desc
#[inline]
fn create_blend_state(device: &ID3D11Device) -> Result<ID3D11BlendState> {
    let mut desc = D3D11_BLEND_DESC::default();
    desc.RenderTarget[0].BlendEnable = true.into();
    desc.RenderTarget[0].BlendOp = D3D11_BLEND_OP_ADD;
    desc.RenderTarget[0].BlendOpAlpha = D3D11_BLEND_OP_ADD;
    desc.RenderTarget[0].SrcBlend = D3D11_BLEND_SRC_ALPHA;
    desc.RenderTarget[0].SrcBlendAlpha = D3D11_BLEND_ONE;
    desc.RenderTarget[0].DestBlend = D3D11_BLEND_INV_SRC_ALPHA;
    desc.RenderTarget[0].DestBlendAlpha = D3D11_BLEND_ONE;
    desc.RenderTarget[0].RenderTargetWriteMask = D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8;
    unsafe {
        let mut state = None;
        device.CreateBlendState(&desc, Some(&mut state))?;
        Ok(state.unwrap())
    }
}

#[inline]
fn create_blend_state_for_subpixel_rendering(device: &ID3D11Device) -> Result<ID3D11BlendState> {
    let mut desc = D3D11_BLEND_DESC::default();
    desc.RenderTarget[0].BlendEnable = true.into();
    desc.RenderTarget[0].BlendOp = D3D11_BLEND_OP_ADD;
    desc.RenderTarget[0].BlendOpAlpha = D3D11_BLEND_OP_ADD;
    desc.RenderTarget[0].SrcBlend = D3D11_BLEND_SRC1_COLOR;
    desc.RenderTarget[0].DestBlend = D3D11_BLEND_INV_SRC1_COLOR;
    // It does not make sense to draw transparent subpixel-rendered text, since it cannot be meaningfully alpha-blended onto anything else.
    desc.RenderTarget[0].SrcBlendAlpha = D3D11_BLEND_ONE;
    desc.RenderTarget[0].DestBlendAlpha = D3D11_BLEND_ZERO;
    desc.RenderTarget[0].RenderTargetWriteMask =
        D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8 & !D3D11_COLOR_WRITE_ENABLE_ALPHA.0 as u8;

    unsafe {
        let mut state = None;
        device.CreateBlendState(&desc, Some(&mut state))?;
        Ok(state.unwrap())
    }
}

#[inline]
fn create_blend_state_for_path_rasterization(device: &ID3D11Device) -> Result<ID3D11BlendState> {
    // If the feature level is set to greater than D3D_FEATURE_LEVEL_9_3, the display
    // device performs the blend in linear space, which is ideal.
    let mut desc = D3D11_BLEND_DESC::default();
    desc.RenderTarget[0].BlendEnable = true.into();
    desc.RenderTarget[0].BlendOp = D3D11_BLEND_OP_ADD;
    desc.RenderTarget[0].BlendOpAlpha = D3D11_BLEND_OP_ADD;
    desc.RenderTarget[0].SrcBlend = D3D11_BLEND_ONE;
    desc.RenderTarget[0].SrcBlendAlpha = D3D11_BLEND_ONE;
    desc.RenderTarget[0].DestBlend = D3D11_BLEND_INV_SRC_ALPHA;
    desc.RenderTarget[0].DestBlendAlpha = D3D11_BLEND_INV_SRC_ALPHA;
    desc.RenderTarget[0].RenderTargetWriteMask = D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8;
    unsafe {
        let mut state = None;
        device.CreateBlendState(&desc, Some(&mut state))?;
        Ok(state.unwrap())
    }
}

#[inline]
fn create_blend_state_for_path_sprite(device: &ID3D11Device) -> Result<ID3D11BlendState> {
    // If the feature level is set to greater than D3D_FEATURE_LEVEL_9_3, the display
    // device performs the blend in linear space, which is ideal.
    let mut desc = D3D11_BLEND_DESC::default();
    desc.RenderTarget[0].BlendEnable = true.into();
    desc.RenderTarget[0].BlendOp = D3D11_BLEND_OP_ADD;
    desc.RenderTarget[0].BlendOpAlpha = D3D11_BLEND_OP_ADD;
    desc.RenderTarget[0].SrcBlend = D3D11_BLEND_ONE;
    desc.RenderTarget[0].SrcBlendAlpha = D3D11_BLEND_ONE;
    desc.RenderTarget[0].DestBlend = D3D11_BLEND_INV_SRC_ALPHA;
    desc.RenderTarget[0].DestBlendAlpha = D3D11_BLEND_ONE;
    desc.RenderTarget[0].RenderTargetWriteMask = D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8;
    unsafe {
        let mut state = None;
        device.CreateBlendState(&desc, Some(&mut state))?;
        Ok(state.unwrap())
    }
}

#[inline]
fn create_vertex_shader(device: &ID3D11Device, bytes: &[u8]) -> Result<ID3D11VertexShader> {
    unsafe {
        let mut shader = None;
        device.CreateVertexShader(bytes, None, Some(&mut shader))?;
        Ok(shader.unwrap())
    }
}

#[inline]
fn create_fragment_shader(device: &ID3D11Device, bytes: &[u8]) -> Result<ID3D11PixelShader> {
    unsafe {
        let mut shader = None;
        device.CreatePixelShader(bytes, None, Some(&mut shader))?;
        Ok(shader.unwrap())
    }
}

#[inline]
fn create_constant_buffer<T>(device: &ID3D11Device) -> Result<Option<ID3D11Buffer>> {
    const { assert!(std::mem::size_of::<T>() != 0 && std::mem::size_of::<T>().is_multiple_of(16)) };
    let desc = D3D11_BUFFER_DESC {
        ByteWidth: std::mem::size_of::<T>() as u32,
        Usage: D3D11_USAGE_DYNAMIC,
        BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
        CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
        MiscFlags: 0,
        StructureByteStride: 0,
    };
    let mut buffer = None;
    unsafe { device.CreateBuffer(&desc, None, Some(&mut buffer)) }?;
    Ok(buffer)
}

#[inline]
fn create_buffer(
    device: &ID3D11Device,
    element_size: usize,
    buffer_size: usize,
) -> Result<ID3D11Buffer> {
    let desc = D3D11_BUFFER_DESC {
        ByteWidth: (element_size * buffer_size) as u32,
        Usage: D3D11_USAGE_DYNAMIC,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
        MiscFlags: D3D11_RESOURCE_MISC_BUFFER_STRUCTURED.0 as u32,
        StructureByteStride: element_size as u32,
    };
    let mut buffer = None;
    unsafe { device.CreateBuffer(&desc, None, Some(&mut buffer)) }?;
    Ok(buffer.unwrap())
}

#[inline]
fn create_buffer_view(
    device: &ID3D11Device,
    buffer: &ID3D11Buffer,
) -> Result<Option<ID3D11ShaderResourceView>> {
    let mut view = None;
    unsafe { device.CreateShaderResourceView(buffer, None, Some(&mut view)) }?;
    Ok(view)
}

#[inline]
fn update_buffer<T>(
    device_context: &ID3D11DeviceContext,
    buffer: &ID3D11Buffer,
    data: &[T],
) -> Result<()> {
    unsafe {
        let mut dest = std::mem::zeroed();
        device_context.Map(buffer, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut dest))?;
        std::ptr::copy_nonoverlapping(data.as_ptr(), dest.pData as _, data.len());
        device_context.Unmap(buffer, 0);
    }
    Ok(())
}

#[inline]
fn update_batch_start(
    device_context: &ID3D11DeviceContext,
    buffer: &ID3D11Buffer,
    first_instance: u32,
) -> Result<()> {
    update_buffer(
        device_context,
        buffer,
        &[BatchParams {
            start_index: first_instance,
            _padding: [0; 3],
        }],
    )
}

#[inline]
fn set_pipeline_state(
    device_context: &ID3D11DeviceContext,
    buffer_view: &[Option<ID3D11ShaderResourceView>],
    topology: D3D_PRIMITIVE_TOPOLOGY,
    vertex_shader: &ID3D11VertexShader,
    fragment_shader: &ID3D11PixelShader,
    blend_state: &ID3D11BlendState,
) {
    unsafe {
        device_context.VSSetShaderResources(1, Some(buffer_view));
        device_context.PSSetShaderResources(1, Some(buffer_view));
        device_context.IASetPrimitiveTopology(topology);
        device_context.VSSetShader(vertex_shader, None);
        device_context.PSSetShader(fragment_shader, None);
        device_context.OMSetBlendState(blend_state, None, 0xFFFFFFFF);
    }
}

#[cfg(debug_assertions)]
fn report_live_objects(device: &ID3D11Device) -> Result<()> {
    let debug_device: ID3D11Debug = device.cast()?;
    unsafe {
        debug_device.ReportLiveDeviceObjects(D3D11_RLDO_DETAIL)?;
    }
    Ok(())
}

const BUFFER_COUNT: usize = 3;

pub(crate) mod shader_resources {
    use anyhow::Result;

    #[cfg(debug_assertions)]
    use windows::{
        Win32::Graphics::Direct3D::{
            Fxc::{D3DCOMPILE_DEBUG, D3DCOMPILE_SKIP_OPTIMIZATION, D3DCompileFromFile},
            ID3DBlob,
        },
        core::{HSTRING, PCSTR},
    };

    #[derive(Copy, Clone, Debug, Eq, PartialEq)]
    pub(crate) enum ShaderModule {
        Quad,
        Shadow,
        Underline,
        PathRasterization,
        PathSprite,
        MonochromeSprite,
        SubpixelSprite,
        PolychromeSprite,
        EmojiRasterization,
    }

    #[derive(Copy, Clone, Debug, Eq, PartialEq)]
    pub(crate) enum ShaderTarget {
        Vertex,
        Fragment,
    }

    pub(crate) struct RawShaderBytes<'t> {
        inner: &'t [u8],

        #[cfg(debug_assertions)]
        _blob: ID3DBlob,
    }

    impl<'t> RawShaderBytes<'t> {
        pub(crate) fn new(module: ShaderModule, target: ShaderTarget) -> Result<Self> {
            #[cfg(not(debug_assertions))]
            {
                Ok(Self::from_bytes(module, target))
            }
            #[cfg(debug_assertions)]
            {
                let blob = build_shader_blob(module, target)?;
                let inner = unsafe {
                    std::slice::from_raw_parts(
                        blob.GetBufferPointer() as *const u8,
                        blob.GetBufferSize(),
                    )
                };
                Ok(Self { inner, _blob: blob })
            }
        }

        pub(crate) fn as_bytes(&'t self) -> &'t [u8] {
            self.inner
        }

        #[cfg(not(debug_assertions))]
        fn from_bytes(module: ShaderModule, target: ShaderTarget) -> Self {
            let bytes = match module {
                ShaderModule::Quad => match target {
                    ShaderTarget::Vertex => QUAD_VERTEX_BYTES,
                    ShaderTarget::Fragment => QUAD_FRAGMENT_BYTES,
                },
                ShaderModule::Shadow => match target {
                    ShaderTarget::Vertex => SHADOW_VERTEX_BYTES,
                    ShaderTarget::Fragment => SHADOW_FRAGMENT_BYTES,
                },
                ShaderModule::Underline => match target {
                    ShaderTarget::Vertex => UNDERLINE_VERTEX_BYTES,
                    ShaderTarget::Fragment => UNDERLINE_FRAGMENT_BYTES,
                },
                ShaderModule::PathRasterization => match target {
                    ShaderTarget::Vertex => PATH_RASTERIZATION_VERTEX_BYTES,
                    ShaderTarget::Fragment => PATH_RASTERIZATION_FRAGMENT_BYTES,
                },
                ShaderModule::PathSprite => match target {
                    ShaderTarget::Vertex => PATH_SPRITE_VERTEX_BYTES,
                    ShaderTarget::Fragment => PATH_SPRITE_FRAGMENT_BYTES,
                },
                ShaderModule::MonochromeSprite => match target {
                    ShaderTarget::Vertex => MONOCHROME_SPRITE_VERTEX_BYTES,
                    ShaderTarget::Fragment => MONOCHROME_SPRITE_FRAGMENT_BYTES,
                },
                ShaderModule::SubpixelSprite => match target {
                    ShaderTarget::Vertex => SUBPIXEL_SPRITE_VERTEX_BYTES,
                    ShaderTarget::Fragment => SUBPIXEL_SPRITE_FRAGMENT_BYTES,
                },
                ShaderModule::PolychromeSprite => match target {
                    ShaderTarget::Vertex => POLYCHROME_SPRITE_VERTEX_BYTES,
                    ShaderTarget::Fragment => POLYCHROME_SPRITE_FRAGMENT_BYTES,
                },
                ShaderModule::EmojiRasterization => match target {
                    ShaderTarget::Vertex => EMOJI_RASTERIZATION_VERTEX_BYTES,
                    ShaderTarget::Fragment => EMOJI_RASTERIZATION_FRAGMENT_BYTES,
                },
            };
            Self { inner: bytes }
        }
    }

    #[cfg(debug_assertions)]
    pub(super) fn build_shader_blob(entry: ShaderModule, target: ShaderTarget) -> Result<ID3DBlob> {
        unsafe {
            use windows::Win32::Graphics::{
                Direct3D::ID3DInclude, Hlsl::D3D_COMPILE_STANDARD_FILE_INCLUDE,
            };

            let shader_name = if matches!(entry, ShaderModule::EmojiRasterization) {
                "color_text_raster.hlsl"
            } else {
                "shaders.hlsl"
            };

            let entry = format!(
                "{}_{}\0",
                entry.as_str(),
                match target {
                    ShaderTarget::Vertex => "vertex",
                    ShaderTarget::Fragment => "fragment",
                }
            );
            let target = match target {
                ShaderTarget::Vertex => "vs_4_1\0",
                ShaderTarget::Fragment => "ps_4_1\0",
            };

            let mut compile_blob = None;
            let mut error_blob = None;
            let shader_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join(&format!("src/{}", shader_name))
                .canonicalize()?;

            let entry_point = PCSTR::from_raw(entry.as_ptr());
            let target_cstr = PCSTR::from_raw(target.as_ptr());

            // really dirty trick because winapi bindings are unhappy otherwise
            let include_handler = &std::mem::transmute::<usize, ID3DInclude>(
                D3D_COMPILE_STANDARD_FILE_INCLUDE as usize,
            );

            let ret = D3DCompileFromFile(
                &HSTRING::from(shader_path.to_str().unwrap()),
                None,
                include_handler,
                entry_point,
                target_cstr,
                D3DCOMPILE_DEBUG | D3DCOMPILE_SKIP_OPTIMIZATION,
                0,
                &mut compile_blob,
                Some(&mut error_blob),
            );
            if ret.is_err() {
                let Some(error_blob) = error_blob else {
                    return Err(anyhow::anyhow!("{ret:?}"));
                };

                let error_string =
                    std::ffi::CStr::from_ptr(error_blob.GetBufferPointer() as *const i8)
                        .to_string_lossy();
                log::error!("Shader compile error: {}", error_string);
                return Err(anyhow::anyhow!("Compile error: {}", error_string));
            }
            Ok(compile_blob.unwrap())
        }
    }

    #[cfg(not(debug_assertions))]
    include!(concat!(env!("OUT_DIR"), "/shaders_bytes.rs"));

    #[cfg(debug_assertions)]
    impl ShaderModule {
        pub fn as_str(self) -> &'static str {
            match self {
                ShaderModule::Quad => "quad",
                ShaderModule::Shadow => "shadow",
                ShaderModule::Underline => "underline",
                ShaderModule::PathRasterization => "path_rasterization",
                ShaderModule::PathSprite => "path_sprite",
                ShaderModule::MonochromeSprite => "monochrome_sprite",
                ShaderModule::SubpixelSprite => "subpixel_sprite",
                ShaderModule::PolychromeSprite => "polychrome_sprite",
                ShaderModule::EmojiRasterization => "emoji_rasterization",
            }
        }
    }
}

mod nvidia {
    use std::{
        ffi::CStr,
        os::raw::{c_char, c_int, c_uint},
    };

    use anyhow::Result;
    use windows::{Win32::System::LibraryLoader::GetProcAddress, core::s};

    use crate::with_dll_library;

    // https://github.com/NVIDIA/nvapi/blob/7cb76fce2f52de818b3da497af646af1ec16ce27/nvapi_lite_common.h#L180
    const NVAPI_SHORT_STRING_MAX: usize = 64;

    // https://github.com/NVIDIA/nvapi/blob/7cb76fce2f52de818b3da497af646af1ec16ce27/nvapi_lite_common.h#L235
    #[allow(non_camel_case_types)]
    type NvAPI_ShortString = [c_char; NVAPI_SHORT_STRING_MAX];

    // https://github.com/NVIDIA/nvapi/blob/7cb76fce2f52de818b3da497af646af1ec16ce27/nvapi_lite_common.h#L447
    #[allow(non_camel_case_types)]
    type NvAPI_SYS_GetDriverAndBranchVersion_t = unsafe extern "C" fn(
        driver_version: *mut c_uint,
        build_branch_string: *mut NvAPI_ShortString,
    ) -> c_int;

    pub(super) fn get_driver_version() -> Result<String> {
        #[cfg(target_pointer_width = "64")]
        let nvidia_dll_name = s!("nvapi64.dll");
        #[cfg(target_pointer_width = "32")]
        let nvidia_dll_name = s!("nvapi.dll");

        with_dll_library(nvidia_dll_name, |nvidia_dll| unsafe {
            let nvapi_query_addr = GetProcAddress(nvidia_dll, s!("nvapi_QueryInterface"))
                .ok_or_else(|| anyhow::anyhow!("Failed to get nvapi_QueryInterface address"))?;
            let nvapi_query: extern "C" fn(u32) -> *mut () = std::mem::transmute(nvapi_query_addr);

            // https://github.com/NVIDIA/nvapi/blob/7cb76fce2f52de818b3da497af646af1ec16ce27/nvapi_interface.h#L41
            let nvapi_get_driver_version_ptr = nvapi_query(0x2926aaad);
            if nvapi_get_driver_version_ptr.is_null() {
                anyhow::bail!("Failed to get NVIDIA driver version function pointer");
            }
            let nvapi_get_driver_version: NvAPI_SYS_GetDriverAndBranchVersion_t =
                std::mem::transmute(nvapi_get_driver_version_ptr);

            let mut driver_version: c_uint = 0;
            let mut build_branch_string: NvAPI_ShortString = [0; NVAPI_SHORT_STRING_MAX];
            let result = nvapi_get_driver_version(
                &mut driver_version as *mut c_uint,
                &mut build_branch_string as *mut NvAPI_ShortString,
            );

            if result != 0 {
                anyhow::bail!(
                    "Failed to get NVIDIA driver version, error code: {}",
                    result
                );
            }
            let major = driver_version / 100;
            let minor = driver_version % 100;
            let branch_string = CStr::from_ptr(build_branch_string.as_ptr());
            Ok(format!(
                "{}.{} {}",
                major,
                minor,
                branch_string.to_string_lossy()
            ))
        })
    }
}

mod amd {
    use std::os::raw::{c_char, c_int, c_void};

    use anyhow::Result;
    use windows::{Win32::System::LibraryLoader::GetProcAddress, core::s};

    use crate::with_dll_library;

    // https://github.com/GPUOpen-LibrariesAndSDKs/AGS_SDK/blob/5d8812d703d0335741b6f7ffc37838eeb8b967f7/ags_lib/inc/amd_ags.h#L145
    const AGS_CURRENT_VERSION: i32 = (6 << 22) | (3 << 12);

    // https://github.com/GPUOpen-LibrariesAndSDKs/AGS_SDK/blob/5d8812d703d0335741b6f7ffc37838eeb8b967f7/ags_lib/inc/amd_ags.h#L204
    // This is an opaque type, using struct to represent it properly for FFI
    #[repr(C)]
    struct AGSContext {
        _private: [u8; 0],
    }

    #[repr(C)]
    pub struct AGSGPUInfo {
        pub driver_version: *const c_char,
        pub radeon_software_version: *const c_char,
        pub num_devices: c_int,
        pub devices: *mut c_void,
    }

    // https://github.com/GPUOpen-LibrariesAndSDKs/AGS_SDK/blob/5d8812d703d0335741b6f7ffc37838eeb8b967f7/ags_lib/inc/amd_ags.h#L429
    #[allow(non_camel_case_types)]
    type agsInitialize_t = unsafe extern "C" fn(
        version: c_int,
        config: *const c_void,
        context: *mut *mut AGSContext,
        gpu_info: *mut AGSGPUInfo,
    ) -> c_int;

    // https://github.com/GPUOpen-LibrariesAndSDKs/AGS_SDK/blob/5d8812d703d0335741b6f7ffc37838eeb8b967f7/ags_lib/inc/amd_ags.h#L436
    #[allow(non_camel_case_types)]
    type agsDeInitialize_t = unsafe extern "C" fn(context: *mut AGSContext) -> c_int;

    pub(super) fn get_driver_version() -> Result<String> {
        #[cfg(target_pointer_width = "64")]
        let amd_dll_name = s!("amd_ags_x64.dll");
        #[cfg(target_pointer_width = "32")]
        let amd_dll_name = s!("amd_ags_x86.dll");

        with_dll_library(amd_dll_name, |amd_dll| unsafe {
            let ags_initialize_addr = GetProcAddress(amd_dll, s!("agsInitialize"))
                .ok_or_else(|| anyhow::anyhow!("Failed to get agsInitialize address"))?;
            let ags_deinitialize_addr = GetProcAddress(amd_dll, s!("agsDeInitialize"))
                .ok_or_else(|| anyhow::anyhow!("Failed to get agsDeInitialize address"))?;

            let ags_initialize: agsInitialize_t = std::mem::transmute(ags_initialize_addr);
            let ags_deinitialize: agsDeInitialize_t = std::mem::transmute(ags_deinitialize_addr);

            let mut context: *mut AGSContext = std::ptr::null_mut();
            let mut gpu_info: AGSGPUInfo = AGSGPUInfo {
                driver_version: std::ptr::null(),
                radeon_software_version: std::ptr::null(),
                num_devices: 0,
                devices: std::ptr::null_mut(),
            };

            let result = ags_initialize(
                AGS_CURRENT_VERSION,
                std::ptr::null(),
                &mut context,
                &mut gpu_info,
            );
            if result != 0 {
                anyhow::bail!("Failed to initialize AMD AGS, error code: {}", result);
            }

            // Vulkan actually returns this as the driver version
            let software_version = if !gpu_info.radeon_software_version.is_null() {
                std::ffi::CStr::from_ptr(gpu_info.radeon_software_version)
                    .to_string_lossy()
                    .into_owned()
            } else {
                "Unknown Radeon Software Version".to_string()
            };

            let driver_version = if !gpu_info.driver_version.is_null() {
                std::ffi::CStr::from_ptr(gpu_info.driver_version)
                    .to_string_lossy()
                    .into_owned()
            } else {
                "Unknown Radeon Driver Version".to_string()
            };

            ags_deinitialize(context);
            Ok(format!("{} ({})", software_version, driver_version))
        })
    }
}

mod dxgi {
    use windows::{
        Win32::Graphics::Dxgi::{IDXGIAdapter1, IDXGIDevice},
        core::Interface,
    };

    pub(super) fn get_driver_version(adapter: &IDXGIAdapter1) -> anyhow::Result<String> {
        let number = unsafe { adapter.CheckInterfaceSupport(&IDXGIDevice::IID as _) }?;
        Ok(format!(
            "{}.{}.{}.{}",
            number >> 48,
            (number >> 32) & 0xFFFF,
            (number >> 16) & 0xFFFF,
            number & 0xFFFF
        ))
    }
}
