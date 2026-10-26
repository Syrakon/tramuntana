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
//! The device must be opened through [`request_device_with_dma_buf`]: wgpu-hal
//! does not enable `VK_EXT_image_drm_format_modifier` itself, and the import
//! refuses to run without it.

#![cfg(any(target_os = "linux", target_os = "freebsd"))]
#![allow(unsafe_code)]

use std::os::fd::{AsRawFd, IntoRawFd};

use ash::vk;
use gpui::external_texture::{ExternalFence, ExternalTextureDescriptor};

/// Open `adapter`'s device with `VK_EXT_image_drm_format_modifier` enabled,
/// which wgpu-hal never enables on its own. The import below creates its
/// image with `DRM_FORMAT_MODIFIER_EXT` tiling, and that tiling is only valid
/// on a device with the extension on.
///
/// Returns `None`, with the cause logged, when the adapter is not Vulkan or
/// cannot enable the extension. The caller then opens the device the usual
/// way, and every import refuses cleanly instead of relying on the driver
/// tolerating invalid usage.
pub(crate) fn request_device_with_dma_buf(
    adapter: &wgpu::Adapter,
    descriptor: &wgpu::DeviceDescriptor<'_>,
) -> Option<(wgpu::Device, wgpu::Queue)> {
    let modifier_extension = ash::ext::image_drm_format_modifier::NAME;
    let open_device = {
        // SAFETY: the hal adapter is only used inside this block, to open a
        // device from it; nothing it hands out outlives the guard.
        let hal_adapter = unsafe { adapter.as_hal::<wgpu::hal::api::Vulkan>() }?;
        let capabilities = hal_adapter.physical_device_capabilities();
        if !capabilities.supports_extension(modifier_extension) {
            log::info!(
                "{} not supported by this adapter; dma-buf frames will not be imported",
                modifier_extension.to_string_lossy()
            );
            return None;
        }
        // The extension depends on VK_KHR_image_format_list,
        // VK_KHR_bind_memory2 and VK_KHR_sampler_ycbcr_conversion, all core
        // from Vulkan 1.2. Older devices are not worth a separate path.
        if capabilities.properties().api_version < vk::API_VERSION_1_2 {
            log::info!("Vulkan device older than 1.2; dma-buf frames will not be imported");
            return None;
        }
        // Lets an imported image be acquired from the producer's queue family
        // as VK_QUEUE_FAMILY_FOREIGN_EXT, the family a dma-buf written outside
        // Vulkan belongs to. Without it imports fall back to
        // VK_QUEUE_FAMILY_EXTERNAL (see `external_queue_family`).
        let foreign_extension = ash::ext::queue_family_foreign::NAME;
        let foreign_supported = capabilities.supports_extension(foreign_extension);
        let add_extension: Box<wgpu::hal::vulkan::CreateDeviceCallback<'_>> =
            Box::new(move |args| {
                if !args.extensions.contains(&modifier_extension) {
                    args.extensions.push(modifier_extension);
                }
                if foreign_supported && !args.extensions.contains(&foreign_extension) {
                    args.extensions.push(foreign_extension);
                }
            });
        // SAFETY: the callback only adds extensions the physical device
        // reports as supported, with their dependencies core in the device's
        // API version (both checked above). It removes nothing.
        let opened = unsafe {
            hal_adapter.open_with_callback(
                descriptor.required_features,
                &descriptor.required_limits,
                &descriptor.memory_hints,
                Some(add_extension),
            )
        };
        match opened {
            Ok(open_device) => open_device,
            Err(err) => {
                log::warn!("opening the Vulkan device with dma-buf import failed: {err:?}");
                return None;
            }
        }
    };
    // SAFETY: `open_device` was opened from this same adapter, with exactly
    // the features `descriptor` asks for.
    match unsafe { adapter.create_device_from_hal(open_device, descriptor) } {
        Ok(pair) => Some(pair),
        Err(err) => {
            log::warn!("wrapping the Vulkan device with dma-buf import failed: {err}");
            None
        }
    }
}

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

