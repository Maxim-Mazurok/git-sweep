//! CLI contracts use disposable repositories and an isolated data directory.
//! These tests never install a LaunchAgent or request notification permission.

use git_sweep::{config::Config, now, storage::State};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};
use tempfile::TempDir;

struct Fixture {
    _temporary: TempDir,
    root: PathBuf,
    data: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("repositories");
        let data = temporary.path().join("data");
        fs::create_dir(&root).unwrap();
        let fixture = Self {
            _temporary: temporary,
            root,
            data,
        };
        let output = fixture
            .command()
            .args(["init", "--no-notifications", "--root"])
            .arg(&fixture.root)
            .output()
            .unwrap();
        success(&output);
        // macOS temporary directories live beneath /private/var, intentionally
        // excluded by production defaults. This fixture explicitly opts in.
        fixture.update_config(|config| config.excludes.clear());
        fixture
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_git-sweep"));
        command
            .env("GIT_SWEEP_HOME", &self.data)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null");
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }

    fn state(&self) -> State {
        serde_json::from_slice(&fs::read(self.data.join("state.json")).unwrap()).unwrap()
    }

    fn update_state(&self, change: impl FnOnce(&mut State)) {
        let mut state = self.state();
        change(&mut state);
        fs::write(
            self.data.join("state.json"),
            serde_json::to_vec_pretty(&state).unwrap(),
        )
        .unwrap();
    }

    fn update_config(&self, change: impl FnOnce(&mut Config)) {
        let path = self.data.join("config.toml");
        let mut config: Config = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        change(&mut config);
        assert!(
            !config.notifications,
            "test must never enable notifications"
        );
        fs::write(path, toml::to_string_pretty(&config).unwrap()).unwrap();
    }

    fn json(&self, name: &str) -> Value {
        serde_json::from_slice(&fs::read(self.data.join(name)).unwrap()).unwrap()
    }

    fn repository(&self, name: &str) -> PathBuf {
        let path = self.root.join(name);
        fs::create_dir_all(&path).unwrap();
        git(&path, &["init", "--quiet"]);
        fs::write(path.join("tracked.txt"), "committed\n").unwrap();
        git(&path, &["add", "tracked.txt"]);
        git(&path, &["commit", "--quiet", "-m", "Initial fixture"]);
        path
    }
}

fn success(output: &Output) {
    assert!(
        output.status.success(),
        "command failed: {}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git(path: &Path, args: &[&str]) -> Vec<u8> {
    let mut command = Command::new("git");
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("GIT_") {
            command.env_remove(key);
        }
    }
    let output = command
        .current_dir(path)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .args([
            "-c",
            "user.name=Git Sweep Test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(args)
        .output()
        .unwrap();
    success(&output);
    output.stdout
}

fn file_manifest(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                pending.push(entry.path());
            } else {
                let relative = entry.path().strip_prefix(root).unwrap().to_path_buf();
                files.insert(relative, fs::read(entry.path()).unwrap());
            }
        }
    }
    files
}

#[test]
fn dry_run_preserves_repository_and_does_not_advance_gc_or_attempt() {
    let fixture = Fixture::new();
    let repository = fixture.repository("sample");
    fs::write(repository.join("tracked.txt"), "uncommitted change\n").unwrap();
    fs::write(repository.join("untracked.txt"), "untracked content\n").unwrap();
    let index = fs::read(repository.join(".git/index")).unwrap();
    let head = git(&repository, &["rev-parse", "HEAD"]);
    let before = fixture.state();

    success(&fixture.run(&["run", "--dry-run"]));

    let after = fixture.state();
    assert_eq!(after.last_gc_at, before.last_gc_at);
    assert_eq!(after.last_attempt_at, before.last_attempt_at);
    assert_eq!(fs::read(repository.join(".git/index")).unwrap(), index);
    assert_eq!(git(&repository, &["rev-parse", "HEAD"]), head);
    assert_eq!(
        fs::read_to_string(repository.join("tracked.txt")).unwrap(),
        "uncommitted change\n"
    );
    assert_eq!(
        fs::read_to_string(repository.join("untracked.txt")).unwrap(),
        "untracked content\n"
    );
    assert!(!fixture.data.join("last-run.json").exists());
    let report = fixture.json("last-dry-run.json");
    assert_eq!(report["dry_run"], true);
    assert_eq!(report["repositories"].as_array().unwrap().len(), 1);
    assert_eq!(report["repositories"][0]["status"], "dry-run");
}

