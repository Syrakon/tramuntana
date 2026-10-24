/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

#![deny(unsafe_code)]

use std::cell::{Cell, RefCell, RefMut};
use std::num::NonZeroU32;
use std::os::fd::OwnedFd;
use std::rc::Rc;
use std::sync::Arc;

use dpi::PhysicalSize;
use embedder_traits::RefreshDriver;
use euclid::default::{Rect, Size2D as UntypedSize2D};
use euclid::{Point2D, Size2D};
use gleam::gl::{self, Gl};
use glow::{HasContext, NativeFramebuffer};
use image::RgbaImage;
use log::{debug, trace, warn};
use raw_window_handle::{DisplayHandle, WindowHandle};
pub use surfman::Error;
use surfman::chains::{PreserveBuffer, SwapChain};
use surfman::{
    Adapter, Connection, Context, ContextAttributeFlags, ContextAttributes, Device, GLApi,
    GLVersion, NativeWidget, Surface, SurfaceAccess, SurfaceInfo, SurfaceTexture, SurfaceType,
};
use webrender_api::units::{DeviceIntRect, DevicePixel};

/// The `RenderingContext` trait defines a set of methods for managing
/// an OpenGL or GLES rendering context.
/// Implementors of this trait are responsible for handling the creation,
/// management, and destruction of the rendering context and its associated
/// resources.
pub trait RenderingContext {
    /// Prepare this [`RenderingContext`] to be rendered upon by Servo. For instance,
    /// by binding a framebuffer to the current OpenGL context.
    fn prepare_for_rendering(&self) {}
    /// Read the contents of this [`Renderingcontext`] into an in-memory image. If the
    /// image cannot be read (for instance, if no rendering has taken place yet), then
    /// `None` is returned.
    ///
    /// In a double-buffered [`RenderingContext`] this is expected to read from the back
    /// buffer. That means that once Servo renders to the context, this should return those
    /// results, even before [`RenderingContext::present`] is called.
    fn read_to_image(&self, source_rectangle: DeviceIntRect) -> Option<RgbaImage>;
    /// Get the current size of this [`RenderingContext`].
    fn size(&self) -> PhysicalSize<u32>;
    /// Get the current size of this [`RenderingContext`] as [`Size2D`].
    fn size2d(&self) -> Size2D<u32, DevicePixel> {
        let size = self.size();
        Size2D::new(size.width, size.height)
    }
    /// Resizes the rendering surface to the given size.
    fn resize(&self, size: PhysicalSize<u32>);
    /// Presents the rendered frame to the screen. In a double-buffered context, this would
    /// swap buffers.
    fn present(&self);
    /// Makes the context the current OpenGL context for this thread.
    /// After calling this function, it is valid to use OpenGL rendering
    /// commands.
    fn make_current(&self) -> Result<(), Error>;
    /// Returns the `gleam` version of the OpenGL or GLES API.
    fn gleam_gl_api(&self) -> Rc<dyn gleam::gl::Gl>;
    /// Returns the OpenGL or GLES API.
    fn glow_gl_api(&self) -> Arc<glow::Context>;
    /// Creates a texture from a given surface and returns the surface texture,
    /// the OpenGL texture object, and the size of the surface. Default to `None`.
    fn create_texture(
        &self,
        _surface: Surface,
    ) -> Option<(SurfaceTexture, u32, UntypedSize2D<i32>)> {
        None
    }
    /// Destroys the texture and returns the surface. Default to `None`.
    fn destroy_texture(&self, _surface_texture: SurfaceTexture) -> Option<Surface> {
        None
    }
    /// The connection to the display server for WebGL. Default to `None`.
    fn connection(&self) -> Option<Connection> {
        None
    }
    /// Return the [`RefreshDriver`] for this [`RenderingContext`]. If `None` is returned,
    /// then the default timer-based [`RefreshDriver`] will be used.
    fn refresh_driver(&self) -> Option<Rc<dyn RefreshDriver>> {
        None
    }
}

/// A rendering context that uses the Surfman library to create and manage
/// the OpenGL context and surface. This struct provides the default implementation
/// of the `RenderingContext` trait, handling the creation, management, and destruction
/// of the rendering context and its associated resources.
///
/// The `SurfmanRenderingContext` struct encapsulates the necessary data and methods
/// to interact with the Surfman library, including creating surfaces, binding surfaces,
/// resizing surfaces, presenting rendered frames, and managing the OpenGL context state.
struct SurfmanRenderingContext {
    gleam_gl: Rc<dyn Gl>,
    glow_gl: Arc<glow::Context>,
    device: RefCell<Device>,
    context: RefCell<Context>,
    refresh_driver: Option<Rc<dyn RefreshDriver>>,
}

impl Drop for SurfmanRenderingContext {
    fn drop(&mut self) {
        let device = &mut self.device.borrow_mut();
        let context = &mut self.context.borrow_mut();
        let _ = device.destroy_context(context);
    }
}

impl SurfmanRenderingContext {
    fn new(
        connection: &Connection,
        adapter: &Adapter,
        refresh_driver: Option<Rc<dyn RefreshDriver>>,
    ) -> Result<Self, Error> {
        let device = connection.create_device(adapter)?;

        let flags = ContextAttributeFlags::ALPHA |
            ContextAttributeFlags::DEPTH |
            ContextAttributeFlags::STENCIL;
        let gl_api = connection.gl_api();
        let version = match &gl_api {
            GLApi::GLES => surfman::GLVersion { major: 3, minor: 0 },
            GLApi::GL => surfman::GLVersion { major: 3, minor: 2 },
        };
        let context_descriptor =
            device.create_context_descriptor(&ContextAttributes { flags, version })?;

        let mut context = device
            .create_context(&context_descriptor, None)
            .inspect_err(|_| {
                print_diagnostics_information_on_context_creation_failure(&device, gl_api, version)
            })?;

        // `glow::Context::from_loader_function` immediately queries `GL_VERSION`, so the
        // context must be current.
        if let Err(error) = device.make_context_current(&context) {
            let _ = device.destroy_context(&mut context);
            return Err(error);
        }

        #[expect(unsafe_code)]
        let gleam_gl = {
            match gl_api {
                GLApi::GL => unsafe {
                    gl::GlFns::load_with(|func_name| device.get_proc_address(&context, func_name))
                },
                GLApi::GLES => unsafe {
                    gl::GlesFns::load_with(|func_name| device.get_proc_address(&context, func_name))
                },
            }
        };

        #[expect(unsafe_code)]
        let glow_gl = unsafe {
            glow::Context::from_loader_function(|function_name| {
                device.get_proc_address(&context, function_name)
            })
        };

        Ok(SurfmanRenderingContext {
            gleam_gl,
            glow_gl: Arc::new(glow_gl),
            device: RefCell::new(device),
            context: RefCell::new(context),
            refresh_driver,
        })
    }

    fn create_surface(&self, surface_type: SurfaceType<NativeWidget>) -> Result<Surface, Error> {
        let device = &mut self.device.borrow_mut();
        let context = &self.context.borrow();
        device.create_surface(context, SurfaceAccess::GPUOnly, surface_type)
    }

    fn bind_surface(&self, surface: Surface) -> Result<(), Error> {
        let device = &self.device.borrow();
        let context = &mut self.context.borrow_mut();
        device
            .bind_surface_to_context(context, surface)
            .map_err(|(err, mut surface)| {
                let _ = device.destroy_surface(context, &mut surface);
                err
            })?;
        Ok(())
    }

    fn create_attached_swap_chain(&self) -> Result<SwapChain<Device>, Error> {
        let device = &mut self.device.borrow_mut();
        let context = &mut self.context.borrow_mut();
        SwapChain::create_attached(device, context, SurfaceAccess::GPUOnly)
    }

    fn resize_bound_surface(&self, size: PhysicalSize<u32>) -> Result<(), Error> {
        if size.width == 0 || size.height == 0 {
            log::error!("Unable to resize to size under 1x1 ({size:?} provided)");
            return Err(Error::Failed);
        }

        let size = Size2D::new(size.width as i32, size.height as i32);
        self.device
            .borrow()
            .resize_bound_surface(&mut self.context.borrow_mut(), size)
    }

    fn present_bound_surface(&self) -> Result<(), Error> {
        self.device
            .borrow()
            .present_bound_surface(&mut self.context.borrow_mut())
    }

    fn framebuffer(&self) -> Option<NativeFramebuffer> {
        let device = &self.device.borrow();
        let context = &self.context.borrow();
        device
            .context_surface_info(context)
            .unwrap_or(None)
            .and_then(|info| info.framebuffer_object)
    }

    fn prepare_for_rendering(&self) {
        let framebuffer_id = self
            .framebuffer()
            .map_or(0, |framebuffer| framebuffer.0.into());
        self.gleam_gl
            .bind_framebuffer(gleam::gl::FRAMEBUFFER, framebuffer_id);
    }

    fn read_to_image(&self, source_rectangle: DeviceIntRect) -> Option<RgbaImage> {
        let framebuffer_id = self
            .framebuffer()
            .map_or(0, |framebuffer| framebuffer.0.into());
        Framebuffer::read_framebuffer_to_image(&self.gleam_gl, framebuffer_id, source_rectangle)
    }

    fn make_current(&self) -> Result<(), Error> {
        let device = &self.device.borrow();
        let context = &mut self.context.borrow();
        device.make_context_current(context)
    }

    fn create_texture(
        &self,
        surface: Surface,
    ) -> Option<(SurfaceTexture, u32, UntypedSize2D<i32>)> {
        let device = &self.device.borrow();
        let context = &mut self.context.borrow_mut();
        let SurfaceInfo {
            id: front_buffer_id,
            size,
            ..
        } = device.surface_info(&surface);
        debug!("... getting texture for surface {:?}", front_buffer_id);
        let surface_texture = device.create_surface_texture(context, surface).unwrap();
        let gl_texture = device
            .surface_texture_object(&surface_texture)
            .map(|tex| tex.0.get())
            .unwrap_or(0);
        Some((surface_texture, gl_texture, size))
    }

    fn destroy_texture(&self, surface_texture: SurfaceTexture) -> Option<Surface> {
        let device = &self.device.borrow();
        let context = &mut self.context.borrow_mut();
        device
            .destroy_surface_texture(context, surface_texture)
            .map_err(|(error, _)| error)
            .ok()
    }

    fn connection(&self) -> Option<Connection> {
        Some(self.device.borrow().connection())
    }

    fn refresh_driver(&self) -> Option<Rc<dyn RefreshDriver>> {
        self.refresh_driver.clone()
    }
}

/// A software rendering context that uses a software OpenGL implementation to render
/// Servo. This will generally have bad performance, but can be used in situations where
/// it is more convenient to have consistent, but slower display output.
///
/// The results of the render can be accessed via [`RenderingContext::read_to_image`].
pub struct SoftwareRenderingContext {
    size: Cell<PhysicalSize<u32>>,
    surfman_rendering_info: SurfmanRenderingContext,
    swap_chain: SwapChain<Device>,
}