/// The single-plane DRM modifiers `device` can import a dma-buf of
/// `vk_format` with and sample from, in the driver's order. Empty when the
/// device is not Vulkan or was opened without
/// `VK_EXT_image_drm_format_modifier`.
pub fn sampleable_modifiers(device: &wgpu::Device, vk_format: vk::Format) -> Vec<u64> {
    // SAFETY: the hal device is only used inside this function.
    let Some(hal_device) = (unsafe { device.as_hal::<wgpu::hal::api::Vulkan>() }) else {
        return Vec::new();
    };
    if !hal_device
        .enabled_device_extensions()
        .contains(&ash::ext::image_drm_format_modifier::NAME)
    {
        return Vec::new();
    }
    let instance = hal_device.shared_instance().raw_instance();
    let phys_dev = hal_device.raw_physical_device();

    // Two calls: the first gives the count, the second fills the list.
    let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
    let mut properties = vk::FormatProperties2::default().push_next(&mut list);
    // SAFETY: `properties` chains `list`, which has no output array yet, so
    // the driver only writes the count.
    unsafe {
        instance.get_physical_device_format_properties2(phys_dev, vk_format, &mut properties)
    };
    let mut entries =
        vec![vk::DrmFormatModifierPropertiesEXT::default(); list.drm_format_modifier_count as usize];
    let mut list = vk::DrmFormatModifierPropertiesListEXT::default()
        .drm_format_modifier_properties(&mut entries);
    let mut properties = vk::FormatProperties2::default().push_next(&mut list);
    // SAFETY: `entries` holds exactly the count the first call returned, and
    // outlives the call.
    unsafe {
        instance.get_physical_device_format_properties2(phys_dev, vk_format, &mut properties)
    };
    let filled = list.drm_format_modifier_count as usize;

    entries[..filled.min(entries.len())]
        .iter()
        .filter(|entry| {
            entry.drm_format_modifier_plane_count == 1
                && entry
                    .drm_format_modifier_tiling_features
                    .contains(vk::FormatFeatureFlags::SAMPLED_IMAGE)
        })
        .map(|entry| entry.drm_format_modifier)
        .filter(|&modifier| import_extent(instance, phys_dev, vk_format, modifier).is_ok())
        .collect()
}

/// The DRM nodes of `device`'s GPU as `(major, minor)`: the primary node,
/// then the render node when there is one. A dma-buf producer compares them
/// with its own GPU, since a buffer from another GPU does not import. Empty
/// when the device is not Vulkan or the driver lacks
/// `VK_EXT_physical_device_drm`.
pub fn drm_nodes(device: &wgpu::Device) -> Vec<(u32, u32)> {
    // SAFETY: the hal device is only used inside this function.
    let Some(hal_device) = (unsafe { device.as_hal::<wgpu::hal::api::Vulkan>() }) else {
        return Vec::new();
    };
    let instance = hal_device.shared_instance().raw_instance();
    let phys_dev = hal_device.raw_physical_device();
    // SAFETY: `phys_dev` belongs to `instance`.
    let extensions =
        match unsafe { instance.enumerate_device_extension_properties(phys_dev) } {
            Ok(extensions) => extensions,
            Err(err) => {
                log::warn!("listing the Vulkan device extensions failed: {err}");
                return Vec::new();
            }
        };
    let supported = extensions.iter().any(|extension| {
        extension.extension_name_as_c_str() == Ok(ash::ext::physical_device_drm::NAME)
    });
    if !supported {
        log::info!(
            "{} not supported; dma-buf producers cannot check they share the GPU",
            ash::ext::physical_device_drm::NAME.to_string_lossy()
        );
        return Vec::new();
    }
    let mut drm = vk::PhysicalDeviceDrmPropertiesEXT::default();
    let mut properties = vk::PhysicalDeviceProperties2::default().push_next(&mut drm);
    // SAFETY: the device supports the extension (checked above), so the
    // driver fills the chained `drm`, which outlives the call.
    unsafe { instance.get_physical_device_properties2(phys_dev, &mut properties) };
    let mut nodes = Vec::with_capacity(2);
    // Vulkan reports node numbers as signed integers; DRM numbers fit in u32.
    if drm.has_primary == vk::TRUE {
        nodes.push((drm.primary_major as u32, drm.primary_minor as u32));
    }
    if drm.has_render == vk::TRUE {
        nodes.push((drm.render_major as u32, drm.render_minor as u32));
    }
    nodes
}