#[test]
fn real_gc_preserves_worktree_and_records_accounting_then_respects_daily_limit() {
    let fixture = Fixture::new();
    let repository = fixture.repository("sample");
    for revision in 0..8 {
        fs::write(
            repository.join("tracked.txt"),
            format!("revision {revision}\n{}", "data".repeat(4096)),
        )
        .unwrap();
        git(&repository, &["add", "tracked.txt"]);
        git(
            &repository,
            &["commit", "--quiet", "-m", "Fixture revision"],
        );
    }
    fs::write(repository.join("tracked.txt"), "uncommitted change\n").unwrap();
    fs::write(repository.join("untracked.txt"), "untracked content\n").unwrap();
    let index = fs::read(repository.join(".git/index")).unwrap();
    let head = git(&repository, &["rev-parse", "HEAD"]);
    // Establish an ineligible timestamp, proving --force overrides cadence.
    fixture.update_state(|state| state.last_gc_at = Some(now()));

    success(&fixture.run(&["run", "--force"]));

    assert_eq!(fs::read(repository.join(".git/index")).unwrap(), index);
    assert_eq!(git(&repository, &["rev-parse", "HEAD"]), head);
    assert_eq!(
        fs::read_to_string(repository.join("tracked.txt")).unwrap(),
        "uncommitted change\n"
    );
    assert_eq!(
        fs::read_to_string(repository.join("untracked.txt")).unwrap(),
        "untracked content\n"
    );
    let state = fixture.state();
    assert!(state.last_gc_at.is_some());
    assert!(state.last_attempt_at.is_some());
    let report = fixture.json("last-run.json");
    assert_eq!(report["dry_run"], false);
    assert_eq!(report["errors"], 0);
    assert_eq!(report["repositories"][0]["status"], "collected");
    for key in [
        "before_bytes",
        "after_bytes",
        "before_allocated_bytes",
        "after_allocated_bytes",
    ] {
        assert!(
            report["repositories"][0][key].as_u64().unwrap() > 0,
            "missing accounting field {key}"
        );
    }
    let report_bytes = fs::read(fixture.data.join("last-run.json")).unwrap();
    let second = fixture.run(&["run"]);
    success(&second);
    assert!(String::from_utf8_lossy(&second.stdout).contains("already completed"));
    assert_eq!(
        fs::read(fixture.data.join("last-run.json")).unwrap(),
        report_bytes
    );
}

