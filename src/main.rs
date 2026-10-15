use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use git_sweep::{config::Config, discovery, due, gc, now, platform, storage};
use serde::{Deserialize, Serialize};
use std::{
    cell::Cell,
    io::IsTerminal,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Parser)]
#[command(
    version,
    about = "Discover Git repositories and maintain them when your Mac is idle."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create configuration; request macOS notification permission interactively.
    Init {
        /// Search roots. Repeat to include external disks or other folders.
        #[arg(long)]
        root: Vec<PathBuf>,
        /// Additional subtrees to exclude. Repeat as needed.
        #[arg(long)]
        exclude: Vec<PathBuf>,
        #[arg(long)]
        no_notifications: bool,
    },
    /// Refresh cached repositories when the weekly scan is due.
    #[command(alias = "discover")]
    Scan {
        /// Refresh immediately, overriding the discovery interval.
        #[arg(long)]
        force: bool,
    },
    /// Run maintenance now without requiring idle state; daily limit still applies.
    #[command(alias = "gc")]
    Run {
        /// Override the daily GC interval. Does not override Git's safety defaults.
        #[arg(long)]
        force: bool,
        /// Read repository statistics without running GC or advancing its timestamp.
        #[arg(long)]
        dry_run: bool,
    },
    /// Perform a due check and maintenance only if idle (launchd entry point).
    Scheduled,
    /// Show saved state without probing resource usage or running Git.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Sample idle/resource eligibility without maintaining repositories.
    Probe,
    /// Print configuration or its file location.
    Config {
        #[arg(long)]
        path: bool,
    },
    /// Install, inspect or remove the hourly macOS LaunchAgent.
    Schedule {
        #[command(subcommand)]
        action: ScheduleAction,
    },
    /// Refresh the notification app and request permission, or send a test.
    Notifications {
        /// Send a fresh test notification using existing permission.
        #[arg(long)]
        test: bool,
    },
}

#[derive(Subcommand)]
enum ScheduleAction {
    Install,
    Uninstall,
    Status,
}

#[derive(Serialize, Deserialize)]
struct Cache {
    version: u32,
    scanned_at: u64,
    roots: Vec<PathBuf>,
    excludes: Vec<PathBuf>,
    repositories: Vec<discovery::Repository>,
}

#[derive(Serialize, Deserialize)]
struct RunReport {
    started_at: u64,
    finished_at: u64,
    dry_run: bool,
    interrupted: bool,
    errors: usize,
    repositories: Vec<gc::GcResult>,
    failures: Vec<RunFailure>,
    saved_bytes: i128,
    saved_allocated_bytes: i128,
}

#[derive(Clone, Serialize, Deserialize)]
struct RunFailure {
    repository: PathBuf,
    error: String,
}

/// Resume a partial pass without repeatedly collecting its first repositories.
/// This is a checkpoint, not a separate per-repository maintenance schedule.
#[derive(Serialize, Deserialize)]
struct PassProgress {
    version: u32,
    started_at: u64,
    attempted: Vec<PathBuf>,
    successful: usize,
    errors: usize,
    // Defaults keep checkpoints written before result persistence readable.
    #[serde(default)]
    repositories: Vec<gc::GcResult>,
    #[serde(default)]
    failures: Vec<RunFailure>,
}

fn main() {
    if let Err(error) = execute(Cli::parse()) {
        eprintln!("git-sweep: {error:#}");
        std::process::exit(1);
    }
}

fn execute(cli: Cli) -> Result<()> {
    let paths = storage::Paths::new()?;
    match &cli.command {
        Command::Status { json } => return status(&paths, *json),
        Command::Config { path } => {
            if *path {
                println!("{}", paths.config.display());
            } else {
                println!("{}", toml::to_string_pretty(&Config::load(&paths)?)?);
            }
            return Ok(());
        }
        Command::Probe => {
            let config = Config::load(&paths)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&platform::probe_idle(&config.idle)?)?
            );
            return Ok(());
        }
        Command::Schedule {
            action: ScheduleAction::Status,
        } => {
            println!("{}", platform::schedule_status()?);
            return Ok(());
        }
        Command::Schedule {
            action: ScheduleAction::Uninstall,
        } => {
            // bootout also stops an active scheduled run; do not wait on its lock.
            platform::uninstall_schedule()?;
            println!("Hourly schedule removed. Configuration, cache, reports and logs retained.");
            return Ok(());
        }
        _ => {}
    }
    paths.ensure()?;
    let Some(_lock) = storage::acquire_lock(&paths)? else {
        println!("Another git-sweep instance is running; skipped.");
        return Ok(());
    };
    let mut log = storage::Logger::new(&paths)?;
    let result = dispatch(cli.command, &paths, &mut log);
    if let Err(error) = &result {
        let _ = log.log("ERROR", &format!("{error:#}"));
    }
    result
}

