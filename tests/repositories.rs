use git_sweep::discovery::{borrowed_sources, discover, discover_with_cancel};
use git_sweep::gc::collect;
use std::ffi::CString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};
use tempfile::TempDir;

fn git(directory: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(directory)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .args([
            "-c",
            "user.name=Git Sweep Tests",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "tag.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn init(path: &Path) {
    fs::create_dir_all(path).unwrap();
    git(path, &["init", "-q"]);
    git(path, &["config", "gc.pruneExpire", "never"]);
    git(path, &["config", "gc.reflogExpire", "never"]);
    git(path, &["config", "gc.reflogExpireUnreachable", "never"]);
}

fn commit(path: &Path) {
    fs::write(path.join("tracked.txt"), "committed\n").unwrap();
    git(path, &["add", "tracked.txt"]);
    git(path, &["commit", "-q", "-m", "Initial commit"]);
}

fn canonical(path: impl AsRef<Path>) -> PathBuf {
    fs::canonicalize(path).unwrap()
}

#[test]
fn recursive_discovery_deduplicates_worktrees_and_finds_bare_and_nested_repos() {
    let temp = TempDir::new().unwrap();
    let main = temp.path().join("main");
    init(&main);
    commit(&main);
    let nested = main.join("checkouts/nested-project");
    init(&nested);
    let linked = temp.path().join("linked checkout");
    git(
        &main,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "linked",
            linked.to_str().unwrap(),
        ],
    );
    let bare = temp.path().join("bare.git");
    git(
        temp.path(),
        &["init", "-q", "--bare", bare.to_str().unwrap()],
    );
    symlink(temp.path(), temp.path().join("loop")).unwrap();
    let report = discover(
        &[temp.path().to_path_buf(), main.clone()],
        &[],
        Path::new("git"),
    )
    .unwrap();
    assert_eq!(report.repositories.len(), 3, "{:?}", report.warnings);
    let main_repo = report
        .repositories
        .iter()
        .find(|repo| repo.git_dir == canonical(main.join(".git")))
        .unwrap();
    assert!(main_repo.paths.contains(&canonical(&main)));
    assert!(main_repo.paths.contains(&canonical(&linked)));
    assert!(
        report
            .repositories
            .iter()
            .any(|repo| repo.git_dir == canonical(nested.join(".git")))
    );
    assert!(
        report
            .repositories
            .iter()
            .any(|repo| repo.git_dir == canonical(&bare))
    );
    assert!(
        report.directories_scanned < 20,
        "must not scan Git object trees or symlink loops"
    );
    let git_root = main.join(".git");
    let explicit = discover(std::slice::from_ref(&git_root), &[], Path::new("git")).unwrap();
    assert_eq!(explicit.repositories.len(), 1);
    assert_eq!(explicit.repositories[0].git_dir, canonical(git_root));
}

#[test]
fn discovers_absorbed_submodules_and_respects_exclusions_and_false_markers() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("source");
    init(&source);
    commit(&source);
    let main = temp.path().join("main");
    init(&main);
    commit(&main);
    git(
        &main,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "-q",
            source.to_str().unwrap(),
            "nested/sub",
        ],
    );
    let false_checkout = main.join("cache/false");
    fs::create_dir_all(&false_checkout).unwrap();
    fs::write(false_checkout.join(".git"), "gitdir: ../../.git\n").unwrap();
    let excluded = main.join("ignored");
    init(&excluded);
    let report = discover(std::slice::from_ref(&main), &[excluded], Path::new("git")).unwrap();
    assert_eq!(report.repositories.len(), 2, "{:?}", report.warnings);
    let main_repo = report
        .repositories
        .iter()
        .find(|repo| repo.git_dir == canonical(main.join(".git")))
        .unwrap();
    assert_eq!(main_repo.paths, vec![canonical(&main)]);
    let module = canonical(main.join(".git/modules/nested/sub"));
    let sub = report
        .repositories
        .iter()
        .find(|repo| repo.git_dir == module)
        .unwrap();
    assert!(sub.paths.contains(&canonical(main.join("nested/sub"))));
    let before = collect(main_repo, Path::new("git"), true, &|| false).unwrap();
    let module_file = module.join("not-a-git-object");
    fs::write(&module_file, vec![1_u8; 100_000]).unwrap();
    let after = collect(main_repo, Path::new("git"), true, &|| false).unwrap();
    assert_eq!(
        before.before_bytes, after.before_bytes,
        "module storage must be accounted separately"
    );
}