/// The queue family a dma-buf belongs to while the producer has it:
/// VK_QUEUE_FAMILY_FOREIGN_EXT when the device has the extension, which is
/// the family for memory written outside Vulkan, else
/// VK_QUEUE_FAMILY_EXTERNAL.
fn external_queue_family(hal_device: &wgpu::hal::vulkan::Device) -> u32 {
    if hal_device
        .enabled_device_extensions()
        .contains(&ash::ext::queue_family_foreign::NAME)
    {
        vk::QUEUE_FAMILY_FOREIGN_EXT
    } else {
        vk::QUEUE_FAMILY_EXTERNAL
    }
}

/// Hand imported `textures` back to the producer after the frame's last use
/// of them: from SHADER_READ_ONLY_OPTIMAL, where the frame sampled them, to
/// GENERAL, and from the device's queue family to [`external_queue_family`].
/// Pairs with the acquire wgpu-hal records on each import's first use. wgpu
/// still sees the textures as sampled afterwards, so none of them may be used
/// again: each frame imports its buffers anew.
///
/// The barrier is recorded through the raw API, which wgpu does not allow in
/// an encoder that also took wgpu commands, so it comes back as a command
/// buffer of its own, to submit right after the frame's.
pub fn release_external_textures(
    device: &wgpu::Device,
    textures: &[wgpu::Texture],
) -> Option<wgpu::CommandBuffer> {
    if textures.is_empty() {
        return None;
    }
    // SAFETY: the hal device is only read here, for handles and the queue
    // family; nothing outlives the guard.
    let hal_device = unsafe { device.as_hal::<wgpu::hal::api::Vulkan>() }?;
    let family = hal_device.queue_family_index();
    let external_family = external_queue_family(&hal_device);
    let range = vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .level_count(1)
        .layer_count(1);
    let barriers: Vec<_> = textures
        .iter()
        .filter_map(|texture| {
            // SAFETY: only the image handle is read.
            let hal_texture = unsafe { texture.as_hal::<wgpu::hal::api::Vulkan>() }?;
            Some(
                vk::ImageMemoryBarrier::default()
                    .image(unsafe { hal_texture.raw_handle() })
                    .subresource_range(range)
                    .src_access_mask(vk::AccessFlags::SHADER_READ)
                    .old_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                    .new_layout(vk::ImageLayout::GENERAL)
                    .src_queue_family_index(family)
                    .dst_queue_family_index(external_family),
            )
        })
        .collect();
    let raw_device = hal_device.raw_device();
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("external_texture_release"),
    });
    // SAFETY: the encoder records nothing but this barrier, and it is
    // submitted after the frame that sampled the images.
    unsafe {
        encoder.as_hal_mut::<wgpu::hal::api::Vulkan, _, _>(|hal_encoder| {
            let Some(hal_encoder) = hal_encoder else {
                return;
            };
            raw_device.cmd_pipeline_barrier(
                hal_encoder.raw_handle(),
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &barriers,
            );
        });
    }
    Some(encoder.finish())
}

/// How an imported image is used: sampled, and copied from too while
/// [`gpui::external_texture::frame_stamp_checks`] reads its frame stamps back.
fn image_usage() -> vk::ImageUsageFlags {
    if gpui::external_texture::frame_stamp_checks() {
        vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_SRC
    } else {
        vk::ImageUsageFlags::SAMPLED
    }
}