impl SoftwareRenderingContext {
    pub fn new(size: PhysicalSize<u32>) -> Result<Self, Error> {
        if size.width == 0 || size.height == 0 {
            log::error!(
                "Unable to create SoftwareRenderingContext with size under 1x1 ({size:?} provided)"
            );
            return Err(Error::Failed);
        }

        let connection = Connection::new()?;
        let adapter = connection.create_software_adapter()?;
        let surfman_rendering_info = SurfmanRenderingContext::new(&connection, &adapter, None)?;

        let surfman_size = Size2D::new(size.width as i32, size.height as i32);
        let surface =
            surfman_rendering_info.create_surface(SurfaceType::Generic { size: surfman_size })?;
        surfman_rendering_info.bind_surface(surface)?;
        surfman_rendering_info.make_current()?;

        let swap_chain = surfman_rendering_info.create_attached_swap_chain()?;
        Ok(SoftwareRenderingContext {
            size: Cell::new(size),
            surfman_rendering_info,
            swap_chain,
        })
    }
}

impl Drop for SoftwareRenderingContext {
    fn drop(&mut self) {
        let device = &mut self.surfman_rendering_info.device.borrow_mut();
        let context = &mut self.surfman_rendering_info.context.borrow_mut();
        let _ = self.swap_chain.destroy(device, context);
    }
}

impl RenderingContext for SoftwareRenderingContext {
    fn prepare_for_rendering(&self) {
        self.surfman_rendering_info.prepare_for_rendering();
    }

    fn read_to_image(&self, source_rectangle: DeviceIntRect) -> Option<RgbaImage> {
        self.surfman_rendering_info.read_to_image(source_rectangle)
    }

    fn size(&self) -> PhysicalSize<u32> {
        self.size.get()
    }

    fn resize(&self, size: PhysicalSize<u32>) {
        assert!(
            size.width > 0 && size.height > 0,
            "Dimensions must be at least 1x1, got {size:?}",
        );

        if self.size.get() == size {
            return;
        }

        self.size.set(size);

        let device = &mut self.surfman_rendering_info.device.borrow_mut();
        let context = &mut self.surfman_rendering_info.context.borrow_mut();
        let size = Size2D::new(size.width as i32, size.height as i32);
        let _ = self.swap_chain.resize(device, context, size);
    }

    #[servo_tracing::instrument(skip_all, name = "SoftwareRenderingContext::present")]
    fn present(&self) {
        let device = &mut self.surfman_rendering_info.device.borrow_mut();
        let context = &mut self.surfman_rendering_info.context.borrow_mut();
        let _ = self
            .swap_chain
            .swap_buffers(device, context, PreserveBuffer::No);
    }

    fn make_current(&self) -> Result<(), Error> {
        self.surfman_rendering_info.make_current()
    }

    fn gleam_gl_api(&self) -> Rc<dyn gleam::gl::Gl> {
        self.surfman_rendering_info.gleam_gl.clone()
    }

    fn glow_gl_api(&self) -> Arc<glow::Context> {
        self.surfman_rendering_info.glow_gl.clone()
    }

    fn create_texture(
        &self,
        surface: Surface,
    ) -> Option<(SurfaceTexture, u32, UntypedSize2D<i32>)> {
        self.surfman_rendering_info.create_texture(surface)
    }

    fn destroy_texture(&self, surface_texture: SurfaceTexture) -> Option<Surface> {
        self.surfman_rendering_info.destroy_texture(surface_texture)
    }

    fn connection(&self) -> Option<Connection> {
        self.surfman_rendering_info.connection()
    }
}

/// Tramuntana-only: opaque handle to a dma-buf exported from the current
/// framebuffer's color attachment. Used by the Stage 2 zero-copy bridge
/// between Servo's GL output and gpui's Vulkan compositor (see
/// `docs/STAGE_2_GPU_SHARING.md`).
///
/// The caller owns `fd` and is responsible for closing it (handled
/// automatically by `OwnedFd`'s Drop impl). The `width`/`height`/`fourcc`/
/// `modifier` fields are needed by the importer to construct a matching
/// `wgpu::Texture` via `VK_EXT_external_memory_dma_buf` +
/// `VK_EXT_image_drm_format_modifier`.
#[derive(Debug)]
pub struct DmaBufHandle {
    /// Stable, unique id of the export-ring buffer this handle came from. Lets
    /// the consumer cache the imported GPU texture per physical buffer instead
    /// of re-importing each frame. Distinct buffers (ring slots, post-resize
    /// rebuilds) always get distinct ids.
    pub buffer_id: u64,
    /// Owned dma-buf file descriptor (O_CLOEXEC). Single-plane only.
    pub fd: OwnedFd,
    pub width: u32,
    pub height: u32,
    /// DRM fourcc, e.g. `DRM_FORMAT_ABGR8888` (0x34324241) or
    /// `DRM_FORMAT_ARGB8888` (0x34325241). Consumer must match this.
    pub fourcc: u32,
    pub stride: u32,
    pub offset: u32,
    /// DRM format modifier. `DRM_FORMAT_MOD_LINEAR` (0) for linear,
    /// vendor-specific tilings otherwise. Consumer must pass this to
    /// Vulkan via `VK_EXT_image_drm_format_modifier`.
    pub modifier: u64,
    /// Optional sync fd from `EGL_ANDROID_native_fence_sync`. When present,
    /// the consumer must wait on it before sampling the texture (e.g. via
    /// `VK_KHR_external_semaphore_fd` with `SYNC_FD` handle type) to avoid
    /// reading partial GL writes. `None` means the driver did not expose
    /// the fence-sync extensions; the consumer should fall back to a
    /// coarser barrier such as `vkQueueWaitIdle` or accept tearing.
    pub fence_fd: Option<OwnedFd>,
    /// Marks the buffer as in use for as long as any clone of it lives. The
    /// producer does not blit into a buffer whose lease is held, so the
    /// consumer keeps a clone until the GPU has finished every frame that
    /// samples the buffer.
    pub lease: DmaBufLease,
}

/// See [`DmaBufHandle::lease`]. Cheap to clone; carries no data. Only held:
/// the producer counts the clones of `_readers` that are alive.
#[derive(Clone, Debug)]
pub struct DmaBufLease {
    _readers: Arc<()>,
}

/// Why [`WindowRenderingContext::export_framebuffer_dma_buf`] (or the
/// offscreen variant) produced no handle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DmaBufExportError {
    /// Every buffer of the export ring is still leased by the consumer. The
    /// framebuffer is untouched: export again on a later frame.
    Busy,
    /// The export path is not available, or failed; the cause is logged.
    Unavailable,
}

/// A [`RenderingContext`] that uses the `surfman` library to render to a
/// `raw-window-handle` identified window. `surfman` will attempt to create an
/// OpenGL context and surface for this window. This is a simple implementation
/// of the [`RenderingContext`] trait, but by default it paints to the entire
/// window surface.
///
/// If you would like to paint to only a portion of the window, consider using
/// [`OffscreenRenderingContext`] by calling [`WindowRenderingContext::offscreen_context`].
pub struct WindowRenderingContext {
    /// The inner size of the window in physical pixels which excludes OS decorations.
    size: Cell<PhysicalSize<u32>>,
    surfman_context: SurfmanRenderingContext,
    /// Lazy-init cache for the Stage 2 dma-buf export path. Holds a private GL
    /// texture (separate from surfman's surface textures, which already have
    /// EGL siblings claimed by WebRender / wgpu_hal::gles), a backing FBO,
    /// an EGLImage built on top of that texture, and the resulting dma-buf
    /// fd. Each call to `export_framebuffer_dma_buf` blits surfman's FBO
    /// onto our private texture (GPU-to-GPU) and dups the cached fd. See
    /// `docs/STAGE_2_GPU_SHARING.md`.
    dma_buf_cache: RefCell<Option<dma_buf_export::Cache>>,
}

impl WindowRenderingContext {
    pub fn new(
        display_handle: DisplayHandle,
        window_handle: WindowHandle,
        size: PhysicalSize<u32>,
    ) -> Result<Self, Error> {
        Self::new_with_optional_refresh_driver(display_handle, window_handle, size, None)
    }

    pub fn new_with_refresh_driver(
        display_handle: DisplayHandle,
        window_handle: WindowHandle,
        size: PhysicalSize<u32>,
        refresh_driver: Rc<dyn RefreshDriver>,
    ) -> Result<Self, Error> {
        Self::new_with_optional_refresh_driver(
            display_handle,
            window_handle,
            size,
            Some(refresh_driver),
        )
    }

    fn new_with_optional_refresh_driver(
        display_handle: DisplayHandle,
        window_handle: WindowHandle,
        size: PhysicalSize<u32>,
        refresh_driver: Option<Rc<dyn RefreshDriver>>,
    ) -> Result<Self, Error> {
        if size.width == 0 || size.height == 0 {
            log::error!(
                "Unable to create WindowRenderingContext with size under 1x1 ({size:?} provided)"
            );
            return Err(Error::Failed);
        }

        let connection = Connection::from_display_handle(display_handle)?;
        let adapter = connection.create_adapter()?;
        let surfman_context = SurfmanRenderingContext::new(&connection, &adapter, refresh_driver)?;

        let native_widget = connection
            .create_native_widget_from_window_handle(
                window_handle,
                Size2D::new(size.width as i32, size.height as i32),
            )
            .expect("Failed to create native widget");

        let surface = surfman_context.create_surface(SurfaceType::Widget { native_widget })?;
        surfman_context.bind_surface(surface)?;
        surfman_context.make_current()?;

        Ok(Self {
            size: Cell::new(size),
            surfman_context,
            dma_buf_cache: RefCell::new(None),
        })
    }

    /// Tramuntana-only: build a `WindowRenderingContext` that does **not** own the
    /// window's drawable surface. The connection is opened from `display_handle`
    /// (so we get a real GPU adapter — WebGL works), but the bound surface is a
    /// `Generic` offscreen one. Use this when the host (e.g. GPUI) already owns
    /// the visible surface and Servo's output will be consumed via
    /// `offscreen_context()` + `read_to_image()`.
    pub fn new_offscreen_only(
        display_handle: DisplayHandle,
        size: PhysicalSize<u32>,
    ) -> Result<Self, Error> {
        if size.width == 0 || size.height == 0 {
            log::error!(
                "Unable to create WindowRenderingContext with size under 1x1 ({size:?} provided)"
            );
            return Err(Error::Failed);
        }

        let connection = Connection::from_display_handle(display_handle)?;
        let adapter = connection.create_hardware_adapter()?;
        let surfman_context = SurfmanRenderingContext::new(&connection, &adapter, None)?;

        let surfman_size = Size2D::new(size.width as i32, size.height as i32);
        let surface =
            surfman_context.create_surface(SurfaceType::Generic { size: surfman_size })?;
        surfman_context.bind_surface(surface)?;
        surfman_context.make_current()?;

        Ok(Self {
            size: Cell::new(size),
            surfman_context,
            dma_buf_cache: RefCell::new(None),
        })
    }

