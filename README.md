# git-sweep

<img src="assets/icon.svg" width="96" height="96" alt="Git Sweep: a Git branch ending in a broom">

A small Rust CLI that discovers Git repositories recursively and runs ordinary `git gc`. On macOS, an optional hourly LaunchAgent waits for an idle, lightly loaded machine. There is no resident monitoring process.

> **AI disclosure:** This project was generated with GPT-6 Sol Ultra in Codex. Review its behavior before running it on repositories you care about.

## Build and start

Requires Rust 1.85+, Git 2.47+, and macOS for scheduling and notifications. Notification setup also needs Xcode Command Line Tools (`xcode-select --install`) to compile the bundled Swift helper.

```sh
git clone https://github.com/Maxim-Mazurok/git-sweep.git
cd git-sweep
cargo build --release --locked
./target/release/git-sweep init
./target/release/git-sweep scan
./target/release/git-sweep run --dry-run
./target/release/git-sweep run
```

To put the command on your Cargo bin path, run `cargo install --path . --locked`. Otherwise replace `git-sweep` in examples below with `./target/release/git-sweep`.

`init` creates private configuration and requests notification permission when run from an interactive terminal. Denying permission does not prevent maintenance. Retry with `git-sweep notifications`, or change permission for **Git Sweep Notifications** in System Settings → Notifications.

`git-sweep notifications` also refreshes the helper bundle and its Launch Services registration. Use `git-sweep notifications --test` to submit a fresh test notification with existing permission. Repeating a permission request does not create a new notification; an already-visible permission banner can retain an older icon.

`run` is an explicit manual request: it does not require idle state. It respects the daily interval; `run --force` overrides that interval. `run --dry-run` only measures storage and asks Git for object statistics. It does **not** predict reclaimed space, change repository contents, or advance the GC timestamp. Actual maintenance records sizes before and after each repository.

This repository's development/tests do not install a schedule, request notification permission, or maintain existing user repositories.

## Search outside your home folder

The initial search root is your home directory. Supply multiple roots during initialization:

```sh
git-sweep init --root "$HOME" --root /Volumes/Projects
```

An explicit root of `/` searches the accessible filesystem, subject to exclusions. Prefer specific roots: a filesystem-wide scan can be slow and may enter mounted network or removable volumes. Discovery does not follow directory symlinks. Default exclusions cover system runtime directories, Google Drive/OneDrive-style `~/Library/CloudStorage`, iCloud `~/Library/Mobile Documents`, trash and Spotlight internals. Add roots/exclusions by editing the generated TOML:

```sh
git-sweep config --path
git-sweep config
git-sweep scan --force
```

Discovery keeps walking inside working trees to find nested projects, such as a checkout inside another repository. It recognizes `.git` files, bare repositories, linked worktrees and submodule stores under `.git/modules`. Shared worktrees are maintained once per common Git directory. Gitfiles can refer to storage outside the search roots; exclusions also apply to the resolved storage path. It skips invalid markers and inaccessible folders with bounded diagnostics. macOS privacy restrictions are not bypassed.

The cache is reused for seven days. Changing roots or exclusions invalidates it immediately; `scan --force` explicitly refreshes it. A cancelled scan retains the previous complete cache. Removed repositories are skipped until the next scan. Temporarily unavailable roots are logged; reconnect a disk and force a scan if you need it included before the next weekly discovery.

## Optional hourly scheduling

```sh
git-sweep schedule install
git-sweep schedule status
git-sweep status
git-sweep probe
git-sweep schedule uninstall
```

Installation copies the current executable to `~/Library/Application Support/git-sweep/bin/git-sweep`, installs `~/Library/LaunchAgents/dev.gitsweep.plist`, and registers it with your logged-in GUI session. Re-run installation after building an update. Uninstalling stops the scheduled process and removes the LaunchAgent; it retains configuration, reports, notification permission and logs. No administrator access is needed.

Each hourly invocation follows these steps:

