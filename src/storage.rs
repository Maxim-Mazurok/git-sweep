//! Private, durable state and bounded diagnostic logs.

use anyhow::{Context, Result, bail};
use flate2::{Compression, write::GzEncoder};
use fs2::FileExt;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Seek, SeekFrom, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const LIVE_LOG_LIMIT: u64 = 8 * 1024 * 1024;
const ARCHIVE_LOG_LIMIT: u64 = 40 * 1024 * 1024;
const MAX_LOG_MESSAGE: usize = 64 * 1024;
static UNIQUE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone)]
pub struct Paths {
    pub base: PathBuf,
    pub config: PathBuf,
    pub state: PathBuf,
    pub cache: PathBuf,
    pub logs: PathBuf,
    pub lock: PathBuf,
}

impl Paths {
    pub fn new() -> Result<Self> {
        let base = if let Some(value) = env::var_os("GIT_SWEEP_HOME") {
            if value.is_empty() {
                bail!("GIT_SWEEP_HOME must not be empty");
            }
            let path = PathBuf::from(value);
            if path.is_absolute() {
                path
            } else {
                env::current_dir()
                    .context("Cannot determine current directory")?
                    .canonicalize()
                    .context("Cannot resolve current directory")?
                    .join(path)
            }
        } else {
            let home = env::var_os("HOME").context("HOME is unset; set GIT_SWEEP_HOME")?;
            if home.is_empty() || !Path::new(&home).is_absolute() {
                bail!("HOME must be an absolute, nonempty path; set GIT_SWEEP_HOME");
            }
            PathBuf::from(home)
                .join("Library")
                .join("Application Support")
                .join("git-sweep")
        };
        Ok(Self::at(base))
    }

    fn at(base: PathBuf) -> Self {
        Self {
            config: base.join("config.toml"),
            state: base.join("state.json"),
            cache: base.join("repos.json"),
            logs: base.join("logs"),
            lock: base.join("instance.lock"),
            base,
        }
    }

    pub fn ensure(&self) -> Result<()> {
        private_directory(&self.base)?;
        private_directory(&self.logs)
    }
}

fn private_directory(path: &Path) -> Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder
        .create(path)
        .with_context(|| format!("Cannot create private directory {}", path.display()))?;
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("Cannot inspect directory {}", path.display()))?;
    if !metadata.file_type().is_dir() {
        bail!(
            "Expected a directory, not a symlink or file: {}",
            path.display()
        );
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("Cannot protect directory {}", path.display()))
}

#[derive(Serialize, Deserialize, Default, Clone, Debug)]
#[serde(default)]
pub struct State {
    pub initialized_at: u64,
    pub last_scan_at: Option<u64>,
    pub last_gc_at: Option<u64>,
    pub last_overdue_notification_at: Option<u64>,
    pub last_attempt_at: Option<u64>,
    pub last_outcome: Option<String>,
    pub notifications_permission_requested: bool,
}

pub fn load_state(paths: &Paths) -> Result<State> {
    Ok(read_json(&paths.state)?.unwrap_or_default())
}

pub fn save_state(paths: &Paths, state: &State) -> Result<()> {
    atomic_json(&paths.state, state)
}

pub fn read_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("Cannot read {}", path.display()));
        }
    };
    let value = serde_json::from_reader(BufReader::new(file)).with_context(|| {
        format!(
            "Invalid JSON in {}; repair or restore this file before continuing",
            path.display()
        )
    })?;
    Ok(Some(value))
}

