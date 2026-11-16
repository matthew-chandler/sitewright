//! The jail a backend worker puts itself in before it touches any site code.
//!
//! A worker needs very little from the kernel: read its stdin, write its stdout, manage
//! memory, read the clock and get random bytes. Everything else a backend can do (SQL,
//! secrets, outbound requests) is a message to the daemon, which decides. So the worker
//! installs a seccomp filter that allows only those system calls and makes every other
//! one fail with `EPERM`: no opening files, no sockets, no new processes. The filter
//! cannot be removed and is inherited by anything the process could become.
//!
//! This holds even if the JavaScript engine has a memory-safety bug: code that escapes
//! the engine still runs inside this process, with no way to reach the daemon's keys or
//! other sites' data except by asking the daemon.
//!
//! seccomp needs no privileges and works on old kernels (the dev VPS runs 5.4, which
//! has no Landlock and does not delegate cgroups to user services).

#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    #[error("could not set a resource limit: {0}")]
    Limit(String),
    #[error("could not install the system-call filter: {0}")]
    Filter(String),
    #[error("backends need Linux on x86_64 or aarch64")]
    Unsupported,
}

/// Address-space ceiling for the whole worker. The JavaScript heap has its own, lower
/// limit; this one is the backstop for memory the engine does not account for.
#[cfg(target_os = "linux")]
fn address_space_limit(js_memory_bytes: u64) -> u64 {
    js_memory_bytes.saturating_mul(2).max(256 * 1024 * 1024)
}

/// Enter the jail. Call once in the worker process, before any site code is loaded.
///
/// The resource limits are set first, because afterwards the process may no longer
/// change limits at all: that call can also target other processes of the same user.
/// For the same reason nothing that sends signals is allowed.
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
pub fn enter(js_memory_bytes: u64) -> Result<(), SandboxError> {
    use rustix::process::{Resource, Rlimit, setrlimit};

    let limit = address_space_limit(js_memory_bytes);
    setrlimit(
        Resource::As,
        Rlimit {
            current: Some(limit),
            maximum: Some(limit),
        },
    )
    .map_err(|e| SandboxError::Limit(e.to_string()))?;

    // No core dumps: they would write the backend's memory to disk.
    setrlimit(
        Resource::Core,
        Rlimit {
            current: Some(0),
            maximum: Some(0),
        },
    )
    .map_err(|e| SandboxError::Limit(e.to_string()))?;

    install_filter()
}

/// Install the system-call filter on the calling thread.
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
fn install_filter() -> Result<(), SandboxError> {
    use std::collections::BTreeMap;

    use seccompiler::{BpfProgram, SeccompAction, SeccompFilter, TargetArch, apply_filter};

    let mut allowed = vec![
        // The message channel.
        libc::SYS_read,
        libc::SYS_write,
        libc::SYS_readv,
        libc::SYS_writev,
        libc::SYS_close,
        // Memory.
        libc::SYS_mmap,
        libc::SYS_munmap,
        libc::SYS_mremap,
        libc::SYS_mprotect,
        libc::SYS_madvise,
        libc::SYS_brk,
        // Clock and randomness.
        libc::SYS_clock_gettime,
        libc::SYS_clock_nanosleep,
        libc::SYS_nanosleep,
        libc::SYS_getrandom,
        // What the Rust runtime and libc use to run, panic and exit.
        libc::SYS_futex,
        libc::SYS_rt_sigaction,
        libc::SYS_rt_sigprocmask,
        libc::SYS_rt_sigreturn,
        libc::SYS_sigaltstack,
        libc::SYS_getpid,
        libc::SYS_gettid,
        libc::SYS_set_robust_list,
        libc::SYS_rseq,
        libc::SYS_exit,
        libc::SYS_exit_group,
    ];
    #[cfg(target_arch = "x86_64")]
    allowed.extend([libc::SYS_gettimeofday, libc::SYS_time]);

    let rules: BTreeMap<i64, Vec<seccompiler::SeccompRule>> =
        allowed.into_iter().map(|nr| (nr, Vec::new())).collect();
    #[cfg(target_arch = "x86_64")]
    let arch = TargetArch::x86_64;
    #[cfg(target_arch = "aarch64")]
    let arch = TargetArch::aarch64;
    let filter = SeccompFilter::new(
        rules,
        // Anything not listed fails with "operation not permitted".
        SeccompAction::Errno(libc::EPERM as u32),
        SeccompAction::Allow,
        arch,
    )
    .map_err(|e| SandboxError::Filter(e.to_string()))?;
    let program: BpfProgram = filter
        .try_into()
        .map_err(|e: seccompiler::BackendError| SandboxError::Filter(e.to_string()))?;
    apply_filter(&program).map_err(|e| SandboxError::Filter(e.to_string()))
}

#[cfg(not(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
)))]
pub fn enter(_js_memory_bytes: u64) -> Result<(), SandboxError> {
    Err(SandboxError::Unsupported)
}

#[cfg(all(
    test,
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod tests {
    #![allow(clippy::unwrap_used)]

    /// The filter applies to the thread that installs it, so the test jails a thread of
    /// its own and reports what that thread could and could not do.
    #[test]
    fn a_jailed_thread_cannot_open_files_sockets_or_processes() {
        let report = std::thread::spawn(|| {
            // Things that must keep working: memory, the clock, randomness.
            super::install_filter().unwrap();
            let grown = vec![7u8; 8 * 1024 * 1024];
            let clock = std::time::Instant::now().elapsed().as_nanos() < u128::MAX;
            let mut random = [0u8; 16];
            let random_ok = getrandom::fill(&mut random).is_ok();

            let file = std::fs::File::open("/etc/hostname").map(|_| ());
            let write = std::fs::write("/tmp/sw-backend-jail-test", b"x");
            let listen = std::net::TcpListener::bind("127.0.0.1:0").map(|_| ());
            let connect = std::net::UdpSocket::bind("0.0.0.0:0").map(|_| ());
            let spawn = std::process::Command::new("/bin/true").status().map(|_| ());
            let thread = std::thread::Builder::new().spawn(|| ()).map(|_| ());
            (
                grown.len(),
                clock && random_ok,
                [file, write, listen, connect, spawn, thread].map(|r| r.map_err(|e| e.kind())),
            )
        })
        .join()
        .unwrap();
        assert_eq!(report.0, 8 * 1024 * 1024);
        assert!(report.1);
        for (what, result) in ["open", "write", "listen", "udp", "spawn", "thread"]
            .iter()
            .zip(report.2)
        {
            assert!(result.is_err(), "{what} was allowed inside the jail");
        }
    }
}
