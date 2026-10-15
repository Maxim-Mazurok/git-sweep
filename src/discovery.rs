//! Recursive discovery using directory entries and Git metadata only.

use crate::gc::run_git;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

const WARNING_LIMIT: usize = 50;
const ALTERNATES_LIMIT: u64 = 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Repository {
    /// Canonical common Git directory. Shared worktrees appear only once.
    pub git_dir: PathBuf,
    /// Checkout paths, bare repositories, and discovered submodule stores.
    pub paths: Vec<PathBuf>,
}

#[derive(Debug, Serialize)]
pub struct DiscoveryReport {
    pub repositories: Vec<Repository>,
    pub warnings: Vec<String>,
    pub directories_scanned: u64,
}

/// Find discovered object stores borrowed through `objects/info/alternates`.
///
/// Git GC in a source repository can remove objects that its own refs no longer
/// need, even when a `git clone --shared` borrower still needs them. This check
/// runs on every maintenance pass, including passes using cached discovery.
/// A present but unreadable alternates file fails the entire check so no source
/// repository is collected under an incomplete dependency map.
pub fn borrowed_sources(repositories: &[Repository]) -> Result<BTreeMap<PathBuf, Vec<PathBuf>>> {
    let mut stores_by_objects: BTreeMap<PathBuf, Vec<PathBuf>> = BTreeMap::new();
    for repo in repositories {
        let objects = repo.git_dir.join("objects");
        match fs::canonicalize(&objects) {
            Ok(path) => stores_by_objects
                .entry(path)
                .or_default()
                .push(repo.git_dir.clone()),
            // A cached repository may have been removed since discovery. It
            // cannot be collected, so it need not be a dependency target.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => {
                return Err(error).with_context(|| format!("resolve {}", objects.display()));
            }
        }
    }

    let mut borrowed: BTreeMap<PathBuf, Vec<PathBuf>> = BTreeMap::new();
    for repo in repositories {
        let objects = repo.git_dir.join("objects");
        let alternates = objects.join("info/alternates");
        // Nonblocking open prevents a FIFO in an untrusted Git directory from
        // hanging the scheduled pass. Symlinks are ambiguous, so defer instead.
        let file = match fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
            .open(&alternates)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // A dangling symlink is still a present alternates marker.
                // Treat it as uncertain rather than silently ignoring it.
                match fs::symlink_metadata(&alternates) {
                    Err(missing) if missing.kind() == std::io::ErrorKind::NotFound => continue,
                    _ => {
                        return Err(error).with_context(|| {
                            format!("read alternate object stores in {}", alternates.display())
                        });
                    }
                }
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("read alternate object stores in {}", alternates.display())
                });
            }
        };
        if !file.metadata()?.is_file() {
            anyhow::bail!(
                "alternate object store list is not a regular file: {}",
                alternates.display()
            );
        }
        let mut contents = Vec::new();
        file.take(ALTERNATES_LIMIT + 1)
            .read_to_end(&mut contents)
            .with_context(|| format!("read alternate object stores in {}", alternates.display()))?;
        if contents.len() as u64 > ALTERNATES_LIMIT {
            anyhow::bail!(
                "alternate object store list exceeds {ALTERNATES_LIMIT} bytes: {}",
                alternates.display()
            );
        }
        let mut entries = contents.split(|byte| *byte == b'\n').peekable();
        while let Some(entry) = entries.next() {
            if entry.is_empty() && entries.peek().is_none() {
                break; // ordinary final newline
            }
            if entry.is_empty() || entry.contains(&0) || entry.ends_with(b"\r") {
                anyhow::bail!(
                    "invalid alternate object store path in {}",
                    alternates.display()
                );
            }
            let alternate = Path::new(std::ffi::OsStr::from_bytes(entry));
            let alternate = if alternate.is_absolute() {
                alternate.to_path_buf()
            } else {
                objects.join(alternate)
            };
            let alternate = match fs::canonicalize(&alternate) {
                Ok(path) => path,
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("resolve alternate object store {}", alternate.display())
                    });
                }
            };
            if let Some(sources) = stores_by_objects.get(&alternate) {
                for source in sources {
                    let borrowers = borrowed.entry(source.clone()).or_default();
                    if !borrowers.contains(&repo.git_dir) {
                        borrowers.push(repo.git_dir.clone());
                    }
                }
            }
        }
    }
    Ok(borrowed)
}

struct Scanner<'a> {
    git: &'a Path,
    should_stop: &'a dyn Fn() -> bool,
    interrupted: bool,
    excludes: Vec<PathBuf>,
    repositories: BTreeMap<PathBuf, Repository>,
    seen_dirs: HashSet<PathBuf>,
    seen_stores: HashSet<PathBuf>,
    warnings: Vec<String>,
    suppressed: u64,
    directories_scanned: u64,
}