pub fn atomic_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path
        .parent()
        .context("State path has no parent directory")?;
    let stem = path.file_name().context("State path has no filename")?;
    let mut temporary = None;
    for _ in 0..20 {
        let mut name = stem.to_os_string();
        name.push(format!(
            ".{}.{}.tmp",
            std::process::id(),
            UNIQUE.fetch_add(1, Ordering::Relaxed)
        ));
        let temporary_path = parent.join(name);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&temporary_path)
        {
            Ok(file) => {
                temporary = Some((TemporaryFile(temporary_path), file));
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("Cannot prepare atomic write to {}", path.display()));
            }
        }
    }
    let (temporary, mut file) = temporary.context("Cannot allocate a temporary state file")?;
    serde_json::to_writer_pretty(&mut file, value)
        .with_context(|| format!("Cannot encode {}", path.display()))?;
    file.write_all(b"\n")?;
    file.sync_all()
        .with_context(|| format!("Cannot sync temporary file for {}", path.display()))?;
    drop(file);
    fs::rename(&temporary.0, path)
        .with_context(|| format!("Cannot atomically replace {}", path.display()))?;
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .with_context(|| format!("Cannot sync directory {}", parent.display()))?;
    Ok(())
}

struct TemporaryFile(PathBuf);

impl Drop for TemporaryFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

pub struct InstanceLock {
    file: File,
}

/// The lock file stays on disk; the operating system releases the lock on exit.
pub fn acquire_lock(paths: &Paths) -> Result<Option<InstanceLock>> {
    paths.ensure()?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&paths.lock)
        .with_context(|| format!("Cannot open instance lock {}", paths.lock.display()))?;
    match FileExt::try_lock_exclusive(&file) {
        Ok(()) => Ok(Some(InstanceLock { file })),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(None),
        Err(error) => Err(error).context("Cannot lock git-sweep state"),
    }
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

/// The caller holds `InstanceLock` while writing logs. Pass diagnostic summaries
/// and filesystem paths, never repository credentials, URLs, or command output.
pub struct Logger {
    directory: PathBuf,
    file: File,
    live_limit: u64,
    archive_limit: u64,
}

impl Logger {
    pub fn new(paths: &Paths) -> Result<Self> {
        Self::with_limits(paths, LIVE_LOG_LIMIT, ARCHIVE_LOG_LIMIT)
    }

    fn with_limits(paths: &Paths, live_limit: u64, archive_limit: u64) -> Result<Self> {
        paths.ensure()?;
        let path = paths.logs.join("debug.log");
        let file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
            .with_context(|| format!("Cannot open log {}", path.display()))?;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        let mut logger = Self {
            directory: paths.logs.clone(),
            file,
            live_limit,
            archive_limit,
        };
        if logger.file.metadata()?.len() > live_limit {
            logger.rotate()?;
        }
        logger.prune()?;
        Ok(logger)
    }

    pub fn log(&mut self, level: &str, message: &str) -> Result<()> {
        let level = single_line(level, 32);
        let message = single_line(message, MAX_LOG_MESSAGE);
        let mut line = format!("{} [{}] {}\n", unix_now(), level, message);
        // Even unusual test limits or a huge message cannot overrun the live cap.
        if line.len() as u64 > self.live_limit {
            let limit = self.live_limit.saturating_sub(1) as usize;
            let mut end = limit.min(line.len());
            while !line.is_char_boundary(end) {
                end -= 1;
            }
            line.truncate(end);
            if self.live_limit > 0 {
                line.push('\n');
            }
        }
        if self
            .file
            .metadata()?
            .len()
            .saturating_add(line.len() as u64)
            > self.live_limit
        {
            self.rotate()?;
        }
        self.file
            .write_all(line.as_bytes())
            .context("Cannot append diagnostic log")
    }

