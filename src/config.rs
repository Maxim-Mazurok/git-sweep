use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::{platform::IdlePolicy, storage::Paths};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub roots: Vec<PathBuf>,
    pub excludes: Vec<PathBuf>,
    pub git: PathBuf,
    pub scan_interval_hours: u64,
    pub gc_interval_hours: u64,
    pub overdue_days: u64,
    pub notification_interval_hours: u64,
    pub notifications: bool,
    pub idle: IdlePolicy,
}

impl Default for Config {
    fn default() -> Self {
        let home = home();
        Self {
            roots: vec![home.clone()],
            excludes: vec![
                home.join("Library/CloudStorage"),
                home.join("Library/Mobile Documents"),
                home.join("Library/Group Containers/UBF8T346G9.OneDriveSyncClientSuite"),
                PathBuf::from("/System"),
                PathBuf::from("/dev"),
                PathBuf::from("/proc"),
                PathBuf::from("/sys"),
                PathBuf::from("/Network"),
                PathBuf::from("/private/var"),
                PathBuf::from("/private/tmp"),
            ],
            git: find_git(),
            scan_interval_hours: 7 * 24,
            gc_interval_hours: 24,
            overdue_days: 14,
            notification_interval_hours: 7 * 24,
            notifications: true,
            idle: IdlePolicy::default(),
        }
    }
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/Users/Shared"))
}

fn find_git() -> PathBuf {
    std::env::var_os("PATH")
        .and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|p| p.join("git"))
                .find(|p| is_executable(p))
        })
        .unwrap_or_else(|| PathBuf::from("/usr/bin/git"))
}

fn is_executable(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

pub fn absolute(path: &Path) -> Result<PathBuf> {
    let expanded = if path == Path::new("~") {
        home()
    } else if let Ok(rest) = path.strip_prefix("~/") {
        home().join(rest)
    } else {
        path.to_path_buf()
    };
    Ok(if expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir()?.join(expanded)
    })
}

impl Config {
    pub fn validate(&mut self) -> Result<()> {
        if self.roots.is_empty() {
            bail!("configure at least one discovery root");
        }
        for path in self.roots.iter_mut().chain(self.excludes.iter_mut()) {
            *path = absolute(path)?;
            if path.exists() {
                *path = path
                    .canonicalize()
                    .with_context(|| format!("resolve {}", path.display()))?;
            }
        }
        self.roots.sort();
        self.roots.dedup();
        self.excludes.sort();
        self.excludes.dedup();
        self.git = absolute(&self.git)?;
        if !is_executable(&self.git) {
            bail!(
                "Git executable does not exist or is not executable: {}",
                self.git.display()
            );
        }
        for (label, value) in [
            ("scan_interval_hours", self.scan_interval_hours),
            ("gc_interval_hours", self.gc_interval_hours),
            ("overdue_days", self.overdue_days),
            (
                "notification_interval_hours",
                self.notification_interval_hours,
            ),
        ] {
            if value == 0 || value > 365 * 24 {
                bail!("{label} must be between 1 and {}", 365 * 24);
            }
        }
        if self.idle.sample_seconds < 1 || self.idle.sample_seconds > 300 {
            bail!("idle.sample_seconds must be between 1 and 300");
        }
        if self.idle.idle_seconds < 30 {
            bail!("idle.idle_seconds must be at least 30");
        }
        if !self.idle.max_cpu_percent.is_finite()
            || !(0.0..=100.0).contains(&self.idle.max_cpu_percent)
            || !self.idle.max_gpu_percent.is_finite()
            || !(0.0..=100.0).contains(&self.idle.max_gpu_percent)
            || !self.idle.min_memory_free_percent.is_finite()
            || !(0.0..=100.0).contains(&self.idle.min_memory_free_percent)
        {
            bail!("idle CPU, GPU and memory thresholds must be percentages between 0 and 100");
        }
        Ok(())
    }

    pub fn load(paths: &Paths) -> Result<Self> {
        let mut config = match std::fs::read_to_string(&paths.config) {
            Ok(text) => toml::from_str(&text)
                .with_context(|| format!("parse {}", paths.config.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => return Err(e).context("read configuration"),
        };
        Self::validate(&mut config)?;
        Ok(config)
    }

    pub fn save_new(&self, paths: &Paths) -> Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let text = toml::to_string_pretty(self)?;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&paths.config)
            .with_context(|| {
                format!(
                    "create {} (existing config is never overwritten)",
                    paths.config.display()
                )
            })?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        Ok(())
    }
}