    pub fn offscreen_context(
        self: &Rc<Self>,
        size: PhysicalSize<u32>,
    ) -> OffscreenRenderingContext {
        OffscreenRenderingContext::new(self.clone(), size)
    }

    /// Stop rendering to the window that was used to create this `WindowRenderingContext`
    /// or last set with [`Self::set_window`].
    ///
    /// TODO: This should be removed once `WebView`s can replace their `RenderingContext`s.
    pub fn take_window(&self) -> Result<(), Error> {
        let device = self.surfman_context.device.borrow_mut();
        let mut context = self.surfman_context.context.borrow_mut();
        let mut surface = device.unbind_surface_from_context(&mut context)?.unwrap();
        device.destroy_surface(&mut context, &mut surface)?;
        Ok(())
    }

    /// Replace the window that this [`WindowRenderingContext`] renders to and give it a new
    /// size.
    ///
    /// TODO: This should be removed once `WebView`s can replace their `RenderingContext`s.
    pub fn set_window(
        &self,
        window_handle: WindowHandle,
        size: PhysicalSize<u32>,
    ) -> Result<(), Error> {
        let device = self.surfman_context.device.borrow_mut();
        let mut context = self.surfman_context.context.borrow_mut();

        let native_widget = device
            .connection()
            .create_native_widget_from_window_handle(
                window_handle,
                Size2D::new(size.width as i32, size.height as i32),
            )
            .expect("Failed to create native widget");

        let surface_access = SurfaceAccess::GPUOnly;
        let surface_type = SurfaceType::Widget { native_widget };
        let surface = device.create_surface(&context, surface_access, surface_type)?;

        device
            .bind_surface_to_context(&mut context, surface)
            .map_err(|(err, mut surface)| {
                let _ = device.destroy_surface(&mut context, &mut surface);
                err
            })?;
        device.make_context_current(&context)?;
        Ok(())
    }

    pub fn surfman_details(&self) -> (RefMut<'_, Device>, RefMut<'_, Context>) {
        (
            self.surfman_context.device.borrow_mut(),
            self.surfman_context.context.borrow_mut(),
        )
    }

    /// Tramuntana-only: return the OpenGL texture name backing the color
    /// attachment of the currently bound offscreen framebuffer. This is the
    /// texture Servo paints into; it is the entry point for cross-API GPU
    /// sharing (Stage 2 of the rumb bridge — see
    /// `docs/STAGE_2_GPU_SHARING.md`).
    ///
    /// Returns `None` if no surface is bound, if the color attachment is a
    /// renderbuffer (not a texture), or if making the context current fails.
    ///
    /// The returned ID is **not** guaranteed to be stable across frames:
    /// surfman's `present_bound_surface` rotates the underlying surface,
    /// which may change the texture identity. Callers that re-use GPU
    /// resources keyed on this ID must re-validate it after every paint.
    pub fn framebuffer_color_texture_id(&self) -> Option<u32> {
        let device = self.surfman_context.device.borrow();
        let context = self.surfman_context.context.borrow();
        device.make_context_current(&context).ok()?;

        let fb_id = device
            .context_surface_info(&context)
            .ok()??
            .framebuffer_object
            .map(|fb| fb.0.get())
            .unwrap_or(0);

        let gl = &self.surfman_context.gleam_gl;
        gl.bind_framebuffer(gleam::gl::FRAMEBUFFER, fb_id);

        let attachment_type = gl.get_framebuffer_attachment_parameter_iv(
            gleam::gl::FRAMEBUFFER,
            gleam::gl::COLOR_ATTACHMENT0,
            gleam::gl::FRAMEBUFFER_ATTACHMENT_OBJECT_TYPE,
        );
        if attachment_type != gleam::gl::TEXTURE as i32 {
            return None;
        }

        let texture_id = gl.get_framebuffer_attachment_parameter_iv(
            gleam::gl::FRAMEBUFFER,
            gleam::gl::COLOR_ATTACHMENT0,
            gleam::gl::FRAMEBUFFER_ATTACHMENT_OBJECT_NAME,
        );
        u32::try_from(texture_id).ok()
    }

    /// Tramuntana-only: export the current framebuffer's color attachment as
    /// a dma-buf file descriptor for zero-copy cross-API sharing. Uses
    /// `EGL_MESA_image_dma_buf_export` (the standard MESA extension; on NVIDIA
    /// it is exposed by the proprietary driver alongside `EGL_NV_*` siblings).
    ///
    /// Returns [`DmaBufExportError::Busy`] while the consumer still holds the
    /// lease of every ring buffer (see [`DmaBufHandle::lease`]), and
    /// [`DmaBufExportError::Unavailable`] if:
    /// - The current FBO has no GL texture color attachment
    ///   (see [`framebuffer_color_texture_id`])
    /// - No EGL display/context is current (e.g. running under GLX)
    /// - The driver does not expose the required extension entry points
    /// - The exported image has more than one plane (we only handle
    ///   single-plane RGBA today)
    /// - Any EGL call fails
    ///
    /// On success, the returned [`DmaBufHandle`] owns the dma-buf fd. The
    /// caller must close it (handled by `OwnedFd`'s Drop). The EGLImage
    /// created internally is destroyed before returning; fd ownership has
    /// transferred to the caller by then.
    ///
    /// **Not** safe to assume the returned dma-buf survives subsequent
    /// paints — surfman may rotate the underlying surface. Treat each
    /// successful export as a single-frame snapshot.
    ///
    /// See `docs/STAGE_2_GPU_SHARING.md` for the full producer/consumer
    /// design.
    ///
    /// `importable` lists the DRM modifiers the consumer can import
    /// `DRM_FORMAT_ABGR8888` with. When given, the export buffers are
    /// allocated with one of them, and the export is `Unavailable` when none
    /// works; `None` lets the driver pick, for a consumer that cannot say.
    pub fn export_framebuffer_dma_buf(
        &self,
        importable: Option<&[u64]>,
    ) -> Result<DmaBufHandle, DmaBufExportError> {
        let size = self.size.get();

        // Identify Servo's bound FBO so we can blit FROM it. We can't export
        // its color attachment directly because that texture is already an
        // EGL sibling claimed by WebRender / wgpu_hal::gles (eglCreateImageKHR
        // returns EGL_BAD_ACCESS). Instead we keep a private texture of our
        // own and copy the framebuffer onto it on the GPU each frame.
        let source_fbo = {
            let device = self.surfman_context.device.borrow();
            let context = self.surfman_context.context.borrow();
            device
                .make_context_current(&context)
                .map_err(|_| DmaBufExportError::Unavailable)?;
            device
                .context_surface_info(&context)
                .ok()
                .flatten()
                .ok_or(DmaBufExportError::Unavailable)?
                .framebuffer_object
                .map(|fb| fb.0.get())
                .unwrap_or(0)
        };

        let mut cache_slot = self.dma_buf_cache.borrow_mut();
        dma_buf_export::blit_and_export(
            &mut cache_slot,
            size,
            source_fbo,
            &self.surfman_context.gleam_gl,
            importable,
        )
    }
}

impl RenderingContext for WindowRenderingContext {
    fn prepare_for_rendering(&self) {
        self.surfman_context.prepare_for_rendering();
    }

    fn read_to_image(&self, source_rectangle: DeviceIntRect) -> Option<RgbaImage> {
        self.surfman_context.read_to_image(source_rectangle)
    }

    fn size(&self) -> PhysicalSize<u32> {
        self.size.get()
    }

    fn resize(&self, size: PhysicalSize<u32>) {
        match self.surfman_context.resize_bound_surface(size) {
            Ok(..) => self.size.set(size),
            Err(error) => warn!("Error resizing surface: {error:?}"),
        }
    }

    #[servo_tracing::instrument(skip_all, name = "WindowRenderingContext::present")]
    fn present(&self) {
        if let Err(error) = self.surfman_context.present_bound_surface() {
            warn!("Error presenting surface: {error:?}");
        }
    }

    fn make_current(&self) -> Result<(), Error> {
        self.surfman_context.make_current()
    }

    fn gleam_gl_api(&self) -> Rc<dyn gleam::gl::Gl> {
        self.surfman_context.gleam_gl.clone()
    }

    fn glow_gl_api(&self) -> Arc<glow::Context> {
        self.surfman_context.glow_gl.clone()
    }

    fn create_texture(
        &self,
        surface: Surface,
    ) -> Option<(SurfaceTexture, u32, UntypedSize2D<i32>)> {
        self.surfman_context.create_texture(surface)
    }

    fn destroy_texture(&self, surface_texture: SurfaceTexture) -> Option<Surface> {
        self.surfman_context.destroy_texture(surface_texture)
    }

    fn connection(&self) -> Option<Connection> {
        self.surfman_context.connection()
    }

    fn refresh_driver(&self) -> Option<Rc<dyn RefreshDriver>> {
        self.surfman_context.refresh_driver()
    }
}

struct Framebuffer {
    gl: Rc<dyn Gl>,
    framebuffer_id: gl::GLuint,
    renderbuffer_id: gl::GLuint,
    texture_id: gl::GLuint,
}

impl Framebuffer {
    fn bind(&self) {
        trace!("Binding FBO {}", self.framebuffer_id);
        self.gl
            .bind_framebuffer(gl::FRAMEBUFFER, self.framebuffer_id)
    }
}

impl Drop for Framebuffer {
    fn drop(&mut self) {
        self.gl.bind_framebuffer(gl::FRAMEBUFFER, 0);
        self.gl.delete_textures(&[self.texture_id]);
        self.gl.delete_renderbuffers(&[self.renderbuffer_id]);
        self.gl.delete_framebuffers(&[self.framebuffer_id]);
    }
}