fn setup(
    paths: &storage::Paths,
    config: &Config,
    state: &mut storage::State,
    interactive: bool,
    log: &mut storage::Logger,
) -> Result<()> {
    if !paths.config.exists() {
        config.save_new(paths)?;
    }
    if state.initialized_at == 0 {
        state.initialized_at = now();
        storage::save_state(paths, state)?;
    }
    if interactive
        && config.notifications
        && !state.notifications_permission_requested
        && std::io::stdin().is_terminal()
    {
        state.notifications_permission_requested = true;
        storage::save_state(paths, state)?;
        match platform::request_notifications() {
            Ok(message) => {
                println!("{message}");
                log.log("INFO", &message)?;
            }
            Err(error) => {
                eprintln!("Notification setup: {error:#}. Retry with `git-sweep notifications`.");
                log.log(
                    "WARN",
                    &format!("notification permission request failed: {error:#}"),
                )?;
            }
        }
    }
    Ok(())
}

fn dispatch(command: Command, paths: &storage::Paths, log: &mut storage::Logger) -> Result<()> {
    if let Command::Init {
        root,
        exclude,
        no_notifications,
    } = command
    {
        if paths.config.exists() {
            bail!(
                "configuration already exists at {}; edit it directly",
                paths.config.display()
            );
        }
        let mut config = Config::default();
        if !root.is_empty() {
            config.roots = root;
        }
        config.excludes.extend(exclude);
        config.notifications = !no_notifications;
        config.validate()?;
        for root in &config.roots {
            if !root.is_dir() {
                bail!("discovery root is not a directory: {}", root.display());
            }
        }
        let mut state = storage::load_state(paths)?;
        setup(paths, &config, &mut state, true, log)?;
        println!("Created {}", paths.config.display());
        log.log("INFO", "initialized configuration")?;
        return Ok(());
    }
    let config = Config::load(paths)?;
    let mut state = storage::load_state(paths)?;
    let scheduled = matches!(command, Command::Scheduled);
    let interactive_setup = !scheduled && !matches!(command, Command::Notifications { .. });
    setup(paths, &config, &mut state, interactive_setup, log)?;
    match command {
        Command::Notifications { test: true } => {
            println!("{}", platform::test_notifications()?);
            Ok(())
        }
        Command::Notifications { test: false } => {
            state.notifications_permission_requested = true;
            storage::save_state(paths, &state)?;
            println!("{}", platform::request_notifications()?);
            Ok(())
        }
        Command::Schedule {
            action: ScheduleAction::Install,
        } => {
            let executable = std::env::current_exe()?.canonicalize()?;
            let plist = platform::install_schedule(&executable)?;
            println!("Installed hourly schedule: {}", plist.display());
            log.log("INFO", "installed hourly LaunchAgent")
        }
        Command::Scan { force } => {
            let cache = repositories(paths, &config, &mut state, log, force, &|| false)?;
            println!(
                "{} distinct Git stores cached; scan timestamp {}.",
                cache.repositories.len(),
                cache.scanned_at
            );
            Ok(())
        }
        Command::Run { force, dry_run } => {
            run(paths, &config, &mut state, log, false, force, dry_run)
        }
        Command::Scheduled => run(paths, &config, &mut state, log, true, false, false),
        _ => unreachable!(),
    }
}

