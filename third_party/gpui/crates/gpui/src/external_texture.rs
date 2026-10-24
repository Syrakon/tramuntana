//! External GPU texture handoff for Stage 2 of the rumb cross-process bridge.
//!
//! Embedders (e.g. `rumb-shell`) implement [`ExternalGpuTexture`] backed by a
//! platform handle the GPU compositor can import without a CPU readback:
//!
//! - On Linux: dma-buf file descriptor exported via
//!   `EGL_MESA_image_dma_buf_export`, imported into Vulkan/wgpu via
//!   `VK_EXT_external_memory_dma_buf` + `VK_EXT_image_drm_format_modifier`.
//!
//! Future platforms (IOSurface on macOS, NT handles on Windows) plug in by
//! adding variants to [`ExternalTextureDescriptor`] and [`ExternalFence`].
//!
//! See `docs/STAGE_2_GPU_SHARING.md` in the parent tramuntana repo for the
//! end-to-end architecture.

use std::os::fd::OwnedFd;
use std::sync::{Arc, PoisonError, RwLock};

use crate::{DevicePixels, Size};

static IMPORTABLE_ABGR8888_MODIFIERS: RwLock<Option<Arc<[u64]>>> = RwLock::new(None);

/// The DRM format modifiers the compositor can import a
/// `DRM_FORMAT_ABGR8888` dma-buf with. A producer allocates its buffers with
/// one of these; a buffer with any other modifier will not be drawn. `None`
/// until the compositor's GPU device exists; an empty list means it imports
/// no dma-buf at all.
pub fn importable_abgr8888_modifiers() -> Option<Arc<[u64]>> {
    IMPORTABLE_ABGR8888_MODIFIERS
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

/// Set by the renderer each time it opens its GPU device. See
/// [`importable_abgr8888_modifiers`].
pub fn set_importable_abgr8888_modifiers(modifiers: Option<Arc<[u64]>>) {
    *IMPORTABLE_ABGR8888_MODIFIERS
        .write()
        .unwrap_or_else(PoisonError::into_inner) = modifiers;
}

/// Trait implemented by embedders that want to hand a GPU-resident texture
/// directly to gpui's compositor. The framework owns nothing — it queries
/// the descriptor each time it draws and constructs (or reuses) the matching
/// wgpu resource internally.
///
/// Implementations must be `Send + Sync` because the element tree can be
/// traversed off the foreground in some scenarios. The actual fd ownership
/// transferred via [`ExternalTextureDescriptor`] is single-use; gpui consumes
/// it during the next paint.
pub trait ExternalGpuTexture: Send + Sync {
    /// Describe the underlying texture for this paint. Called once per
    /// frame the texture is on screen. The descriptor carries owned handles
    /// (`OwnedFd` etc.) — implementations should re-export from the source
    /// engine on each call rather than caching.
    fn descriptor(&self) -> Option<ExternalTextureDescriptor>;

    /// Optional GPU completion fence. The compositor will wait on this
    /// before issuing draws that sample the texture. Returning `None`
    /// forces gpui to fall back to a coarser barrier (e.g. queue-wide
    /// idle wait), which is correct but slower.
    fn fence(&self) -> Option<ExternalFence>;

    /// Stable, unique id of the buffer the next [`Self::descriptor`] will
    /// export. Lets the compositor cache the imported GPU texture per physical
    /// buffer and skip re-importing — and even skip calling `descriptor()` —
    /// when the producer hands back the same buffer. `None` until the first
    /// frame is produced. Distinct buffers (double-buffer ring slots,
    /// post-resize rebuilds) must return distinct ids.
    fn buffer_id(&self) -> Option<u64>;

    /// Logical size of the texture in device pixels. Used for layout
    /// hints and bounds clipping; must match the texture extent the
    /// descriptor will export.
    fn size(&self) -> Size<DevicePixels>;

    /// A token that keeps the buffer [`Self::buffer_id`] names marked as in
    /// use. The compositor holds it until the GPU has finished the frame that
    /// samples the buffer, so the producer can tell when writing into it again
    /// is safe. `None` (the default) for producers that do not track readers.
    fn read_lease(&self) -> Option<ExternalReadLease> {
        None
    }
}

/// Held by the compositor while a submitted frame may still read an external
/// buffer; dropped once that frame's GPU work is done. What it holds is up to
/// the producer: its drop is the signal.
pub struct ExternalReadLease {
    _token: Box<dyn std::any::Any + Send + Sync>,
}

impl ExternalReadLease {
    /// Wrap the producer's token.
    pub fn new(token: impl std::any::Any + Send + Sync) -> Self {
        Self {
            _token: Box::new(token),
        }
    }
}

impl std::fmt::Debug for ExternalReadLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ExternalReadLease")
    }
}

/// Platform-tagged handle to a GPU texture exported by another process or
/// graphics API. Currently only Linux's dma-buf path is wired up.
#[derive(Debug)]
#[non_exhaustive]
pub enum ExternalTextureDescriptor {
    /// Linux dma-buf single-plane RGBA texture.
    DmaBuf {
        /// Stable, unique id of this physical buffer (see
        /// [`ExternalGpuTexture::buffer_id`]). The compositor keys its import
        /// cache on it.
        buffer_id: u64,
        /// Owned dma-buf fd. Single-plane only. Closed by gpui after import.
        fd: OwnedFd,
        /// DRM fourcc (e.g. `DRM_FORMAT_ABGR8888 = 0x34324241`).
        fourcc: u32,
        /// DRM format modifier. `0` is `DRM_FORMAT_MOD_LINEAR`.
        modifier: u64,
        /// Row stride in bytes.
        stride: u32,
        /// Plane offset in bytes (usually 0 for single-plane RGBA).
        offset: u32,
        /// Texture width in pixels.
        width: u32,
        /// Texture height in pixels.
        height: u32,
    },
}

/// Platform-tagged sync primitive the consumer must wait on before sampling.
#[derive(Debug)]
#[non_exhaustive]
pub enum ExternalFence {
    /// Linux `sync_file` fd, typically obtained via
    /// `EGL_ANDROID_native_fence_sync`. Importable into Vulkan as
    /// `VK_KHR_external_semaphore_fd` with handle type `SYNC_FD`.
    SyncFd(OwnedFd),
}

/// Type alias for a sharable handle to an [`ExternalGpuTexture`].
///
/// Most call sites work in terms of `Arc<dyn ExternalGpuTexture>` so the same
/// handle can flow through several elements without copying.
pub type ExternalGpuTextureHandle = Arc<dyn ExternalGpuTexture>;
