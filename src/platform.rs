//! macOS integration. No resident monitor: launchd starts one short-lived check.
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct IdlePolicy {
    pub idle_seconds: u64,
    pub sample_seconds: u64,
    pub max_cpu_percent: f64,
    pub max_gpu_percent: f64,
    pub min_memory_free_percent: f64,
    pub require_ac: bool,
}

impl Default for IdlePolicy {
    fn default() -> Self {
        Self {
            idle_seconds: 600,
            sample_seconds: 30,
            max_cpu_percent: 20.0,
            max_gpu_percent: 35.0,
            min_memory_free_percent: 15.0,
            require_ac: true,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct IdleReport {
    pub eligible: bool,
    pub reason: String,
}

impl IdleReport {
    fn denied(reason: impl Into<String>) -> Self {
        Self {
            eligible: false,
            reason: reason.into(),
        }
    }
}

pub fn probe_idle(policy: &IdlePolicy) -> Result<IdleReport> {
    validate_policy(policy)?;
    imp::probe_idle(policy)
}

/// A fast guard during maintenance. CPU is sampled only before maintenance,
/// because Git's own packing CPU would otherwise be mistaken for other work.
pub fn still_idle(policy: &IdlePolicy) -> bool {
    imp::still_idle(policy)
}

pub fn background_priority() -> Result<()> {
    imp::background_priority()
}
pub fn install_schedule(executable: &Path) -> Result<PathBuf> {
    imp::install_schedule(executable)
}
pub fn uninstall_schedule() -> Result<()> {
    imp::uninstall_schedule()
}
pub fn schedule_status() -> Result<String> {
    imp::schedule_status()
}
pub fn request_notifications() -> Result<String> {
    imp::request_notifications()
}
pub fn test_notifications() -> Result<String> {
    imp::test_notifications()
}
pub fn notify_overdue(days: u64) -> Result<()> {
    imp::notify_overdue(days)
}

fn validate_policy(policy: &IdlePolicy) -> Result<()> {
    if policy.sample_seconds == 0 || policy.sample_seconds > 300 {
        bail!("idle sample_seconds must be between 1 and 300");
    }
    for (name, value) in [
        ("max_cpu_percent", policy.max_cpu_percent),
        ("max_gpu_percent", policy.max_gpu_percent),
        ("min_memory_free_percent", policy.min_memory_free_percent),
    ] {
        if !value.is_finite() || !(0.0..=100.0).contains(&value) {
            bail!("{name} must be a finite percentage between 0 and 100");
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
mod imp {
    use super::*;
    use std::{
        fs,
        io::Read,
        os::unix::{ffi::OsStrExt, fs::PermissionsExt, process::CommandExt},
        process::{Command, Output, Stdio},
        sync::atomic::{AtomicU8, Ordering},
        thread,
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };

    const LABEL: &str = "dev.gitsweep";
    const NOTIFICATION_SOURCE: &str = include_str!("../macos/NotificationHelper.swift");
    const NOTIFICATION_INFO: &str = include_str!("../macos/Info.plist");
    const NOTIFICATION_ICON: &[u8] = include_bytes!("../macos/GitSweep.icns");
    const GPU_SAMPLE_INTERVAL: Duration = Duration::from_secs(5);
    const GPU_BUSY_SAMPLES_TO_STOP: u8 = 3;
    static GPU_BUSY_STREAK: AtomicU8 = AtomicU8::new(0);

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGEventSourceSecondsSinceLastEventType(state_id: i32, event_type: u32) -> f64;
    }
    #[link(name = "CoreServices", kind = "framework")]
    unsafe extern "C" {
        fn LSRegisterURL(url: *const std::ffi::c_void, update: u8) -> i32;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFURLCreateFromFileSystemRepresentation(
            allocator: *const std::ffi::c_void,
            bytes: *const u8,
            length: isize,
            is_directory: u8,
        ) -> *const std::ffi::c_void;
        fn CFRelease(value: *const std::ffi::c_void);
    }
    unsafe extern "C" {
        fn mach_host_self() -> u32;
        fn mach_task_self() -> u32;
        fn mach_port_deallocate(task: u32, port: u32) -> i32;
        fn host_statistics(host: u32, flavor: i32, info: *mut i32, count: *mut u32) -> i32;
        fn setiopolicy_np(io_type: i32, scope: i32, policy: i32) -> i32;
    }

    fn idle_seconds() -> Result<f64> {
        // Combined session includes hardware and synthetic/remote input.
        let idle = unsafe { CGEventSourceSecondsSinceLastEventType(0, u32::MAX) };
        if !idle.is_finite() || idle < 0.0 {
            bail!("macOS session idle time unavailable");
        }
        Ok(idle)
    }

    fn cpu_ticks() -> Result<[u32; 4]> {
        let mut ticks = [0u32; 4];
        let mut count = 4u32;
        // HOST_CPU_LOAD_INFO has exactly four natural_t counters.
        let host = unsafe { mach_host_self() };
        let result = unsafe { host_statistics(host, 3, ticks.as_mut_ptr().cast(), &mut count) };
        unsafe {
            mach_port_deallocate(mach_task_self(), host);
        }
        if result != 0 || count != 4 {
            bail!("macOS CPU counters unavailable (Mach status {result})");
        }
        Ok(ticks)
    }

    pub fn probe_idle(policy: &IdlePolicy) -> Result<IdleReport> {
        let initial_idle = idle_seconds()?;
        if initial_idle < policy.idle_seconds as f64 {
            return Ok(IdleReport::denied(format!(
                "session active: idle {:.0}s, need {}s",
                initial_idle, policy.idle_seconds
            )));
        }
        if let Some(reason) = hard_resource_objection(policy)? {
            return Ok(IdleReport::denied(reason));
        }
        let before = cpu_ticks()?;
        let started = Instant::now();
        let mut gpu_samples = vec![gpu_activity()?];
        let mut last_gpu_sample = Instant::now();
        // Tiny input polls make the probe stop early when someone resumes work.
        // No resource subprocesses are kept alive while waiting.
        while started.elapsed() < Duration::from_secs(policy.sample_seconds) {
            thread::sleep(
                Duration::from_secs(1).min(
                    Duration::from_secs(policy.sample_seconds).saturating_sub(started.elapsed()),
                ),
            );
            if idle_seconds()? + 1.0 < initial_idle + started.elapsed().as_secs_f64() {
                return Ok(IdleReport::denied("input resumed during resource sample"));
            }
            if last_gpu_sample.elapsed() >= GPU_SAMPLE_INTERVAL {
                gpu_samples.push(gpu_activity()?);
                last_gpu_sample = Instant::now();
            }
        }
        if last_gpu_sample.elapsed() >= Duration::from_secs(1) {
            gpu_samples.push(gpu_activity()?);
        }
        let cpu = cpu_percent(before, cpu_ticks()?)?;
        if cpu > policy.max_cpu_percent {
            return Ok(IdleReport::denied(format!(
                "CPU busy: {cpu:.1}% across all cores, limit {:.1}%",
                policy.max_cpu_percent
            )));
        }
        if let Some(reason) = hard_resource_objection(policy)? {
            return Ok(IdleReport::denied(reason));
        }
        let gpu = gpu_sample_average(&gpu_samples)?;
        if gpu > policy.max_gpu_percent {
            return Ok(IdleReport::denied(format!(
                "GPU busy: average {gpu:.1}% across {} samples over {}s, limit {:.1}%",
                gpu_samples.len(),
                policy.sample_seconds,
                policy.max_gpu_percent
            )));
        }
        let idle = idle_seconds()?;
        if idle < policy.idle_seconds as f64 {
            return Ok(IdleReport::denied("input resumed after resource sample"));
        }
        Ok(IdleReport {
            eligible: true,
            reason: format!(
                "idle {idle:.0}s; CPU {cpu:.1}% across all cores; GPU average {gpu:.1}% across {} samples over {}s; memory, thermal and power gates passed",
                gpu_samples.len(),
                policy.sample_seconds
            ),
        })
    }

    pub fn still_idle(policy: &IdlePolicy) -> bool {
        if !idle_seconds().is_ok_and(|idle| idle >= policy.idle_seconds as f64)
            || !hard_resource_objection(policy).is_ok_and(|reason| reason.is_none())
        {
            return false;
        }
        let Ok(gpu) = gpu_activity() else {
            return false;
        };
        let streak = update_gpu_busy_streak(&GPU_BUSY_STREAK, gpu > policy.max_gpu_percent);
        streak < GPU_BUSY_SAMPLES_TO_STOP
    }

    fn update_gpu_busy_streak(streak: &AtomicU8, busy: bool) -> u8 {
        if busy {
            let mut current = streak.load(Ordering::Relaxed);
            loop {
                let next = current.saturating_add(1);
                match streak.compare_exchange_weak(
                    current,
                    next,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => return next,
                    Err(observed) => current = observed,
                }
            }
        } else {
            streak.store(0, Ordering::Relaxed);
            0
        }
    }

    fn gpu_sample_average(samples: &[f64]) -> Result<f64> {
        if samples.is_empty()
            || samples
                .iter()
                .any(|value| !value.is_finite() || !(0.0..=100.0).contains(value))
        {
            bail!("GPU sample window is empty or invalid");
        }
        Ok(samples.iter().sum::<f64>() / samples.len() as f64)
    }

    fn hard_resource_objection(policy: &IdlePolicy) -> Result<Option<String>> {
        if policy.require_ac {
            let power = command("/usr/bin/pmset", &["-g", "batt"], 5)?;
            if !power.lines().next().unwrap_or("").contains("'AC Power'") {
                return Ok(Some(
                    "waiting for AC power (battery or unknown power source)".into(),
                ));
            }
        }
        let memory = parse_memory_free(&command("/usr/bin/memory_pressure", &["-Q"], 5)?)?;
        if memory < policy.min_memory_free_percent {
            return Ok(Some(format!(
                "memory busy: {memory:.0}% free, need {:.0}%",
                policy.min_memory_free_percent
            )));
        }
        let thermal = command("/usr/bin/pmset", &["-g", "therm"], 5)?;
        if thermal_objection(&thermal)? {
            return Ok(Some("macOS reports thermal or performance pressure".into()));
        }
        Ok(None)
    }

    fn gpu_activity() -> Result<f64> {
        // These registry fields differ by GPU generation. Recognize Apple and
        // Intel/AMD names; unknown layouts defer maintenance.
        let registry = command(
            "/usr/sbin/ioreg",
            &["-r", "-c", "IOAccelerator", "-d", "1", "-l", "-w", "0"],
            5,
        )?;
        parse_gpu_activity(&registry)
    }

    pub fn background_priority() -> Result<()> {
        if unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, 19) } != 0 {
            return Err(std::io::Error::last_os_error()).context("set low CPU priority");
        }
        // IOPOL_TYPE_DISK, IOPOL_SCOPE_PROCESS, IOPOL_THROTTLE. Children inherit.
        if unsafe { setiopolicy_np(0, 0, 3) } != 0 {
            return Err(std::io::Error::last_os_error()).context("set low disk I/O priority");
        }
        Ok(())
    }

    fn home() -> Result<PathBuf> {
        let path = std::env::var_os("HOME")
            .map(PathBuf::from)
            .context("HOME is not set")?;
        if !path.is_absolute() {
            bail!("HOME must be absolute");
        }
        Ok(path)
    }
    fn support() -> Result<PathBuf> {
        Ok(home()?.join("Library/Application Support/git-sweep"))
    }
    fn plist_path() -> Result<PathBuf> {
        Ok(home()?
            .join("Library/LaunchAgents")
            .join(format!("{LABEL}.plist")))
    }
    fn domain() -> String {
        format!("gui/{}", unsafe { libc::getuid() })
    }
    fn service() -> String {
        format!("{}/{LABEL}", domain())
    }

    fn custom_state_home() -> Result<Option<PathBuf>> {
        std::env::var_os("GIT_SWEEP_HOME")
            .map(|value| {
                let value = PathBuf::from(value);
                if value.is_absolute() {
                    Ok(value)
                } else {
                    Ok(std::env::current_dir()?.join(value))
                }
            })
            .transpose()
    }

    fn schedule_plist(binary: &Path, custom_home: Option<&Path>) -> Result<String> {
        let binary = xml(binary
            .to_str()
            .context("scheduler executable path must be UTF-8")?);
        let environment = if let Some(path) = custom_home {
            format!(
                "<key>EnvironmentVariables</key><dict><key>GIT_SWEEP_HOME</key><string>{}</string></dict>",
                xml(path
                    .to_str()
                    .context("GIT_SWEEP_HOME must be UTF-8 for launchd")?)
            )
        } else {
            String::new()
        };
        Ok(format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>Label</key><string>{LABEL}</string>
<key>ProgramArguments</key><array><string>{binary}</string><string>scheduled</string></array>
<key>StartInterval</key><integer>3600</integer>
<key>RunAtLoad</key><true/>
<key>ProcessType</key><string>Background</string>
<key>Nice</key><integer>19</integer>
<key>LowPriorityIO</key><true/>
<key>StandardOutPath</key><string>/dev/null</string>
<key>StandardErrorPath</key><string>/dev/null</string>
{environment}
</dict></plist>
"#
        ))
    }

    pub fn install_schedule(executable: &Path) -> Result<PathBuf> {
        let root = support()?.join("bin");
        fs::create_dir_all(&root).context("create installed binary directory")?;
        let installed = root.join("git-sweep");
        let plist = plist_path()?;
        fs::create_dir_all(plist.parent().unwrap())?;
        let contents = schedule_plist(&installed, custom_state_home()?.as_deref())?;
        let old_binary = if installed.exists() {
            Some(fs::read(&installed)?)
        } else {
            None
        };
        let old_plist = if plist.exists() {
            Some(fs::read(&plist)?)
        } else {
            None
        };
        let loaded = launchctl(&["print", &service()])?.status.success();
        if loaded {
            launchctl_checked(&["bootout", &service()])?;
        }
        let update = (|| -> Result<()> {
            atomic_write(
                &installed,
                &fs::read(executable).context("read executable to install")?,
                0o755,
            )?;
            atomic_write(&plist, contents.as_bytes(), 0o600)?;
            launchctl_checked(&[
                "bootstrap",
                &domain(),
                plist.to_str().context("launchd plist path must be UTF-8")?,
            ])?;
            Ok(())
        })();
        if let Err(error) = update {
            let rollback = (|| -> Result<()> {
                // Bootstrap may have succeeded before a later transport error.
                if launchctl(&["print", &service()])?.status.success() {
                    launchctl_checked(&["bootout", &service()])?;
                }
                restore(&installed, old_binary.as_deref(), 0o755)?;
                restore(&plist, old_plist.as_deref(), 0o600)?;
                if loaded {
                    launchctl_checked(&["bootstrap", &domain(), plist.to_str().unwrap()])?;
                }
                Ok(())
            })();
            return match rollback {
                Ok(()) => {
                    Err(error
                        .context("schedule installation failed; previous installation restored"))
                }
                Err(rollback) => Err(error.context(format!(
                    "schedule installation failed; rollback also failed: {rollback:#}"
                ))),
            };
        }
        Ok(plist)
    }

    pub fn uninstall_schedule() -> Result<()> {
        if launchctl(&["print", &service()])?.status.success() {
            launchctl_checked(&["bootout", &service()])?;
        }
        let path = plist_path()?;
        if path.exists() {
            fs::remove_file(path).context("remove launchd agent plist")?;
        }
        // Keep installed binary, configuration, helper permission and logs.
        Ok(())
    }

    pub fn schedule_status() -> Result<String> {
        let output = launchctl(&["print", &service()])?;
        if !output.status.success() {
            return Ok(if plist_path()?.exists() {
                "Installed plist; agent is not loaded in this GUI session."
            } else {
                "Not installed."
            }
            .into());
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let details: Vec<_> = text
            .lines()
            .filter(|line| {
                let line = line.trim();
                line.starts_with("state =")
                    || line.starts_with("runs =")
                    || line.starts_with("last exit code =")
                    || line.starts_with("pid =")
            })
            .map(str::trim)
            .collect();
        Ok(format!(
            "Loaded; hourly checks when logged in. {}",
            details.join("; ")
        ))
    }

    fn launchctl(args: &[&str]) -> Result<Output> {
        bounded(
            Command::new("/bin/launchctl").args(args),
            Duration::from_secs(15),
        )
    }
    fn launchctl_checked(args: &[&str]) -> Result<()> {
        let output = launchctl(args)?;
        if !output.status.success() {
            bail!(
                "launchctl {} failed ({}): {}",
                args.first().unwrap_or(&""),
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    }

    fn restore(path: &Path, old: Option<&[u8]>, mode: u32) -> Result<()> {
        match old {
            Some(bytes) => atomic_write(path, bytes, mode),
            None if path.exists() => {
                fs::remove_file(path).context("remove new installation while rolling back")
            }
            None => Ok(()),
        }
    }

    fn atomic_write(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
        use std::io::Write;
        let tmp = path.with_extension(format!("tmp-{}-{}", std::process::id(), stamp()));
        let result = (|| -> Result<()> {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp)?;
            fs::set_permissions(&tmp, fs::Permissions::from_mode(mode))?;
            file.write_all(bytes)?;
            file.sync_all()?;
            fs::rename(&tmp, path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(tmp);
        }
        result.with_context(|| format!("write {}", path.display()))
    }
    fn stamp() -> u128 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    }

    fn notification_helper_is_current(app: &Path) -> bool {
        app.join("Contents/MacOS/GitSweepNotifications").is_file()
            && fs::read_to_string(app.join("Contents/Resources/NotificationHelper.swift"))
                .ok()
                .as_deref()
                == Some(NOTIFICATION_SOURCE)
            && fs::read_to_string(app.join("Contents/Info.plist"))
                .ok()
                .as_deref()
                == Some(NOTIFICATION_INFO)
            && fs::read(app.join("Contents/Resources/GitSweep.icns"))
                .ok()
                .as_deref()
                == Some(NOTIFICATION_ICON)
    }

    fn ensure_notification_helper() -> Result<PathBuf> {
        let root = support()?;
        fs::create_dir_all(&root)?;
        let app = root.join("Git Sweep Notifications.app");
        if notification_helper_is_current(&app) {
            return Ok(app);
        }
        let staging = root.join(format!(
            ".notification-helper-{}-{}.app",
            std::process::id(),
            stamp()
        ));
        let result = (|| -> Result<()> {
            fs::create_dir_all(staging.join("Contents/MacOS"))?;
            fs::create_dir_all(staging.join("Contents/Resources"))?;
            let staged_source = staging.join("Contents/Resources/NotificationHelper.swift");
            let staged_binary = staging.join("Contents/MacOS/GitSweepNotifications");
            fs::write(&staged_source, NOTIFICATION_SOURCE)?;
            fs::write(staging.join("Contents/Info.plist"), NOTIFICATION_INFO)?;
            fs::write(
                staging.join("Contents/Resources/GitSweep.icns"),
                NOTIFICATION_ICON,
            )?;
            let output = bounded(
                Command::new("/usr/bin/xcrun")
                    .arg("swiftc")
                    .arg("-O")
                    .arg("-framework")
                    .arg("AppKit")
                    .arg("-framework")
                    .arg("UserNotifications")
                    .arg(&staged_source)
                    .arg("-o")
                    .arg(&staged_binary),
                Duration::from_secs(120),
            )?;
            if !output.status.success() {
                bail!(
                    "notification helper compilation failed; Xcode Command Line Tools required: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
            let output = bounded(
                Command::new("/usr/bin/codesign")
                    .args([
                        "--force",
                        "--sign",
                        "-",
                        "--identifier",
                        "dev.gitsweep.notifications",
                    ])
                    .arg(&staging),
                Duration::from_secs(30),
            )?;
            if !output.status.success() {
                bail!(
                    "notification helper signing failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
            // Keep stable app identity and path so macOS can remember permission.
            if app.exists() {
                fs::remove_dir_all(&app)?;
            }
            fs::rename(&staging, &app)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_dir_all(staging);
        }
        result?;
        Ok(app)
    }

    fn register_notification_helper(app: &Path) -> Result<()> {
        let path = app.as_os_str().as_bytes();
        // SAFETY: Core Foundation copies the valid path buffer; the returned
        // owned URL is released after synchronous Launch Services registration.
        let status = unsafe {
            let url = CFURLCreateFromFileSystemRepresentation(
                std::ptr::null(),
                path.as_ptr(),
                path.len() as isize,
                1,
            );
            if url.is_null() {
                bail!("cannot create notification helper file URL");
            }
            let status = LSRegisterURL(url, 1);
            CFRelease(url);
            status
        };
        if status != 0 {
            bail!("cannot refresh notification app registration (Launch Services status {status})");
        }
        Ok(())
    }

    fn notification(action: &str, days: Option<u64>) -> Result<String> {
        let app = if action == "request" || action == "test" {
            ensure_notification_helper()?
        } else {
            // Never start the Swift compiler from a resource-deferred check.
            // Interactive permission setup prepares the helper ahead of time.
            let app = support()?.join("Git Sweep Notifications.app");
            if !app.join("Contents/MacOS/GitSweepNotifications").is_file() {
                bail!(
                    "notification helper is not prepared; run `git-sweep notifications` interactively"
                );
            }
            app
        };
        // Re-register the final stable bundle, including when reused. Replacing
        // a signed bundle alone need not refresh its cached icon metadata.
        register_notification_helper(&app)?;
        let result_path = support()?.join(format!(
            ".notification-result-{}-{}",
            std::process::id(),
            stamp()
        ));
        let result = (|| -> Result<String> {
            let mut cmd = Command::new("/usr/bin/open");
            cmd.args(["-W", "-n", "-a"])
                .arg(&app)
                .arg("--args")
                .arg(action)
                .arg(&result_path);
            if let Some(days) = days {
                cmd.arg(days.to_string());
            }
            let output = bounded(&mut cmd, Duration::from_secs(120))?;
            if !output.status.success() {
                bail!(
                    "open notification helper failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
            let message = fs::read_to_string(&result_path)
                .context("notification helper returned no result")?;
            if message.starts_with("error:") {
                bail!("{}", message.trim());
            }
            Ok(message.trim().to_owned())
        })();
        let _ = fs::remove_file(result_path);
        result
    }
    pub fn request_notifications() -> Result<String> {
        match notification("request", None)?.as_str() {
            "granted" => Ok("Notifications allowed. Git Sweep will alert when cleanup is overdue.".into()),
            "denied" => Ok("Notification permission denied. Enable Git Sweep Notifications in System Settings > Notifications to receive overdue alerts.".into()),
            message => bail!("unexpected notification permission result: {message}"),
        }
    }
    pub fn test_notifications() -> Result<String> {
        if notification("test", None)? != "sent" {
            bail!("test notification was not submitted");
        }
        Ok("Test notification submitted. Check the new Git Sweep notification for its icon; an existing permission banner may retain its old image.".into())
    }
    pub fn notify_overdue(days: u64) -> Result<()> {
        let message = notification("notify", Some(days))?;
        if message != "sent" {
            bail!("overdue notification was not submitted: {message}");
        }
        Ok(())
    }

    fn command(program: &str, args: &[&str], timeout_seconds: u64) -> Result<String> {
        let output = bounded(
            Command::new(program).args(args).env("LC_ALL", "C"),
            Duration::from_secs(timeout_seconds),
        )?;
        if !output.status.success() {
            bail!(
                "{program} failed ({}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        String::from_utf8(output.stdout)
            .with_context(|| format!("{program} returned invalid UTF-8"))
    }

    // Read pipes concurrently to avoid deadlocks. Cap retained output, drain the
    // rest, and kill the entire child process group if a system tool stalls.
    fn bounded(command: &mut Command, timeout: Duration) -> Result<Output> {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        unsafe {
            command.pre_exec(|| {
                if libc::setpgid(0, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let description = format!("{:?}", command.get_program());
        let mut child = command
            .spawn()
            .with_context(|| format!("start {description}"))?;
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let out_thread = thread::spawn(move || capped_read(stdout));
        let err_thread = thread::spawn(move || capped_read(stderr));
        let start = Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Ok(status),
                Ok(None) if start.elapsed() < timeout => thread::sleep(Duration::from_millis(20)),
                Ok(None) => {
                    unsafe {
                        libc::kill(-(child.id() as i32), libc::SIGKILL);
                    }
                    let _ = child.wait();
                    break Err(anyhow::anyhow!(
                        "{description} timed out after {}s",
                        timeout.as_secs()
                    ));
                }
                Err(error) => {
                    unsafe {
                        libc::kill(-(child.id() as i32), libc::SIGKILL);
                    }
                    let _ = child.wait();
                    break Err(error.into());
                }
            }
        };
        let stdout = out_thread
            .join()
            .map_err(|_| anyhow::anyhow!("stdout reader panicked"))??;
        let stderr = err_thread
            .join()
            .map_err(|_| anyhow::anyhow!("stderr reader panicked"))??;
        Ok(Output {
            status: status?,
            stdout,
            stderr,
        })
    }
    fn capped_read(mut reader: impl Read) -> std::io::Result<Vec<u8>> {
        let mut output = Vec::new();
        let mut buffer = [0u8; 8192];
        loop {
            let count = reader.read(&mut buffer)?;
            if count == 0 {
                return Ok(output);
            }
            let keep = count.min((1024 * 1024usize).saturating_sub(output.len()));
            output.extend_from_slice(&buffer[..keep]);
        }
    }

    pub(super) fn cpu_percent(before: [u32; 4], after: [u32; 4]) -> Result<f64> {
        let delta: Vec<u64> = before
            .into_iter()
            .zip(after)
            .map(|(a, b)| b.wrapping_sub(a) as u64)
            .collect();
        let total: u64 = delta.iter().sum();
        if total == 0 {
            bail!("CPU counters did not advance");
        }
        Ok(100.0 * (total - delta[2]) as f64 / total as f64)
    }
    pub(super) fn parse_memory_free(text: &str) -> Result<f64> {
        let raw = text
            .lines()
            .find_map(|line| line.strip_prefix("System-wide memory free percentage:"))
            .context("memory_pressure did not report free percentage")?
            .trim()
            .strip_suffix('%')
            .context("invalid memory percentage")?;
        let value: f64 = raw.trim().parse().context("invalid memory percentage")?;
        if !value.is_finite() || !(0.0..=100.0).contains(&value) {
            bail!("invalid memory percentage");
        }
        Ok(value)
    }
    pub(super) fn parse_gpu_activity(text: &str) -> Result<f64> {
        let keys = [
            "Device Utilization %",
            "GPU Activity(%)",
            "GPU Core Utilization",
            "Renderer Utilization %",
            "Tiler Utilization %",
        ];
        let mut maximum: Option<f64> = None;
        for line in text
            .lines()
            .filter(|line| line.contains("\"PerformanceStatistics\""))
        {
            let mut preferred: Option<f64> = None;
            let mut fallback: Option<f64> = None;
            for key in keys {
                let pattern = format!("\"{key}\"");
                for (_, rest) in line
                    .match_indices(&pattern)
                    .map(|(i, _)| (i, &line[i + pattern.len()..]))
                {
                    let rest = rest
                        .trim_start()
                        .strip_prefix('=')
                        .context("invalid GPU activity field")?
                        .trim_start();
                    let number: String = rest
                        .chars()
                        .take_while(|c| c.is_ascii_digit() || *c == '.')
                        .collect();
                    let value: f64 = number.parse().context("invalid GPU activity percentage")?;
                    if !value.is_finite() || !(0.0..=100.0).contains(&value) {
                        bail!("invalid GPU activity percentage");
                    }
                    if key == "Device Utilization %" {
                        preferred = Some(preferred.unwrap_or(0.0).max(value));
                    } else {
                        fallback = Some(fallback.unwrap_or(0.0).max(value));
                    }
                }
            }
            if let Some(value) = preferred.or(fallback) {
                maximum = Some(maximum.unwrap_or(0.0).max(value));
            } else {
                bail!(
                    "GPU activity metric unavailable; scheduler will defer rather than risk interrupting GPU work"
                );
            }
        }
        maximum.context("GPU activity metric unavailable; scheduler will defer rather than risk interrupting GPU work")
    }
    pub(super) fn thermal_objection(text: &str) -> Result<bool> {
        if text.trim().is_empty() {
            bail!("macOS thermal status unavailable");
        }
        let mut recognized = false;
        for line in text.lines() {
            if line.contains("No thermal warning level")
                || line.contains("No performance warning level")
                || line.contains("No CPU power status")
            {
                recognized = true;
            }
            if let Some((key, raw)) = line.split_once('=') {
                let key = key.trim();
                let raw = raw.trim().trim_end_matches(';');
                if [
                    "CPU_Speed_Limit",
                    "CPU_Scheduler_Limit",
                    "CPU_Available_CPUs",
                    "Thermal_Level",
                    "ThermalWarningLevel",
                ]
                .contains(&key)
                {
                    let value: f64 = raw.parse().context("invalid thermal status value")?;
                    recognized = true;
                    if (key.ends_with("_Limit") && value < 100.0)
                        || (key.starts_with("Thermal") && value > 0.0)
                    {
                        return Ok(true);
                    }
                }
            }
        }
        if !recognized {
            bail!("unrecognized macOS thermal status; maintenance deferred");
        }
        Ok(false)
    }
    fn xml(text: &str) -> String {
        text.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
            .replace('\'', "&apos;")
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn cpu_counter_delta_and_rollover() {
            assert_eq!(cpu_percent([0; 4], [10, 10, 80, 0]).unwrap(), 20.0);
            assert_eq!(
                cpu_percent([u32::MAX - 4, 0, 0, 0], [5, 0, 90, 0]).unwrap(),
                10.0
            );
            assert!(cpu_percent([0; 4], [0; 4]).is_err());
        }
        #[test]
        fn native_idle_and_cpu_metrics_are_readable() {
            assert!(idle_seconds().unwrap().is_finite());
            let before = cpu_ticks().unwrap();
            thread::sleep(Duration::from_millis(30));
            assert!((0.0..=100.0).contains(&cpu_percent(before, cpu_ticks().unwrap()).unwrap()));
        }
        #[test]
        fn memory_requires_valid_percentage() {
            assert_eq!(
                parse_memory_free("System-wide memory free percentage: 48%\n").unwrap(),
                48.0
            );
            assert!(parse_memory_free("System-wide memory free percentage: NaN%\n").is_err());
            assert!(parse_memory_free("missing").is_err());
        }
        #[test]
        fn gpu_handles_apple_and_intel_and_unknown() {
            assert_eq!(parse_gpu_activity(r#""PerformanceStatistics" = {"Device Utilization %"=7,"Renderer Utilization %"=15}"#).unwrap(), 7.0);
            assert_eq!(
                parse_gpu_activity(r#""PerformanceStatistics" = {"GPU Activity(%)"=95}"#).unwrap(),
                95.0
            );
            assert!(parse_gpu_activity(r#""PerformanceStatistics" = {"Other"=5}"#).is_err());
            assert!(
                parse_gpu_activity(r#""PerformanceStatistics" = {"Device Utilization %"=500}"#)
                    .is_err()
            );
        }
        #[test]
        fn thermal_status_pressure() {
            assert!(
                !thermal_objection("Note: No thermal warning level has been recorded\n").unwrap()
            );
            assert!(thermal_objection("CPU_Speed_Limit = 80\n").unwrap());
            assert!(thermal_objection("Unknown output").is_err());
        }
        #[test]
        fn transient_gpu_spikes_do_not_stop_maintenance() {
            let streak = AtomicU8::new(0);
            assert_eq!(update_gpu_busy_streak(&streak, true), 1);
            assert_eq!(update_gpu_busy_streak(&streak, true), 2);
            assert_eq!(update_gpu_busy_streak(&streak, false), 0);
            assert_eq!(update_gpu_busy_streak(&streak, true), 1);
            assert_eq!(update_gpu_busy_streak(&streak, true), 2);
            assert_eq!(update_gpu_busy_streak(&streak, true), 3);
        }
        #[test]
        fn gpu_window_tolerates_spikes_but_rejects_sustained_load() {
            assert!(
                gpu_sample_average(&[42.0, 5.0, 5.0, 5.0, 5.0, 5.0, 5.0]).unwrap()
                    < IdlePolicy::default().max_gpu_percent
            );
            assert!(
                gpu_sample_average(&[60.0; 7]).unwrap() > IdlePolicy::default().max_gpu_percent
            );
            assert!(gpu_sample_average(&[]).is_err());
        }
        #[test]
        fn launchd_escapes_paths() {
            let text =
                schedule_plist(Path::new("/a & b/<x>"), Some(Path::new("/custom&home"))).unwrap();
            assert!(text.contains("/a &amp; b/&lt;x&gt;"));
            assert!(text.contains("/custom&amp;home"));
            assert!(text.contains("<string>scheduled</string>"));
            assert!(!text.contains("KeepAlive"));
        }

        #[test]
        fn notification_cache_requires_current_icon() {
            let directory = tempfile::tempdir().unwrap();
            let app = directory.path().join("Notifications.app");
            fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
            fs::create_dir_all(app.join("Contents/Resources")).unwrap();
            fs::write(app.join("Contents/MacOS/GitSweepNotifications"), []).unwrap();
            fs::write(
                app.join("Contents/Resources/NotificationHelper.swift"),
                NOTIFICATION_SOURCE,
            )
            .unwrap();
            fs::write(app.join("Contents/Info.plist"), NOTIFICATION_INFO).unwrap();
            let icon = app.join("Contents/Resources/GitSweep.icns");

            assert!(!notification_helper_is_current(&app));
            fs::write(&icon, NOTIFICATION_ICON).unwrap();
            assert!(notification_helper_is_current(&app));
            fs::write(&icon, b"outdated icon").unwrap();
            assert!(!notification_helper_is_current(&app));
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::*;
    pub fn probe_idle(_: &IdlePolicy) -> Result<IdleReport> {
        bail!("automatic idle detection is supported only on macOS; use manual maintenance")
    }
    pub fn still_idle(_: &IdlePolicy) -> bool {
        false
    }
    pub fn background_priority() -> Result<()> {
        Ok(())
    }
    pub fn install_schedule(_: &Path) -> Result<PathBuf> {
        bail!("launchd scheduling requires macOS")
    }
    pub fn uninstall_schedule() -> Result<()> {
        bail!("launchd scheduling requires macOS")
    }
    pub fn schedule_status() -> Result<String> {
        Ok("launchd scheduling requires macOS".into())
    }
    pub fn request_notifications() -> Result<String> {
        bail!("native notifications require macOS")
    }
    pub fn test_notifications() -> Result<String> {
        bail!("native notifications require macOS")
    }
    pub fn notify_overdue(_: u64) -> Result<()> {
        bail!("native notifications require macOS")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn policy_rejects_invalid_sampling_and_nan() {
        let mut policy = IdlePolicy {
            sample_seconds: 0,
            ..IdlePolicy::default()
        };
        assert!(validate_policy(&policy).is_err());
        policy.sample_seconds = 30;
        policy.max_cpu_percent = f64::NAN;
        assert!(validate_policy(&policy).is_err());
        policy.max_cpu_percent = 20.0;
        policy.max_gpu_percent = f64::NAN;
        assert!(validate_policy(&policy).is_err());
    }
    #[test]
    fn old_idle_config_gets_independent_gpu_default() {
        let policy: IdlePolicy = toml::from_str("idle_seconds = 600\nsample_seconds = 30\nmax_cpu_percent = 20.0\nmin_memory_free_percent = 15.0\nrequire_ac = true\n").unwrap();
        assert_eq!(policy.max_gpu_percent, 35.0);
    }
    #[test]
    fn policy_rejects_misspelled_thresholds() {
        assert!(toml::from_str::<IdlePolicy>("idle_second = 3600").is_err());
    }
}