fn repositories(
    paths: &storage::Paths,
    config: &Config,
    state: &mut storage::State,
    log: &mut storage::Logger,
    force: bool,
    stop: &dyn Fn() -> bool,
) -> Result<Cache> {
    let mut excludes = config.excludes.clone();
    excludes.push(paths.base.clone());
    excludes.sort();
    let existing: Option<Cache> = storage::read_json(&paths.cache)?;
    if let Some(cache) = existing {
        if !force
            && cache.version == 1
            && cache.roots == config.roots
            && cache.excludes == excludes
            && !due(
                Some(cache.scanned_at),
                config.scan_interval_hours * 3600,
                now(),
            )
        {
            log.log(
                "DEBUG",
                &format!(
                    "using cached discovery: {} Git stores",
                    cache.repositories.len()
                ),
            )?;
            return Ok(cache);
        }
    }
    log.log(
        "INFO",
        &format!("discovery started roots={:?}", config.roots),
    )?;
    let report = discovery::discover_with_cancel(&config.roots, &excludes, &config.git, stop)?;
    if stop() {
        bail!("discovery interrupted; previous complete cache retained");
    }
    // Keep raw Git diagnostics in a private report, outside routine text logs.
    storage::atomic_json(&paths.base.join("last-scan.json"), &report)?;
    log.log(
        "INFO",
        &format!(
            "discovery completed directories={} repositories={} warnings={}",
            report.directories_scanned,
            report.repositories.len(),
            report.warnings.len()
        ),
    )?;
    let cache = Cache {
        version: 1,
        scanned_at: now(),
        roots: config.roots.clone(),
        excludes,
        repositories: report.repositories,
    };
    storage::atomic_json(&paths.cache, &cache)?;
    state.last_scan_at = Some(cache.scanned_at);
    storage::save_state(paths, state)?;
    Ok(cache)
}

fn overdue(
    paths: &storage::Paths,
    config: &Config,
    state: &mut storage::State,
    log: &mut storage::Logger,
) -> Result<()> {
    let reference = state.last_gc_at.unwrap_or(state.initialized_at);
    let age = now().saturating_sub(reference);
    if config.notifications
        && age >= config.overdue_days * 86400
        && due(
            state.last_overdue_notification_at,
            config.notification_interval_hours * 3600,
            now(),
        )
    {
        // Rate limit attempts as well as delivery, including denied permission.
        state.last_overdue_notification_at = Some(now());
        storage::save_state(paths, state)?;
        match platform::notify_overdue(age / 86400) {
            Ok(()) => log.log("WARN", "maintenance overdue; macOS notification submitted")?,
            Err(error) => log.log("WARN", &format!("overdue notification failed: {error:#}"))?,
        }
    }
    Ok(())
}

