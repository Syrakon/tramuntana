/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! The content process sandbox on Linux.
//!
//! A content process runs the page, and with it the media stack: GStreamer opens the GPU,
//! talks to the sound server and the compositor, and spawns `gst-plugin-scanner`. A sandbox
//! that empties the filesystem and allows a fixed list of system calls cannot host that, so
//! this one takes the other side of the trade:
//!
//! - Landlock limits the filesystem to what the system, fonts, media and GPU drivers need.
//!   The user's home, and with it the browser profile (cookies, passwords, history), stays
//!   out of reach. Writing is limited to devices, a few caches and thread names.
//! - Landlock also forbids TCP: the network goes through the main process.
//! - Landlock scopes forbid signals and abstract Unix sockets outside the sandbox.
//! - seccomp denies the system calls that widen the kernel attack surface or reach other
//!   processes (ptrace, namespaces, mounts, bpf, io_uring, keyrings, modules).
//!
//! The sandbox fails closed: any error is returned, and the caller must not run content.

use std::collections::BTreeMap;
use std::env;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use embedder_traits::resources;
use landlock::{
    ABI, Access, AccessFs, AccessNet, CompatLevel, Compatible, RestrictSelfAttr, Ruleset,
    RulesetAttr, RulesetCreatedAttr, RulesetError, RulesetStatus, Scope, path_beneath_rules,
};
use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
    SeccompRule, TargetArch,
};

/// The newest Landlock ABI this policy knows about. Older kernels enforce what they can.
const LANDLOCK_ABI: ABI = ABI::V9;

#[cfg(target_arch = "x86_64")]
const SECCOMP_ARCH: TargetArch = TargetArch::x86_64;
#[cfg(target_arch = "aarch64")]
const SECCOMP_ARCH: TargetArch = TargetArch::aarch64;

/// Why the content process could not enter its sandbox.
#[derive(Debug)]
pub enum SandboxError {
    /// Building or enforcing the Landlock ruleset failed.
    Landlock(RulesetError),
    /// The kernel enforced none of the Landlock ruleset.
    LandlockNotEnforced,
    /// Landlock covered only the calling thread, and other threads already exist.
    ThreadsNotCovered(usize),
    /// Counting the threads of this process failed.
    ThreadCount(std::io::Error),
    /// Building or installing a seccomp filter failed.
    Seccomp(String),
}

impl fmt::Display for SandboxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SandboxError::Landlock(error) => write!(f, "Landlock: {error}"),
            SandboxError::LandlockNotEnforced => {
                write!(f, "the kernel does not enforce Landlock")
            },
            SandboxError::ThreadsNotCovered(threads) => write!(
                f,
                "Landlock cannot restrict all threads on this kernel, and {threads} threads exist"
            ),
            SandboxError::ThreadCount(error) => write!(f, "counting threads: {error}"),
            SandboxError::Seccomp(error) => write!(f, "seccomp: {error}"),
        }
    }
}

impl std::error::Error for SandboxError {}

impl From<RulesetError> for SandboxError {
    fn from(error: RulesetError) -> Self {
        SandboxError::Landlock(error)
    }
}

/// Restricts this content process, and all of its threads, for the rest of its life.
pub fn enter_content_process_sandbox() -> Result<(), SandboxError> {
    let paths = SandboxPaths::from_environment();
    paths.create_caches();
    restrict_filesystem_and_network(&paths)?;
    restrict_system_calls()
}

/// The paths a content process may use, worked out from the environment.
struct SandboxPaths {
    /// Read, list and execute.
    read: Vec<PathBuf>,
    /// Read, and connect to the Unix sockets inside.
    sockets: Vec<PathBuf>,
    /// Read, and write to the files that exist inside.
    existing_files: Vec<PathBuf>,
    /// Everything, writing included.
    write: Vec<PathBuf>,
}

impl SandboxPaths {
    fn from_environment() -> Self {
        let home = env::var_os("HOME").map(PathBuf::from);
        let xdg = |variable: &str, fallback: &str| {
            env::var_os(variable)
                .map(PathBuf::from)
                .or_else(|| home.as_ref().map(|home| home.join(fallback)))
        };
        let config = xdg("XDG_CONFIG_HOME", ".config");
        let data = xdg("XDG_DATA_HOME", ".local/share");
        let cache = xdg("XDG_CACHE_HOME", ".cache");
        let runtime = env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);