impl Framebuffer {
    fn new(gl: Rc<dyn Gl>, size: PhysicalSize<u32>) -> Self {
        let framebuffer_ids = gl.gen_framebuffers(1);
        gl.bind_framebuffer(gl::FRAMEBUFFER, framebuffer_ids[0]);

        let texture_ids = gl.gen_textures(1);
        gl.bind_texture(gl::TEXTURE_2D, texture_ids[0]);
        gl.tex_image_2d(
            gl::TEXTURE_2D,
            0,
            gl::RGBA as gl::GLint,
            size.width as gl::GLsizei,
            size.height as gl::GLsizei,
            0,
            gl::RGBA,
            gl::UNSIGNED_BYTE,
            None,
        );
        gl.tex_parameter_i(
            gl::TEXTURE_2D,
            gl::TEXTURE_MAG_FILTER,
            gl::NEAREST as gl::GLint,
        );
        gl.tex_parameter_i(
            gl::TEXTURE_2D,
            gl::TEXTURE_MIN_FILTER,
            gl::NEAREST as gl::GLint,
        );

        gl.framebuffer_texture_2d(
            gl::FRAMEBUFFER,
            gl::COLOR_ATTACHMENT0,
            gl::TEXTURE_2D,
            texture_ids[0],
            0,
        );

        gl.bind_texture(gl::TEXTURE_2D, 0);

        let renderbuffer_ids = gl.gen_renderbuffers(1);
        let depth_rb = renderbuffer_ids[0];
        gl.bind_renderbuffer(gl::RENDERBUFFER, depth_rb);
        gl.renderbuffer_storage(
            gl::RENDERBUFFER,
            gl::DEPTH_COMPONENT24,
            size.width as gl::GLsizei,
            size.height as gl::GLsizei,
        );
        gl.framebuffer_renderbuffer(
            gl::FRAMEBUFFER,
            gl::DEPTH_ATTACHMENT,
            gl::RENDERBUFFER,
            depth_rb,
        );

        Self {
            gl,
            framebuffer_id: *framebuffer_ids
                .first()
                .expect("Guaranteed by GL operations"),
            renderbuffer_id: *renderbuffer_ids
                .first()
                .expect("Guaranteed by GL operations"),
            texture_id: *texture_ids.first().expect("Guaranteed by GL operations"),
        }
    }

    fn read_to_image(&self, source_rectangle: DeviceIntRect) -> Option<RgbaImage> {
        Self::read_framebuffer_to_image(&self.gl, self.framebuffer_id, source_rectangle)
    }

    fn read_framebuffer_to_image(
        gl: &Rc<dyn Gl>,
        framebuffer_id: u32,
        source_rectangle: DeviceIntRect,
    ) -> Option<RgbaImage> {
        gl.bind_framebuffer(gl::FRAMEBUFFER, framebuffer_id);

        // For some reason, OSMesa fails to render on the 3rd
        // attempt in headless mode, under some conditions.
        // I think this can only be some kind of synchronization
        // bug in OSMesa, but explicitly un-binding any vertex
        // array here seems to work around that bug.
        // See https://github.com/servo/servo/issues/18606.
        gl.bind_vertex_array(0);

        let mut pixels = gl.read_pixels(
            source_rectangle.min.x,
            source_rectangle.min.y,
            source_rectangle.width(),
            source_rectangle.height(),
            gl::RGBA,
            gl::UNSIGNED_BYTE,
        );
        let gl_error = gl.get_error();
        if gl_error != gl::NO_ERROR {
            warn!("GL error code 0x{gl_error:x} set after read_pixels");
        }

        // flip image vertically (texture is upside down)
        let source_rectangle = source_rectangle.to_usize();
        let orig_pixels = pixels.clone();
        let stride = source_rectangle.width() * 4;
        for y in 0..source_rectangle.height() {
            let dst_start = y * stride;
            let src_start = (source_rectangle.height() - y - 1) * stride;
            let src_slice = &orig_pixels[src_start..src_start + stride];
            pixels[dst_start..dst_start + stride].clone_from_slice(&src_slice[..stride]);
        }

        RgbaImage::from_raw(
            source_rectangle.width() as u32,
            source_rectangle.height() as u32,
            pixels,
        )
    }
}

pub struct OffscreenRenderingContext {
    parent_context: Rc<WindowRenderingContext>,
    size: Cell<PhysicalSize<u32>>,
    framebuffer: RefCell<Framebuffer>,
    /// Stage 2 dma-buf export cache for this context's FBO (tramuntana). Sized
    /// to `size`; rebuilt when the offscreen FBO is resized. Mirrors the cache
    /// on [`WindowRenderingContext`] but sources from our own framebuffer.
    dma_buf_cache: RefCell<Option<dma_buf_export::Cache>>,
}

type RenderToParentCallback = Box<dyn Fn(&glow::Context, Rect<i32>) + Send + Sync>;

impl OffscreenRenderingContext {
    fn new(parent_context: Rc<WindowRenderingContext>, size: PhysicalSize<u32>) -> Self {
        assert!(
            size.width != 0 && size.height != 0,
            "Dimensions must be at least 1x1, got {size:?}",
        );

        let framebuffer = RefCell::new(Framebuffer::new(parent_context.gleam_gl_api(), size));
        Self {
            parent_context,
            size: Cell::new(size),
            framebuffer,
            dma_buf_cache: RefCell::new(None),
        }
    }

    /// Tramuntana-only: export this offscreen context's framebuffer as a
    /// dma-buf for zero-copy Stage 2 sharing — the same path as
    /// [`WindowRenderingContext::export_framebuffer_dma_buf`], but sourcing
    /// from our own FBO. The embedder can resize that FBO safely (the
    /// `RenderingContext::resize` impl for this type just swaps the FBO),
    /// unlike the surfman window surface. See `docs/STAGE_2_GPU_SHARING.md`.
    ///
    /// `importable` lists the DRM modifiers the consumer can import
    /// `DRM_FORMAT_ABGR8888` with. When given, the export buffers are
    /// allocated with one of them, and the export is `Unavailable` when none
    /// works; `None` lets the driver pick, for a consumer that cannot say.
    pub fn export_framebuffer_dma_buf(
        &self,
        importable: Option<&[u64]>,
    ) -> Result<DmaBufHandle, DmaBufExportError> {
        // Make our GL/EGL context current so the EGL export queries see the
        // right display/context.
        self.parent_context
            .make_current()
            .map_err(|_| DmaBufExportError::Unavailable)?;
        let source_fbo = self.framebuffer.borrow().framebuffer_id;
        let gl = self.parent_context.gleam_gl_api();
        let mut cache = self.dma_buf_cache.borrow_mut();
        dma_buf_export::blit_and_export(&mut cache, self.size.get(), source_fbo, &gl, importable)
    }

    pub fn parent_context(&self) -> &WindowRenderingContext {
        &self.parent_context
    }

    pub fn render_to_parent_callback(&self) -> Option<RenderToParentCallback> {
        // Don't accept a `None` context for the source framebuffer.
        let front_framebuffer_id =
            NonZeroU32::new(self.framebuffer.borrow().framebuffer_id).map(NativeFramebuffer)?;
        let parent_context_framebuffer_id = self.parent_context.surfman_context.framebuffer();
        let size = self.size.get();
        let size = Size2D::new(size.width as i32, size.height as i32);
        Some(Box::new(move |gl, target_rect| {
            Self::blit_framebuffer(
                gl,
                Rect::new(Point2D::origin(), size.to_i32()),
                front_framebuffer_id,
                target_rect,
                parent_context_framebuffer_id,
            );
        }))
    }

    #[expect(unsafe_code)]
    fn blit_framebuffer(
        gl: &glow::Context,
        source_rect: Rect<i32>,
        source_framebuffer_id: NativeFramebuffer,
        target_rect: Rect<i32>,
        target_framebuffer_id: Option<NativeFramebuffer>,
    ) {
        use glow::HasContext as _;
        unsafe {
            gl.clear_color(0.0, 0.0, 0.0, 0.0);
            gl.scissor(
                target_rect.origin.x,
                target_rect.origin.y,
                target_rect.width(),
                target_rect.height(),
            );
            gl.enable(gl::SCISSOR_TEST);
            gl.clear(gl::COLOR_BUFFER_BIT);
            gl.disable(gl::SCISSOR_TEST);

            gl.bind_framebuffer(gl::READ_FRAMEBUFFER, Some(source_framebuffer_id));
            gl.bind_framebuffer(gl::DRAW_FRAMEBUFFER, target_framebuffer_id);

            gl.blit_framebuffer(
                source_rect.origin.x,
                source_rect.origin.y,
                source_rect.origin.x + source_rect.width(),
                source_rect.origin.y + source_rect.height(),
                target_rect.origin.x,
                target_rect.origin.y,
                target_rect.origin.x + target_rect.width(),
                target_rect.origin.y + target_rect.height(),
                gl::COLOR_BUFFER_BIT,
                gl::NEAREST,
            );
            gl.bind_framebuffer(gl::FRAMEBUFFER, target_framebuffer_id);
        }
    }
}

impl RenderingContext for OffscreenRenderingContext {
    fn size(&self) -> PhysicalSize<u32> {
        self.size.get()
    }

    fn resize(&self, new_size: PhysicalSize<u32>) {
        assert!(
            new_size.width != 0 && new_size.height != 0,
            "Dimensions must be at least 1x1, got {new_size:?}",
        );

        let old_size = self.size.get();
        if old_size == new_size {
            return;
        }

        let gl = self.parent_context.gleam_gl_api();
        let new_framebuffer = Framebuffer::new(gl.clone(), new_size);

        let old_framebuffer =
            std::mem::replace(&mut *self.framebuffer.borrow_mut(), new_framebuffer);
        self.size.set(new_size);

        let blit_size = new_size.min(old_size);
        let rect = Rect::new(
            Point2D::origin(),
            Size2D::new(blit_size.width, blit_size.height),
        )
        .to_i32();

        let Some(old_framebuffer_id) =
            NonZeroU32::new(old_framebuffer.framebuffer_id).map(NativeFramebuffer)
        else {
            return;
        };
        let new_framebuffer_id =
            NonZeroU32::new(self.framebuffer.borrow().framebuffer_id).map(NativeFramebuffer);
        Self::blit_framebuffer(
            &self.glow_gl_api(),
            rect,
            old_framebuffer_id,
            rect,
            new_framebuffer_id,
        );
    }

    fn prepare_for_rendering(&self) {
        self.framebuffer.borrow().bind();
    }

    fn present(&self) {}

    fn make_current(&self) -> Result<(), surfman::Error> {
        self.parent_context.make_current()
    }

    fn gleam_gl_api(&self) -> Rc<dyn gleam::gl::Gl> {
        self.parent_context.gleam_gl_api()
    }

    fn glow_gl_api(&self) -> Arc<glow::Context> {
        self.parent_context.glow_gl_api()
    }

    fn create_texture(
        &self,
        surface: Surface,
    ) -> Option<(SurfaceTexture, u32, UntypedSize2D<i32>)> {
        self.parent_context.create_texture(surface)
    }

    fn destroy_texture(&self, surface_texture: SurfaceTexture) -> Option<Surface> {
        self.parent_context.destroy_texture(surface_texture)
    }

    fn connection(&self) -> Option<Connection> {
        self.parent_context.connection()
    }

    fn read_to_image(&self, source_rectangle: DeviceIntRect) -> Option<RgbaImage> {
        self.framebuffer.borrow().read_to_image(source_rectangle)
    }

