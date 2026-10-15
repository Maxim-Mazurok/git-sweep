//! Conservative Git maintenance and storage accounting.

use crate::discovery::Repository;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const OUTPUT_LIMIT: usize = 16 * 1024;
const GC_TIMEOUT: Duration = Duration::from_secs(20 * 60);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GcResult {
    pub repo: PathBuf,
    pub before_bytes: u64,
    pub after_bytes: u64,
    pub before_allocated_bytes: u64,
    pub after_allocated_bytes: u64,
    /// False when cancellation interrupted accounting. Byte counters then show
    /// zero change, rather than incorrectly reporting partial totals as savings.
    pub measurements_complete: bool,
    pub status: String,
    pub detail: String,
}

/// A known alternate-object borrower makes pruning this source unsafe: the
/// borrower can still need objects no longer reachable from the source's refs.
pub fn skipped_alternate_source(repo: &Repository, borrower_count: usize) -> GcResult {
    GcResult {
        repo: repo.git_dir.clone(),
        before_bytes: 0,
        after_bytes: 0,
        before_allocated_bytes: 0,
        after_allocated_bytes: 0,
        measurements_complete: false,
        status: "skipped_alternate_source".into(),
        detail: format!(
            "GC deferred: {borrower_count} discovered repositories borrow objects from this store."
        ),
    }
}

#[derive(Default, Debug)]
struct Storage {
    logical: u64,
    allocated: u64,
    lock: Option<PathBuf>,
    interrupted: bool,
}

/// Account for this common Git directory only. Submodule stores under `modules`
/// are separate repositories, and are discovered and maintained independently.
/// Symlink targets and alternates are never followed. Hard links count once.
fn storage(root: &Path, should_stop: &dyn Fn() -> bool) -> Result<Storage> {
    let mut stats = Storage::default();
    let mut seen = HashSet::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        if should_stop() {
            stats.interrupted = true;
            break;
        }
        let metadata = match fs::symlink_metadata(&path) {
            Ok(value) => value,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).with_context(|| format!("inspect {}", path.display())),
        };
        if !metadata.is_dir()
            && metadata.nlink() > 1
            && !seen.insert((metadata.dev(), metadata.ino()))
        {
            continue;
        }
        stats.logical = stats.logical.saturating_add(metadata.len());
        stats.allocated = stats
            .allocated
            .saturating_add(metadata.blocks().saturating_mul(512));
        if path.extension() == Some(OsStr::new("lock")) && stats.lock.is_none() {
            stats.lock = Some(path.clone());
        }
        if metadata.is_dir() {
            for entry in fs::read_dir(&path).with_context(|| format!("read {}", path.display()))? {
                let entry = entry.with_context(|| format!("read entry in {}", path.display()))?;
                if path == root && entry.file_name() == "modules" {
                    continue;
                }
                pending.push(entry.path());
            }
        }
    }
    Ok(stats)
}

fn live_gc_pid(root: &Path) -> bool {
    let path = root.join("gc.pid");
    let text = match fs::read_to_string(&path) {
        Ok(value) => value,
        Err(error) => return error.kind() != std::io::ErrorKind::NotFound,
    };
    let mut fields = text.split_whitespace();
    let pid = match fields.next().and_then(|value| value.parse::<i32>().ok()) {
        Some(value) if value > 0 => value,
        _ => return true,
    };
    let hostname = fields.next().unwrap_or("");
    let mut host = [0_u8; 256];
    // SAFETY: host is a valid writable buffer of the provided length.
    if unsafe { libc::gethostname(host.as_mut_ptr().cast(), host.len()) } != 0 {
        return true;
    }
    let end = host
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(host.len());
    if hostname.as_bytes() != &host[..end] {
        return true;
    }
    // SAFETY: signal 0 checks process existence without sending a signal.
    unsafe {
        libc::kill(pid, 0) == 0
            || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    }
}