        let mut read: Vec<PathBuf> = [
            "/usr",
            "/etc",
            "/opt",
            "/bin",
            "/sbin",
            "/lib",
            "/lib64",
            "/var/lib",
            "/var/cache/fontconfig",
            "/sys",
            "/proc",
            "/run/udev",
        ]
        .iter()
        .map(PathBuf::from)
        .collect();
        let under = |base: &Option<PathBuf>, names: &[&str]| -> Vec<PathBuf> {
            base.iter()
                .flat_map(|base| names.iter().map(move |name| base.join(name)))
                .collect()
        };
        read.extend(under(&config, &["fontconfig", "pulse", "vulkan"]));
        read.extend(under(
            &data,
            &["fonts", "vulkan", "gstreamer-1.0", "flatpak/exports"],
        ));
        read.extend(under(&home, &[".fonts", ".pulse", ".pulse-cookie"]));
        read.extend(env::current_exe().ok());
        read.extend(resources::sandbox_access_files());
        read.extend(resources::sandbox_access_files_dirs());

        let mut sockets = vec![
            PathBuf::from("/run/dbus"),
            PathBuf::from("/run/systemd/journal"),
        ];
        sockets.extend(runtime);

        // Threads name themselves through `/proc/self/task/<tid>/comm`. That
        // reaches this process only, which already runs its own code.
        let existing_files = vec![PathBuf::from("/proc/self/task")];

        let mut write = vec![PathBuf::from("/dev")];
        write.extend(under(
            &cache,
            &[
                "fontconfig",
                "gstreamer-1.0",
                "nvidia",
                "mesa_shader_cache",
                "mesa_shader_cache_db",
                "radv_builtin_shaders",
            ],
        ));
        write.extend(under(&home, &[".nv"]));

        SandboxPaths {
            read,
            sockets,
            existing_files,
            write,
        }
    }

    /// Landlock grants access to paths that exist, so the caches the media stack fills are
    /// made beforehand.
    fn create_caches(&self) {
        for path in &self.write {
            if path.starts_with("/dev") {
                continue;
            }
            if let Err(error) = fs::create_dir_all(path) {
                log::warn!(
                    "Content process sandbox: {} stays unwritable: {error}",
                    path.display()
                );
            }
        }
    }
}

fn restrict_filesystem_and_network(paths: &SandboxPaths) -> Result<(), SandboxError> {
    let read = AccessFs::from_read(LANDLOCK_ABI);
    let sockets = read | AccessFs::ResolveUnix;
    let existing_files = read | AccessFs::WriteFile | AccessFs::Truncate;
    let write = AccessFs::from_all(LANDLOCK_ABI);

    let status = Ruleset::default()
        .set_compatibility(CompatLevel::BestEffort)
        .handle_access(AccessFs::from_all(LANDLOCK_ABI))?
        // No rule allows a port: the content process neither binds nor connects over TCP.
        .handle_access(AccessNet::from_all(LANDLOCK_ABI))?
        .scope(Scope::AbstractUnixSocket | Scope::Signal)?
        .create()?
        .add_rules(path_beneath_rules(&paths.read, read))?
        .add_rules(path_beneath_rules(&paths.sockets, sockets))?
        .add_rules(path_beneath_rules(&paths.existing_files, existing_files))?
        .add_rules(path_beneath_rules(&paths.write, write))?
        .all_threads(true)?
        .restrict_self()?;

    if status.ruleset == RulesetStatus::NotEnforced {
        return Err(SandboxError::LandlockNotEnforced);
    }
    if !status.all_threads {
        let threads = thread_count()?;
        if threads > 1 {
            return Err(SandboxError::ThreadsNotCovered(threads));
        }
    }
    log::info!(
        "Content process sandbox: Landlock {:?}, all threads: {}",
        status.ruleset,
        status.all_threads
    );
    Ok(())
}

fn thread_count() -> Result<usize, SandboxError> {
    fs::read_dir(Path::new("/proc/self/task"))
        .map(|tasks| tasks.count())
        .map_err(SandboxError::ThreadCount)
}

