//! The activation journal and its startup recovery.
//!
//! Every replacement of an installed package (an approved update, a rollback,
//! an unattended boot update) writes `<data_dir>/activation/<id>.json` before
//! it touches anything, updates it after each stage and removes it once the
//! swap is committed. The journal names every path the swap will use, so the
//! on-disk state can always be read back to one of two answers:
//!
//! * **stage [`ActivationStage::Committed`]**: the new package passed its
//!   activation check and is kept. Recovery finishes the cleanup.
//! * **any earlier stage**: the replacement did not finish. Recovery restores
//!   the package, the prior version (and the prior's backup) and the pin that
//!   were there before it started, and removes what was coming in.
//!
//! Both answers are reached from what exists on disk, not from the stage
//! alone, so recovery is idempotent and a kill during recovery is recovered by
//! the next start. The live undo of a failed activation runs the same code
//! ([`SwapPaths::restore`]).
//!
//! [`recover_activations`] runs once at startup, synchronously and before the
//! registry's first scan, then sweeps the hidden swap and install leftovers of
//! every plugin.

use crate::registry::{approvals_path, load_approvals, write_approvals, ApprovalPin};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Mutex, PoisonError},
};

/// Test knob: `TULIPROX_PLUGIN_TEST_CRASH_AT=<stage>` aborts the process
/// (`std::process::abort()`) right after the activation journal recorded that
/// stage, so a kill at every point of a replacement can be reproduced. Read
/// once per activation; never configuration. Stages: see
/// [`ActivationStage::as_str`].
pub const ENV_PLUGIN_TEST_CRASH_AT: &str = "TULIPROX_PLUGIN_TEST_CRASH_AT";

/// The journals live in `<data_dir>/activation/`; `activation` is a reserved id.
pub(crate) const JOURNAL_DIR: &str = "activation";

/// How far a replacement got. Recorded in the journal after each step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivationStage {
    /// The journal exists; nothing else changed yet.
    Journaled,
    /// The current package is copied into the prior slot (the old prior waits
    /// in a backup).
    PriorRetained,
    /// The new pin is written.
    PinSwapped,
    /// The new package is in place of the current one, which waits aside.
    Entered,
    /// The registry reloaded with the new package.
    Reloaded,
    /// The activation check passed. A rollback whose settings still have to be
    /// written waits here until the caller settles it.
    Checked,
    /// Kept: only the cleanup of the aside and the backup is left.
    Committed,
}

impl ActivationStage {
    pub const ALL: [Self; 7] = [
        Self::Journaled,
        Self::PriorRetained,
        Self::PinSwapped,
        Self::Entered,
        Self::Reloaded,
        Self::Checked,
        Self::Committed,
    ];

    /// The name used in the journal and by [`ENV_PLUGIN_TEST_CRASH_AT`].
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Journaled => "journaled",
            Self::PriorRetained => "prior_retained",
            Self::PinSwapped => "pin_swapped",
            Self::Entered => "entered",
            Self::Reloaded => "reloaded",
            Self::Checked => "checked",
            Self::Committed => "committed",
        }
    }

    /// The stage called `name`, if there is one.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> { Self::ALL.into_iter().find(|stage| stage.as_str() == name.trim()) }
}

impl std::fmt::Display for ActivationStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(self.as_str()) }
}

/// What started a replacement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivationKind {
    /// `PUT …/approve` of a staged update.
    Approve,
    /// `POST …/rollback`.
    Rollback,
    /// The boot update or "update all eligible", without a review.
    Unattended,
}

/// Every path a swap of one plugin uses, chosen before it starts. Hidden
/// (`.swap~<role>~<id>~…`) next to the target and the prior, so a scan skips
/// them and [`crate::install`]'s sweep recognises them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SwapPaths {
    /// `<plugins dir>/<id>`.
    pub(crate) target: PathBuf,
    /// `<data_dir>/plugins-prev/<id>`.
    pub(crate) prior: PathBuf,
    /// The incoming package, next to the target until it is renamed in.
    pub(crate) incoming: PathBuf,
    /// The current package while the incoming one is in its place.
    pub(crate) aside: PathBuf,
    /// The new prior while it is built, next to the prior.
    pub(crate) fresh: PathBuf,
    /// The old prior while the new one replaces it.
    pub(crate) backup: PathBuf,
}

