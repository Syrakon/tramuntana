/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::ffi::OsStr;
use std::{env, process};

#[cfg(target_os = "macos")]
use gaol::profile::{Operation, PathPattern, Profile};
use serde::{Deserialize, Serialize};
use servo_config::opts::Opts;
use servo_config::prefs::Preferences;

use crate::event_loop::NewScriptEventLoopProcessInfo;
use crate::serviceworker::ServiceWorkerUnprivilegedContent;

#[derive(Deserialize, Serialize)]
#[expect(clippy::large_enum_variant)]
pub enum UnprivilegedContent {
    ScriptEventLoop(NewScriptEventLoopProcessInfo),
    ServiceWorker(ServiceWorkerUnprivilegedContent),
}

impl UnprivilegedContent {
    pub fn opts(&self) -> Opts {
        match self {
            UnprivilegedContent::ScriptEventLoop(content) => content.opts.clone(),
            UnprivilegedContent::ServiceWorker(content) => content.opts.clone(),
        }
    }

    pub fn prefs(&self) -> &Preferences {
        match self {
            UnprivilegedContent::ScriptEventLoop(content) => &content.prefs,
            UnprivilegedContent::ServiceWorker(content) => &content.prefs,
        }
    }
}

/// Our content process sandbox profile on Mac. As restrictive as possible.
#[cfg(target_os = "macos")]
pub fn content_process_sandbox_profile() -> Profile {
    use std::path::PathBuf;

    use embedder_traits::resources;
    use gaol::platform;

    let mut operations = vec![
        Operation::FileReadAll(PathPattern::Literal(PathBuf::from("/dev/urandom"))),
        Operation::FileReadAll(PathPattern::Subpath(PathBuf::from("/Library/Fonts"))),
        Operation::FileReadAll(PathPattern::Subpath(PathBuf::from("/System/Library/Fonts"))),
        Operation::FileReadAll(PathPattern::Subpath(PathBuf::from(
            "/System/Library/Frameworks/ApplicationServices.framework",
        ))),
        Operation::FileReadAll(PathPattern::Subpath(PathBuf::from(
            "/System/Library/Frameworks/CoreGraphics.framework",
        ))),
        Operation::FileReadMetadata(PathPattern::Literal(PathBuf::from("/"))),
        Operation::FileReadMetadata(PathPattern::Literal(PathBuf::from("/Library"))),
        Operation::FileReadMetadata(PathPattern::Literal(PathBuf::from("/System"))),
        Operation::FileReadMetadata(PathPattern::Literal(PathBuf::from("/etc"))),
        Operation::SystemInfoRead,
        Operation::PlatformSpecific(platform::macos::Operation::MachLookup(
            b"com.apple.FontServer".to_vec(),
        )),
    ];

    operations.extend(
        resources::sandbox_access_files()
            .into_iter()
            .map(|p| Operation::FileReadAll(PathPattern::Literal(p))),
    );
    operations.extend(
        resources::sandbox_access_files_dirs()
            .into_iter()
            .map(|p| Operation::FileReadAll(PathPattern::Subpath(p))),
    );

    Profile::new(operations).expect("Failed to create sandbox profile!")
}

/// Other platforms have no gaol profile. On Linux the content process sandboxes itself, see
/// `linux_sandbox`.
#[cfg(not(target_os = "macos"))]
pub fn content_process_sandbox_profile() {
    log::error!("Sandboxed multiprocess is not supported on this platform.");
    process::exit(1);
}

// On Linux the child is spawned as is, and enters its sandbox itself once it knows its
// options: see `linux_sandbox`.
#[cfg(not(any(
    target_os = "ios",
    all(target_os = "macos", not(target_arch = "aarch64"))
)))]
pub fn spawn_multiprocess(
    content: UnprivilegedContent,
) -> Result<crate::process_manager::Process, ipc_channel::IpcError> {
    use ipc_channel::ipc::{IpcOneShotServer, IpcSender};
    // Note that this function can panic, due to process creation,
    // avoiding this panic would require a mechanism for dealing
    // with low-resource scenarios.
    let (server, token) = IpcOneShotServer::<IpcSender<UnprivilegedContent>>::new()
        .expect("Failed to create IPC one-shot server.");

    let path_to_self = env::current_exe().expect("Failed to get current executor.");
    let mut child_process = process::Command::new(path_to_self);
    setup_common(&mut child_process, token);

    #[allow(clippy::zombie_processes)]
    let child = child_process
        .spawn()
        .expect("Failed to start unsandboxed child process!");

    let (_receiver, sender) = server.accept().expect("Server failed to accept.");
    sender.send(content)?;

    Ok(crate::process_manager::Process::Unsandboxed(child))
}