fn run(
    paths: &storage::Paths,
    config: &Config,
    state: &mut storage::State,
    log: &mut storage::Logger,
    scheduled: bool,
    force: bool,
    dry_run: bool,
) -> Result<()> {
    // This branch must remain above priority changes, hardware probes and discovery.
    if !force && !dry_run && !due(state.last_gc_at, config.gc_interval_hours * 3600, now()) {
        log.log(
            "DEBUG",
            "daily maintenance already completed; resource sampling skipped",
        )?;
        if !scheduled {
            println!("Daily maintenance already completed; use --force to run again.");
        }
        return Ok(());
    }
    let signals = Arc::new(AtomicBool::new(false));
    for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        signal_hook::flag::register(signal, signals.clone())?;
    }
    if scheduled {
        platform::background_priority()?;
        let idle = match platform::probe_idle(&config.idle) {
            Ok(idle) => idle,
            Err(error) => {
                log.log(
                    "WARN",
                    &format!("resource sampling unavailable; deferred: {error:#}"),
                )?;
                overdue(paths, config, state, log)?;
                return Ok(());
            }
        };
        log.log(
            "INFO",
            &format!("idle probe eligible={} {}", idle.eligible, idle.reason),
        )?;
        if !idle.eligible {
            overdue(paths, config, state, log)?;
            return Ok(());
        }
    }
    let interrupted = Cell::new(false);
    let last_probe = Cell::new(Instant::now());
    let stop = || {
        if signals.load(Ordering::Relaxed) {
            interrupted.set(true);
        }
        if scheduled && !interrupted.get() && last_probe.get().elapsed() >= Duration::from_secs(5) {
            last_probe.set(Instant::now());
            if !platform::still_idle(&config.idle) {
                interrupted.set(true);
            }
        }
        interrupted.get()
    };
    let started_at = now();
    if !dry_run {
        state.last_attempt_at = Some(started_at);
        storage::save_state(paths, state)?;
    }
    let cache = match repositories(paths, config, state, log, false, &stop) {
        Ok(cache) => cache,
        Err(error) if interrupted.get() => {
            state.last_outcome =
                Some("interrupted during discovery; daily timestamp unchanged".into());
            storage::save_state(paths, state)?;
            log.log("INFO", &format!("{error:#}"))?;
            overdue(paths, config, state, log)?;
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    // Reinspect known borrowers on every pass, even while discovery is cached.
    // An unreadable alternates file aborts the pass before any GC begins.
    let borrowed_sources = if dry_run {
        Default::default()
    } else {
        discovery::borrowed_sources(&cache.repositories)?
    };
    if !borrowed_sources.is_empty() {
        log.log(
            "INFO",
            &format!(
                "alternate-object protection deferred {} source stores",
                borrowed_sources.len()
            ),
        )?;
    }
    let progress_path = paths.base.join("progress.json");
    let mut progress = if !dry_run && !force {
        storage::read_json::<PassProgress>(&progress_path)?
    } else {
        None
    }
    // A crash after committing state but before deleting the checkpoint must
    // not make the next day's pass skip all repositories again.
    .filter(|progress| {
        !state
            .last_gc_at
            .is_some_and(|last| last >= progress.started_at)
    })
    .unwrap_or(PassProgress {
        version: 1,
        started_at,
        attempted: vec![],
        successful: 0,
        errors: 0,
        repositories: vec![],
        failures: vec![],
    });
    if progress.version != 1 {
        bail!(
            "unsupported maintenance progress version; inspect {}",
            progress_path.display()
        );
    }
    // Older checkpoints held only paths and counts. Recover any results still
    // present in their latest partial report before the next run overwrites it.
    // A checkpoint interrupted more than once with the old format may already
    // have lost earlier details; new checkpoints persist every result directly.
    if !dry_run
        && !progress.attempted.is_empty()
        && progress.repositories.is_empty()
        && progress.failures.is_empty()
    {
        if let Some(previous) = storage::read_json::<RunReport>(&paths.base.join("last-run.json"))?
        {
            if !previous.dry_run
                && previous.started_at >= progress.started_at
                && previous.finished_at >= previous.started_at
            {
                progress.repositories = previous
                    .repositories
                    .into_iter()
                    .filter(|result| progress.attempted.contains(&result.repo))
                    .collect();
                progress.failures = previous
                    .failures
                    .into_iter()
                    .filter(|failure| progress.attempted.contains(&failure.repository))
                    .collect();
            }
        }
    }
    let already_attempted = progress.attempted.len();
    let mut report = RunReport {
        started_at: progress.started_at,
        finished_at: 0,
        dry_run,
        interrupted: false,
        errors: progress.errors,
        repositories: progress.repositories.clone(),
        failures: progress.failures.clone(),
        saved_bytes: 0,
        saved_allocated_bytes: 0,
    };
    for repo in &cache.repositories {
        if stop() {
            break;
        }
        if !dry_run && progress.attempted.contains(&repo.git_dir) {
            continue;
        }
        let collected = if let Some(borrowers) = borrowed_sources.get(&repo.git_dir) {
            Ok(gc::skipped_alternate_source(repo, borrowers.len()))
        } else {
            gc::collect(repo, &config.git, dry_run, &stop)
        };
        match collected {
            Ok(result) => {
                if result.status == "cancelled" {
                    interrupted.set(true);
                }
                let change =
                    result.before_allocated_bytes as i128 - result.after_allocated_bytes as i128;
                log.log(
                    "INFO",
                    &format!(
                        "repository={} status={} saved_allocated_bytes={change}",
                        result.repo.display(),
                        result.status
                    ),
                )?;
                if !matches!(
                    result.status.as_str(),
                    "collected" | "dry-run" | "missing" | "cancelled" | "skipped_alternate_source"
                ) {
                    report.errors += 1;
                    progress.errors += 1;
                }
                if !dry_run && result.status != "cancelled" {
                    progress.attempted.push(repo.git_dir.clone());
                    if matches!(
                        result.status.as_str(),
                        "collected" | "skipped_alternate_source"
                    ) {
                        progress.successful += 1;
                    }
                    progress.repositories.push(result.clone());
                    storage::atomic_json(&progress_path, &progress)?;
                }
                if !scheduled {
                    println!(
                        "{} {} ({change:+} bytes)",
                        result.status,
                        result.repo.display()
                    );
                }
                report.repositories.push(result);
            }
            Err(error) => {
                report.errors += 1;
                progress.errors += 1;
                log.log(
                    "ERROR",
                    &format!(
                        "repository={} failed; details in run report",
                        repo.git_dir.display()
                    ),
                )?;
                if !scheduled {
                    eprintln!("failed {}: {error:#}", repo.git_dir.display());
                }
                let failure = RunFailure {
                    repository: repo.git_dir.clone(),
                    error: format!("{error:#}"),
                };
                if !dry_run {
                    progress.attempted.push(repo.git_dir.clone());
                    progress.failures.push(failure.clone());
                    storage::atomic_json(&progress_path, &progress)?;
                }
                report.failures.push(failure);
            }
        }
    }
    report.finished_at = now();
    report.interrupted = interrupted.get();
    report.saved_bytes = report
        .repositories
        .iter()
        .map(|r| r.before_bytes as i128 - r.after_bytes as i128)
        .sum();
    report.saved_allocated_bytes = report
        .repositories
        .iter()
        .map(|r| r.before_allocated_bytes as i128 - r.after_allocated_bytes as i128)
        .sum();
    let outcome = format!(
        "{} stores in pass; {already_attempted} previously attempted in resumed pass; {} errors/deferrals; dry_run={dry_run}; interrupted={}",
        report.repositories.len(),
        report.errors,
        report.interrupted
    );
    storage::atomic_json(
        &paths.base.join(if dry_run {
            "last-dry-run.json"
        } else {
            "last-run.json"
        }),
        &report,
    )?;
    if !dry_run {
        state.last_outcome = Some(outcome.clone());
        if !report.interrupted {
            // A completed best-effort sweep includes logged failures; retry them next day.
            // An all-failed sweep never counts as completed maintenance.
            if progress.successful > 0 || cache.repositories.is_empty() {
                state.last_gc_at = Some(report.finished_at);
            }
        }
        storage::save_state(paths, state)?;
        if !report.interrupted && progress_path.exists() {
            std::fs::remove_file(&progress_path)?;
        }
        overdue(paths, config, state, log)?;
    }
    log.log("INFO", &outcome)?;
    if !scheduled {
        println!(
            "{outcome}\nNet allocated storage reduction: {} bytes ({:.3} GB)\nReport: {}",
            report.saved_allocated_bytes,
            report.saved_allocated_bytes as f64 / 1_000_000_000.0,
            paths
                .base
                .join(if dry_run {
                    "last-dry-run.json"
                } else {
                    "last-run.json"
                })
                .display()
        );
    }
    if report.errors > 0 && !scheduled {
        bail!("maintenance had errors or deferred repositories; see logs and report");
    }
    Ok(())
}

fn status(paths: &storage::Paths, json: bool) -> Result<()> {
    let state = storage::load_state(paths)?;
    let cache: Option<Cache> = storage::read_json(&paths.cache)?;
    let value = serde_json::json!({
        "data_directory": paths.base,
        "configuration": paths.config,
        "cached_repositories": cache.as_ref().map(|c| c.repositories.len()).unwrap_or(0),
        "state": state,
    });
    if json {
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        println!(
            "Data: {}\nConfig: {}\nCached Git stores: {}\nLast scan: {:?}\nLast completed GC pass: {:?}\nLast outcome: {}",
            paths.base.display(),
            paths.config.display(),
            value["cached_repositories"],
            state.last_scan_at,
            state.last_gc_at,
            state.last_outcome.as_deref().unwrap_or("never run")
        );
    }
    Ok(())
}