fn exists(path: &Path) -> bool { fs::symlink_metadata(path).is_ok() }

/// Remove a file, a symlink (never what it points at) or a directory tree. A
/// path that is not there is fine.
pub(crate) fn remove_path(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

/// Keep the first error of a sequence of steps that must all be tried.
fn keep_first(first: &mut io::Result<()>, step: io::Result<()>) {
    if first.is_ok() {
        *first = step;
    }
}

impl SwapPaths {
    /// Put back what was there before the swap started, judged by what exists:
    /// the current package from the aside, the old prior from the backup (or
    /// no prior when there was none), and nothing incoming. Idempotent; every
    /// step is tried and the first error is returned.
    pub(crate) fn restore(&self, had_prior: bool) -> io::Result<()> {
        let mut first = Ok(());
        if exists(&self.aside) {
            keep_first(&mut first, remove_path(&self.target).and_then(|()| fs::rename(&self.aside, &self.target)));
        }
        keep_first(&mut first, remove_path(&self.incoming));
        if exists(&self.backup) {
            keep_first(&mut first, remove_path(&self.prior).and_then(|()| fs::rename(&self.backup, &self.prior)));
        } else if !had_prior {
            // Whatever is in the prior slot was put there by this swap.
            keep_first(&mut first, remove_path(&self.prior));
        }
        keep_first(&mut first, remove_path(&self.fresh));
        first
    }

    /// Delete what only [`SwapPaths::restore`] would have needed. Never touches
    /// the target or the prior. Idempotent.
    pub(crate) fn commit(&self) -> io::Result<()> {
        let mut first = Ok(());
        for path in [&self.aside, &self.backup, &self.incoming, &self.fresh] {
            keep_first(&mut first, remove_path(path));
        }
        first
    }
}

/// `<data_dir>/activation/<id>.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Journal {
    pub(crate) id: String,
    pub(crate) kind: ActivationKind,
    /// The version being replaced (restored by a recovery).
    pub(crate) from: String,
    /// The version the replacement activates.
    pub(crate) to: String,
    /// The pin before the replacement; `None` removes the pin on restore.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) previous_pin: Option<ApprovalPin>,
    /// Whether `paths.prior` held a prior version before the swap.
    pub(crate) had_prior: bool,
    pub(crate) paths: SwapPaths,
    pub(crate) stage: ActivationStage,
    /// Where this journal is written; not stored.
    #[serde(skip)]
    file: PathBuf,
    /// The stage at which a test crash is due; not stored.
    #[serde(skip)]
    crash_at: Option<Crash>,
}

/// A crash requested for a test of one data dir: a panic instead of an abort,
/// so the test survives it and finds the disk as a kill would leave it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Crash {
    Abort(ActivationStage),
    Panic(ActivationStage),
}

/// Test crashes by data dir, see [`inject_test_crash`].
static TEST_CRASHES: Mutex<Vec<(PathBuf, ActivationStage)>> = Mutex::new(Vec::new());

/// For tests of this crate's users: make the next activations under
/// `data_dir` panic right after the journal recorded `stage` (`None` clears
/// it). The panic ends the blocking task that runs the step, so nothing after
/// it runs, like a kill; the caller sees a `500`. In-process fault injection,
/// keyed by data dir so parallel tests do not interfere.
#[doc(hidden)]
pub fn inject_test_crash(data_dir: &Path, stage: Option<ActivationStage>) {
    let mut crashes = TEST_CRASHES.lock().unwrap_or_else(PoisonError::into_inner);
    crashes.retain(|(dir, _)| dir != data_dir);
    if let Some(stage) = stage {
        crashes.push((data_dir.to_path_buf(), stage));
    }
}

fn crash_for(data_dir: &Path) -> Option<Crash> {
    let injected = TEST_CRASHES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .find(|(dir, _)| dir == data_dir)
        .map(|(_, stage)| Crash::Panic(*stage));
    injected.or_else(|| crash_knob().map(Crash::Abort))
}