    fn rotate(&mut self) -> Result<()> {
        if self.file.metadata()?.len() == 0 {
            return Ok(());
        }
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let filename = format!(
            "git-sweep-{:030}-{:010}-{:016}.log.gz",
            timestamp,
            std::process::id(),
            UNIQUE.fetch_add(1, Ordering::Relaxed)
        );
        let destination = self.directory.join(filename);
        let temporary = TemporaryFile(destination.with_extension("gz.tmp"));
        let archive = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&temporary.0)
            .context("Cannot create compressed log archive")?;
        self.file.seek(SeekFrom::Start(0))?;
        let mut encoder = GzEncoder::new(archive, Compression::fast());
        io::copy(&mut self.file, &mut encoder).context("Cannot compress diagnostic log")?;
        let archive = encoder.finish().context("Cannot finish compressed log")?;
        archive.sync_all()?;
        drop(archive);
        fs::rename(&temporary.0, &destination).context("Cannot publish compressed log archive")?;
        self.file
            .set_len(0)
            .context("Cannot reset live diagnostic log")?;
        self.prune()
    }

    fn prune(&self) -> Result<()> {
        let mut archives = Vec::new();
        let mut total = 0u64;
        for entry in fs::read_dir(&self.directory).context("Cannot list log archives")? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            // A crash during compression may leave an unpublished archive.
            // The process lock guarantees no writer still owns this temp file.
            if name.strip_suffix(".tmp").is_some_and(is_archive_name) {
                let metadata = fs::symlink_metadata(entry.path())?;
                if metadata.file_type().is_file() {
                    fs::remove_file(entry.path())
                        .context("Cannot remove incomplete compressed log archive")?;
                }
                continue;
            }
            if !is_archive_name(name) {
                continue;
            }
            let metadata = fs::symlink_metadata(entry.path())?;
            if !metadata.file_type().is_file() {
                continue;
            }
            total = total.saturating_add(metadata.len());
            archives.push((name.to_owned(), entry.path(), metadata.len()));
        }
        archives.sort_by(|left, right| left.0.cmp(&right.0));
        for (_, path, size) in archives {
            if total <= self.archive_limit {
                break;
            }
            fs::remove_file(&path)
                .with_context(|| format!("Cannot prune old log archive {}", path.display()))?;
            total = total.saturating_sub(size);
        }
        Ok(())
    }
}