/// Execute normal Git GC with its configured expiration policies. Never force
/// locks, run aggressive GC, or request immediate pruning.
pub fn collect(
    repo: &Repository,
    git: &Path,
    dry_run: bool,
    should_stop: &dyn Fn() -> bool,
) -> Result<GcResult> {
    let root_metadata = match fs::symlink_metadata(&repo.git_dir) {
        Ok(value) => Some(value),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error).with_context(|| format!("inspect {}", repo.git_dir.display()));
        }
    };
    if root_metadata.is_none() {
        return Ok(GcResult {
            repo: repo.git_dir.clone(),
            before_bytes: 0,
            after_bytes: 0,
            before_allocated_bytes: 0,
            after_allocated_bytes: 0,
            measurements_complete: false,
            status: "missing".into(),
            detail: "Cached Git directory no longer exists.".into(),
        });
    }
    if !root_metadata
        .as_ref()
        .is_some_and(|metadata| metadata.is_dir())
        || fs::canonicalize(&repo.git_dir)? != repo.git_dir
    {
        return Ok(GcResult {
            repo: repo.git_dir.clone(), before_bytes: 0, after_bytes: 0,
            before_allocated_bytes: 0, after_allocated_bytes: 0,
            measurements_complete: false,
            status: "skipped_path".into(),
            detail: "Cached Git directory changed into a symlink or no longer resolves to its original canonical path; rediscover first.".into(),
        });
    }
    let before = storage(&repo.git_dir, should_stop)?;
    let mut result = GcResult {
        repo: repo.git_dir.clone(),
        before_bytes: before.logical,
        after_bytes: before.logical,
        before_allocated_bytes: before.allocated,
        after_allocated_bytes: before.allocated,
        measurements_complete: !before.interrupted,
        status: String::new(),
        detail: String::new(),
    };
    if before.interrupted {
        result.before_bytes = 0;
        result.after_bytes = 0;
        result.before_allocated_bytes = 0;
        result.after_allocated_bytes = 0;
        result.status = "cancelled".into();
        result.detail = "Storage accounting interrupted before GC; sizes unavailable and no maintenance started.".into();
        return Ok(result);
    }
    if dry_run {
        let outcome = run_git(
            git,
            &repo.git_dir,
            &["count-objects", "-v"],
            Duration::from_secs(30),
            should_stop,
        )?;
        result.status = if outcome.cancelled {
            "cancelled"
        } else if outcome.success() {
            "dry-run"
        } else {
            "error"
        }
        .into();
        result.detail = format!(
            "Read-only inventory; savings cannot be predicted without repacking. {}",
            outcome.description()
        );
        return Ok(result);
    }
    if should_stop() {
        result.status = "cancelled".into();
        result.detail = "Maintenance stopped before starting Git.".into();
        return Ok(result);
    }
    if let Some(lock) = before.lock {
        result.status = "locked".into();
        result.detail = format!("Existing Git lock: {}", lock.display());
        return Ok(result);
    }
    if live_gc_pid(&repo.git_dir) {
        result.status = "locked".into();
        result.detail =
            "Existing gc.pid belongs to a live process, another host, or cannot be verified."
                .into();
        return Ok(result);
    }
    // These commands are not conventional hooks and hooksPath does not disable
    // them. In particular, ignoring recentObjectsHook could delete objects the
    // user's retention policy protects. Skip instead of overriding that policy.
    let hooks = run_git(
        git,
        &repo.git_dir,
        &[
            "config",
            "--get-regexp",
            "^(gc\\.recentobjectshook|core\\.alternaterefscommand)$",
        ],
        Duration::from_secs(15),
        should_stop,
    )?;
    if hooks.cancelled
        || hooks.timed_out
        || (!hooks.status.success() && hooks.status.code() != Some(1))
    {
        result.status = if hooks.cancelled {
            "cancelled"
        } else {
            "error"
        }
        .into();
        result.detail = format!("Cannot safely inspect Git hooks: {}", hooks.description());
        return Ok(result);
    }
    if !hooks.stdout.trim().is_empty() {
        result.status = "skipped_hook".into();
        result.detail = "Custom gc.recentObjectsHook or core.alternateRefsCommand configured; preserve its object retention policy without executing custom commands.".into();
        return Ok(result);
    }
    let outcome = run_git(
        git,
        &repo.git_dir,
        &["gc", "--no-detach"],
        GC_TIMEOUT,
        should_stop,
    )?;
    let after = storage(&repo.git_dir, should_stop)?;
    if after.interrupted {
        result.measurements_complete = false;
        result.status = "cancelled".into();
        result.detail = format!(
            "{}; post-GC storage accounting interrupted. Sizes remain equal to pre-GC counters because actual savings are unavailable.",
            outcome.description()
        );
        return Ok(result);
    }
    result.after_bytes = after.logical;
    result.after_allocated_bytes = after.allocated;
    result.status = if outcome.cancelled {
        "cancelled"
    } else if outcome.timed_out {
        "timeout"
    } else if outcome.success() {
        "collected"
    } else {
        "error"
    }
    .into();
    result.detail = outcome.description();
    Ok(result)
}

