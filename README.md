# tramuntana

tramuntana is a fork of [Servo](https://github.com/servo/servo), the web
browser engine written in [Rust](https://github.com/rust-lang/rust). It is the
engine of rumb, a browser whose interface is drawn with
[gpui](https://github.com/zed-industries/zed).

The `main` branch is always Servo upstream with the fork's patches on top.
[SYNCING.md](SYNCING.md) describes how upstream is merged in, by hand and by
the weekly CI job.

## What the fork adds

The patches exist because rumb needs them.

- **Embedding in gpui.** Servo renders offscreen and hands each frame to gpui
  as a dma-buf, with a fence, so the page is composited without a copy. A
  subset of gpui is vendored in `third_party/gpui`, with a scene primitive and
  a wgpu pipeline for these external textures.
- **Embedder API.** A request filter consulted before every fetch, user
  stylesheets that can be swapped per WebView, user scripts per page, and
  `registered_domain_name` re-exported. rumb builds its content blocker on
  these.
- **Accessibility.** gpui elements can carry accessibility metadata, exposed
  over AT-SPI through AccessKit on Linux.
- **Media.** Media Source Extensions, with source buffers played by the
  GStreamer backend.
- **Web platform.** `document.all`, the `SVGAElement` interface, and
  `/favicon.ico` fetched when a page declares no icon.
- **Fixes** in layout, SVG, WebGL, paint, devtools and the network stack, among
  them cookies saved while running and not only at a clean exit.

The history of `main` after the last upstream merge lists every patch.

## Building

tramuntana depends on a fork of Stylo, expected as a sibling directory:

```sh
git clone https://gitlab.com/syrakon/stylo.git
git clone https://gitlab.com/syrakon/tramuntana.git
```

After that it builds like Servo; the instructions below are upstream's. The
bridge to gpui hands frames over as dma-bufs, so that part is Linux only.

## Servo

Servo welcomes contribution from everyone. Check out:

- The [Servo Book](https://book.servo.org) for documentation
- [servo.org](https://servo.org/) for news and guides

Problems that also happen in upstream Servo belong in the
[Servo issue tracker](https://github.com/servo/servo/issues). Problems with
the patches listed above belong in this repository.

## Getting started

For more detailed build instructions, see the Servo Book under [Getting the Code] and [Building Servo].

[Getting the Code]: https://book.servo.org/building/getting-the-code.html
[Building Servo]: https://book.servo.org/building/building.html

### macOS

- Download and install [Xcode](https://developer.apple.com/xcode/) and [`brew`](https://brew.sh/).
- Install `uv`: `curl -LsSf https://astral.sh/uv/install.sh | sh` 
- Install `rustup`: `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`
- Restart your shell to make sure `cargo` is available
- Install the other dependencies: `./mach bootstrap`
- Build servoshell: `./mach build`

### Linux

- Install `curl`:
  - Arch: `sudo pacman -S --needed curl`
  - Debian, Ubuntu: `sudo apt install curl`
  - Fedora: `sudo dnf install curl`
  - Gentoo: `sudo emerge net-misc/curl`
- Install `uv`: `curl -LsSf https://astral.sh/uv/install.sh | sh` 
- Install `rustup`: `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`
- Restart your shell to make sure `cargo` is available
- Install the other dependencies: `./mach bootstrap`
- Build servoshell: `./mach build`

### Windows

- Download [`uv`](https://docs.astral.sh/uv/getting-started/installation/#standalone-installer), and [`rustup`](https://win.rustup.rs/)
  - Be sure to select *Quick install via the Visual Studio Community installer*
- Ensure that [`winget`](https://learn.microsoft.com/en-us/windows/package-manager/winget/) is available. It should be preinstalled on Windows 10 1809+ and Windows 11, otherwise can be [`manually installed`](https://github.com/microsoft/winget-cli#installing-the-client).
- In the Visual Studio Installer, ensure the following components are installed:
  - **Windows 10/11 SDK (anything >= 10.0.19041.0)** (`Microsoft.VisualStudio.Component.Windows{10, 11}SDK.{>=19041}`)
  - **MSVC v143 - VS 2022 C++ x64/x86 build tools (Latest)** (`Microsoft.VisualStudio.Component.VC.Tools.x86.x64`)
  - **C++ ATL for latest v143 build tools (x86 & x64)** (`Microsoft.VisualStudio.Component.VC.ATL`)
- Restart your shell to make sure `cargo` is available
- Install the other dependencies: `.\mach bootstrap`
- Build servoshell: `.\mach build`

### Android

- Ensure that the following environment variables are set:
  - `ANDROID_SDK_ROOT`
  - `ANDROID_NDK_ROOT`: `$ANDROID_SDK_ROOT/ndk/28.2.13676358/`
 `ANDROID_SDK_ROOT` can be any directory (such as `~/android-sdk`).
  All of the Android build dependencies will be installed there.
- Install the latest version of the [Android command-line
  tools](https://developer.android.com/studio#command-tools) to
  `$ANDROID_SDK_ROOT/cmdline-tools/latest`.
- Run the following command to install the necessary components:
  ```shell
  sudo $ANDROID_SDK_ROOT/cmdline-tools/latest/bin/sdkmanager --install \
   "build-tools;36.0.0" \
   "emulator" \
   "ndk;29.0.14206865" \
   "platform-tools" \
   "platforms;android-37" \
   "system-images;android-37;google_apis;x86_64"
  ```
- Follow the instructions above for the platform you are building on

### OpenHarmony

- Follow the instructions above for the platform you are building on to prepare the environment.
- Depending on the target distribution (e.g. `HarmonyOS NEXT` vs pure `OpenHarmony`) the build configuration will differ slightly.
- Ensure that the following environment variables are set
  - `DEVECO_SDK_HOME` (Required when targeting `HarmonyOS NEXT`)
  - `OHOS_BASE_SDK_HOME` (Required when targeting `OpenHarmony`)
  - `OHOS_SDK_NATIVE` (e.g. `${DEVECO_SDK_HOME}/default/openharmony/native` or `${OHOS_BASE_SDK_HOME}/${API_VERSION}/native`)
  - `SERVO_OHOS_SIGNING_CONFIG`: Path to json file containing a valid signing configuration for the demo app.
- Review the detailed instructions at [Building for OpenHarmony].
- The target distribution can be modified by passing `--flavor=<default|harmonyos>` to `mach <build|package|install>`.