    fn refresh_driver(&self) -> Option<Rc<dyn RefreshDriver>> {
        self.parent_context().refresh_driver()
    }
}

fn print_diagnostics_information_on_context_creation_failure(
    device: &Device,
    desired_api: GLApi,
    desired_version: GLVersion,
) {
    println!("===============================================================");
    println!(
        "Could not create a {desired_api:?} {:?}.{:?} context when starting Servo.",
        desired_version.major, desired_version.minor
    );

    let version = surfman::GLVersion { major: 1, minor: 0 };
    match device
        .create_context_descriptor(&ContextAttributes {
            flags: ContextAttributeFlags::empty(),
            version,
        })
        .and_then(|context_descriptor| device.create_context(&context_descriptor, None))
    {
        Ok(mut context) => {
            #[expect(unsafe_code)]
            let glow_gl = unsafe {
                glow::Context::from_loader_function(|function_name| {
                    device.get_proc_address(&context, function_name)
                })
            };

            println!(
                "It's likely that your version of OpenGL ({:?}.{:?}) is too old.",
                glow_gl.version().major,
                glow_gl.version().minor
            );
            println!("If not, please file a bug at https://github.com/servo/servo/issues.");
            let _ = device.destroy_context(&mut context);
        },
        Err(_) => {
            println!("Could not create any {desired_api:?} context.");
            println!("Ensure that OpenGL is working on your system.");
            println!("If it is, please file a bug at https://github.com/servo/servo/issues.");
        },
    }
    println!("===============================================================\n");
}

/// FFI helpers for `EGL_MESA_image_dma_buf_export` + `EGL_KHR_fence_sync`.
/// Isolated in a sub-module so the file-level `#![deny(unsafe_code)]` only
/// relaxes here, not everywhere. See
/// [`WindowRenderingContext::export_framebuffer_dma_buf`].
///
/// Surfman's surface texture is already claimed as an EGL sibling by
/// WebRender / wgpu_hal::gles, so we can't `eglCreateImageKHR` against it
/// directly. We instead keep a **private intermediate texture** that we own
/// exclusively, blit the framebuffer onto it each frame on the GPU, and
/// export that one. The dma-buf fd and EGLImage are cached across frames
/// and only torn down on resize.
#[allow(unsafe_code)]
mod dma_buf_export {
    use std::ffi::{CStr, c_char, c_void};
    use std::fs::{File, OpenOptions};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::path::PathBuf;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use dpi::PhysicalSize;
    use gleam::gl::{self, Gl};
    use khronos_egl as egl;

    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use super::{DmaBufExportError, DmaBufHandle, DmaBufLease};

    /// `EGL_GL_TEXTURE_2D_KHR` from `EGL_KHR_gl_texture_2D_image`.
    const EGL_GL_TEXTURE_2D_KHR: egl::Enum = 0x30B1;
    /// `EGL_SYNC_NATIVE_FENCE_ANDROID` from `EGL_ANDROID_native_fence_sync`.
    const EGL_SYNC_NATIVE_FENCE_ANDROID: egl::Enum = 0x3144;
    /// `EGL_NO_NATIVE_FENCE_FD_ANDROID` (returned on dup failure).
    const EGL_NO_NATIVE_FENCE_FD_ANDROID: i32 = -1;
    const EGL_TRUE: egl::Boolean = 1;

    /// Opaque EGLSyncKHR pointer.
    type EGLSyncKHR = *mut std::ffi::c_void;

    type EglCreateImageKhr = unsafe extern "system" fn(
        dpy: egl::EGLDisplay,
        ctx: egl::EGLContext,
        target: egl::Enum,
        buffer: egl::EGLClientBuffer,
        attrib_list: *const egl::Int,
    ) -> egl::EGLImage;

    type EglDestroyImageKhr =
        unsafe extern "system" fn(dpy: egl::EGLDisplay, image: egl::EGLImage) -> egl::Boolean;

    type EglExportDmabufImageQueryMesa = unsafe extern "system" fn(
        dpy: egl::EGLDisplay,
        image: egl::EGLImage,
        fourcc: *mut i32,
        num_planes: *mut i32,
        modifiers: *mut u64,
    ) -> egl::Boolean;

    type EglExportDmabufImageMesa = unsafe extern "system" fn(
        dpy: egl::EGLDisplay,
        image: egl::EGLImage,
        fds: *mut i32,
        strides: *mut egl::Int,
        offsets: *mut egl::Int,
    ) -> egl::Boolean;

    type EglCreateSyncKhr = unsafe extern "system" fn(
        dpy: egl::EGLDisplay,
        type_: egl::Enum,
        attrib_list: *const egl::Int,
    ) -> EGLSyncKHR;

    type EglDestroySyncKhr =
        unsafe extern "system" fn(dpy: egl::EGLDisplay, sync: EGLSyncKHR) -> egl::Boolean;

    type EglDupNativeFenceFdAndroid =
        unsafe extern "system" fn(dpy: egl::EGLDisplay, sync: EGLSyncKHR) -> i32;

    struct EglFuncs {
        create_image: EglCreateImageKhr,
        destroy_image: EglDestroyImageKhr,
        export_query: EglExportDmabufImageQueryMesa,
        export_image: EglExportDmabufImageMesa,
        create_sync: Option<EglCreateSyncKhr>,
        destroy_sync: Option<EglDestroySyncKhr>,
        dup_fence_fd: Option<EglDupNativeFenceFdAndroid>,
        /// Needed only for GBM buffers. gleam implements it for GLES alone and
        /// panics on desktop GL, so it is loaded here.
        image_target_texture: Option<GlEglImageTargetTexture2dOes>,
    }

    type GlEglImageTargetTexture2dOes =
        unsafe extern "system" fn(target: gl::GLenum, image: *const c_void);

    /// `EGL_LINUX_DMA_BUF_EXT` and its attributes, from
    /// `EGL_EXT_image_dma_buf_import` and `EGL_EXT_image_dma_buf_import_modifiers`.
    const EGL_LINUX_DMA_BUF_EXT: egl::Enum = 0x3270;
    const EGL_LINUX_DRM_FOURCC_EXT: egl::Int = 0x3271;
    const EGL_DMA_BUF_PLANE0_FD_EXT: egl::Int = 0x3272;
    const EGL_DMA_BUF_PLANE0_OFFSET_EXT: egl::Int = 0x3273;
    const EGL_DMA_BUF_PLANE0_PITCH_EXT: egl::Int = 0x3274;
    const EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT: egl::Int = 0x3443;
    const EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT: egl::Int = 0x3444;
    /// `EGL_DEVICE_EXT` from `EGL_EXT_device_query`.
    const EGL_DEVICE_EXT: egl::Int = 0x322C;
    /// `EGL_DRM_DEVICE_FILE_EXT` from `EGL_EXT_device_drm`.
    const EGL_DRM_DEVICE_FILE_EXT: egl::Int = 0x3233;
    /// `EGL_DRM_RENDER_NODE_FILE_EXT` from `EGL_EXT_device_drm_render_node`.
    const EGL_DRM_RENDER_NODE_FILE_EXT: egl::Int = 0x3377;
    /// `DRM_FORMAT_ABGR8888`: RGBA8 in memory byte order, what a GL RGBA8
    /// texture is.
    const DRM_FORMAT_ABGR8888: u32 = 0x34324241;
    /// `GBM_BO_USE_RENDERING`: the buffer is a render target.
    const GBM_BO_USE_RENDERING: u32 = 1 << 2;

    type EglQueryDisplayAttribExt = unsafe extern "system" fn(
        dpy: egl::EGLDisplay,
        attribute: egl::Int,
        value: *mut egl::Attrib,
    ) -> egl::Boolean;

    type EglQueryDeviceStringExt =
        unsafe extern "system" fn(device: *mut c_void, name: egl::Int) -> *const c_char;

    /// A GBM device on the DRM node the current EGL display renders with. It
    /// allocates the export buffers with a DRM modifier the consumer can
    /// import, which a GL texture cannot be asked for: the driver picks the
    /// modifier of a texture itself, and on NVIDIA it picks a compressed one
    /// Vulkan does not import.
    ///
    /// libgbm is loaded at run time, so a system without it still gets the
    /// GL-texture export.
    struct Gbm {
        device: *mut c_void,
        bo_create_with_modifiers2:
            unsafe extern "C" fn(*mut c_void, u32, u32, u32, *const u64, u32, u32) -> *mut c_void,
        bo_get_plane_count: unsafe extern "C" fn(*mut c_void) -> i32,
        bo_get_fd_for_plane: unsafe extern "C" fn(*mut c_void, i32) -> i32,
        bo_get_stride_for_plane: unsafe extern "C" fn(*mut c_void, i32) -> u32,
        bo_get_offset: unsafe extern "C" fn(*mut c_void, i32) -> u32,
        bo_get_modifier: unsafe extern "C" fn(*mut c_void) -> u64,
        bo_destroy: unsafe extern "C" fn(*mut c_void),
        device_destroy: unsafe extern "C" fn(*mut c_void),
        /// The DRM node `device` was created on. Must outlive it.
        _node: File,
        /// Keeps every entry point above valid.
        _library: libloading::Library,
    }

    impl Gbm {
        /// Open a GBM device on the DRM node behind `display`. `None`, with
        /// the cause logged, when libgbm, the EGL device extensions or the
        /// node are missing.
        fn open(egl_instance: &egl::Instance<egl::Static>, display: egl::EGLDisplay) -> Option<Self> {
            let node = Self::node_path(egl_instance, display)?;
            let node = match OpenOptions::new().read(true).write(true).open(&node) {
                Ok(file) => file,
                Err(error) => {
                    log::warn!("Stage 2 GBM: cannot open {node:?}: {error}");
                    return None;
                },
            };
            // SAFETY: libgbm runs no initialisation code with preconditions
            // when loaded.
            let library = match unsafe { libloading::Library::new("libgbm.so.1") } {
                Ok(library) => library,
                Err(error) => {
                    log::info!("Stage 2 GBM: libgbm not loaded ({error}); exporting GL textures");
                    return None;
                },
            };
            // SAFETY: each type below is the C signature of the symbol it is
            // read from, as declared in gbm.h.
            let create_device: unsafe extern "C" fn(i32) -> *mut c_void =
                unsafe { *library.get(b"gbm_create_device\0").ok()? };
            let mut gbm = unsafe {
                Self {
                    device: std::ptr::null_mut(),
                    bo_create_with_modifiers2: *library.get(b"gbm_bo_create_with_modifiers2\0").ok()?,
                    bo_get_plane_count: *library.get(b"gbm_bo_get_plane_count\0").ok()?,
                    bo_get_fd_for_plane: *library.get(b"gbm_bo_get_fd_for_plane\0").ok()?,
                    bo_get_stride_for_plane: *library.get(b"gbm_bo_get_stride_for_plane\0").ok()?,
                    bo_get_offset: *library.get(b"gbm_bo_get_offset\0").ok()?,
                    bo_get_modifier: *library.get(b"gbm_bo_get_modifier\0").ok()?,
                    bo_destroy: *library.get(b"gbm_bo_destroy\0").ok()?,
                    device_destroy: *library.get(b"gbm_device_destroy\0").ok()?,
                    _node: node,
                    _library: library,
                }
            };
            // SAFETY: the fd is an open DRM node that `gbm` keeps open for as
            // long as the device lives.
            gbm.device = unsafe { create_device(gbm._node.as_raw_fd()) };
            if gbm.device.is_null() {
                log::warn!("Stage 2 GBM: gbm_create_device failed");
                return None;
            }
            Some(gbm)
        }