impl Scanner<'_> {
    fn warn(&mut self, message: String) {
        if self.warnings.len() < WARNING_LIMIT {
            self.warnings.push(message);
        } else {
            self.suppressed += 1;
        }
    }

    fn excluded(&self, path: &Path) -> bool {
        self.excludes
            .iter()
            .any(|exclude| path.starts_with(exclude))
    }

    fn register(&mut self, candidate: &Path, checkout: &Path, gitfile: bool) -> bool {
        if self.interrupted || (self.should_stop)() {
            self.interrupted = true;
            return false;
        }
        let canonical = match fs::canonicalize(candidate) {
            Ok(value) => value,
            Err(error) => {
                self.warn(format!("{}: {error}", candidate.display()));
                return false;
            }
        };
        if self.excluded(&canonical) {
            return false;
        }
        // A gitfile referring to an ancestor's ordinary .git directory is an
        // incidental marker (used by some build/cache tools), not a checkout.
        if gitfile && canonical.file_name().is_some_and(|name| name == ".git") {
            if let Some(parent) = canonical.parent() {
                if checkout != parent && checkout.starts_with(parent) {
                    return false;
                }
            }
        }
        let output = match run_git(
            self.git,
            &canonical,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            Duration::from_secs(15),
            self.should_stop,
        ) {
            Ok(value) if value.success() => value,
            Ok(value) if value.cancelled => {
                self.interrupted = true;
                return false;
            }
            Ok(value) => {
                self.warn(format!("{}: {}", candidate.display(), value.description()));
                return false;
            }
            Err(error) => {
                self.warn(format!("{}: {error:#}", candidate.display()));
                return false;
            }
        };
        let common_path = output.stdout.strip_suffix('\n').unwrap_or(&output.stdout);
        let common = match fs::canonicalize(common_path) {
            Ok(value) => value,
            Err(error) => {
                self.warn(format!("{}: {error}", candidate.display()));
                return false;
            }
        };
        if self.excluded(&common) {
            return false;
        }
        let repo = self
            .repositories
            .entry(common.clone())
            .or_insert_with(|| Repository {
                git_dir: common.clone(),
                paths: Vec::new(),
            });
        if !repo.paths.contains(&checkout.to_path_buf()) {
            repo.paths.push(checkout.to_path_buf());
        }
        if self.seen_stores.insert(common.clone()) {
            self.scan_modules(&common.join("modules"));
        }
        true
    }

    fn scan_modules(&mut self, modules: &Path) {
        let mut pending = vec![modules.to_path_buf()];
        while let Some(path) = pending.pop() {
            if self.interrupted || (self.should_stop)() {
                self.interrupted = true;
                break;
            }
            if self.excluded(&path) {
                continue;
            }
            let metadata = match fs::symlink_metadata(&path) {
                Ok(value) if value.is_dir() => value,
                _ => continue,
            };
            if metadata.file_type().is_symlink() {
                continue;
            }
            if path.join("HEAD").is_file()
                && (path.join("objects").is_dir() || path.join("commondir").is_file())
                && self.register(&path, &path, false)
            {
                continue;
            }
            let entries = match fs::read_dir(&path) {
                Ok(value) => value,
                Err(error) => {
                    self.warn(format!("{}: {error}", path.display()));
                    continue;
                }
            };
            for entry in entries {
                match entry {
                    Ok(value) => pending.push(value.path()),
                    Err(error) => self.warn(format!("{}: {error}", path.display())),
                }
            }
        }
    }

    fn git_marker(&mut self, marker: &Path, checkout: &Path) {
        let metadata = match fs::symlink_metadata(marker) {
            Ok(value) => value,
            Err(error) => {
                self.warn(format!("{}: {error}", marker.display()));
                return;
            }
        };
        if metadata.file_type().is_symlink() {
            return;
        }
        if metadata.is_dir() {
            self.register(marker, checkout, false);
        } else if metadata.is_file() {
            if metadata.len() > 16 * 1024 {
                self.warn(format!(
                    "{}: oversized .git marker skipped",
                    marker.display()
                ));
                return;
            }
            let read_marker = || -> std::io::Result<String> {
                let mut text = String::new();
                fs::File::open(marker)?
                    .take(16 * 1024 + 1)
                    .read_to_string(&mut text)?;
                if text.len() > 16 * 1024 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "oversized .git marker",
                    ));
                }
                Ok(text)
            };
            match read_marker() {
                Ok(text) => {
                    if let Some(target) =
                        text.trim_end_matches(['\n', '\r']).strip_prefix("gitdir: ")
                    {
                        let target = Path::new(target);
                        let resolved = if target.is_absolute() {
                            target.to_path_buf()
                        } else {
                            checkout.join(target)
                        };
                        self.register(&resolved, checkout, true);
                    } else {
                        self.warn(format!("{}: invalid .git marker", marker.display()));
                    }
                }
                Err(error) => self.warn(format!("{}: {error}", marker.display())),
            }
        }
    }

    fn scan(&mut self, roots: &[PathBuf]) {
        let mut pending = roots.to_vec();
        while let Some(path) = pending.pop() {
            if self.interrupted || (self.should_stop)() {
                self.interrupted = true;
                break;
            }
            if self.excluded(&path) {
                continue;
            }
            let metadata = match fs::symlink_metadata(&path) {
                Ok(value) => value,
                Err(error) => {
                    self.warn(format!("{}: {error}", path.display()));
                    continue;
                }
            };
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                continue;
            }
            let canonical = match fs::canonicalize(&path) {
                Ok(value) => value,
                Err(error) => {
                    self.warn(format!("{}: {error}", path.display()));
                    continue;
                }
            };
            if self.excluded(&canonical) || !self.seen_dirs.insert(canonical.clone()) {
                continue;
            }
            let entries = match fs::read_dir(&canonical) {
                Ok(value) => value,
                Err(error) => {
                    self.warn(format!("{}: {error}", canonical.display()));
                    continue;
                }
            };
            self.directories_scanned += 1;
            let mut head = false;
            let mut objects = false;
            let mut config = false;
            let mut children = Vec::new();
            for entry in entries {
                match entry {
                    Ok(entry) => {
                        let name = entry.file_name();
                        if name == ".git" {
                            self.git_marker(&entry.path(), &canonical);
                            continue;
                        }
                        head |= name == "HEAD";
                        objects |= name == "objects";
                        config |= name == "config";
                        if matches!(
                            name.to_str(),
                            Some(".Trash" | ".Trashes" | ".Spotlight-V100" | ".fseventsd")
                        ) {
                            continue;
                        }
                        match entry.file_type() {
                            Ok(kind) if kind.is_dir() => children.push(entry.path()),
                            Err(error) => self.warn(format!("{}: {error}", entry.path().display())),
                            _ => (),
                        }
                    }
                    Err(error) => self.warn(format!("{}: {error}", canonical.display())),
                }
            }
            if head
                && objects
                && config
                && canonical.join("HEAD").is_file()
                && canonical.join("objects").is_dir()
            {
                let valid_store = self.register(&canonical, &canonical, false);
                // A bare repository is storage, not a working tree. Never walk
                // its objects, refs, hooks, or external object alternates.
                if valid_store {
                    continue;
                }
            }
            pending.extend(children);
        }
    }
}

