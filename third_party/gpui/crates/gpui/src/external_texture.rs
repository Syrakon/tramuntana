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
use std::sync::Arc;

use crate::{DevicePixels, Size};

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

    /// Logical size of the texture in device pixels. Used for layout
    /// hints and bounds clipping; must match the texture extent the
    /// descriptor will export.
    fn size(&self) -> Size<DevicePixels>;
}

/// Platform-tagged handle to a GPU texture exported by another process or
/// graphics API. Currently only Linux's dma-buf path is wired up.
#[derive(Debug)]
#[non_exhaustive]
pub enum ExternalTextureDescriptor {
    /// Linux dma-buf single-plane RGBA texture.
    DmaBuf {
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