        /// The DRM render node (else the primary node) the EGL display is on.
        fn node_path(
            egl_instance: &egl::Instance<egl::Static>,
            display: egl::EGLDisplay,
        ) -> Option<PathBuf> {
            // SAFETY: each type is the signature of the entry point named.
            let query_display: EglQueryDisplayAttribExt =
                unsafe { load(egl_instance, "eglQueryDisplayAttribEXT")? };
            let query_device: EglQueryDeviceStringExt =
                unsafe { load(egl_instance, "eglQueryDeviceStringEXT")? };
            let mut device: egl::Attrib = 0;
            // SAFETY: `display` is the current, initialised display and
            // `device` a valid out pointer.
            if unsafe { query_display(display, EGL_DEVICE_EXT, &mut device) } != EGL_TRUE {
                log::warn!("Stage 2 GBM: eglQueryDisplayAttribEXT(EGL_DEVICE_EXT) failed");
                return None;
            }
            for name in [EGL_DRM_RENDER_NODE_FILE_EXT, EGL_DRM_DEVICE_FILE_EXT] {
                // SAFETY: `device` came from the query above. The returned
                // string, when not null, is owned by EGL and stays valid.
                let path = unsafe { query_device(device as *mut c_void, name) };
                if !path.is_null() {
                    let path = unsafe { CStr::from_ptr(path) };
                    return Some(PathBuf::from(path.to_string_lossy().into_owned()));
                }
            }
            log::warn!("Stage 2 GBM: the EGL device names no DRM node");
            None
        }
    }

    impl Drop for Gbm {
        fn drop(&mut self) {
            // Null when `gbm_create_device` failed in `open`.
            if self.device.is_null() {
                return;
            }
            // SAFETY: `device` was created by `gbm_create_device` and every
            // buffer object allocated from it is destroyed before the cache
            // drops its `Gbm` (see `Cache::drop`).
            unsafe { (self.device_destroy)(self.device) };
        }
    }

    /// Monotonic source of unique [`ExportBuffer`] / [`DmaBufHandle`] ids. One
    /// per physical buffer created (first build + each resize rebuild); never
    /// reused, so the consumer can key its import cache on it without aliasing
    /// across rebuilds.
    static NEXT_BUFFER_ID: AtomicU64 = AtomicU64::new(0);

    /// Number of dma-buf slots in the export ring. The producer round-robins
    /// through these so it never blits into the buffer the consumer is still
    /// sampling from a previous frame — the write-after-read hazard a single
    /// reused buffer has.
    ///
    /// 2 (double-buffering) is the minimum that makes the common case — the
    /// consumer at most one frame behind — safe. Bump to 3 (triple) if tearing
    /// ever shows under a deeper GPU pipeline (wgpu + the compositor can keep
    /// two frames in flight); the only cost is one extra full-resolution buffer
    /// (~8 MB at 1080p). The forward fence (`make_fence_fd`) covers the
    /// read-after-write direction; this ring covers write-after-read.
    const EXPORT_RING_LEN: usize = 2;

    /// How long every buffer of the ring may stay leased before the producer
    /// writes into one anyway. The consumer releases a lease a frame or two
    /// after it stops showing a buffer; one held this long means a consumer
    /// that has stopped drawing, and waiting longer would freeze the tab.
    const BUSY_TIMEOUT: Duration = Duration::from_millis(250);

    /// One slot of the export ring: a private GL texture + FBO, exported once
    /// as a dma-buf. The producer blits into a different slot each frame.
    struct ExportBuffer {
        /// Stable unique id of this physical buffer (see
        /// [`DmaBufHandle::buffer_id`]). Keys the consumer's import cache.
        buffer_id: u64,
        /// Our private GL texture, owned by this buffer. Never bound to any
        /// surfman surface, so EGL doesn't see another sibling claim it. Kept
        /// purely for lifetime/ownership tracking and the build-time log; the
        /// EGLImage built from it must outlive the texture.
        #[allow(dead_code)]
        gl_texture: u32,
        /// FBO whose color attachment is `gl_texture`. The BLIT destination.
        gl_fbo: u32,
        /// Owned dma-buf fd. Dup'd into each emitted [`DmaBufHandle`] so the
        /// consumer can close their copy independently.
        dma_buf_fd: OwnedFd,
        /// Owned EGLImage backing `gl_texture`. Kept alive so the dma-buf fd
        /// remains valid; destroyed on cache invalidation.
        egl_image: egl::EGLImage,
        /// Held by every [`DmaBufHandle`] emitted for this buffer; a strong
        /// count above one means the consumer may still read it.
        readers: Arc<()>,
        /// Export format, queried per buffer. Identical across the ring in
        /// practice (same size/format) but each EGLImage reports its own.
        fourcc: u32,
        modifier: u64,
        stride: u32,
        offset: u32,
        /// The GBM buffer object behind the dma-buf, or null when the buffer
        /// is an exported GL texture. Destroyed after `egl_image`.
        gbm_bo: *mut c_void,
    }

    /// Per-(WindowRenderingContext, size) cached state for the Stage 2 export:
    /// a ring of [`ExportBuffer`]s plus the shared EGL handles, rebuilt only on
    /// resize.
    pub(super) struct Cache {
        /// The export ring. Length is [`EXPORT_RING_LEN`].
        buffers: Vec<ExportBuffer>,
        /// Index of the slot to blit into on the next export. Advances
        /// round-robin, wrapping at `buffers.len()`.
        next: usize,
        /// When an export first found every buffer leased, if the latest one
        /// did. Reset by any export that finds a free buffer.
        busy_since: Option<Instant>,
        /// EGL display handle the cache was built with. Stored so we can tear
        /// down each `egl_image` and any fence sync correctly.
        egl_display: egl::EGLDisplay,
        /// EGL extension entry points. Loaded once, reused across frames and
        /// shared by every buffer in the ring.
        egl_funcs: EglFuncs,
        /// Ring dimensions. All buffers match; used for the resize check and to
        /// stamp emitted [`DmaBufHandle`]s.
        width: u32,
        height: u32,
        /// The modifiers the consumer said it imports when the ring was
        /// built; a different list rebuilds it.
        importable: Option<Vec<u64>>,
        /// Allocator of the ring's buffers, when they came from GBM. `Drop`
        /// destroys the buffers before this field drops.
        gbm: Option<Gbm>,
    }

    /// Destroy the EGLImages backing a set of export buffers, and the GBM
    /// buffer objects behind them, so the kernel can release the dma-buf
    /// memory once all importers close their copies. The owned `dma_buf_fd`s
    /// close themselves via `OwnedFd`'s Drop afterwards (when the
    /// `Vec<ExportBuffer>` drops). Used both on cache teardown and on the
    /// partial-failure path in `build_cache`.
    fn destroy_export_buffers(
        buffers: &[ExportBuffer],
        egl_funcs: &EglFuncs,
        display: egl::EGLDisplay,
        gbm: Option<&Gbm>,
    ) {
        for buf in buffers {
            unsafe {
                (egl_funcs.destroy_image)(display, buf.egl_image);
            }
            if let Some(gbm) = gbm.filter(|_| !buf.gbm_bo.is_null()) {
                // SAFETY: `gbm_bo` was allocated from this device and nothing
                // uses it after its EGLImage is gone.
                unsafe { (gbm.bo_destroy)(buf.gbm_bo) };
            }
        }
        // GL textures and FBOs leak deliberately: we don't have access to the
        // surfman gl handle here, and they belong to a context that is about to
        // be destroyed when the RenderingContext drops anyway. Not worth
        // carrying an Rc<dyn Gl> in Cache to clean GL ids the driver releases on
        // context teardown.
    }

    impl Drop for Cache {
        fn drop(&mut self) {
            destroy_export_buffers(
                &self.buffers,
                &self.egl_funcs,
                self.egl_display,
                self.gbm.as_ref(),
            );
        }
    }

    /// Load a function pointer for an EGL extension by name and transmute it
    /// to the requested fn-pointer type. Returns `None` if the driver does
    /// not expose the entry point.
    unsafe fn load<T: Copy>(
        egl_instance: &egl::Instance<egl::Static>,
        name: &str,
    ) -> Option<T> {
        let raw = egl_instance.get_proc_address(name)?;
        debug_assert_eq!(
            std::mem::size_of::<T>(),
            std::mem::size_of::<extern "system" fn()>()
        );
        Some(unsafe { std::mem::transmute_copy::<extern "system" fn(), T>(&raw) })
    }