/// The stage named by [`ENV_PLUGIN_TEST_CRASH_AT`], if it names one.
#[must_use]
pub fn crash_knob() -> Option<ActivationStage> {
    std::env::var(ENV_PLUGIN_TEST_CRASH_AT).ok().and_then(|value| ActivationStage::from_name(&value))
}

/// `<data_dir>/activation`.
pub(crate) fn journal_dir(data_dir: &Path) -> PathBuf { data_dir.join(JOURNAL_DIR) }

pub(crate) fn journal_path(data_dir: &Path, id: &str) -> PathBuf { journal_dir(data_dir).join(format!("{id}.json")) }

/// Fsync a directory, so a rename or removal inside it is durable.
fn sync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        fs::File::open(dir)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

/// Write `bytes` to `path` atomically and durably: a synced temp file next to
/// it, renamed over it, then the directory synced.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path.parent().ok_or_else(|| io::Error::other("the path has no parent directory"))?;
    fs::create_dir_all(dir)?;
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let written = (|| {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&tmp, path)?;
        sync_dir(dir)
    })();
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    written
}

impl Journal {
    /// A journal at [`ActivationStage::Journaled`]; nothing is written yet.
    pub(crate) fn new(
        data_dir: &Path,
        kind: ActivationKind,
        id: &str,
        versions: (String, String),
        previous_pin: Option<ApprovalPin>,
        had_prior: bool,
        paths: SwapPaths,
    ) -> Self {
        let (from, to) = versions;
        Self {
            id: id.to_string(),
            kind,
            from,
            to,
            previous_pin,
            had_prior,
            paths,
            stage: ActivationStage::Journaled,
            file: journal_path(data_dir, id),
            crash_at: crash_for(data_dir),
        }
    }

    /// The journal of `id` under `data_dir`, if one is there and readable.
    pub(crate) fn read(data_dir: &Path, id: &str) -> Option<Self> {
        let file = journal_path(data_dir, id);
        let mut journal: Self = serde_json::from_slice(&fs::read(&file).ok()?).ok()?;
        journal.file = file;
        journal.crash_at = crash_for(data_dir);
        Some(journal)
    }

    /// Write the journal for the first time. Blocking.
    pub(crate) fn begin(&self) -> io::Result<()> {
        self.persist()?;
        self.crash_point();
        Ok(())
    }

    /// Record `stage`. Blocking. An error leaves the earlier stage on disk,
    /// which recovery reads correctly as long as `stage` is not
    /// [`ActivationStage::Committed`]: the caller must then undo instead of
    /// committing.
    pub(crate) fn advance(&mut self, stage: ActivationStage) -> io::Result<()> {
        let earlier = self.stage;
        self.stage = stage;
        if let Err(err) = self.persist() {
            self.stage = earlier;
            return Err(err);
        }
        self.crash_point();
        Ok(())
    }

    /// Remove the journal: the replacement is settled. Blocking.
    pub(crate) fn remove(&self) -> io::Result<()> {
        remove_path(&self.file)?;
        self.file.parent().map_or(Ok(()), sync_dir)
    }

    fn persist(&self) -> io::Result<()> {
        let text = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
        write_atomic(&self.file, &text)
    }

    fn crash_point(&self) {
        match self.crash_at {
            Some(Crash::Panic(stage)) if stage == self.stage => {
                panic!("injected activation crash of plugin `{}` at stage {stage}", self.id);
            }
            Some(Crash::Abort(stage)) if stage == self.stage => {
                log::warn!(
                    "{ENV_PLUGIN_TEST_CRASH_AT}={stage}: aborting during the activation of plugin `{}` ({} -> {})",
                    self.id,
                    self.from,
                    self.to
                );
                log::logger().flush();
                std::process::abort();
            }
            _ => {}
        }
    }
}

/// What startup recovery did with one journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryOutcome {
    /// The replacement had not committed: the package, the prior and the pin
    /// from before it are back.
    RolledBack,
    /// The replacement had committed: its cleanup was finished, the new
    /// package stays.
    Completed,
    /// Recovery failed; the journal stays and the next start tries again.
    /// A replacement of this plugin is refused until then.
    Failed(String),
}

/// One journal found at startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveredActivation {
    pub id: String,
    /// `None` when the journal could not be read.
    pub kind: Option<ActivationKind>,
    pub from: String,
    pub to: String,
    /// The last stage the journal recorded; `None` when it could not be read.
    pub stage: Option<ActivationStage>,
    pub outcome: RecoveryOutcome,
}