/// Recursively discover repositories, including nested checkouts, bare repos,
/// submodules, and linked worktrees. Filesystem failures are bounded warnings.
/// Gitfiles may reference stores outside the scan roots; explicit exclusions
/// still apply to resolved common stores.
pub fn discover(roots: &[PathBuf], excludes: &[PathBuf], git: &Path) -> Result<DiscoveryReport> {
    discover_with_cancel(roots, excludes, git, &|| false)
}

/// A cancelled scan returns an error, so callers cannot mistake partial results
/// for a complete replacement of their repository cache.
pub fn discover_with_cancel(
    roots: &[PathBuf],
    excludes: &[PathBuf],
    git: &Path,
    should_stop: &dyn Fn() -> bool,
) -> Result<DiscoveryReport> {
    anyhow::ensure!(!roots.is_empty(), "at least one discovery root is required");
    let cwd = std::env::current_dir().context("resolve current directory")?;
    let absolute = |path: &Path| {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            cwd.join(path)
        }
    };
    let excludes = excludes
        .iter()
        .map(|path| {
            let path = absolute(path);
            fs::canonicalize(&path).unwrap_or(path)
        })
        .collect();
    let mut scanner = Scanner {
        git,
        should_stop,
        interrupted: false,
        excludes,
        repositories: BTreeMap::new(),
        seen_dirs: HashSet::new(),
        seen_stores: HashSet::new(),
        warnings: Vec::new(),
        suppressed: 0,
        directories_scanned: 0,
    };
    scanner.scan(&roots.iter().map(|path| absolute(path)).collect::<Vec<_>>());
    anyhow::ensure!(
        !scanner.interrupted,
        "repository discovery interrupted; complete cache preserved"
    );
    anyhow::ensure!(
        scanner.directories_scanned > 0,
        "no discovery root could be scanned"
    );
    if scanner.suppressed > 0 {
        scanner.warnings.push(format!(
            "{} additional discovery warnings suppressed",
            scanner.suppressed
        ));
    }
    let mut repositories = scanner.repositories.into_values().collect::<Vec<_>>();
    for repo in &mut repositories {
        repo.paths.sort();
    }
    Ok(DiscoveryReport {
        repositories,
        warnings: scanner.warnings,
        directories_scanned: scanner.directories_scanned,
    })
}