    /// Build the export ring. With `importable` (the modifiers the consumer
    /// imports, see [`blit_and_export`]) the buffers come from GBM with one of
    /// those modifiers, else from exported GL textures; a ring the consumer
    /// cannot import is refused, so the caller falls back to the CPU path.
    fn build_cache(
        size: PhysicalSize<u32>,
        gl: &Rc<dyn Gl>,
        importable: Option<&[u64]>,
    ) -> Option<Cache> {
        let egl_instance = egl::Instance::new(egl::Static);
        let display = egl_instance.get_current_display()?;
        let context = egl_instance.get_current_context()?;

        let egl_funcs = EglFuncs {
            create_image: unsafe { load(&egl_instance, "eglCreateImageKHR")? },
            destroy_image: unsafe { load(&egl_instance, "eglDestroyImageKHR")? },
            export_query: unsafe { load(&egl_instance, "eglExportDMABUFImageQueryMESA")? },
            export_image: unsafe { load(&egl_instance, "eglExportDMABUFImageMESA")? },
            create_sync: unsafe { load(&egl_instance, "eglCreateSyncKHR") },
            destroy_sync: unsafe { load(&egl_instance, "eglDestroySyncKHR") },
            dup_fence_fd: unsafe { load(&egl_instance, "eglDupNativeFenceFDANDROID") },
            image_target_texture: unsafe { load(&egl_instance, "glEGLImageTargetTexture2DOES") },
        };

        // Build the export ring. On any per-buffer failure, tear down the
        // buffers built so far before bailing so we don't leak EGLImages /
        // dma-buf fds.
        let gbm = importable
            .filter(|modifiers| !modifiers.is_empty() && egl_funcs.image_target_texture.is_some())
            .and_then(|_| Gbm::open(&egl_instance, display.as_ptr()));
        let mut buffers = Vec::with_capacity(EXPORT_RING_LEN);
        for i in 0..EXPORT_RING_LEN {
            let buffer = match (gbm.as_ref(), importable) {
                (Some(gbm), Some(modifiers)) => {
                    build_gbm_export_buffer(size, gl, display.as_ptr(), &egl_funcs, gbm, modifiers)
                },
                _ => build_export_buffer(size, gl, display.as_ptr(), context.as_ptr(), &egl_funcs),
            };
            match buffer {
                Some(buffer) => buffers.push(buffer),
                None => {
                    log::warn!(
                        "Stage 2 cache: failed to build export buffer {}/{EXPORT_RING_LEN}",
                        i + 1
                    );
                    destroy_export_buffers(&buffers, &egl_funcs, display.as_ptr(), gbm.as_ref());
                    return None;
                }
            }
        }

        if let Some(modifiers) = importable {
            if let Some(buffer) = buffers.iter().find(|b| !modifiers.contains(&b.modifier)) {
                log::warn!(
                    "Stage 2 cache: the consumer cannot import modifier 0x{:016x} (it imports \
                     {modifiers:x?}); no dma-buf export",
                    buffer.modifier
                );
                destroy_export_buffers(&buffers, &egl_funcs, display.as_ptr(), gbm.as_ref());
                return None;
            }
        }

        log::info!(
            "Stage 2 cache: built {EXPORT_RING_LEN}-buffer export ring {}x{} ({})",
            size.width,
            size.height,
            if gbm.is_some() { "GBM" } else { "GL textures" }
        );

        Some(Cache {
            buffers,
            next: 0,
            busy_since: None,
            egl_display: display.as_ptr(),
            egl_funcs,
            width: size.width,
            height: size.height,
            importable: importable.map(<[u64]>::to_vec),
            gbm,
        })
    }

    /// Build one ring slot on a GBM buffer object allocated with one of
    /// `modifiers`, imported into GL as an EGLImage-backed texture + FBO.
    fn build_gbm_export_buffer(
        size: PhysicalSize<u32>,
        gl: &Rc<dyn Gl>,
        display: egl::EGLDisplay,
        egl_funcs: &EglFuncs,
        gbm: &Gbm,
        modifiers: &[u64],
    ) -> Option<ExportBuffer> {
        // SAFETY: `gbm.device` is a live GBM device and `modifiers` holds
        // `modifiers.len()` values for the whole call.
        let bo = unsafe {
            (gbm.bo_create_with_modifiers2)(
                gbm.device,
                size.width,
                size.height,
                DRM_FORMAT_ABGR8888,
                modifiers.as_ptr(),
                modifiers.len() as u32,
                GBM_BO_USE_RENDERING,
            )
        };
        if bo.is_null() {
            log::warn!("Stage 2 GBM: gbm_bo_create_with_modifiers2 failed for {modifiers:x?}");
            return None;
        }
        // Every exit below that fails destroys `bo`.
        let fail = |what: &str| -> Option<ExportBuffer> {
            log::warn!("Stage 2 GBM: {what}");
            // SAFETY: `bo` came from this device and nothing else holds it.
            unsafe { (gbm.bo_destroy)(bo) };
            None
        };

        // SAFETY: `bo` is a live buffer object for every call below.
        let (planes, modifier, stride, offset, raw_fd) = unsafe {
            (
                (gbm.bo_get_plane_count)(bo),
                (gbm.bo_get_modifier)(bo),
                (gbm.bo_get_stride_for_plane)(bo, 0),
                (gbm.bo_get_offset)(bo, 0),
                (gbm.bo_get_fd_for_plane)(bo, 0),
            )
        };
        if raw_fd < 0 {
            return fail("gbm_bo_get_fd_for_plane failed");
        }
        // SAFETY: GBM returned a new fd that nothing else owns.
        let dma_buf_fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
        if planes != 1 {
            return fail(&format!("modifier 0x{modifier:016x} has {planes} planes; single-plane only"));
        }

        let attribs: [egl::Int; 17] = [
            egl::WIDTH,
            size.width as egl::Int,
            egl::HEIGHT,
            size.height as egl::Int,
            EGL_LINUX_DRM_FOURCC_EXT,
            DRM_FORMAT_ABGR8888 as egl::Int,
            EGL_DMA_BUF_PLANE0_FD_EXT,
            dma_buf_fd.as_raw_fd(),
            EGL_DMA_BUF_PLANE0_OFFSET_EXT,
            offset as egl::Int,
            EGL_DMA_BUF_PLANE0_PITCH_EXT,
            stride as egl::Int,
            EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT,
            (modifier & 0xffff_ffff) as u32 as egl::Int,
            EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT,
            (modifier >> 32) as u32 as egl::Int,
            egl::NONE,
        ];
        // SAFETY: `attribs` is NONE-terminated and outlives the call; EGL
        // dups the fd it names, so `dma_buf_fd` stays ours.
        let image = unsafe {
            (egl_funcs.create_image)(
                display,
                egl::NO_CONTEXT,
                EGL_LINUX_DMA_BUF_EXT,
                std::ptr::null_mut(),
                attribs.as_ptr(),
            )
        };
        if image.is_null() {
            return fail(&format!(
                "eglCreateImageKHR(EGL_LINUX_DMA_BUF_EXT) refused modifier 0x{modifier:016x}"
            ));
        }

        let Some(image_target_texture) = egl_funcs.image_target_texture else {
            // SAFETY: the image was created above and nothing uses it now.
            unsafe { (egl_funcs.destroy_image)(display, image) };
            return fail("glEGLImageTargetTexture2DOES is not available");
        };
        let textures = gl.gen_textures(1);
        let texture = *textures.first()?;
        gl.bind_texture(gl::TEXTURE_2D, texture);
        // SAFETY: the GL context is current (the caller made it so), the
        // texture is bound to TEXTURE_2D and `image` is a live EGLImage.
        unsafe { image_target_texture(gl::TEXTURE_2D, image as *const c_void) };
        gl.tex_parameter_i(gl::TEXTURE_2D, gl::TEXTURE_MIN_FILTER, gl::LINEAR as i32);
        gl.tex_parameter_i(gl::TEXTURE_2D, gl::TEXTURE_MAG_FILTER, gl::LINEAR as i32);
        gl.bind_texture(gl::TEXTURE_2D, 0);

        let fbos = gl.gen_framebuffers(1);
        let fbo = *fbos.first()?;
        gl.bind_framebuffer(gl::FRAMEBUFFER, fbo);
        gl.framebuffer_texture_2d(
            gl::FRAMEBUFFER,
            gl::COLOR_ATTACHMENT0,
            gl::TEXTURE_2D,
            texture,
            0,
        );
        let status = gl.check_frame_buffer_status(gl::FRAMEBUFFER);
        gl.bind_framebuffer(gl::FRAMEBUFFER, 0);
        if status != gl::FRAMEBUFFER_COMPLETE {
            gl.delete_framebuffers(&fbos);
            gl.delete_textures(&textures);
            // SAFETY: the image was created above and nothing uses it now.
            unsafe { (egl_funcs.destroy_image)(display, image) };
            return fail(&format!(
                "FBO on modifier 0x{modifier:016x} is not framebuffer-complete (status 0x{status:x})"
            ));
        }

        let buffer_id = NEXT_BUFFER_ID.fetch_add(1, Ordering::Relaxed);
        log::info!(
            "Stage 2 cache: built GBM ring buffer {}x{} (id={buffer_id}, modifier=0x{modifier:016x}, stride={stride}, offset={offset})",
            size.width,
            size.height
        );
        Some(ExportBuffer {
            buffer_id,
            gl_texture: texture,
            gl_fbo: fbo,
            dma_buf_fd,
            egl_image: image,
            readers: Arc::new(()),
            fourcc: DRM_FORMAT_ABGR8888,
            modifier,
            stride,
            offset,
            gbm_bo: bo,
        })
    }

