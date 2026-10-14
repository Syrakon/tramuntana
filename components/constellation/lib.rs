/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

#![deny(unsafe_code)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::panic)]

#[macro_use]
mod tracing;

mod broadcastchannel;
mod browsingcontext;
mod constellation;
mod constellation_webview;
mod embedder;
mod event_loop;
mod logging;
mod pipeline;
#[cfg(all(
    feature = "multiprocess",
    target_os = "linux",
    not(target_env = "ohos"),
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod linux_sandbox;
#[cfg(feature = "multiprocess")]
mod process_manager;
#[cfg(feature = "multiprocess")]
mod sandboxing;
mod screenshot_readiness_request;
mod serviceworker;
mod session_history;

pub use crate::constellation::{
    Constellation, InitialConstellationState, TemporaryStorageFactory,
};
pub use crate::embedder::ConstellationToEmbedderMsg;
pub use crate::event_loop::EventLoop;
#[cfg(feature = "multiprocess")]
pub use crate::event_loop::NewScriptEventLoopProcessInfo;
pub use crate::logging::{FromEmbedderLogger, FromScriptLogger};
#[cfg(all(
    feature = "multiprocess",
    target_os = "linux",
    not(target_env = "ohos"),
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
pub use crate::linux_sandbox::{SandboxError, enter_content_process_sandbox};
#[cfg(feature = "multiprocess")]
pub use crate::sandboxing::{UnprivilegedContent, content_process_sandbox_profile};