/// Put `previous` back as the pin of `id`, straight on disk (no registry is
/// loaded yet). An unreadable `approvals.json` approves nothing and is left
/// for the next explicit approval to set aside, so it is not touched.
fn restore_pin_on_disk(data_dir: &Path, id: &str, previous: Option<&ApprovalPin>) -> io::Result<()> {
    let path = approvals_path(data_dir);
    let Ok(mut approvals) = load_approvals(&path) else {
        log::warn!("plugin `{id}`: the approvals file is unreadable, so its pin is not restored (nothing is approved)");
        return Ok(());
    };
    let unchanged = approvals.get(id) == previous;
    if unchanged {
        return Ok(());
    }
    match previous {
        Some(pin) => {
            approvals.insert(id.to_string(), pin.clone());
        }
        None => {
            approvals.remove(id);
        }
    }
    write_approvals(&path, &approvals)
}

fn recover_one(data_dir: &Path, journal: &Journal) -> RecoveryOutcome {
    let id = &journal.id;
    if journal.stage == ActivationStage::Committed {
        if let Err(err) = journal.paths.commit() {
            log::error!("plugin `{id}`: cannot finish the committed activation of {}: {err}", journal.to);
            return RecoveryOutcome::Failed(err.to_string());
        }
        if let Err(err) = journal.remove() {
            log::warn!("plugin `{id}`: cannot remove its activation journal: {err}");
        }
        log::warn!(
            "plugin `{id}`: the activation of {} was interrupted after it was committed; it stays active",
            journal.to
        );
        return RecoveryOutcome::Completed;
    }
    let restored = journal
        .paths
        .restore(journal.had_prior)
        .and_then(|()| restore_pin_on_disk(data_dir, id, journal.previous_pin.as_ref()));
    if let Err(err) = restored {
        log::error!(
            "plugin `{id}`: cannot restore {} after the interrupted activation of {} (stage {}): {err}; retried at \
             the next start",
            journal.from,
            journal.to,
            journal.stage
        );
        return RecoveryOutcome::Failed(err.to_string());
    }
    if let Err(err) = journal.remove() {
        log::warn!("plugin `{id}`: cannot remove its activation journal: {err}");
    }
    let settings_note = if journal.kind == ActivationKind::Rollback && journal.stage == ActivationStage::Checked {
        // The route writes the rolled-back version's settings between `checked`
        // and `committed`; config.yml may hold them now. They are at the same or
        // a lower settings version, which the settings migration brings forward.
        "; if its settings were already saved, config.yml holds those of the older version, which the settings \
         migration brings forward"
    } else {
        ""
    };
    log::warn!(
        "plugin `{id}`: the {} of {} -> {} was interrupted at stage {}; restored {}{settings_note}",
        match journal.kind {
            ActivationKind::Approve => "update",
            ActivationKind::Rollback => "rollback",
            ActivationKind::Unattended => "unattended update",
        },
        journal.from,
        journal.to,
        journal.stage,
        journal.from
    );
    RecoveryOutcome::RolledBack
}

/// The id a `.swap~<role>~<id>~<pid>~<n>` name belongs to, with its role.
fn swap_name_parts(name: &str) -> Option<(&str, &str)> {
    let mut parts = name.strip_prefix(".swap~")?.split('~');
    let role = parts.next()?;
    let id = parts.next()?;
    Some((role, id))
}

/// Remove the hidden leftovers of interrupted swaps and installs in `dir`
/// (`.swap~…`, `.incoming-…`, `.old-…`, `.upload-…`). A `.swap~old~<id>~…`
/// whose plugin directory is missing (a kill inside a swap that predates the
/// journal) is the only copy of that package: it is renamed back instead.
fn sweep_dir(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let path = entry.path();
        if let Some(("old", id)) = swap_name_parts(&name) {
            let target = dir.join(id);
            if crate::install::is_installable_id(id) && !exists(&target) {
                match fs::rename(&path, &target) {
                    Ok(()) => log::warn!("plugin `{id}`: restored its package from an interrupted swap"),
                    Err(err) => log::error!("plugin `{id}`: cannot restore its package from {}: {err}", path.display()),
                }
                continue;
            }
        }
        let leftover = name.starts_with(".swap~")
            || name.starts_with(".incoming-")
            || name.starts_with(".old-")
            || name.starts_with(".upload-");
        if leftover {
            match remove_path(&path) {
                Ok(()) => log::warn!("removed the leftover {} of an interrupted plugin install", path.display()),
                Err(err) => log::warn!("cannot remove the leftover {}: {err}", path.display()),
            }
        }
    }
}