#[test]
fn weekly_cache_retains_discovery_until_forced_and_finds_nested_checkout() {
    let fixture = Fixture::new();
    fixture.repository("outer");
    success(&fixture.run(&["scan"]));
    let initial_cache = fs::read(fixture.data.join("repos.json")).unwrap();
    assert_eq!(
        fixture.json("repos.json")["repositories"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    fixture.repository("outer/nested");

    success(&fixture.run(&["scan"]));
    assert_eq!(
        fs::read(fixture.data.join("repos.json")).unwrap(),
        initial_cache
    );
    success(&fixture.run(&["scan", "--force"]));
    assert_eq!(
        fixture.json("repos.json")["repositories"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn already_completed_scheduled_run_skips_long_probe_and_discovery() {
    let fixture = Fixture::new();
    fixture.update_config(|config| config.idle.sample_seconds = 300);
    fixture.update_state(|state| state.last_gc_at = Some(now()));
    let state_before = fs::read(fixture.data.join("state.json")).unwrap();
    let start = Instant::now();

    success(&fixture.run(&["scheduled"]));

    assert!(
        start.elapsed() < Duration::from_secs(2),
        "daily skip unexpectedly probed resources"
    );
    assert_eq!(
        fs::read(fixture.data.join("state.json")).unwrap(),
        state_before
    );
    assert!(!fixture.data.join("repos.json").exists());
    assert!(!fixture.data.join("last-run.json").exists());
    let log = fs::read_to_string(fixture.data.join("logs/debug.log")).unwrap();
    assert!(log.contains("resource sampling skipped"));
}

#[test]
fn corrupt_config_and_state_fail_without_overwriting_evidence() {
    let fixture = Fixture::new();
    let config_path = fixture.data.join("config.toml");
    let state_path = fixture.data.join("state.json");
    let good_config = fs::read(&config_path).unwrap();
    let good_state = fs::read(&state_path).unwrap();
    let bad_config = b"roots = [unterminated";
    fs::write(&config_path, bad_config).unwrap();

    let output = fixture.run(&["run", "--force"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("parse"));
    assert_eq!(fs::read(&config_path).unwrap(), bad_config);
    assert_eq!(fs::read(&state_path).unwrap(), good_state);

    fs::write(&config_path, &good_config).unwrap();
    let bad_state = b"{invalid state";
    fs::write(&state_path, bad_state).unwrap();
    let output = fixture.run(&["run", "--force"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Invalid JSON"));
    assert_eq!(fs::read(&state_path).unwrap(), bad_state);
    assert_eq!(fs::read(&config_path).unwrap(), good_config);
    assert!(!fixture.data.join("repos.json").exists());
    assert!(!fixture.data.join("last-run.json").exists());
}

#[test]
fn all_locked_repositories_do_not_count_as_completed_maintenance() {
    let fixture = Fixture::new();
    let repository = fixture.repository("locked");
    success(&fixture.run(&["scan"]));
    let lock = repository.join(".git/index.lock");
    fs::write(&lock, "belongs to another Git operation").unwrap();

    let output = fixture.run(&["run", "--force"]);

    assert!(!output.status.success());
    assert!(fixture.state().last_gc_at.is_none());
    assert!(fixture.state().last_attempt_at.is_some());
    assert_eq!(
        fs::read_to_string(lock).unwrap(),
        "belongs to another Git operation"
    );
    let report = fixture.json("last-run.json");
    assert_eq!(report["repositories"][0]["status"], "locked");
    assert_eq!(report["errors"], 1);
}

#[test]
fn gc_skips_source_of_shared_clone_and_keeps_borrower_readable() {
    let fixture = Fixture::new();
    let source = fixture.repository("source");
    let original = git(&source, &["rev-parse", "HEAD"]);
    let borrower = fixture.root.join("borrower");
    git(
        &fixture.root,
        &[
            "clone",
            "--quiet",
            "--shared",
            source.to_str().unwrap(),
            borrower.to_str().unwrap(),
        ],
    );
    let source_store = source.join(".git").canonicalize().unwrap();

    success(&fixture.run(&["run", "--force"]));

    let report = fixture.json("last-run.json");
    let results = report["repositories"].as_array().unwrap();
    assert_eq!(results.len(), 2);
    let source_result = results
        .iter()
        .find(|result| result["repo"] == source_store.to_str().unwrap())
        .unwrap();
    assert_eq!(source_result["status"], "skipped_alternate_source");
    assert_eq!(source_result["measurements_complete"], false);
    for field in [
        "before_bytes",
        "after_bytes",
        "before_allocated_bytes",
        "after_allocated_bytes",
    ] {
        assert_eq!(
            source_result[field], 0,
            "skipped source must not claim measured savings"
        );
    }
    let borrower_result = results
        .iter()
        .find(|result| result["status"] == "collected")
        .unwrap();
    let borrower_savings = borrower_result["before_allocated_bytes"].as_u64().unwrap() as i128
        - borrower_result["after_allocated_bytes"].as_u64().unwrap() as i128;
    assert_eq!(
        report["saved_allocated_bytes"].as_i64().unwrap() as i128,
        borrower_savings
    );
    assert_eq!(report["errors"], 0);
    assert!(fixture.state().last_gc_at.is_some());
    assert_eq!(git(&borrower, &["rev-parse", "HEAD"]), original);
    git(&borrower, &["fsck", "--full"]);
}

#[test]
fn unreadable_cached_alternates_abort_before_any_gc() {
    let fixture = Fixture::new();
    let source = fixture.repository("source");
    let borrower = fixture.root.join("borrower");
    git(
        &fixture.root,
        &[
            "clone",
            "--quiet",
            "--shared",
            source.to_str().unwrap(),
            borrower.to_str().unwrap(),
        ],
    );
    success(&fixture.run(&["scan"]));
    let source_store = source.join(".git").canonicalize().unwrap();
    let before = file_manifest(&source_store);
    let alternates = borrower.join(".git/objects/info/alternates");
    fs::remove_file(&alternates).unwrap();
    fs::create_dir(&alternates).unwrap();

    let output = fixture.run(&["run", "--force"]);

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("alternates"));
    assert_eq!(file_manifest(&source_store), before);
    assert!(fixture.state().last_gc_at.is_none());
    assert!(!fixture.data.join("last-run.json").exists());
}

#[test]
fn interrupted_pass_resumes_without_maintaining_already_attempted_store() {
    let fixture = Fixture::new();
    let first = fixture.repository("first");
    let second = fixture.repository("second");
    success(&fixture.run(&["scan"]));
    let first_store = first.join(".git").canonicalize().unwrap();
    let second_store = second.join(".git").canonicalize().unwrap();
    let before = file_manifest(&first_store);
    let pass_start = now().saturating_sub(1);
    let prior_result = serde_json::json!({
        "repo": first_store,
        "before_bytes": 1024,
        "after_bytes": 512,
        "before_allocated_bytes": 8192,
        "after_allocated_bytes": 4096,
        "measurements_complete": true,
        "status": "collected",
        "detail": "previous partial run"
    });
    let progress = serde_json::json!({
        "version": 1,
        "started_at": pass_start,
        "attempted": [first_store],
        "successful": 1,
        "errors": 0,
    });
    fs::write(
        fixture.data.join("progress.json"),
        serde_json::to_vec_pretty(&progress).unwrap(),
    )
    .unwrap();
    // Legacy checkpoints lacked per-repository results. Their latest partial
    // report can still supply the details when the next invocation resumes.
    let partial_report = serde_json::json!({
        "started_at": pass_start,
        "finished_at": now(),
        "dry_run": false,
        "interrupted": true,
        "errors": 0,
        "repositories": [prior_result],
        "failures": [],
        "saved_bytes": 512,
        "saved_allocated_bytes": 4096
    });
    fs::write(
        fixture.data.join("last-run.json"),
        serde_json::to_vec_pretty(&partial_report).unwrap(),
    )
    .unwrap();

    let progress_before = fs::read(fixture.data.join("progress.json")).unwrap();
    success(&fixture.run(&["run", "--dry-run"]));
    assert_eq!(
        fs::read(fixture.data.join("progress.json")).unwrap(),
        progress_before,
        "read-only inventory modified resumable maintenance progress"
    );

    success(&fixture.run(&["run"]));

    assert_eq!(file_manifest(&first_store), before);
    assert!(fixture.state().last_gc_at.is_some());
    assert!(!fixture.data.join("progress.json").exists());
    let report = fixture.json("last-run.json");
    let repositories = report["repositories"].as_array().unwrap();
    assert_eq!(repositories.len(), 2, "completed store was processed again");
    assert_eq!(repositories[0]["repo"], first_store.to_str().unwrap());
    assert_eq!(repositories[0]["status"], "collected");
    assert_eq!(repositories[1]["repo"], second_store.to_str().unwrap());
    assert_eq!(repositories[1]["status"], "collected");
    assert_eq!(report["started_at"], pass_start);
    assert_eq!(
        report["saved_allocated_bytes"].as_i64().unwrap(),
        repositories
            .iter()
            .map(|result| {
                result["before_allocated_bytes"].as_i64().unwrap()
                    - result["after_allocated_bytes"].as_i64().unwrap()
            })
            .sum::<i64>()
    );
}

#[test]
fn resumed_pass_report_preserves_prior_savings_and_failures() {
    let fixture = Fixture::new();
    let first = fixture.repository("first");
    let failed = fixture.repository("failed");
    let second = fixture.repository("second");
    success(&fixture.run(&["scan"]));
    let first_store = first.join(".git").canonicalize().unwrap();
    let failed_store = failed.join(".git").canonicalize().unwrap();
    let second_store = second.join(".git").canonicalize().unwrap();
    let first_before = file_manifest(&first_store);
    let failed_before = file_manifest(&failed_store);
    let pass_start = now().saturating_sub(60);
    let progress = serde_json::json!({
        "version": 1,
        "started_at": pass_start,
        "attempted": [first_store, failed_store],
        "successful": 1,
        "errors": 1,
        "repositories": [{
            "repo": first_store,
            "before_bytes": 1024,
            "after_bytes": 512,
            "before_allocated_bytes": 8192,
            "after_allocated_bytes": 4096,
            "measurements_complete": true,
            "status": "collected",
            "detail": "previous partial run"
        }],
        "failures": [{
            "repository": failed_store,
            "error": "previous Git failure"
        }]
    });
    fs::write(
        fixture.data.join("progress.json"),
        serde_json::to_vec_pretty(&progress).unwrap(),
    )
    .unwrap();

    let output = fixture.run(&["run"]);

    assert!(
        !output.status.success(),
        "prior failure must remain visible"
    );
    assert_eq!(file_manifest(&first_store), first_before);
    assert_eq!(file_manifest(&failed_store), failed_before);
    assert!(fixture.state().last_gc_at.is_some());
    assert!(!fixture.data.join("progress.json").exists());
    let report = fixture.json("last-run.json");
    assert_eq!(report["started_at"], pass_start);
    assert_eq!(report["errors"], 1);
    let repositories = report["repositories"].as_array().unwrap();
    assert_eq!(repositories.len(), 2);
    assert_eq!(repositories[0]["repo"], first_store.to_str().unwrap());
    assert_eq!(repositories[1]["repo"], second_store.to_str().unwrap());
    assert_eq!(repositories[1]["status"], "collected");
    assert_eq!(
        report["failures"][0]["repository"],
        failed_store.to_str().unwrap()
    );
    assert_eq!(report["failures"][0]["error"], "previous Git failure");
    assert_eq!(
        report["saved_bytes"].as_i64().unwrap(),
        repositories
            .iter()
            .map(|result| {
                result["before_bytes"].as_i64().unwrap() - result["after_bytes"].as_i64().unwrap()
            })
            .sum::<i64>()
    );
    assert_eq!(
        report["saved_allocated_bytes"].as_i64().unwrap(),
        repositories
            .iter()
            .map(|result| {
                result["before_allocated_bytes"].as_i64().unwrap()
                    - result["after_allocated_bytes"].as_i64().unwrap()
            })
            .sum::<i64>()
    );
}

#[test]
fn rejects_non_executable_configured_git_without_advancing_state() {
    let fixture = Fixture::new();
    fixture.repository("sample");
    let fake_git = fixture.root.join("not-executable-git");
    fs::write(&fake_git, "not a Git executable\n").unwrap();
    fs::set_permissions(&fake_git, fs::Permissions::from_mode(0o600)).unwrap();
    fixture.update_config(|config| config.git = fake_git.clone());
    let state_before = fs::read(fixture.data.join("state.json")).unwrap();

    let output = fixture.run(&["run", "--force"]);

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("not executable"));
    assert_eq!(
        fs::read(fixture.data.join("state.json")).unwrap(),
        state_before
    );
    assert!(!fixture.data.join("repos.json").exists());
    assert_eq!(
        fs::read_to_string(fake_git).unwrap(),
        "not a Git executable\n"
    );
}

#[test]
fn completed_pass_checkpoint_left_by_crash_does_not_skip_next_days_maintenance() {
    let fixture = Fixture::new();
    let repository = fixture.repository("sample");
    success(&fixture.run(&["scan"]));
    let store = repository.join(".git").canonicalize().unwrap();
    let previous_completion = now().saturating_sub(25 * 3600);
    fixture.update_state(|state| state.last_gc_at = Some(previous_completion));
    // Simulate a crash between saving completed state and unlinking progress.
    let stale_progress = serde_json::json!({
        "version": 1,
        "started_at": previous_completion.saturating_sub(60),
        "attempted": [store],
        "successful": 1,
        "errors": 0,
    });
    fs::write(
        fixture.data.join("progress.json"),
        serde_json::to_vec_pretty(&stale_progress).unwrap(),
    )
    .unwrap();

    success(&fixture.run(&["run"]));

    let report = fixture.json("last-run.json");
    let repositories = report["repositories"].as_array().unwrap();
    assert_eq!(
        repositories.len(),
        1,
        "stale checkpoint skipped today's pass"
    );
    assert_eq!(repositories[0]["repo"], store.to_str().unwrap());
    assert_eq!(repositories[0]["status"], "collected");
    assert!(fixture.state().last_gc_at.unwrap() > previous_completion);
    assert!(!fixture.data.join("progress.json").exists());
}

#[test]
fn repeated_init_never_overwrites_existing_configuration_or_state() {
    let fixture = Fixture::new();
    let config = fs::read(fixture.data.join("config.toml")).unwrap();
    let state = fs::read(fixture.data.join("state.json")).unwrap();

    let output = fixture.run(&["init", "--no-notifications"]);

    assert!(!output.status.success());
    assert_eq!(fs::read(fixture.data.join("config.toml")).unwrap(), config);
    assert_eq!(fs::read(fixture.data.join("state.json")).unwrap(), state);
}

#[test]
fn status_on_fresh_data_directory_is_read_only() {
    let temporary = tempfile::tempdir().unwrap();
    let data = temporary.path().join("not-created");
    let output = Command::new(env!("CARGO_BIN_EXE_git-sweep"))
        .env("GIT_SWEEP_HOME", &data)
        .args(["status", "--json"])
        .output()
        .unwrap();

    success(&output);
    assert!(!data.exists());
    let status: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(status["cached_repositories"], 0);
    assert!(status["state"]["last_gc_at"].is_null());
}