#[test]
fn dry_run_is_read_only_and_gc_preserves_working_files_and_references() {
    let temp = TempDir::new().unwrap();
    init(temp.path());
    for number in 0..40 {
        fs::write(
            temp.path().join(format!("data-{number}")),
            format!("file {number}\n{}", "compress me\n".repeat(300)),
        )
        .unwrap();
    }
    commit(temp.path());
    git(temp.path(), &["add", "."]);
    git(
        temp.path(),
        &["commit", "-q", "-m", "Add compressible files"],
    );
    fs::write(temp.path().join("tracked.txt"), "uncommitted edits\n").unwrap();
    fs::write(temp.path().join("untracked.txt"), "untracked work\n").unwrap();
    let refs = git(temp.path(), &["show-ref"]);
    let index_before = fs::read(temp.path().join(".git/index")).unwrap();
    let config_before = fs::read(temp.path().join(".git/config")).unwrap();
    let count_before = git(temp.path(), &["count-objects", "-v"]);
    let repo = discover(&[temp.path().to_path_buf()], &[], Path::new("git"))
        .unwrap()
        .repositories
        .remove(0);
    let dry = collect(&repo, Path::new("git"), true, &|| false).unwrap();
    assert_eq!(dry.status, "dry-run");
    assert_eq!(dry.before_bytes, dry.after_bytes);
    assert_eq!(count_before, git(temp.path(), &["count-objects", "-v"]));
    assert!(!temp.path().join(".git/gc.pid").exists());
    let result = collect(&repo, Path::new("git"), false, &|| false).unwrap();
    assert_eq!(result.status, "collected", "{}", result.detail);
    assert!(result.after_bytes < result.before_bytes, "{:?}", result);
    assert_eq!(refs, git(temp.path(), &["show-ref"]));
    assert_eq!(
        fs::read(temp.path().join(".git/index")).unwrap(),
        index_before
    );
    assert_eq!(
        fs::read(temp.path().join(".git/config")).unwrap(),
        config_before
    );
    assert_eq!(
        fs::read_to_string(temp.path().join("tracked.txt")).unwrap(),
        "uncommitted edits\n"
    );
    assert_eq!(
        fs::read_to_string(temp.path().join("untracked.txt")).unwrap(),
        "untracked work\n"
    );
    git(temp.path(), &["fsck", "--full"]);
}

#[test]
fn skips_locks_and_custom_retention_hooks_without_running_them() {
    let temp = TempDir::new().unwrap();
    init(temp.path());
    commit(temp.path());
    let repo = discover(&[temp.path().to_path_buf()], &[], Path::new("git"))
        .unwrap()
        .repositories
        .remove(0);
    let lock = repo.git_dir.join("index.lock");
    fs::write(&lock, "existing operation").unwrap();
    let locked = collect(&repo, Path::new("git"), false, &|| false).unwrap();
    assert_eq!(locked.status, "locked");
    assert_eq!(fs::read_to_string(&lock).unwrap(), "existing operation");
    fs::remove_file(lock).unwrap();
    let hook = temp.path().join("custom-retention-hook.sh");
    fs::write(&hook, "#!/bin/sh\ntouch hook-was-run\n").unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o700)).unwrap();
    git(
        temp.path(),
        &["config", "gc.recentObjectsHook", hook.to_str().unwrap()],
    );
    let skipped = collect(&repo, Path::new("git"), false, &|| false).unwrap();
    assert_eq!(skipped.status, "skipped_hook", "{}", skipped.detail);
    assert!(!temp.path().join("hook-was-run").exists());
    assert!(!repo.git_dir.join("hook-was-run").exists());
}

#[test]
fn cancellation_never_returns_partial_discovery_or_starts_gc() {
    let temp = TempDir::new().unwrap();
    init(temp.path());
    let repo = discover(&[temp.path().to_path_buf()], &[], Path::new("git"))
        .unwrap()
        .repositories
        .remove(0);
    assert!(
        discover_with_cancel(&[temp.path().to_path_buf()], &[], Path::new("git"), &|| {
            true
        })
        .is_err()
    );
    let result = collect(&repo, Path::new("git"), false, &|| true).unwrap();
    assert_eq!(result.status, "cancelled");
    assert!(!result.measurements_complete);
    assert!(!repo.git_dir.join("gc.pid").exists());
}