fn is_archive_name(name: &str) -> bool {
    let Some(body) = name
        .strip_prefix("git-sweep-")
        .and_then(|body| body.strip_suffix(".log.gz"))
    else {
        return false;
    };
    let pieces: Vec<&str> = body.split('-').collect();
    pieces.len() == 3
        && pieces[0].len() >= 20
        && !pieces[1].is_empty()
        && !pieces[2].is_empty()
        && pieces
            .iter()
            .all(|piece| piece.bytes().all(|b| b.is_ascii_digit()))
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn single_line(value: &str, limit: usize) -> String {
    let mut output = String::with_capacity(value.len().min(limit));
    for character in value.chars() {
        if output.len() + 16 > limit {
            output.push_str(" [truncated]");
            break;
        }
        if character.is_control() || matches!(character, '\u{2028}' | '\u{2029}') {
            output.extend(character.escape_default());
        } else {
            output.push(character);
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::read::GzDecoder;
    use std::io::Read;
    use tempfile::TempDir;

    fn fixture() -> (TempDir, Paths) {
        let directory = tempfile::tempdir().unwrap();
        let paths = Paths::at(directory.path().join("private"));
        paths.ensure().unwrap();
        (directory, paths)
    }

    #[test]
    fn missing_state_defaults_and_atomic_state_roundtrips_privately() {
        let (_directory, paths) = fixture();
        assert_eq!(load_state(&paths).unwrap().last_gc_at, None);
        let mut state = State {
            initialized_at: 42,
            last_gc_at: Some(77),
            ..State::default()
        };
        save_state(&paths, &state).unwrap();
        state.last_gc_at = Some(99);
        save_state(&paths, &state).unwrap();
        let loaded = load_state(&paths).unwrap();
        assert_eq!(loaded.last_gc_at, Some(99));
        assert_eq!(loaded.initialized_at, 42);
        assert_eq!(
            fs::metadata(&paths.state).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&paths.base).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert!(!fs::read_dir(&paths.base).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp")
        }));
    }

    #[test]
    fn corrupted_state_is_preserved_and_reported() {
        let (_directory, paths) = fixture();
        fs::write(&paths.state, "{broken").unwrap();
        let error = load_state(&paths).unwrap_err().to_string();
        assert!(error.contains("Invalid JSON"));
        assert!(error.contains(&paths.state.display().to_string()));
        assert_eq!(fs::read_to_string(&paths.state).unwrap(), "{broken");
    }

    #[test]
    fn older_state_gets_new_field_defaults() {
        let (_directory, paths) = fixture();
        fs::write(&paths.state, r#"{"initialized_at":42}"#).unwrap();
        let loaded = load_state(&paths).unwrap();
        assert_eq!(loaded.initialized_at, 42);
        assert!(!loaded.notifications_permission_requested);
    }

    #[test]
    fn refuses_symlinked_state_lock_and_live_log() {
        use std::os::unix::fs::symlink;

        let (directory, paths) = fixture();
        let outside = directory.path().join("outside.json");
        fs::write(&outside, "{}").unwrap();
        symlink(&outside, &paths.state).unwrap();
        assert!(load_state(&paths).is_err());
        symlink(&outside, &paths.lock).unwrap();
        assert!(acquire_lock(&paths).is_err());
        symlink(&outside, paths.logs.join("debug.log")).unwrap();
        assert!(Logger::new(&paths).is_err());
        assert_eq!(fs::read_to_string(outside).unwrap(), "{}");
    }

    #[test]
    fn lock_excludes_another_instance_and_releases_on_drop() {
        let (_directory, paths) = fixture();
        let first = acquire_lock(&paths).unwrap().unwrap();
        assert!(acquire_lock(&paths).unwrap().is_none());
        drop(first);
        assert!(acquire_lock(&paths).unwrap().is_some());
    }

    #[test]
    fn rotates_compresses_and_evicts_without_touching_unrelated_files() {
        let (_directory, paths) = fixture();
        fs::write(paths.logs.join("unrelated.log.gz"), "keep").unwrap();
        let incomplete = paths.logs.join(
            "git-sweep-000000000000000000000000000001-0000000001-0000000000000001.log.gz.tmp",
        );
        fs::write(&incomplete, "incomplete").unwrap();
        let mut logger = Logger::with_limits(&paths, 160, 240).unwrap();
        assert!(!incomplete.exists());
        for index in 0..100 {
            logger
                .log("INFO", &format!("sample {index}: {}", "abcdefgh".repeat(8)))
                .unwrap();
        }
        let mut archive_bytes = 0;
        let mut archive_count = 0;
        for entry in fs::read_dir(&paths.logs).unwrap() {
            let entry = entry.unwrap();
            if is_archive_name(&entry.file_name().to_string_lossy()) {
                archive_bytes += entry.metadata().unwrap().len();
                archive_count += 1;
                let mut decoded = String::new();
                GzDecoder::new(File::open(entry.path()).unwrap())
                    .read_to_string(&mut decoded)
                    .unwrap();
                assert!(decoded.contains("sample"));
            }
        }
        assert!(archive_count > 0);
        assert!(archive_bytes <= 240);
        assert!(fs::metadata(paths.logs.join("debug.log")).unwrap().len() <= 160);
        assert_eq!(
            fs::read_to_string(paths.logs.join("unrelated.log.gz")).unwrap(),
            "keep"
        );
    }

    #[test]
    fn log_escapes_control_characters_and_bounds_oversized_messages() {
        let (_directory, paths) = fixture();
        let mut logger = Logger::with_limits(&paths, 160, 500).unwrap();
        logger.log("INFO", "first\nsecond\r\u{1b}").unwrap();
        let content = fs::read_to_string(paths.logs.join("debug.log")).unwrap();
        assert_eq!(content.lines().count(), 1);
        assert!(content.contains(r"first\nsecond\r\u{1b}"));
        logger.log("INFO", &"🙂".repeat(100_000)).unwrap();
        let content = fs::read_to_string(paths.logs.join("debug.log")).unwrap();
        assert!(content.len() <= 160);
        assert!(content.ends_with('\n'));
    }
}