/// System calls a page never needs, whose kernel code is a common route out of a sandbox or
/// whose purpose is to reach other processes.
const DENIED_SYSCALLS: &[libc::c_long] = &[
    libc::SYS_ptrace,
    libc::SYS_process_vm_readv,
    libc::SYS_process_vm_writev,
    libc::SYS_kcmp,
    libc::SYS_pidfd_getfd,
    libc::SYS_unshare,
    libc::SYS_setns,
    libc::SYS_mount,
    libc::SYS_umount2,
    libc::SYS_pivot_root,
    libc::SYS_chroot,
    libc::SYS_mount_setattr,
    libc::SYS_move_mount,
    libc::SYS_open_tree,
    libc::SYS_fsopen,
    libc::SYS_fsconfig,
    libc::SYS_fsmount,
    libc::SYS_fspick,
    libc::SYS_open_by_handle_at,
    libc::SYS_name_to_handle_at,
    libc::SYS_bpf,
    libc::SYS_perf_event_open,
    libc::SYS_userfaultfd,
    libc::SYS_io_uring_setup,
    libc::SYS_io_uring_enter,
    libc::SYS_io_uring_register,
    libc::SYS_keyctl,
    libc::SYS_add_key,
    libc::SYS_request_key,
    libc::SYS_fanotify_init,
    libc::SYS_init_module,
    libc::SYS_finit_module,
    libc::SYS_delete_module,
    libc::SYS_kexec_load,
    libc::SYS_kexec_file_load,
    libc::SYS_reboot,
    libc::SYS_swapon,
    libc::SYS_swapoff,
    libc::SYS_acct,
    libc::SYS_quotactl,
    libc::SYS_settimeofday,
    libc::SYS_clock_settime,
    libc::SYS_clock_adjtime,
    libc::SYS_adjtimex,
    #[cfg(target_arch = "x86_64")]
    libc::SYS_iopl,
    #[cfg(target_arch = "x86_64")]
    libc::SYS_ioperm,
    #[cfg(target_arch = "x86_64")]
    libc::SYS_modify_ldt,
    #[cfg(target_arch = "x86_64")]
    libc::SYS_uselib,
];

/// `clone` flags that create namespaces. Creating a user namespace in particular opens
/// kernel code that is otherwise reserved to root.
const NAMESPACE_FLAGS: &[libc::c_int] = &[
    libc::CLONE_NEWUSER,
    libc::CLONE_NEWNS,
    libc::CLONE_NEWNET,
    libc::CLONE_NEWPID,
    libc::CLONE_NEWIPC,
    libc::CLONE_NEWUTS,
    libc::CLONE_NEWCGROUP,
];

fn restrict_system_calls() -> Result<(), SandboxError> {
    let seccomp = |error: &dyn fmt::Display| SandboxError::Seccomp(error.to_string());

    let mut denied: BTreeMap<i64, Vec<SeccompRule>> = DENIED_SYSCALLS
        .iter()
        .map(|syscall| (i64::from(*syscall), Vec::new()))
        .collect();
    // On both architectures the flags are the first argument of `clone`.
    let namespace_rules = NAMESPACE_FLAGS
        .iter()
        .map(|flag| {
            let flag = u64::from(flag.unsigned_abs());
            SeccompCondition::new(0, SeccompCmpArgLen::Qword, SeccompCmpOp::MaskedEq(flag), flag)
                .and_then(|condition| SeccompRule::new(vec![condition]))
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| seccomp(&error))?;
    denied.insert(i64::from(libc::SYS_clone), namespace_rules);
    let denied = SeccompFilter::new(
        denied,
        SeccompAction::Allow,
        SeccompAction::Errno(libc::EPERM.unsigned_abs()),
        SECCOMP_ARCH,
    )
    .map_err(|error| seccomp(&error))?;

    // `clone3` takes its flags in memory, out of reach of a seccomp filter. Answering that
    // it does not exist makes the C library fall back to `clone`, whose flags are checked.
    let clone3 = SeccompFilter::new(
        BTreeMap::from([(i64::from(libc::SYS_clone3), Vec::new())]),
        SeccompAction::Allow,
        SeccompAction::Errno(libc::ENOSYS.unsigned_abs()),
        SECCOMP_ARCH,
    )
    .map_err(|error| seccomp(&error))?;

    for filter in [denied, clone3] {
        let program = BpfProgram::try_from(filter).map_err(|error| seccomp(&error))?;
        seccompiler::apply_filter_all_threads(&program).map_err(|error| seccomp(&error))?;
    }
    log::info!("Content process sandbox: seccomp filters installed");
    Ok(())
}
