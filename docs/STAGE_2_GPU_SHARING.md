# Stage 2 — Zero-copy GPU texture sharing between Servo and GPUI

## Goal

Eliminate the two CPU↔GPU copies per frame in the rumb embedding:

```
Stage 1 (current):
  Servo paint → GL FBO (GPU) → glReadPixels → RAM Vec<u8>
                                              ↓ (R↔B swap in CPU)
                                              wgpu::Queue::write_texture → GPU again

Stage 2 (target):
  Servo paint → GL FBO (GPU) ──dma-buf──→ wgpu::Texture (same GPU, no readback)
                              ──fence fd─→ Vulkan wait
```

## Why now

GPUI is vendored inside tramuntana (commit `387748e`). Both sides
are in the same tree, so a cross-cutting change is feasible without
coordinating two repos. The current `read_to_image` + `RenderImage`
path tops out around 60 fps even on a static page on an RTX 3060 —
WebGL workloads (three.js, GPU canvases) bottleneck on the readback,
not on Servo's paint.

## Architecture

### Producer side: tramuntana (Servo, OpenGL via surfman)

Servo renders into an offscreen FBO (the `new_offscreen_only`
`WindowRenderingContext` from commit `0871238`). The FBO is backed by
a GL texture that lives in the surfman-managed GL context.