#[cfg(all(target_os = "macos", not(target_arch = "aarch64")))]
pub fn spawn_multiprocess(
    content: UnprivilegedContent,
) -> Result<crate::process_manager::Process, ipc_channel::IpcError> {
    use gaol::sandbox::{self, Sandbox, SandboxMethods};
    use ipc_channel::ipc::{IpcOneShotServer, IpcSender};

    // TODO: Move this impl out of the function. It is only currently here to avoid
    // duplicating the feature flagging.
    #[allow(non_local_definitions)]
    impl CommandMethods for gaol::sandbox::Command {
        fn arg<T>(&mut self, arg: T)
        where
            T: AsRef<OsStr>,
        {
            self.arg(arg);
        }

        fn env<T, U>(&mut self, key: T, val: U)
        where
            T: AsRef<OsStr>,
            U: AsRef<OsStr>,
        {
            self.env(key, val);
        }
    }

    // Note that this function can panic, due to process creation,
    // avoiding this panic would require a mechanism for dealing
    // with low-resource scenarios.
    let (server, token) = IpcOneShotServer::<IpcSender<UnprivilegedContent>>::new()
        .expect("Failed to create IPC one-shot server.");

    // If there is a sandbox, use the `gaol` API to create the child process.
    let process = if content.opts().sandbox {
        let mut command = sandbox::Command::me().expect("Failed to get current sandbox.");
        setup_common(&mut command, token);

        let profile = content_process_sandbox_profile();
        crate::process_manager::Process::Sandboxed(
            Sandbox::new(profile)
                .start(&mut command)
                .expect("Failed to start sandboxed child process!")
                .pid as u32,
        )
    } else {
        let path_to_self = env::current_exe().expect("Failed to get current executor.");
        let mut child_process = process::Command::new(path_to_self);
        setup_common(&mut child_process, token);

        crate::process_manager::Process::Unsandboxed(
            child_process
                .spawn()
                .expect("Failed to start unsandboxed child process!"),
        )
    };

    let (_receiver, sender) = server.accept().expect("Server failed to accept.");
    sender.send(content)?;

    Ok(process)
}

#[cfg(target_os = "ios")]
pub fn spawn_multiprocess(_content: UnprivilegedContent) -> Result<Process, Error> {
    log::error!("Multiprocess is not supported on iOS.");
    process::exit(1);
}

fn setup_common<C: CommandMethods>(command: &mut C, token: String) {
    C::arg(command, "--content-process");
    C::arg(command, token);

    if let Ok(value) = env::var("RUST_BACKTRACE") {
        C::env(command, "RUST_BACKTRACE", value);
    }

    if let Ok(value) = env::var("RUST_LOG") {
        C::env(command, "RUST_LOG", value);
    }

    C::env(command, "GLIBC_TUNABLES", content_process_glibc_tunables());
}

/// glibc gives each thread that contends on malloc an arena of its own, up to
/// eight per core, and memory freed into one arena is never handed to another.
/// A content process runs some forty threads, so a Twitch page held 195 MB of
/// system heap for 121 MB allocated. Two arenas brought that child from 600 MB
/// to 560 MB of PSS at the same CPU; one arena saved nothing more. glibc reads
/// tunables only at startup, so they go in the child's environment. A value the
/// user set is kept, and wins if it already names `arena_max`.
fn content_process_glibc_tunables() -> String {
    const ARENA_MAX: &str = "glibc.malloc.arena_max=2";
    match env::var("GLIBC_TUNABLES") {
        Ok(user) if user.contains("glibc.malloc.arena_max") => user,
        Ok(user) if !user.is_empty() => format!("{user}:{ARENA_MAX}"),
        _ => ARENA_MAX.to_owned(),
    }
}

/// A trait to unify commands launched as multiprocess with or without a sandbox.
trait CommandMethods {
    /// A command line argument.
    fn arg<T>(&mut self, arg: T)
    where
        T: AsRef<OsStr>;

    /// An environment variable.
    fn env<T, U>(&mut self, key: T, val: U)
    where
        T: AsRef<OsStr>,
        U: AsRef<OsStr>;
}

impl CommandMethods for process::Command {
    fn arg<T>(&mut self, arg: T)
    where
        T: AsRef<OsStr>,
    {
        self.arg(arg);
    }

    fn env<T, U>(&mut self, key: T, val: U)
    where
        T: AsRef<OsStr>,
        U: AsRef<OsStr>,
    {
        self.env(key, val);
    }
}