/// Read back the frame stamp of `texture`, imported from buffer `buffer_id`,
/// and log an error when it is not `expected`. See
/// [`gpui::external_texture::ExternalGpuTexture::frame_stamp`] for where the
/// producer writes it. Blocks until the GPU copy is done: debug mode only.
pub fn check_frame_stamp(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    buffer_id: u64,
    expected: u32,
) {
    // One texel per copy, each at an offset the copy alignment allows.
    const SLOT: u64 = wgpu::COPY_BUFFER_ALIGNMENT * 64;
    let size = texture.size();
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("frame_stamp_readback"),
        size: SLOT * 2,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("frame_stamp_check"),
    });
    let corners = [(0, 0), (size.width - 1, size.height - 1)];
    for (slot, (x, y)) in corners.into_iter().enumerate() {
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d { x, y, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: slot as u64 * SLOT,
                    bytes_per_row: None,
                    rows_per_image: None,
                },
            },
            wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
        );
    }
    let submission = queue.submit([encoder.finish()]);
    let slice = readback.slice(..);
    let (mapped_tx, mapped_rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |result| {
        // The receiver only goes away once the check has given up.
        if mapped_tx.send(result).is_err() {
            log::debug!("frame stamp check: the copy was mapped after the check gave up");
        }
    });
    if let Err(err) = device.poll(wgpu::PollType::Wait {
        submission_index: Some(submission),
        timeout: None,
    }) {
        log::warn!("frame stamp check of buffer {buffer_id}: waiting for the copy failed: {err}");
        return;
    }
    match mapped_rx.try_recv() {
        Ok(Ok(())) => {}
        Ok(Err(err)) => {
            log::warn!("frame stamp check of buffer {buffer_id}: mapping the copy failed: {err}");
            return;
        }
        Err(err) => {
            log::warn!("frame stamp check of buffer {buffer_id}: the copy was not mapped: {err}");
            return;
        }
    }
    let found = {
        let bytes = slice.get_mapped_range();
        let texel = |slot: usize| {
            let at = slot * SLOT as usize;
            u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
        };
        [texel(0), texel(1)]
    };
    readback.unmap();
    if found == [expected; 2] {
        log::trace!("frame stamp {expected} of buffer {buffer_id} checked");
    } else {
        log::error!(
            "frame stamp mismatch on buffer {buffer_id}: expected {expected}, the first texel \
             holds {} and the last {}",
            found[0],
            found[1]
        );
    }
}