1. Acquire a single-instance OS lock. If a completed maintenance pass is less than 24 hours old, exit without resource probes or discovery.
2. Require at least 10 minutes without input, AC power, at least 15% free memory and no reported thermal/performance pressure.
3. Sample aggregate CPU utilization and GPU activity for 30 seconds, checking for resumed input while waiting. Require no more than 20% average busy CPU across all cores and 35% average GPU activity; recheck the other resource gates afterward. A single high GPU reading cannot reject the startup probe.
4. Refresh discovery if its weekly interval is due. Maintain cached repositories sequentially at nice 19 and throttled disk I/O priority, using one packing thread and a 64 MiB delta-search window budget.
5. Recheck session activity, GPU, memory, thermal and power gates during work. Stop for sustained GPU load after three consecutive high readings about five seconds apart; input and the other resource gates stop work on the next check. Leave the daily timestamp unchanged, so another hourly check can retry. CPU is not re-gated during GC because Git's own packing would affect it.

Unsupported or unreadable resource metrics defer work rather than guessing. GPU counters are registry dependent; an unfamiliar driver may defer maintenance until supported. `probe` shows eligibility without running GC. CoreGraphics input counters include remote/synthetic input. Low CPU/GPU activity is a heuristic, not a guarantee that every background workload is inactive.

The LaunchAgent checks once when loaded and roughly hourly while logged in. It does not wake a sleeping or powered-off Mac. Missed checks need no catch-up job: eligibility and saved timestamps are evaluated on the next invocation. There is no fixed daily cleaning hour and no process kept alive between checks.

After 14 days without a completed pass, a deferred check submits a macOS notification, at most once per seven days (including failed delivery attempts). The timer starts at initialization if no GC has completed. Scheduled runs never prompt for notification permission; permission setup belongs to interactive initialization. Delivery depends on macOS permission and notification/Focus settings.

## Configuration and records

Default location: `~/Library/Application Support/git-sweep/`.

| File | Purpose |
|---|---|
| `config.toml` | Search roots, exclusions, Git executable, intervals, idle thresholds |
| `repos.json` | Discovered common Git directories and checkout paths |
| `state.json` | Initialization, last full discovery, last completed GC pass, attempts and notification timestamps |
| `last-run.json` | Most recent actual run, total savings, per-repository before/after sizes, status and diagnostics |
| `progress.json` | Partial-pass checkpoint; completed repositories are skipped when an interrupted pass resumes |
| `last-dry-run.json` | Most recent read-only inventory |
| `last-scan.json` | Discovery results and detailed warnings |
| `logs/debug.log` | Bounded diagnostic event log |

Edit `config.toml` directly; changes apply on the next invocation that reads configuration, without reinstalling the schedule. Omitted fields use defaults. Unknown field names, including names inside `[idle]`, cause an error so typos cannot silently change behavior.

Every configuration option:

| Option | Default | Meaning |
|---|---|---|
| `roots` | `["~"]` | Directories to search recursively. Supply at least one path; add other user folders or mounted disks to discover repositories outside your home. Nested repositories are included. Overlapping roots and shared Git stores are deduplicated. |
| `excludes` | Cloud and system paths listed below | Directory subtrees to omit from discovery, including resolved Git storage reached through `.git` files. Entries are literal paths, not glob patterns. An explicit list **replaces** the default list; retain any default exclusions you still want. Changing this list or `roots` invalidates the discovery cache. |
| `git` | First executable `git` on `PATH`, falling back to `/usr/bin/git` | Path to the Git executable used for discovery, statistics and GC. `init` saves the detected path, so launchd does not need your interactive shell's `PATH`. Use an executable file, without command arguments, and ensure it meets the minimum Git version above. |
| `scan_interval_hours` | `168` (7 days) | Minimum age of a completed discovery cache before it is refreshed. Maintenance reuses cached repository locations between scans. A missing cache, changed roots/exclusions, or explicit `scan --force` bypasses this interval. This does not create a separate discovery timer. |
| `gc_interval_hours` | `24` | Minimum time between completed best-effort maintenance passes. Both manual `run` and scheduled checks respect it; `run --force` bypasses it. A scheduled check exits before discovery or resource sampling while this interval has not elapsed. Interrupted passes resume from their checkpoint. |
| `overdue_days` | `14` | Days without a completed maintenance pass before an overdue notification can be attempted. Measured from the last completed pass, or initialization if none has completed. It never forces GC on a busy machine. |
| `notification_interval_hours` | `168` (7 days) | Minimum time between overdue notification **attempts**. Failed delivery or denied permission also starts this cooldown, avoiding an hourly retry loop. Applies only while maintenance remains overdue and notifications are enabled. |
| `notifications` | `true` | Enables overdue notifications and the automatic permission request during interactive setup. `false` suppresses both, without disabling maintenance. Setting it to `true` does not itself grant macOS permission; use `git-sweep notifications` to request or retry permission explicitly. |
| `idle.idle_seconds` | `600` (10 minutes) | Minimum time since session input before scheduled maintenance may start or continue. Input resuming during the sample or maintenance defers/stops work. Whole seconds, at least `30`. |
| `idle.sample_seconds` | `30` | Duration of the CPU and GPU sampling window before scheduled maintenance starts. The sample can end early if input resumes. Whole seconds from `1` through `300`; shorter samples are quicker but can miss bursts of activity. |
| `idle.max_cpu_percent` | `20.0` | Maximum average busy CPU percentage **across all cores** during the sample. Finite percentage from `0` through `100`. CPU is not re-gated during GC because Git itself uses CPU. |
| `idle.max_gpu_percent` | `35.0` | Maximum average GPU activity during the startup sample. During GC, three consecutive readings above this threshold stop work. Finite percentage from `0` through `100`. Uses the device utilization field when available, with other driver fields as fallback. |
| `idle.min_memory_free_percent` | `15.0` | Minimum system-wide free-memory percentage reported by macOS `memory_pressure -Q`. Finite percentage from `0` through `100`. Lower reported free memory defers/stops scheduled work. |
| `idle.require_ac` | `true` | Requires macOS to report AC power for scheduled work. Unplugging stops maintenance at a subsequent check. Set `false` to allow battery operation; all other idle/resource gates still apply. |

The four interval fields (`scan_interval_hours`, `gc_interval_hours`, `overdue_days`, and `notification_interval_hours`) accept whole integers from `1` through `8760`, in the units named by each field. Intervals measure elapsed time, rather than resetting at midnight. Idle settings govern `scheduled` and `probe`; manual `run` does not require these resource gates. Missing or unsupported resource metrics still defer scheduled maintenance, even with permissive thresholds.

Default `excludes` entries are:

- `~/Library/CloudStorage`
- `~/Library/Mobile Documents`
- `~/Library/Group Containers/UBF8T346G9.OneDriveSyncClientSuite`
- `/System`, `/dev`, `/proc`, `/sys`, `/Network`, `/private/var`, `/private/tmp`

Discovery also always skips symlinked directories, `.Trash`, `.Trashes`, `.Spotlight-V100`, `.fseventsd`, and its own application-data directory. Removing entries from `excludes` does not disable those built-in skips.

Path values support `~` and `~/...`. Relative paths resolve against the command's working directory, **not** the configuration file's directory. Prefer absolute paths or `~/...` for scheduled runs; environment variables such as `$HOME` are not expanded inside path strings.

Default interval and idle settings (`idle.*` keys belong under the `[idle]` table):

```toml
scan_interval_hours = 168
gc_interval_hours = 24
overdue_days = 14
notification_interval_hours = 168
notifications = true

[idle]
idle_seconds = 600
sample_seconds = 30
max_cpu_percent = 20.0
max_gpu_percent = 35.0
min_memory_free_percent = 15.0
require_ac = true
```

The hourly launchd wake-up interval, log-size limits, Git packing thread count, packing window budget and per-repository timeout are fixed implementation settings, not additional TOML options.

