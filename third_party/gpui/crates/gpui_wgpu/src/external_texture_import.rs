//! Stage 2 dma-buf → wgpu::Texture import.
//!
//! Takes a Linux dma-buf descriptor produced by tramuntana
//! (`paint_api::DmaBufHandle` re-exposed as
//! `gpui::external_texture::ExternalTextureDescriptor::DmaBuf`) and creates a
//! `wgpu::Texture` whose backing memory IS that dma-buf — no copy, no
//! staging. Implemented by:
//!
//! 1. Reaching into the wgpu Vulkan backend via `Device::as_hal` and
//!    `wgpu::hal::vulkan::Device::raw_device` to get the raw `ash::Device`.
//! 2. Creating a `VkImage` with `tiling = DRM_FORMAT_MODIFIER_EXT` and
//!    chaining `VkExternalMemoryImageCreateInfo` (handle type
//!    `DMA_BUF_EXT`) and
//!    `VkImageDrmFormatModifierExplicitCreateInfoEXT` (carrying the
//!    NVIDIA-specific modifier returned by the producer).
//! 3. Allocating `VkDeviceMemory` with `VkImportMemoryFdInfoKHR` to
//!    pull the fd. Vulkan dup's the fd internally so we transfer
//!    ownership at this point.
//! 4. Binding image to memory.
//! 5. Wrapping as `wgpu_hal::vulkan::Texture` (memory ownership transferred
//!    to wgpu-hal via `TextureMemory::Dedicated`), then via
//!    `Device::create_texture_from_hal` into a `wgpu::Texture`.
//!
//! Linux/FreeBSD-only. The non-Linux build of `gpui_wgpu` excludes this
//! module entirely.
//!
//! Currently unused — the render pipeline (`ExternalSpritePipeline`) that
//! consumes the resulting `wgpu::Texture` is Stage 2b.4.

#![cfg(any(target_os = "linux", target_os = "freebsd"))]
#![allow(unsafe_code)]

use std::os::fd::{AsRawFd, IntoRawFd};

use ash::vk;
use gpui::external_texture::{ExternalFence, ExternalTextureDescriptor};

/// Import an external GPU texture descriptor (currently only the
/// `DmaBuf` variant) into a `wgpu::Texture` backed by the same physical
/// memory. Returns `None` and logs the failure cause if any step fails;
/// callers should fall back to Stage 1 (CPU readback) when this happens.
pub fn import_external_texture(
    device: &wgpu::Device,
    descriptor: ExternalTextureDescriptor,
) -> Option<wgpu::Texture> {
    match descriptor {
        ExternalTextureDescriptor::DmaBuf {
            // buffer_id keys the compositor's import cache (see wgpu_renderer);
            // the importer itself doesn't need it.
            buffer_id: _,
            fd,
            fourcc,
            modifier,
            stride,
            offset,
            width,
            height,
        } => import_dma_buf(
            device, fd, fourcc, modifier, stride, offset, width, height,
        ),
        // ExternalTextureDescriptor is #[non_exhaustive] to leave room for
        // future IOSurface / Win32Handle variants. Linux build only knows
        // DmaBuf — drop the rest with a log so we notice if/when the enum
        // grows.
        other => {
            log::warn!(
                "import_external_texture: descriptor variant not supported on this build: {other:?}"
            );
            None
        }
    }
}