/// Startup recovery: settle every activation
/// journal under `data_dir` (see the module docs), then sweep the swap and
/// install leftovers of every plugin in `plugins_dir`, the prior dir and
/// staging. Logs a WARN per plugin and returns what it found, in id order.
///
/// Synchronous and blocking: call it on the blocking pool, before
/// [`crate::PluginSystem::load`] scans, while nothing else installs.
/// Idempotent: a second call finds nothing.
#[must_use]
pub fn recover_activations(data_dir: &Path, plugins_dir: &Path) -> Vec<RecoveredActivation> {
    let dir = journal_dir(data_dir);
    let mut found = Vec::new();
    let mut files: Vec<PathBuf> = match fs::read_dir(&dir) {
        Ok(entries) => entries.flatten().map(|entry| entry.path()).collect(),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(err) => {
            log::error!("cannot read the plugin activation journals in {}: {err}", dir.display());
            Vec::new()
        }
    };
    files.sort();
    for file in files {
        let name = file.file_name().map(|name| name.to_string_lossy().to_string()).unwrap_or_default();
        if file.extension().is_some_and(|extension| extension == "tmp") {
            // A journal write that never got renamed: the earlier one (if any) stands.
            let _ = fs::remove_file(&file);
            continue;
        }
        let Some(id) = name.strip_suffix(".json").filter(|id| crate::install::is_installable_id(id)) else {
            continue;
        };
        match Journal::read(data_dir, id) {
            Some(journal) if journal.id == id => {
                let outcome = recover_one(data_dir, &journal);
                found.push(RecoveredActivation {
                    id: journal.id.clone(),
                    kind: Some(journal.kind),
                    from: journal.from.clone(),
                    to: journal.to.clone(),
                    stage: Some(journal.stage),
                    outcome,
                });
            }
            _ => {
                log::error!(
                    "plugin `{id}`: its activation journal {} cannot be read; it is left in place and the plugin \
                     cannot be replaced until it is removed",
                    file.display()
                );
                found.push(RecoveredActivation {
                    id: id.to_string(),
                    kind: None,
                    from: String::new(),
                    to: String::new(),
                    stage: None,
                    outcome: RecoveryOutcome::Failed("the activation journal cannot be read".to_string()),
                });
            }
        }
    }
    // After the journals: their asides and backups are leftovers only once settled.
    let pending: Vec<&str> = found
        .iter()
        .filter(|recovered| matches!(recovered.outcome, RecoveryOutcome::Failed(_)))
        .map(|recovered| recovered.id.as_str())
        .collect();
    if pending.is_empty() {
        for swept in [plugins_dir.to_path_buf(), data_dir.join(crate::install::PRIOR_DIR), data_dir.join("staging")] {
            sweep_dir(&swept);
        }
    } else {
        log::warn!(
            "not sweeping plugin swap leftovers while activation journals are unsettled: {}",
            pending.join(", ")
        );
    }
    found
}