pub(crate) struct GitOutput {
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
    pub cancelled: bool,
    pub timed_out: bool,
}

impl GitOutput {
    pub fn success(&self) -> bool {
        self.status.success() && !self.cancelled && !self.timed_out
    }

    pub fn description(&self) -> String {
        let reason = if self.cancelled {
            "cancelled".into()
        } else if self.timed_out {
            "timed out".into()
        } else {
            self.status.to_string()
        };
        let output = [self.stdout.trim(), self.stderr.trim()]
            .into_iter()
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>()
            .join("; ");
        if output.is_empty() {
            reason
        } else {
            format!("{reason}: {output}")
        }
    }
}

fn read_bounded(mut stream: impl Read) -> String {
    let mut retained = Vec::new();
    let mut buffer = [0_u8; 4096];
    let mut truncated = false;
    loop {
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => {
                let keep = count.min(OUTPUT_LIMIT.saturating_sub(retained.len()));
                retained.extend_from_slice(&buffer[..keep]);
                truncated |= keep < count;
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    let mut output = String::from_utf8_lossy(&retained).into_owned();
    if truncated {
        output.push_str("\n[output truncated]");
    }
    output
}

/// Isolate Git and all its subprocesses in a process group. Drain pipes on
/// separate threads, retaining bounded output, so noisy Git cannot deadlock.
pub(crate) fn run_git(
    git: &Path,
    git_dir: &Path,
    args: &[&str],
    timeout: Duration,
    should_stop: &dyn Fn() -> bool,
) -> Result<GitOutput> {
    let mut command = Command::new(git);
    command
        .current_dir(git_dir)
        .arg("--git-dir")
        .arg(git_dir)
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "gc.autoDetach=false",
            "-c",
            "gc.detach=false",
            "-c",
            "pack.threads=1",
            "-c",
            "pack.windowMemory=64m",
        ])
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("GIT_") {
            command.env_remove(key);
        }
    }
    command
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_NO_REPLACE_OBJECTS", "1");
    // SAFETY: pre_exec calls only async-signal-safe libc functions; it allocates
    // no memory and changes only the child process before exec.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            libc::setpriority(libc::PRIO_PROCESS, 0, 19);
            Ok(())
        });
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("start {}", git.display()))?;
    let stdout = child.stdout.take().expect("stdout configured");
    let stderr = child.stderr.take().expect("stderr configured");
    let stdout_reader = thread::spawn(move || read_bounded(stdout));
    let stderr_reader = thread::spawn(move || read_bounded(stderr));
    let started = Instant::now();
    let mut cancelled = false;
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait().context("wait for Git")? {
            break status;
        }
        cancelled = should_stop();
        timed_out = started.elapsed() >= timeout;
        if cancelled || timed_out {
            terminate_group(child.id() as i32);
            break child.wait().context("reap interrupted Git")?;
        }
        thread::sleep(if started.elapsed() < Duration::from_millis(250) {
            Duration::from_millis(10)
        } else {
            Duration::from_millis(250)
        });
    };
    // A Git helper should never outlive Git; kill any remaining members before
    // joining readers, which could otherwise wait forever for inherited pipes.
    // SAFETY: the negative PID targets only the child-created process group.
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    Ok(GitOutput {
        status,
        stdout: stdout_reader
            .join()
            .unwrap_or_else(|_| "[stdout reader failed]".into()),
        stderr: stderr_reader
            .join()
            .unwrap_or_else(|_| "[stderr reader failed]".into()),
        cancelled,
        timed_out,
    })
}

fn terminate_group(pid: i32) {
    // SAFETY: pid is the process group created for this child by setsid.
    unsafe {
        libc::kill(-pid, libc::SIGTERM);
    }
    thread::sleep(Duration::from_millis(500));
    // SAFETY: same process group; terminating descendants prevents orphan GC.
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn subprocess_timeout_is_distinct_from_user_cancellation() {
        let temp = tempfile::TempDir::new().unwrap();
        let executable = temp.path().join("sleeping-git");
        fs::write(&executable, "#!/bin/sh\nsleep 60 &\nwait\n").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let started = Instant::now();
        let output = run_git(
            &executable,
            temp.path(),
            &[],
            Duration::from_millis(40),
            &|| false,
        )
        .unwrap();
        assert!(output.timed_out);
        assert!(!output.cancelled);
        assert!(!output.success());
        assert!(started.elapsed() < Duration::from_secs(3));
    }
}