    /// Build one ring slot: a private RGBA8 texture + FBO, wrapped as an
    /// EGLImage and exported as a single-plane dma-buf. Cleans up its own
    /// partial GL state on failure. `display` / `context` are the current EGL
    /// handles; `egl_funcs` the shared entry points.
    fn build_export_buffer(
        size: PhysicalSize<u32>,
        gl: &Rc<dyn Gl>,
        display: egl::EGLDisplay,
        context: egl::EGLContext,
        egl_funcs: &EglFuncs,
    ) -> Option<ExportBuffer> {
        // Create the private intermediate texture: RGBA8, linear filtering,
        // wrap clamp-to-edge. The format is DRM_FORMAT_ABGR8888 (0x34324241);
        // the modifier is the driver's choice and cannot be asked for. NVIDIA
        // picks a compressed block-linear one (0x0300000000e08014 on a 3060)
        // that its own Vulkan driver does not import, which is why the GBM
        // path exists; this one is the fallback.
        let textures = gl.gen_textures(1);
        let texture = *textures.first()?;
        gl.bind_texture(gl::TEXTURE_2D, texture);
        gl.tex_image_2d(
            gl::TEXTURE_2D,
            0,
            gl::RGBA8 as i32,
            size.width as i32,
            size.height as i32,
            0,
            gl::RGBA,
            gl::UNSIGNED_BYTE,
            None,
        );
        gl.tex_parameter_i(gl::TEXTURE_2D, gl::TEXTURE_MIN_FILTER, gl::LINEAR as i32);
        gl.tex_parameter_i(gl::TEXTURE_2D, gl::TEXTURE_MAG_FILTER, gl::LINEAR as i32);
        gl.tex_parameter_i(
            gl::TEXTURE_2D,
            gl::TEXTURE_WRAP_S,
            gl::CLAMP_TO_EDGE as i32,
        );
        gl.tex_parameter_i(
            gl::TEXTURE_2D,
            gl::TEXTURE_WRAP_T,
            gl::CLAMP_TO_EDGE as i32,
        );
        gl.bind_texture(gl::TEXTURE_2D, 0);

        let fbos = gl.gen_framebuffers(1);
        let fbo = *fbos.first()?;
        gl.bind_framebuffer(gl::FRAMEBUFFER, fbo);
        gl.framebuffer_texture_2d(
            gl::FRAMEBUFFER,
            gl::COLOR_ATTACHMENT0,
            gl::TEXTURE_2D,
            texture,
            0,
        );
        let status = gl.check_frame_buffer_status(gl::FRAMEBUFFER);
        gl.bind_framebuffer(gl::FRAMEBUFFER, 0);
        if status != gl::FRAMEBUFFER_COMPLETE {
            log::warn!(
                "Stage 2 cache: intermediate FBO is not framebuffer-complete (status 0x{status:x})"
            );
            gl.delete_framebuffers(&fbos);
            gl.delete_textures(&textures);
            return None;
        }

        // Wrap our private texture as an EGLImage.
        let attrib_list: [egl::Int; 1] = [egl::NONE];
        let buffer = texture as usize as egl::EGLClientBuffer;
        let image = unsafe {
            (egl_funcs.create_image)(
                display,
                context,
                EGL_GL_TEXTURE_2D_KHR,
                buffer,
                attrib_list.as_ptr(),
            )
        };
        if image.is_null() {
            log::warn!(
                "Stage 2 cache: eglCreateImageKHR returned EGL_NO_IMAGE_KHR for our private texture id {texture}"
            );
            gl.delete_framebuffers(&fbos);
            gl.delete_textures(&textures);
            return None;
        }

        let mut fourcc: i32 = 0;
        let mut num_planes: i32 = 0;
        let mut modifier: u64 = 0;
        let ok = unsafe {
            (egl_funcs.export_query)(
                display,
                image,
                &mut fourcc,
                &mut num_planes,
                &mut modifier,
            )
        };
        if ok != EGL_TRUE {
            log::warn!("Stage 2 cache: eglExportDMABUFImageQueryMESA failed");
            unsafe {
                (egl_funcs.destroy_image)(display, image);
            }
            gl.delete_framebuffers(&fbos);
            gl.delete_textures(&textures);
            return None;
        }
        if num_planes != 1 {
            log::warn!(
                "Stage 2 cache: dma-buf export returned {num_planes} planes; single-plane only"
            );
            unsafe {
                (egl_funcs.destroy_image)(display, image);
            }
            gl.delete_framebuffers(&fbos);
            gl.delete_textures(&textures);
            return None;
        }

        let mut fd_raw: i32 = -1;
        let mut stride: egl::Int = 0;
        let mut offset: egl::Int = 0;
        let ok = unsafe {
            (egl_funcs.export_image)(
                display,
                image,
                &mut fd_raw,
                &mut stride,
                &mut offset,
            )
        };
        if ok != EGL_TRUE || fd_raw < 0 {
            log::warn!("Stage 2 cache: eglExportDMABUFImageMESA failed (returned fd = {fd_raw})");
            unsafe {
                (egl_funcs.destroy_image)(display, image);
            }
            gl.delete_framebuffers(&fbos);
            gl.delete_textures(&textures);
            return None;
        }

        let dma_buf_fd = unsafe { OwnedFd::from_raw_fd(fd_raw) };
        let buffer_id = NEXT_BUFFER_ID.fetch_add(1, Ordering::Relaxed);

        log::info!(
            "Stage 2 cache: built ring buffer {}x{} (id={buffer_id}, texture={}, fbo={}, fourcc=0x{:08x}, modifier=0x{:016x}, stride={}, offset={})",
            size.width, size.height, texture, fbo, fourcc as u32, modifier, stride, offset
        );

        Some(ExportBuffer {
            buffer_id,
            gl_texture: texture,
            gl_fbo: fbo,
            dma_buf_fd,
            egl_image: image,
            readers: Arc::new(()),
            fourcc: fourcc as u32,
            modifier,
            stride: stride as u32,
            offset: offset as u32,
            gbm_bo: std::ptr::null_mut(),
        })
    }

    fn make_fence_fd(cache: &Cache) -> Option<OwnedFd> {
        let create_sync = cache.egl_funcs.create_sync?;
        let destroy_sync = cache.egl_funcs.destroy_sync?;
        let dup_fence_fd = cache.egl_funcs.dup_fence_fd?;

        let attribs: [egl::Int; 1] = [egl::NONE];
        let sync = unsafe {
            create_sync(
                cache.egl_display,
                EGL_SYNC_NATIVE_FENCE_ANDROID,
                attribs.as_ptr(),
            )
        };
        if sync.is_null() {
            log::warn!("Stage 2 fence: eglCreateSyncKHR returned NULL");
            return None;
        }
        let raw_fd = unsafe { dup_fence_fd(cache.egl_display, sync) };
        unsafe {
            destroy_sync(cache.egl_display, sync);
        }
        if raw_fd == EGL_NO_NATIVE_FENCE_FD_ANDROID {
            log::warn!("Stage 2 fence: eglDupNativeFenceFDANDROID returned -1");
            return None;
        }
        Some(unsafe { OwnedFd::from_raw_fd(raw_fd) })
    }

    /// The ring slot to blit into next: the first one from `cache.next` on
    /// whose lease no consumer holds a clone. This is the write-after-read fix:
    /// the consumer keeps a lease until the GPU has finished every frame that
    /// samples a buffer, so a free lease means nothing can still be reading it.
    /// `None` when every slot is leased, unless that has gone on for longer
    /// than [`BUSY_TIMEOUT`]: then the next slot is taken anyway.
    fn pick_slot(cache: &mut Cache) -> Option<usize> {
        let len = cache.buffers.len();
        let free = (0..len)
            .map(|k| (cache.next + k) % len)
            .find(|&i| Arc::strong_count(&cache.buffers[i].readers) == 1);
        let idx = match free {
            Some(idx) => idx,
            None => {
                let since = *cache.busy_since.get_or_insert_with(Instant::now);
                if since.elapsed() < BUSY_TIMEOUT {
                    return None;
                }
                log::warn!(
                    "Stage 2 export: every ring buffer still leased after {BUSY_TIMEOUT:?}; \
                     writing into one the consumer may be reading"
                );
                cache.next
            },
        };
        cache.busy_since = None;
        cache.next = (idx + 1) % len;
        Some(idx)
    }

    pub(super) fn blit_and_export(
        cache_slot: &mut Option<Cache>,
        size: PhysicalSize<u32>,
        source_fbo: u32,
        gl: &Rc<dyn Gl>,
        importable: Option<&[u64]>,
    ) -> Result<DmaBufHandle, DmaBufExportError> {
        // Invalidate the cache if the window resized or the consumer now
        // imports other modifiers.
        if let Some(c) = cache_slot.as_ref() {
            if c.width != size.width ||
                c.height != size.height ||
                c.importable.as_deref() != importable
            {
                *cache_slot = None;
            }
        }
        if cache_slot.is_none() {
            *cache_slot = Some(
                build_cache(size, gl, importable).ok_or(DmaBufExportError::Unavailable)?,
            );
        }
        let cache = cache_slot.as_mut().ok_or(DmaBufExportError::Unavailable)?;

        // Copy the slot's scalar fields out so the immutable borrow ends before
        // `make_fence_fd` and the fd dup re-borrow `cache`.
        let idx = pick_slot(cache).ok_or(DmaBufExportError::Busy)?;
        let (gl_fbo, buffer_id, fourcc, modifier, stride, offset) = {
            let buf = &cache.buffers[idx];
            (buf.gl_fbo, buf.buffer_id, buf.fourcc, buf.modifier, buf.stride, buf.offset)
        };

        // GPU-side blit: surfman's FBO -> our private FBO.
        // Servo's FBO has Y=0 at the top in our coordinate space; for the
        // consumer (gpui) sampling a wgpu texture we want the same
        // orientation. glBlitFramebuffer copies in source coords directly, no
        // flip needed.
        gl.bind_framebuffer(gl::READ_FRAMEBUFFER, source_fbo);
        gl.bind_framebuffer(gl::DRAW_FRAMEBUFFER, gl_fbo);
        gl.blit_framebuffer(
            0,
            0,
            size.width as i32,
            size.height as i32,
            0,
            0,
            size.width as i32,
            size.height as i32,
            gl::COLOR_BUFFER_BIT,
            gl::NEAREST,
        );
        // Restore default binding so subsequent surfman ops aren't surprised.
        gl.bind_framebuffer(gl::FRAMEBUFFER, 0);
        gl.flush();

        let fence_fd = make_fence_fd(cache);

        // Dup this slot's dma-buf fd so the consumer owns an independent fd it
        // can close without affecting our cache.
        let fd = cache.buffers[idx].dma_buf_fd.try_clone().map_err(|error| {
            log::warn!("Stage 2 export: failed to dup cached dma-buf fd: {error}");
            DmaBufExportError::Unavailable
        })?;

        Ok(DmaBufHandle {
            buffer_id,
            fd,
            width: cache.width,
            height: cache.height,
            fourcc,
            stride,
            offset,
            modifier,
            fence_fd,
            lease: DmaBufLease {
                _readers: cache.buffers[idx].readers.clone(),
            },
        })
    }
}

#[cfg(test)]
mod test {
    use dpi::PhysicalSize;
    use euclid::{Box2D, Point2D, Size2D};
    use gleam::gl;
    use image::Rgba;
    use surfman::{Connection, ContextAttributeFlags, ContextAttributes, Error, GLApi, GLVersion};

    use super::Framebuffer;
    use crate::rendering_context::SoftwareRenderingContext;

    #[test]
    #[expect(unsafe_code)]
    fn test_read_pixels() -> Result<(), Error> {
        let connection = Connection::new()?;
        let adapter = connection.create_software_adapter()?;
        let device = connection.create_device(&adapter)?;
        let context_descriptor = device.create_context_descriptor(&ContextAttributes {
            version: GLVersion::new(3, 0),
            flags: ContextAttributeFlags::empty(),
        })?;
        let mut context = device.create_context(&context_descriptor, None)?;

        let gl = match connection.gl_api() {
            GLApi::GL => unsafe { gl::GlFns::load_with(|s| device.get_proc_address(&context, s)) },
            GLApi::GLES => unsafe {
                gl::GlesFns::load_with(|s| device.get_proc_address(&context, s))
            },
        };

        device.make_context_current(&context)?;

        {
            const SIZE: u32 = 16;
            let framebuffer = Framebuffer::new(gl, PhysicalSize::new(SIZE, SIZE));
            framebuffer.bind();
            framebuffer
                .gl
                .clear_color(12.0 / 255.0, 34.0 / 255.0, 56.0 / 255.0, 78.0 / 255.0);
            framebuffer.gl.clear(gl::COLOR_BUFFER_BIT);

            let rect = Box2D::from_origin_and_size(Point2D::zero(), Size2D::new(SIZE, SIZE));
            let img = framebuffer
                .read_to_image(rect.to_i32())
                .expect("Should have been able to read back image.");
            assert_eq!(img.width(), SIZE);
            assert_eq!(img.height(), SIZE);

            let expected_pixel: Rgba<u8> = Rgba([12, 34, 56, 78]);
            assert!(img.pixels().all(|&p| p == expected_pixel));
        }

        device.destroy_context(&mut context)?;

        Ok(())
    }

    #[test]
    fn test_minimum_size_error() {
        let result = SoftwareRenderingContext::new(PhysicalSize {
            width: 0,
            height: 1,
        });
        match result {
            Err(surfman::Error::Failed) => (),
            _ => panic!("Expected {:?}", surfman::Error::Failed),
        }
    }
}