/// Whether an unsettled journal of `id` blocks a new replacement.
pub(crate) fn journal_pending(data_dir: &Path, id: &str) -> bool { exists(&journal_path(data_dir, id)) }

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(root: &Path) -> SwapPaths {
        SwapPaths {
            target: root.join("plugins/demo"),
            prior: root.join("prev/demo"),
            incoming: root.join("plugins/.swap~in~demo~1~1"),
            aside: root.join("plugins/.swap~old~demo~1~2"),
            fresh: root.join("prev/.swap~new~demo~1~3"),
            backup: root.join("prev/.swap~bak~demo~1~4"),
        }
    }

    fn put(path: &Path, text: &str) {
        fs::create_dir_all(path).unwrap();
        fs::write(path.join("v"), text).unwrap();
    }

    fn read(path: &Path) -> Option<String> { fs::read_to_string(path.join("v")).ok() }

    #[test]
    fn stage_names_round_trip() {
        for stage in ActivationStage::ALL {
            assert_eq!(ActivationStage::from_name(stage.as_str()), Some(stage));
            assert_eq!(serde_json::to_string(&stage).unwrap(), format!("\"{}\"", stage.as_str()));
        }
        assert_eq!(ActivationStage::from_name("nope"), None);
    }

    #[test]
    fn restore_puts_back_the_aside_and_the_backup_and_drops_the_rest() {
        let root = tempfile::tempdir().unwrap();
        let paths = paths(root.path());
        put(&paths.target, "new");
        put(&paths.aside, "old");
        put(&paths.prior, "copy-of-old");
        put(&paths.backup, "old-prior");
        put(&paths.fresh, "half");
        paths.restore(true).unwrap();
        assert_eq!(read(&paths.target).as_deref(), Some("old"));
        assert_eq!(read(&paths.prior).as_deref(), Some("old-prior"));
        assert!(!exists(&paths.aside) && !exists(&paths.backup) && !exists(&paths.fresh));
        // Idempotent.
        paths.restore(true).unwrap();
        assert_eq!(read(&paths.target).as_deref(), Some("old"));
        assert_eq!(read(&paths.prior).as_deref(), Some("old-prior"));
    }

    #[test]
    fn restore_without_an_earlier_prior_removes_the_new_one() {
        let root = tempfile::tempdir().unwrap();
        let paths = paths(root.path());
        put(&paths.target, "old");
        put(&paths.prior, "copy-of-old");
        put(&paths.incoming, "new");
        paths.restore(false).unwrap();
        assert_eq!(read(&paths.target).as_deref(), Some("old"));
        assert!(!exists(&paths.prior) && !exists(&paths.incoming));
    }

    #[test]
    fn restore_after_a_kill_between_the_two_renames_of_enter() {
        let root = tempfile::tempdir().unwrap();
        let paths = paths(root.path());
        put(&paths.aside, "old");
        put(&paths.incoming, "new");
        put(&paths.prior, "old-prior");
        paths.restore(true).unwrap();
        assert_eq!(read(&paths.target).as_deref(), Some("old"));
        assert_eq!(read(&paths.prior).as_deref(), Some("old-prior"), "the prior was never moved");
    }

    #[test]
    fn commit_never_touches_the_target_or_the_prior() {
        let root = tempfile::tempdir().unwrap();
        let paths = paths(root.path());
        put(&paths.target, "new");
        put(&paths.aside, "old");
        put(&paths.prior, "copy-of-old");
        put(&paths.backup, "old-prior");
        paths.commit().unwrap();
        paths.commit().unwrap();
        assert_eq!(read(&paths.target).as_deref(), Some("new"));
        assert_eq!(read(&paths.prior).as_deref(), Some("copy-of-old"));
        assert!(!exists(&paths.aside) && !exists(&paths.backup));
    }

    #[test]
    fn the_sweep_renames_back_an_orphaned_aside_and_removes_the_rest() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        put(&dir.join(".swap~old~demo~1~1"), "only-copy");
        put(&dir.join("other"), "live");
        put(&dir.join(".swap~old~other~1~1"), "stale");
        put(&dir.join(".incoming-other-1-1"), "x");
        put(&dir.join(".old-other-1-2"), "x");
        sweep_dir(dir);
        let mut names: Vec<String> =
            fs::read_dir(dir).unwrap().map(|entry| entry.unwrap().file_name().to_string_lossy().to_string()).collect();
        names.sort();
        assert_eq!(names, ["demo", "other"]);
        assert_eq!(read(&dir.join("demo")).as_deref(), Some("only-copy"));
        assert_eq!(read(&dir.join("other")).as_deref(), Some("live"));
    }

    #[test]
    fn swap_names_split_into_role_and_id() {
        assert_eq!(swap_name_parts(".swap~old~demo~12~3"), Some(("old", "demo")));
        assert_eq!(swap_name_parts(".swap~bak~a-b-c~1~1"), Some(("bak", "a-b-c")));
        assert_eq!(swap_name_parts(".upload-1"), None);
    }
}