/// Block until the producer's GPU work behind `fence` has completed, so the
/// draw that samples the imported texture reads a finished frame instead of
/// racing the blit that fills the dma-buf.
///
/// `fence` is a Linux `sync_file` fd (exported on the producer via
/// `EGL_ANDROID_native_fence_sync` right after the blit). It becomes readable
/// (`POLLIN`) once the GL commands it snapshots — Servo's paint plus our blit
/// into the shared texture — finish on the GPU.
///
/// We wait on the CPU with `poll()` rather than importing the fd as a
/// `VkSemaphore` and waiting on the queue. Reason: gpui draws through wgpu, and
/// wgpu does not expose a way to inject a wait semaphore into the submission
/// that samples the texture. A separate wait-only `vkQueueSubmit` would only
/// order ahead of wgpu's submission by relying on driver-specific intra-queue
/// behaviour — the exact "correct by luck" hazard this fence exists to remove.
/// The CPU wait is unconditionally correct, and because a lot of scene-building
/// happens between the producer's `glFlush` and this call, the blit is usually
/// already done by the time we get here: the stall is near-zero when the GPU
/// keeps up and only blocks (correctly, as back-pressure) under load.
///
/// Best-effort: on timeout or `poll` error we log and return, letting the draw
/// proceed rather than hang the UI thread on a fence that never signals.
pub fn wait_external_fence(fence: ExternalFence) {
    match fence {
        ExternalFence::SyncFd(fd) => {
            // A blit at 1920x1080 completes in well under a millisecond;
            // anything near this bound means the producer fence never signalled
            // (driver bug) and blocking the UI thread longer helps no one.
            const TIMEOUT_MS: libc::c_int = 1000;
            let mut pfd = libc::pollfd {
                fd: fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let result = loop {
                // SAFETY: `pfd` is a single valid `pollfd`; `fd` is kept alive
                // by the `OwnedFd` binding for the whole loop, so the
                // descriptor stays valid across the call.
                let rc = unsafe { libc::poll(&mut pfd, 1, TIMEOUT_MS) };
                if rc < 0 {
                    let err = std::io::Error::last_os_error();
                    if err.kind() == std::io::ErrorKind::Interrupted {
                        continue; // EINTR — resume waiting rather than skip it
                    }
                    break Err(err);
                }
                break Ok(rc);
            };
            match result {
                // 1 fd ready: the sync_file signalled (or surfaced POLLERR/HUP,
                // in which case there is nothing better to do than proceed).
                Ok(1) => {}
                Ok(_) => log::warn!(
                    "wait_external_fence: sync fd did not signal within {TIMEOUT_MS}ms; \
                     sampling a possibly-incomplete frame"
                ),
                Err(err) => log::warn!("wait_external_fence: poll() failed: {err}"),
            }
            // `fd` (OwnedFd) drops here, closing the sync_file descriptor now
            // that it has been waited on.
        }
        // ExternalFence is #[non_exhaustive]; future variants (MachPort,
        // Win32Event) need their own wait path. Until one exists, NOT waiting
        // is unsound, so make the gap loud instead of silently racing.
        other => log::warn!(
            "wait_external_fence: unsupported fence variant on this build: {other:?}"
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn import_dma_buf(
    device: &wgpu::Device,
    fd: std::os::fd::OwnedFd,
    fourcc: u32,
    modifier: u64,
    stride: u32,
    offset: u32,
    width: u32,
    height: u32,
) -> Option<wgpu::Texture> {
    // Map DRM fourcc to Vulkan / wgpu format. Currently only the two
    // 8-bit-per-channel variants the producer is known to emit.
    const DRM_FORMAT_ABGR8888: u32 = 0x34324241;
    const DRM_FORMAT_ARGB8888: u32 = 0x34325241;
    let (vk_format, wgpu_format) = match fourcc {
        DRM_FORMAT_ABGR8888 => (vk::Format::R8G8B8A8_UNORM, wgpu::TextureFormat::Rgba8Unorm),
        DRM_FORMAT_ARGB8888 => (vk::Format::B8G8R8A8_UNORM, wgpu::TextureFormat::Bgra8Unorm),
        other => {
            log::warn!(
                "import_dma_buf: unsupported DRM fourcc 0x{other:08x}; only ABGR8888 and ARGB8888 mapped"
            );
            return None;
        }
    };

    // SAFETY: we hold `device` (and its hal device) only for the
    // duration of this function. We don't escape any wgpu_hal handles
    // past the wgpu wrapper at the end.
    let hal_device_guard = unsafe { device.as_hal::<wgpu::hal::api::Vulkan>() }?;
    let hal_device: &wgpu::hal::vulkan::Device = &hal_device_guard;
    let raw_device = hal_device.raw_device();
    let phys_dev = hal_device.raw_physical_device();
    let instance = hal_device.shared_instance().raw_instance();

    // Build VkImageCreateInfo with the two extension structs chained.
    let mut external_memory_info = vk::ExternalMemoryImageCreateInfo::default()
        .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);

    let plane_layout = vk::SubresourceLayout {
        offset: offset as u64,
        size: 0,
        row_pitch: stride as u64,
        array_pitch: 0,
        depth_pitch: 0,
    };
    let plane_layouts = [plane_layout];
    let mut drm_modifier_info = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
        .drm_format_modifier(modifier)
        .plane_layouts(&plane_layouts);

    let image_create_info = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(vk_format)
        .extent(vk::Extent3D {
            width,
            height,
            depth: 1,
        })
        .mip_levels(1)
        .array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
        .usage(vk::ImageUsageFlags::SAMPLED)
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .initial_layout(vk::ImageLayout::UNDEFINED)
        .push_next(&mut external_memory_info)
        .push_next(&mut drm_modifier_info);

    let vk_image = match unsafe { raw_device.create_image(&image_create_info, None) } {
        Ok(image) => image,
        Err(err) => {
            log::warn!("import_dma_buf: vkCreateImage failed: {err:?}");
            return None;
        }
    };

    // Memory allocation: import the dma-buf fd. Vulkan takes ownership
    // of the fd on a successful call, so we hand off via IntoRawFd.
    let mem_reqs = unsafe { raw_device.get_image_memory_requirements(vk_image) };
    let mem_props = unsafe { instance.get_physical_device_memory_properties(phys_dev) };
    let mem_type_index = match (0..mem_props.memory_type_count).find(|&i| {
        mem_reqs.memory_type_bits & (1 << i) != 0
    }) {
        Some(i) => i,
        None => {
            log::warn!(
                "import_dma_buf: no Vulkan memory type compatible with dma-buf (bits={:b})",
                mem_reqs.memory_type_bits
            );
            unsafe {
                raw_device.destroy_image(vk_image, None);
            }
            return None;
        }
    };

    let raw_fd = fd.into_raw_fd();
    let mut import_info = vk::ImportMemoryFdInfoKHR::default()
        .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
        .fd(raw_fd);
    let alloc_info = vk::MemoryAllocateInfo::default()
        .allocation_size(mem_reqs.size)
        .memory_type_index(mem_type_index)
        .push_next(&mut import_info);

    let vk_memory = match unsafe { raw_device.allocate_memory(&alloc_info, None) } {
        Ok(memory) => memory,
        Err(err) => {
            log::warn!("import_dma_buf: vkAllocateMemory (dma-buf import) failed: {err:?}");
            // Vulkan documents that on failure it does NOT take ownership of
            // the fd, so close it ourselves.
            // SAFETY: the fd was valid when we extracted it from OwnedFd, and
            // Vulkan didn't claim ownership because allocate_memory failed.
            unsafe {
                libc::close(raw_fd);
                raw_device.destroy_image(vk_image, None);
            }
            return None;
        }
    };

    if let Err(err) = unsafe { raw_device.bind_image_memory(vk_image, vk_memory, 0) } {
        log::warn!("import_dma_buf: vkBindImageMemory failed: {err:?}");
        unsafe {
            raw_device.free_memory(vk_memory, None);
            raw_device.destroy_image(vk_image, None);
        }
        return None;
    }

    // Build wgpu_hal::vulkan::Texture, transferring ownership of the
    // VkImage and VkDeviceMemory. wgpu-hal will free them when the
    // wrapping wgpu::Texture is dropped.
    let hal_label = format!("rumb-external-dma-buf-{width}x{height}");
    let hal_desc = wgpu::hal::TextureDescriptor {
        label: Some(&hal_label),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu_format,
        usage: wgpu::wgt::TextureUses::RESOURCE,
        memory_flags: wgpu::hal::MemoryFlags::empty(),
        view_formats: vec![],
    };
    let hal_texture = unsafe {
        hal_device.texture_from_raw(
            vk_image,
            &hal_desc,
            None,
            wgpu::hal::vulkan::TextureMemory::Dedicated(vk_memory),
        )
    };

    let wgpu_desc = wgpu::TextureDescriptor {
        label: Some("rumb-external-dma-buf"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu_format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    };
    let texture = unsafe {
        device.create_texture_from_hal::<wgpu::hal::api::Vulkan>(hal_texture, &wgpu_desc)
    };
    Some(texture)
}