**Why direct export of Servo's FBO does not work** (validated at
runtime 2026-05-17): WebRender / `wgpu_hal::gles::egl` internally call
`eglCreateImageKHR` against surfman's surface textures to do their
own work. EGL only allows one EGLImage sibling per source object, so
our `eglCreateImageKHR` returns `EGL_BAD_ACCESS` ("Source is already
an EGL sibling"). The texture is busy as far as EGL is concerned.

**Workaround that does work**: keep a **private intermediate texture**
that we own exclusively (never bound to surfman, never seen by
WebRender), and blit the framebuffer onto it on the GPU each frame.
Cost: one `glBlitFramebuffer` per export, which is microseconds at
1280×800 on a modern GPU — still many orders of magnitude cheaper
than the CPU readback path it replaces.

Lifecycle, after each `webview.paint()`:

1. Build cache (once, lazily on first call, or on resize):
   - `glGenTextures` / `glTexImage2D` for an `RGBA8` private texture.
   - `glGenFramebuffers` / `glFramebufferTexture2D` for a private FBO
     with that texture as `COLOR_ATTACHMENT0`.
   - `eglCreateImageKHR(EGL_GL_TEXTURE_2D, our_texture_id)` →
     `EGLImageKHR` (no sibling collision now — the texture is
     exclusively ours).
   - `eglExportDMABUFImageQueryMESA` (fourcc, num_planes, modifier).
   - `eglExportDMABUFImageMESA` (dma-buf fd, stride, offset).
   - Cache everything. `EGLImage` and `fd` are owned by the cache
     for as long as the size doesn't change.
2. Per-frame export:
   - Bind Servo's FBO as `READ_FRAMEBUFFER`.
   - Bind our cached FBO as `DRAW_FRAMEBUFFER`.
   - `glBlitFramebuffer` (color buffer, `NEAREST`).
   - `glFlush` to push the blit into the command stream.
   - `eglCreateSyncKHR(EGL_SYNC_NATIVE_FENCE_ANDROID)` then
     `eglDupNativeFenceFDANDROID` → fence sync fd snapshotting the
     blit.
   - `OwnedFd::try_clone` of the cached dma-buf fd so the consumer
     gets its own fd.
3. Return `DmaBufHandle { fd (dup), fence_fd, fourcc, width, height,
   stride, offset, modifier }` to the embedder. The cache continues
   to own the EGLImage and the original dma-buf fd; the next call
   reuses both.

New public API on `WindowRenderingContext`:

```rust
pub struct DmaBufHandle {
    pub fd: OwnedFd,              // O_CLOEXEC
    pub fence_fd: OwnedFd,        // GL → consumer sync
    pub width: u32,
    pub height: u32,
    pub drm_fourcc: u32,          // DRM_FORMAT_ARGB8888 etc.
    pub stride: u32,
    pub offset: u32,
    pub modifier: u64,            // DRM_FORMAT_MOD_LINEAR or vendor
    pub num_planes: u32,          // typically 1 for RGBA
}

impl WindowRenderingContext {
    /// Export the current framebuffer as a dma-buf for cross-API sharing.
    /// Caller owns both fds and must close them.
    /// Returns None if the EGL implementation does not support
    /// EGL_MESA_image_dma_buf_export.
    pub fn export_framebuffer_dma_buf(&self) -> Option<DmaBufHandle>;
}
```

Reuses the existing `surfman_details()` / EGL access patterns. Lives
next to `read_to_image` in
`components/shared/paint/rendering_context.rs` for discoverability.

### Consumer side: gpui (wgpu, Vulkan)

Two changes needed in gpui (vendored at
`tramuntana/third_party/gpui/`):

**1. New `ExternalGpuTexture` trait + dedicated element — outside the atlas.**

The atlas is CPU-side by design (`PlatformAtlas::get_or_insert_with`
takes `Cow<[u8]>`). Routing external textures through it requires
rewriting the trait. Initially I considered adding a variant to
`ImageSource`, but the path from `Img::paint` → `source.use_data()` →
`Arc<RenderImage>` → `window.paint_image()` is fundamentally
CPU-side: every step assumes a `RenderImage`. Threading an `External`
variant through it would require five separate forks of `Option<…>`
plumbing.

Cleaner: a parallel element type. `ImageSource` stays untouched and
keeps serving CPU-backed images; `ExternalImage` (new) takes an
`Arc<dyn ExternalGpuTexture>` and emits its own scene primitive.

```rust
// crates/gpui/src/external_texture.rs
pub trait ExternalGpuTexture: Send + Sync {
    fn descriptor(&self) -> Option<ExternalTextureDescriptor>;
    fn fence(&self) -> Option<ExternalFence>;
    fn size(&self) -> Size<DevicePixels>;
}

pub enum ExternalTextureDescriptor {
    DmaBuf { fd, fourcc, modifier, stride, offset, width, height },
    // future: IOSurface { ... }, Win32Handle { ... }
}

pub enum ExternalFence {
    SyncFd(OwnedFd),
    // future: MachPort, Win32Event
}
```

`Window::paint_external_image()` (new method, sibling of
`paint_image`) emits a new scene primitive
(`ExternalSprite { handle, bounds, ... }`) routed through a new
`ExternalSpritePipeline` in `gpui_wgpu`. The existing
`PolychromeSpritePipeline` stays unchanged.

**2. New scene primitive renderer in `gpui_wgpu`.**

Add `ExternalSpritePipeline` next to `PolychromeSpritePipeline`. Uses
the same vertex shader but samples the externally-bound texture
directly (no atlas lookup). Each frame:

- For each `ExternalSprite`, register the `wgpu::Texture` in a transient
  `wgpu::BindGroup`.
- If the provider has a `fence_fd`, submit a Vulkan `VkSemaphore`
  import via `VK_KHR_external_semaphore_fd` and wait on it before the
  first draw call that samples the texture.

### Producer fence → consumer wait, in detail

The fence is the part most likely to fail visually if skipped or
rushed.

Producer (after paint):
```
glFenceSync(GL_SYNC_GPU_COMMANDS_COMPLETE, 0)  // returns GLsync
eglDupNativeFenceFDANDROID(display, glsync)    // returns int fd
```

Consumer (before sample):
```rust
let raw_sem_create = ash::vk::SemaphoreCreateInfo::default();
let semaphore = device.create_semaphore(&raw_sem_create, None)?;

let import = ash::vk::ImportSemaphoreFdInfoKHR::default()
    .semaphore(semaphore)
    .handle_type(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD)
    .fd(fence_fd);
khr_external_semaphore_fd.import_semaphore_fd(&import)?;

// submit a wait-only on the present queue
let wait_info = vk::SubmitInfo::default()
    .wait_semaphores(&[semaphore])
    .wait_dst_stage_mask(&[vk::PipelineStageFlags::FRAGMENT_SHADER]);
device.queue_submit(queue, &[wait_info], vk::Fence::null())?;
```

`wgpu` doesn't expose `vk::Semaphore` directly — we drop down to
`wgpu_hal::vulkan` via `Device::as_hal::<vulkan::Api>()` and access the
raw `ash::Device`.

## Why outside the atlas

The atlas is designed around batching many small images into a few
textures. Servo's output is one big texture per frame, replaced
wholesale every frame. Atlas insertion would either:

- Re-allocate every frame (defeating the atlas's reuse purpose), or
- Pin a large atlas slot that's mostly unused.

A dedicated path avoids both problems and keeps the existing atlas
unchanged for the rest of gpui's image needs (icons, font glyphs,
inline images).

## NVIDIA-specific risks

The target dev machine is NVIDIA RTX 3060 with the proprietary
driver. Known sharp edges:

1. **EGL_MESA_image_dma_buf_export** is implemented by NVIDIA but
   only for textures with specific formats and tilings. NVIDIA may
   return a non-linear modifier; Vulkan import must use
   `VK_EXT_image_drm_format_modifier` and not assume `LINEAR`.
2. **Sync fd**: NVIDIA exposes `EGL_NV_nvrm_fence_sync`, not the
   standard `EGL_ANDROID_native_fence_sync` (despite the latter being
   the cross-vendor de facto). The export call name differs — verify
   at runtime which extension string is present and dispatch
   accordingly.
3. **Wayland vs X11**: under Wayland with NVIDIA driver < 555, dma-buf
   feedback is incomplete. Verify driver version when launching;
   fall back to Stage 1 if the export probe fails.
4. **Modifier mismatch**: this one happens. NVIDIA GL stores an RGBA8
   texture with modifier `0x0300000000e08014` (block-linear, compressed),
   and NVIDIA Vulkan imports only `0x0300000000606010`–`…15` and `LINEAR`.
   Until 2026-10 the import ran anyway, as invalid usage that the driver
   tolerated. See "Modifier negotiation" below.

### Modifier negotiation

The consumer says what it can import, and the producer allocates with that:

- wgpu-hal never enables `VK_EXT_image_drm_format_modifier`, though the
  import needs it for `DRM_FORMAT_MODIFIER_EXT` tiling.
  `external_texture_import::request_device_with_dma_buf` opens the device
  through `open_with_callback` with the extension added. When it cannot,
  the device opens as before and every import is refused.
- After opening the device, `gpui_wgpu` publishes the single-plane
  `R8G8B8A8_UNORM` modifiers it can import from a dma-buf and sample
  (`sampleable_modifiers`) through
  `gpui::external_texture::set_importable_abgr8888_modifiers`. The list is
  empty when the extension is off.
- rumb passes that list to `export_framebuffer_dma_buf(importable)`. The
  export ring is then allocated with GBM (`gbm_bo_create_with_modifiers2`),
  on the DRM node of the current EGL display (`EGL_EXT_device_drm`), and
  imported into GL with `EGL_LINUX_DMA_BUF_EXT` and the modifier.
  `libgbm.so.1` is loaded at run time.
- Without GBM, the ring falls back to exported GL textures. A ring whose
  modifier is not in the list is refused (`Unavailable`), so rumb takes the
  CPU path rather than handing gpui a buffer it cannot draw. `None` (the
  consumer cannot say) keeps the old behaviour.
- `import_dma_buf` still checks every import with
  `vkGetPhysicalDeviceImageFormatProperties2` (modifier, size, importable)
  and picks the memory type with `vkGetMemoryFdPropertiesKHR`.

### Buffer reuse: read leases

The export ring has two buffers. The producer must not blit into one while
a submitted gpui frame may still sample it (write-after-read). Each ring
buffer carries a lease (`DmaBufHandle::lease`, an `Arc` the producer counts):

- rumb keeps the handle of the frame on screen, so that buffer stays leased
  while it is shown.
- `gpui_wgpu` takes a clone (`ExternalGpuTexture::read_lease`) for every
  external sprite it draws, and drops the frame's clones in an
  `on_submitted_work_done` callback. Those callbacks run in
  `WgpuRenderer::release_finished_frames`, which the Linux platform calls
  before asking the app for a frame, and again at the start of `draw`.
- `blit_and_export` picks the first buffer whose lease nobody holds. If none
  is free it returns `DmaBufExportError::Busy`, leaves the framebuffer
  alone, and rumb exports on the next tick. After 250 ms of `Busy` it
  writes into a buffer anyway and logs it, so a consumer that stopped
  drawing cannot freeze the tab.

rumb does not hand a new frame to gpui until its fence has signalled
(`Frames` in `engine.rs`, checked with a zero-timeout `poll`). Until then
the previous frame stays on screen, so gpui's own fence wait almost never
blocks the UI thread.

## Fallback

When the export fails (`DmaBufExportError::Unavailable`), rumb reads the
tab's framebuffer back through the CPU (`read_to_image`, swapped to BGRA)
and draws it as a gpui image. The next export that works switches the tab
back. `RUMB_CPU_FRAMES=1` forces the CPU path for every tab: for a driver
the export breaks on, and for testing that path.

## Out of scope

- macOS (IOSurface): different path entirely, not covered here.
- Windows (D3D11/D3D12 shared resources via NT handles): same.
- Wayland subsurfaces (let the compositor composite directly): rejected
  because it would push the browser chrome out of the engine.

## Resolved design decisions

1. **Who imports the dma-buf into a `wgpu::Texture`** — `gpui_wgpu`
   does it internally. Reason: a `wgpu::Texture` is bound to the
   `wgpu::Device` that created it, and `gpui_wgpu` neither exposes
   its `Device` nor would benefit from doing so. Embedders pass an
   opaque descriptor through a trait, gpui owns the resulting texture
   and its lifetime. The trait stays platform-agnostic via an enum
   that can grow IOSurface/Win32 variants later:

   ```rust
   pub trait ExternalGpuTexture: Send + Sync {
       fn descriptor(&self) -> ExternalTextureDescriptor;
       fn fence(&self) -> Option<ExternalFence>;
       fn size(&self) -> Size<DevicePixels>;
   }

   pub enum ExternalTextureDescriptor {
       DmaBuf {
           fd: OwnedFd,
           fourcc: u32,
           modifier: u64,
           stride: u32,
           offset: u32,
       },
       // future: IOSurface { ... }, Win32Handle { ... }
   }

   pub enum ExternalFence {
       SyncFd(OwnedFd),  // Linux
       // future: MachPort(...), Win32(...)
   }
   ```

2. **FBO texture stability across frames** — assumed unstable. surfman's
   `present_bound_surface()`
   ([`rendering_context.rs:211-227`](../components/shared/paint/rendering_context.rs))
   does `unbind_surface_from_context` → `present_surface` →
   `bind_surface_to_context`, and `swap_buffers` is also called
   ([line 387](../components/shared/paint/rendering_context.rs)). The
   underlying GL texture identity is not guaranteed across calls. So
   `gpui_wgpu` should re-import each frame; if profiling shows this
   is a bottleneck, add a small cache keyed by `(width, height,
   fourcc, modifier, stride, offset)` so successive identical
   descriptors reuse the imported `wgpu::Texture`. First
   implementation: no cache, measure, then decide.

3. **Color format and channel order** — detect at startup via a
   round-trip probe. Render a 4-pixel test pattern (red, green, blue,
   white) to the FBO, run the full export→import→sample path, and
   read back the resulting pixels via wgpu. If channels are
   permuted, install a `swizzle = vec3(b, g, r)` in
   `ExternalSpritePipeline`'s fragment shader. Three bytes of cost,
   robust against driver quirks. Probe runs once and caches the
   answer for the session.

## Phase order (implementation)

Ordered so each step is independently verifiable.

- **1a.** Add `framebuffer_gl_texture_id() -> Option<u32>` to
  `WindowRenderingContext`. Returns the GL texture name of the
  current color attachment. No EGL involvement. Test from rumb-shell:
  log the ID per frame to confirm whether it rotates on this
  hardware.
- **1b.** Add `export_framebuffer_dma_buf() -> Option<DmaBufHandle>`.
  Uses `EGL_MESA_image_dma_buf_export` (or NVIDIA equivalent).
  Doesn't change rumb yet; the call is exercised by a unit test or
  scratch binary.
- **1c.** Add fence export — `glFenceSync` + `eglDupNativeFenceFD`.
  Returns an `OwnedFd` packaged into the same `DmaBufHandle`.
- **2a.** Define `ExternalGpuTexture` trait and
  `ExternalTextureDescriptor` enum in gpui (probably in
  `crates/gpui/src/elements/img.rs` next to `ImageSource`, or a new
  `external_texture.rs` if the surface grows). Add the
  `ImageSource::External(Arc<dyn ExternalGpuTexture>)` variant.
- **2b.** Implement dma-buf import in `gpui_wgpu` using
  `wgpu_hal::vulkan::Device::as_hal` →
  `ash::Device::create_image_from_fd` flow with
  `VK_EXT_external_memory_dma_buf` +
  `VK_EXT_image_drm_format_modifier`.
- **2c.** Hook fence wait via `VK_KHR_external_semaphore_fd` into the
  submission boundary.
- **2d.** Format-detection probe at gpui_wgpu init time.
- **3.** Replace rumb-shell's `FrameSlot`/`RenderImage` plumbing with
  an `ExternalGpuTexture` impl backed by tramuntana's
  `DmaBufHandle`.
- **4.** Feature flag `stage2-gpu-share` in rumb-shell; probe at
  startup, fall back to Stage 1 on failure.
- **5.** Benchmark and visual regression test.