/// The largest image of `vk_format` with `modifier` that `phys_dev` can
/// import from a dma-buf and sample, or why it cannot.
fn import_extent(
    instance: &ash::Instance,
    phys_dev: vk::PhysicalDevice,
    vk_format: vk::Format,
    modifier: u64,
) -> Result<vk::Extent3D, String> {
    let mut external_format_info = vk::PhysicalDeviceExternalImageFormatInfo::default()
        .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let mut modifier_format_info = vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default()
        .drm_format_modifier(modifier)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    let format_info = vk::PhysicalDeviceImageFormatInfo2::default()
        .format(vk_format)
        .ty(vk::ImageType::TYPE_2D)
        .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
        .usage(image_usage())
        .push_next(&mut external_format_info)
        .push_next(&mut modifier_format_info);
    let mut external_properties = vk::ExternalImageFormatProperties::default();
    let mut format_properties =
        vk::ImageFormatProperties2::default().push_next(&mut external_properties);
    // SAFETY: both structure chains are built above and live until the call
    // returns; `phys_dev` belongs to `instance`.
    unsafe {
        instance.get_physical_device_image_format_properties2(
            phys_dev,
            &format_info,
            &mut format_properties,
        )
    }
    .map_err(|err| format!("the device cannot sample it: {err:?}"))?;
    // Read before `external_properties`, which the chain borrows until here.
    let max_extent = format_properties.image_format_properties.max_extent;
    if !external_properties
        .external_memory_properties
        .external_memory_features
        .contains(vk::ExternalMemoryFeatureFlags::IMPORTABLE)
    {
        return Err("not importable from a dma-buf".to_owned());
    }
    Ok(max_extent)
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

    if !hal_device
        .enabled_device_extensions()
        .contains(&ash::ext::image_drm_format_modifier::NAME)
    {
        log::warn!(
            "import_dma_buf: the device was opened without VK_EXT_image_drm_format_modifier; \
             refusing the import (see request_device_with_dma_buf)"
        );
        return None;
    }

    // Ask before creating: a modifier the device does not know is invalid
    // usage in vkCreateImage, not an error it reports.
    let max_extent = match import_extent(instance, phys_dev, vk_format, modifier) {
        Ok(extent) => extent,
        Err(cause) => {
            log::warn!(
                "import_dma_buf: fourcc 0x{fourcc:08x} with modifier 0x{modifier:016x}: {cause} \
                 (this device imports {:x?})",
                sampleable_modifiers(device, vk_format)
            );
            return None;
        }
    };
    if width > max_extent.width || height > max_extent.height {
        log::warn!(
            "import_dma_buf: {width}x{height} is over the {}x{} this device imports with \
             modifier 0x{modifier:016x}",
            max_extent.width,
            max_extent.height
        );
        return None;
    }

    // The memory types this particular dma-buf can be imported into.
    let external_memory_fd = ash::khr::external_memory_fd::Device::new(instance, raw_device);
    let mut fd_properties = vk::MemoryFdPropertiesKHR::default();
    // SAFETY: `fd` is a valid dma-buf fd, kept open by the `OwnedFd` for the
    // whole call; the call only reads it.
    let fd_properties_result = unsafe {
        external_memory_fd.get_memory_fd_properties(
            vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
            fd.as_raw_fd(),
            &mut fd_properties,
        )
    };
    if let Err(err) = fd_properties_result {
        log::warn!("import_dma_buf: vkGetMemoryFdPropertiesKHR failed: {err:?}");
        return None;
    }

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
        .usage(image_usage())
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
    let memory_type_bits = mem_reqs.memory_type_bits & fd_properties.memory_type_bits;
    let mem_type_index = match (0..mem_props.memory_type_count).find(|&i| {
        memory_type_bits & (1 << i) != 0
    }) {
        Some(i) => i,
        None => {
            log::warn!(
                "import_dma_buf: no Vulkan memory type fits both the image and the dma-buf \
                 (image bits={:b}, dma-buf bits={:b})",
                mem_reqs.memory_type_bits,
                fd_properties.memory_type_bits
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
        usage: if gpui::external_texture::frame_stamp_checks() {
            wgpu::wgt::TextureUses::RESOURCE | wgpu::wgt::TextureUses::COPY_SRC
        } else {
            wgpu::wgt::TextureUses::RESOURCE
        },
        memory_flags: wgpu::hal::MemoryFlags::empty(),
        view_formats: vec![],
    };
    let mut hal_texture = unsafe {
        hal_device.texture_from_raw(
            vk_image,
            &hal_desc,
            None,
            wgpu::hal::vulkan::TextureMemory::Dedicated(vk_memory),
        )
    };
    // SAFETY: the producer leaves the dma-buf in GENERAL layout, owned by
    // its own queue family, and the renderer hands the image back with
    // `release_external_textures` after the frame that samples it.
    unsafe { hal_texture.set_external_queue_family(external_queue_family(hal_device)) };

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
        usage: if gpui::external_texture::frame_stamp_checks() {
            wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_SRC
        } else {
            wgpu::TextureUsages::TEXTURE_BINDING
        },
        view_formats: &[],
    };
    let texture = unsafe {
        device.create_texture_from_hal::<wgpu::hal::api::Vulkan>(hal_texture, &wgpu_desc)
    };
    Some(texture)
}