#[test]
fn unavailable_roots_do_not_return_an_empty_successful_scan() {
    let temp = TempDir::new().unwrap();
    let unavailable = temp.path().join("unreadable");
    fs::create_dir(&unavailable).unwrap();
    fs::set_permissions(&unavailable, fs::Permissions::from_mode(0o000)).unwrap();
    let can_read = fs::read_dir(&unavailable).is_ok();
    let result = discover(std::slice::from_ref(&unavailable), &[], Path::new("git"));
    fs::set_permissions(&unavailable, fs::Permissions::from_mode(0o700)).unwrap();
    if !can_read {
        assert!(
            result.is_err(),
            "unreadable roots must not overwrite a complete cache"
        );
    }
    assert!(discover(&[temp.path().join("missing")], &[], Path::new("git")).is_err());
}

#[test]
fn alternate_sources_are_detected_with_absolute_and_relative_paths() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("source");
    init(&source);
    commit(&source);
    let borrower = temp.path().join("borrower");
    git(
        temp.path(),
        &[
            "clone",
            "--quiet",
            "--shared",
            source.to_str().unwrap(),
            borrower.to_str().unwrap(),
        ],
    );
    let repositories = discover(&[temp.path().to_path_buf()], &[], Path::new("git"))
        .unwrap()
        .repositories;
    let source_store = canonical(source.join(".git"));
    let borrower_store = canonical(borrower.join(".git"));
    let dependencies = borrowed_sources(&repositories).unwrap();
    assert_eq!(dependencies[&source_store], vec![borrower_store.clone()]);

    // Relative paths in alternates are relative to the object directory.
    fs::write(
        borrower_store.join("objects/info/alternates"),
        "../../../source/.git/objects\n",
    )
    .unwrap();
    let dependencies = borrowed_sources(&repositories).unwrap();
    assert_eq!(dependencies[&source_store], vec![borrower_store.clone()]);

    // An uncertain dependency map must stop maintenance before any GC starts.
    let alternates = borrower_store.join("objects/info/alternates");
    fs::write(&alternates, "missing-objects\n").unwrap();
    assert!(borrowed_sources(&repositories).is_err());
    fs::write(&alternates, "../../../source/.git/objects\r\n").unwrap();
    assert!(borrowed_sources(&repositories).is_err());
    fs::remove_file(&alternates).unwrap();
    fs::create_dir(&alternates).unwrap();
    assert!(borrowed_sources(&repositories).is_err());
    fs::remove_dir(&alternates).unwrap();
    let fifo = CString::new(alternates.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    let start = Instant::now();
    assert!(borrowed_sources(&repositories).is_err());
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "FIFO blocked discovery"
    );
}

#[test]
fn interrupted_git_terminates_its_subprocess_group() {
    let temp = TempDir::new().unwrap();
    init(temp.path());
    let repo = discover(&[temp.path().to_path_buf()], &[], Path::new("git"))
        .unwrap()
        .repositories
        .remove(0);
    let fake_git = temp.path().join("fake-git");
    fs::write(&fake_git, "#!/bin/sh\nfor arg in \"$@\"; do\n case \"$arg\" in\n  --get-regexp) exit 1;;\n  --no-detach) sleep 60 &\n   printf '%s\\n' \"$!\" > \"$0.pid\"\n   wait;;\n esac\ndone\n").unwrap();
    fs::set_permissions(&fake_git, fs::Permissions::from_mode(0o700)).unwrap();
    let pid_file = temp.path().join("fake-git.pid");
    let started = Instant::now();
    let result = collect(&repo, &fake_git, false, &|| {
        // Cancel only after the fake Git process has created its descendant.
        // A fixed delay can expire during repository checks on a busy system.
        fs::read_to_string(&pid_file).is_ok_and(|text| text.trim().parse::<i32>().is_ok())
            || started.elapsed() > Duration::from_secs(10)
    })
    .unwrap();
    assert_eq!(result.status, "cancelled");
    assert!(started.elapsed() < Duration::from_secs(15));
    let pid: i32 = fs::read_to_string(&pid_file)
        .expect("fake Git did not create its descendant before the cancellation deadline")
        .trim()
        .parse()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        // SAFETY: signal 0 only checks whether the disposable subprocess exists.
        if unsafe { libc::kill(pid, 0) } != 0 {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("Git descendant {pid} survived process group cancellation");
}