One global GC timestamp records a **completed best-effort pass**, not each repository. A pass with some successes and some locked/failed repositories counts as completed; those failures are reported and retried the next day. A store safely skipped because another repository borrows its objects also counts as processed. An interrupted or all-failed pass does not advance the timestamp. A small checkpoint records repositories already attempted in a partial pass, so hourly retries resume where they stopped instead of repeatedly maintaining the first repositories. Empty discovery is a completed no-op. `--force` starts a new pass and permits an immediate manual retry. State/cache/report writes use atomic replacement and filesystem sync; corrupt JSON is reported and preserved, not silently discarded.

Live text logs stay below **8 MiB**, with size-based rotation and streaming gzip compression. Oldest archives are removed to keep their combined compressed size below **40 MiB**. These budgets stay under 10 MB text and 50 MB compressed. Each log line is bounded and control characters are escaped. Routine logs contain paths/statuses; raw Git output lives in private JSON reports. Directory permissions are 0700; state/log files are 0600.

Set `GIT_SWEEP_HOME` to use a separate configuration/state/log directory. Schedule installation preserves an absolute version of this setting in its plist. The installed executable and notification helper retain stable paths under the default application-support directory.

## Git behavior

- Uses normal `git gc --no-detach` with existing expiration configuration. Never adds `--aggressive`, `--prune=now`, or `--force`.
- Existing `.lock` files or a live/unverifiable `gc.pid` defer that repository. Stale locks are not deleted automatically.
- Disables hooks and fsmonitor for its Git subprocesses, clears inherited `GIT_*` redirections and disables lazy fetching. Repositories with `gc.recentObjectsHook` or `core.alternateRefsCommand` are skipped so custom object-retention protections are not bypassed.
- Before every real pass, reads `objects/info/alternates` in every cached repository. If a discovered repository borrows objects from another discovered store, GC of that source store is skipped with `skipped_alternate_source`. An unreadable, malformed or unresolved alternates entry defers the entire pass. This prevents a known [`git clone --shared` corruption risk](https://git-scm.com/docs/git-clone.html). Borrowers outside the cached discovery set, including those created since the last weekly scan, and borrowers using only `GIT_ALTERNATE_OBJECT_DIRECTORIES` cannot be detected. Include shared/reference clones in `roots` and run `git-sweep scan --force` after creating one, or exclude its source store from GC.
- A 20-minute per-repository timeout and cancellation terminate the entire Git process group. Git's atomic pack replacement protects existing object data, though interrupted maintenance can leave temporary files/locks; inspect logs before removing them.
- Uncommitted working files, the index and stashes are not reset or cleaned. Ordinary GC can expire reflogs and prune old unreachable objects according to Git configuration. It does not clean build outputs, working directories or LFS data.
- Measures logical and allocated storage before/after maintenance, deduplicating hard links inside each common store and excluding separately measured submodule stores. Interrupted accounting sets `measurements_complete = false` and a zero delta; that means savings are unknown. Changes from concurrent Git activity and APFS block sharing can affect measurements; size reductions are not guaranteed physical free-space gains.

See [Git GC documentation](https://git-scm.com/docs/git-gc) for retention and concurrency behavior.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --locked
cargo build --release --locked
```

Tests use disposable repositories and isolated application data. They cover recursive discovery, worktree deduplication, submodules, actual GC preservation, cancellation/timeouts, daily/weekly cadence, corrupt state, locking, log budgets and native metric parsers. No test installs a LaunchAgent or requests notification permission.

### App icon

`assets/icon.svg` is the editable vector source. Regenerate `assets/icon.png` and the macOS multi-resolution `macos/GitSweep.icns` with:

```sh
python3 scripts/build_icon.py
cargo build --release --locked
```

Rendering uses native AppKit, Swift and `iconutil`; no third-party graphics tools are required. The ICNS is embedded in the Rust executable and copied into the notification app bundle before signing. Changes invalidate the cached helper icon. Existing installations can refresh it by running the rebuilt executable's `notifications` command; this also checks notification permission. Reinstall the schedule with the rebuilt executable to update its binary as well.
