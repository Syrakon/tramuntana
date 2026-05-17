# gpui (vendored)

Minimal subset of the [GPUI](https://gpui.rs) UI framework, vendored from
[zed-industries/zed](https://github.com/zed-industries/zed) at commit
`f96aadeee06654404d6569fbd2ee931a4c6ad9e8`.

License: Apache-2.0. Each crate keeps its own `LICENSE-APACHE` file.

## Why this lives here

Tramuntana is the Servo fork that powers rumb. To enable a future
GPU-shared compositor path between Servo's paint output and the UI
layer, GPUI is brought into the engine tree rather than consumed as
an external workspace dependency. `third_party/` reflects that the
code originates outside Servo and is licensed differently from the
rest of `components/` (MPL-2.0).

## Subset

Closure of `gpui` and `gpui_platform` over their internal path
dependencies in Zed's workspace:

| crate            | purpose                                       |
|------------------|-----------------------------------------------|
| `gpui`           | UI framework (element tree, layout, paint)   |
| `gpui_platform`  | Platform abstraction (Wayland, X11, etc.)    |
| `gpui_util`      | Utilities used by gpui                       |
| `collections`    | Shared collection types                      |
| `http_client`    | Trait + helpers for HTTP, used by gpui       |
| `reqwest_client` | `reqwest`-based impl of `http_client`        |
| `scheduler`      | Task scheduling primitives                   |

Examples under `gpui/examples/` were intentionally excluded. The
upstream Zed repo remains the reference for them.

## Updating

Manual cherry-pick. Track upstream fixes by reading
`zed-industries/zed:crates/<name>/` and copying changes by hand. No
automated subtree merge — gpui evolves toward editor needs that don't
apply to a browser engine.
