//! A workspace on disk: the file surface and the git history of `@almadar/workspace`'s `WorkspaceService`
//! (`service.ts`, `open-workspace.ts`, `internal/workspace-manager.ts`), without the observer fan-out, mounts,
//! orbital/session accessors, GitHub linking, semantic index and identity ledger (see the gap ledger).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::clock::Clock;
use super::git::{GitClient, GitHistoryEntry};
use super::layout::{
    self, sandboxed_path, APP_MARKER, APP_PATHSPECS, ORBITALS_DIR, ORBITAL_SPEC_FILE, SESSIONS_DIR,
};
use super::port::{HistoryFileChange, HistoryState, SnapshotMeta, TreeNode, WorkspacePort};
use crate::error::IntegrationError;

const KIND_TRAILER: &str = "Almadar-Kind";
const RESTORES_TRAILER: &str = "Restores";
const UNDOES_TRAILER: &str = "Undoes";
const REDOES_TRAILER: &str = "Redoes";
const ARCHIVED_REF: &str = "refs/almadar/archived";
const ARCHIVE_INCREMENTS_KEY: &str = "almadar.archiveIncrements";
const MAX_ARCHIVE_INCREMENTS: u64 = 20;
const SNAPSHOT_KINDS: [&str; 7] = [
    "user", "agent", "restore", "undo", "redo", "snapshot", "merge",
];

/// Where a workspace's git history is archived.
pub trait ArchiveBackend: Send + Sync {
    /// Replace the whole archive with one whole-history bundle.
    fn save_bundle(&self, bytes: &[u8]) -> Result<(), IntegrationError>;
    fn append_increment(&self, bytes: &[u8]) -> Result<(), IntegrationError>;
    /// The whole-history bundle then its increments in order; `None` when nothing is archived.
    fn load_bundles(&self) -> Result<Option<Vec<Vec<u8>>>, IntegrationError>;
}

fn io(error: std::io::Error) -> IntegrationError {
    IntegrationError::plain(error.to_string())
}

pub struct DiskWorkspace {
    work_dir: PathBuf,
    git: Option<GitClient>,
    archive: Option<Arc<dyn ArchiveBackend>>,
    clock: Clock,
    lock: Mutex<()>,
}

pub struct OpenOptions<'a> {
    pub root: &'a Path,
    pub user_id: &'a str,
    pub app_id: &'a str,
    pub project_name: &'a str,
    /// Files to restore into a freshly minted directory when no local one carries `app_id`.
    pub restore: Option<&'a dyn RestoreSource>,
    pub archive: Option<Arc<dyn ArchiveBackend>>,
    pub clock: Clock,
}

pub trait RestoreSource {
    fn list_files(&self) -> Result<Vec<String>, IntegrationError>;
    fn read_file(&self, path: &str) -> Result<Option<String>, IntegrationError>;
}

struct Marker {
    app_id: String,
    created_at: u64,
}

fn read_marker(work_dir: &Path) -> Option<Marker> {
    let raw = fs::read_to_string(work_dir.join(APP_MARKER)).ok()?;
    let parsed: serde_json::Value = serde_json::from_str(&raw).ok()?;
    parsed.get("userId")?.as_str()?;
    Some(Marker {
        app_id: parsed.get("appId")?.as_str()?.to_string(),
        created_at: parsed.get("createdAt")?.as_f64()? as u64,
    })
}

fn find_local_dir(root: &Path, user_id: &str, app_id: &str) -> Option<PathBuf> {
    let user_dir = root.join(user_id);
    let entries = fs::read_dir(&user_dir).ok()?;
    for entry in entries.flatten() {
        let session = user_dir.join(entry.file_name());
        if !session.is_dir() {
            continue;
        }
        if read_marker(&session).is_some_and(|m| m.app_id == app_id) {
            return Some(session);
        }
    }
    None
}

fn mint_session_dir(root: &Path, user_id: &str, clock: &Clock) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let ms = clock.now_ms();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    let salt = (nanos
        ^ ((std::process::id() as u64) << 8)
        ^ (COUNTER.fetch_add(1, Ordering::Relaxed) * 0x9e37))
        & 0xffff;
    let date = crate::wallclock::utc_date(ms as i64);
    root.join(user_id)
        .join(format!("{date}_{}{salt:04x}", layout::base36(ms)))
}

fn guard_foreign_mint_target(work_dir: &Path) -> Result<(), IntegrationError> {
    if read_marker(work_dir).is_some()
        || work_dir.join(layout::SCHEMA_FILE).exists()
        || work_dir.join(SESSIONS_DIR).exists()
        || !work_dir.exists()
        || std::env::var("ALMADAR_WORKSPACE_ADOPT_NONEMPTY").is_ok_and(|v| v == "1")
    {
        return Ok(());
    }
    let Ok(entries) = fs::read_dir(work_dir) else {
        return Ok(());
    };
    let foreign: Vec<String> = entries
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| !layout::WORKSPACE_OWNED_ENTRIES.contains(&name.as_str()))
        .collect();
    if foreign.is_empty() {
        return Ok(());
    }
    let shown: Vec<&str> = foreign.iter().take(5).map(String::as_str).collect();
    Err(IntegrationError::plain(format!(
        "refusing to mint a workspace into {} — it already contains non-workspace content ({}{}). Pass an empty/dedicated workDir, or set ALMADAR_WORKSPACE_ADOPT_NONEMPTY=1 to adopt this directory anyway.",
        work_dir.display(),
        shown.join(", "),
        if foreign.len() > 5 { ", …" } else { "" }
    )))
}

fn write_if_missing(path: &Path, content: &str) -> Result<(), IntegrationError> {
    if path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(io)?;
    }
    fs::write(path, content).map_err(io)
}

fn pathspecs() -> Vec<String> {
    APP_PATHSPECS.iter().map(|s| s.to_string()).collect()
}

impl DiskWorkspace {
    /// Resume the workspace for `app_id` under `root/user_id`, else restore or mint it, and make it ready.
    pub fn open(options: OpenOptions<'_>) -> Result<Self, IntegrationError> {
        let OpenOptions {
            root,
            user_id,
            app_id,
            project_name,
            restore,
            archive,
            clock,
        } = options;
        let work_dir = match find_local_dir(root, user_id, app_id) {
            Some(dir) => dir,
            None => {
                let dir = mint_session_dir(root, user_id, &clock);
                fs::create_dir_all(&dir).map_err(io)?;
                if let Some(source) = restore {
                    restore_files(&dir, source)?;
                }
                dir
            }
        };
        guard_foreign_mint_target(&work_dir)?;
        fs::create_dir_all(&work_dir).map_err(io)?;
        for dir in [
            layout::ALMADAR_DIR,
            ORBITALS_DIR,
            SESSIONS_DIR,
            layout::COORDINATOR_DIR,
        ] {
            fs::create_dir_all(work_dir.join(dir)).map_err(io)?;
        }
        write_if_missing(
            &work_dir.join(layout::SCHEMA_FILE),
            &layout::schema_template(project_name),
        )?;
        write_if_missing(
            &work_dir.join(layout::USER_MEMORY),
            &layout::user_template(user_id),
        )?;
        write_if_missing(
            &work_dir.join(layout::PROJECT_MEMORY),
            &layout::project_template(project_name, Some(app_id), &clock.iso()),
        )?;
        let existing = read_marker(&work_dir);
        if existing.as_ref().is_none_or(|m| m.app_id != app_id) {
            let created = existing.map_or(clock.now_ms(), |m| m.created_at);
            fs::write(
                work_dir.join(APP_MARKER),
                layout::app_marker(app_id, user_id, created),
            )
            .map_err(io)?;
        }
        let git = match ensure_git_init(&work_dir, &clock) {
            Ok(git) => Some(git),
            Err(error) => {
                eprintln!(
                    "[workspace] git-unavailable in {}: {}",
                    work_dir.display(),
                    error.message
                );
                None
            }
        };
        fs::write(
            work_dir.join(layout::INDEX_MANIFEST),
            layout::empty_index_manifest(clock.now_ms()),
        )
        .map_err(io)?;
        Ok(Self {
            work_dir,
            git,
            archive,
            clock,
            lock: Mutex::new(()),
        })
    }

    pub fn work_dir(&self) -> &Path {
        &self.work_dir
    }

    fn guard(&self) -> std::sync::MutexGuard<'_, ()> {
        self.lock.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Upload only what came after the last archived commit (or everything, past the increment cap).
    pub fn archive(&self) -> Result<(), IntegrationError> {
        let _guard = self.guard();
        let (Some(git), Some(archive)) = (&self.git, &self.archive) else {
            return Ok(());
        };
        let Some(head) = git.head_sha()? else {
            return Ok(());
        };
        let since = git.ref_sha(ARCHIVED_REF);
        let increments: u64 = git
            .get_config(ARCHIVE_INCREMENTS_KEY)
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let full = since.is_none() || increments >= MAX_ARCHIVE_INCREMENTS;
        let bundle = self.work_dir.join(".almadar-archive.bundle");
        let uploaded = (|| {
            if !git.bundle_since(&bundle, if full { None } else { since.as_deref() })? {
                return Ok(false);
            }
            let bytes = fs::read(&bundle).map_err(io)?;
            if full {
                archive.save_bundle(&bytes)?;
            } else {
                archive.append_increment(&bytes)?;
            }
            Ok(true)
        })();
        let _ = fs::remove_file(&bundle);
        if !uploaded? {
            return Ok(());
        }
        git.update_ref(ARCHIVED_REF, &head)?;
        git.set_config(
            ARCHIVE_INCREMENTS_KEY,
            &(if full { 0 } else { increments + 1 }).to_string(),
        )
    }

    /// Bring the archived history back into `.git`, keeping the working tree as it is.
    pub fn hydrate_from_archive(&self) -> Result<bool, IntegrationError> {
        let _guard = self.guard();
        let (Some(git), Some(archive)) = (&self.git, &self.archive) else {
            return Ok(false);
        };
        let Some(bundles) = archive.load_bundles()? else {
            return Ok(false);
        };
        let Some((base, increments)) = bundles.split_first() else {
            return Ok(false);
        };
        let parent = self.work_dir.parent().unwrap_or(Path::new("."));
        let name = self
            .work_dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let bundle_path = parent.join(format!("{name}.bundle"));
        let clone_dir = parent.join(format!("{name}.hydrate"));
        let outcome = (|| {
            fs::write(&bundle_path, base).map_err(io)?;
            let _ = fs::remove_dir_all(&clone_dir);
            git.clone_bundle(&bundle_path, &clone_dir)?;
            let clone = GitClient::new(&clone_dir, self.clock.clone());
            for increment in increments {
                fs::write(&bundle_path, increment).map_err(io)?;
                clone.fetch_bundle(&bundle_path)?;
            }
            let dot_git = self.work_dir.join(".git");
            let _ = fs::remove_dir_all(&dot_git);
            fs::rename(clone_dir.join(".git"), &dot_git).map_err(io)
        })();
        let _ = fs::remove_dir_all(&clone_dir);
        let _ = fs::remove_file(&bundle_path);
        outcome?;
        git.ensure_identity()?;
        if let Some(head) = git.head_sha()? {
            git.update_ref(ARCHIVED_REF, &head)?;
        }
        git.set_config(ARCHIVE_INCREMENTS_KEY, &increments.len().to_string())?;
        Ok(true)
    }

    fn sandbox(&self, rel: &str) -> Result<PathBuf, IntegrationError> {
        sandboxed_path(&self.work_dir, rel)
    }

    fn list_snapshots_unlocked(&self, limit: Option<&str>) -> Vec<SnapshotMeta> {
        let Some(git) = &self.git else {
            return Vec::new();
        };
        git.history(&pathspecs(), limit)
            .iter()
            .map(to_snapshot_meta)
            .collect()
    }

    fn commit_history_step(
        &self,
        git: &GitClient,
        message: &str,
        trailers: &[(&str, String)],
    ) -> Result<Option<String>, IntegrationError> {
        match git.commit(message, trailers)? {
            Some(oid) => Ok(Some(oid)),
            None => git.head_sha(),
        }
    }

    /// Stage the app files as git has them at `oid` (`None` = no app files), in the index and the working tree.
    fn stage_app_tree(&self, git: &GitClient, oid: Option<&str>) -> Result<(), IntegrationError> {
        let specs = pathspecs();
        let mut to_restore = Vec::new();
        let mut to_remove = Vec::new();
        if let Some(oid) = oid {
            for change in git.diff_from(oid, &specs)? {
                if change.status != 'A' {
                    to_restore.push(change.path);
                } else if !(is_orbital_spec(&change.path)
                    && self.has_orbital_at(git, oid, &orbital_of_spec(&change.path)))
                {
                    to_remove.push(change.path);
                }
            }
            git.restore_paths(oid, &to_restore)?;
        } else {
            to_remove.extend(git.tracked_files(&specs)?);
        }
        git.remove_paths(&to_remove)
    }

    fn has_orbital_at(&self, git: &GitClient, oid: &str, orbital: &str) -> bool {
        git.show(oid, &format!("{ORBITALS_DIR}/{orbital}.orb"))
            .is_some()
            || git
                .show(oid, &format!("{ORBITALS_DIR}/{orbital}.lolo"))
                .is_some()
    }
}

fn restore_files(root: &Path, source: &dyn RestoreSource) -> Result<(), IntegrationError> {
    let root = layout::normalize(root);
    for relative in source.list_files()? {
        let normalised = relative.replace('\\', "/");
        let target = layout::normalize(&root.join(&normalised));
        if target == root || !target.starts_with(&root) {
            continue;
        }
        let Some(content) = source.read_file(&normalised)? else {
            continue;
        };
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(io)?;
        }
        fs::write(&target, content).map_err(io)?;
    }
    Ok(())
}

fn ensure_git_init(work_dir: &Path, clock: &Clock) -> Result<GitClient, IntegrationError> {
    let git = GitClient::new(work_dir, clock.clone());
    git.init()?;
    let ignore_path = work_dir.join(".gitignore");
    let existed = ignore_path.exists();
    let current = layout::gitignore();
    let matches = existed
        && fs::read_to_string(&ignore_path)
            .map(|text| text == current)
            .unwrap_or(false);
    if !matches {
        fs::write(&ignore_path, &current).map_err(io)?;
        if existed && git.head_sha()?.is_some() {
            let almadar = vec![layout::ALMADAR_DIR.to_string()];
            git.untrack(&almadar)?;
            git.add_paths(&almadar)?;
            git.commit(
                "snapshot: track the app state in .almadar",
                &[(KIND_TRAILER, "snapshot".to_string())],
            )?;
        }
    }
    let legacy = work_dir.join(layout::LEGACY_ARCHIVE_DIR);
    if legacy.exists() {
        if git.head_sha()?.is_some() {
            git.remove_paths(&[layout::LEGACY_ARCHIVE_DIR.to_string()])?;
            git.commit(
                &format!(
                    "snapshot: drop {} (git history keeps removed orbitals)",
                    layout::LEGACY_ARCHIVE_DIR
                ),
                &[(KIND_TRAILER, "snapshot".to_string())],
            )?;
        }
        let _ = fs::remove_dir_all(&legacy);
    }
    Ok(git)
}

fn is_orbital_spec(rel: &str) -> bool {
    let prefix = format!("{SESSIONS_DIR}/");
    let suffix = format!("/{ORBITAL_SPEC_FILE}");
    if !rel.starts_with(&prefix)
        || !rel.ends_with(&suffix)
        || rel.len() < prefix.len() + suffix.len()
    {
        return false;
    }
    let orbital = &rel[prefix.len()..rel.len() - suffix.len()];
    !orbital.is_empty() && !orbital.contains('/')
}

fn orbital_of_spec(rel: &str) -> String {
    rel[SESSIONS_DIR.len() + 1..rel.len() - ORBITAL_SPEC_FILE.len() - 1].to_string()
}

fn to_snapshot_meta(entry: &GitHistoryEntry) -> SnapshotMeta {
    let non_empty = |key: &str| {
        entry
            .trailer(key)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    };
    SnapshotMeta {
        id: entry.oid.clone(),
        timestamp_ms: entry.timestamp_ms,
        reason: entry
            .subject
            .strip_prefix("snapshot: ")
            .unwrap_or(&entry.subject)
            .to_string(),
        kind: entry
            .trailer(KIND_TRAILER)
            .filter(|k| SNAPSHOT_KINDS.contains(k))
            .map(str::to_string),
        restores: non_empty(RESTORES_TRAILER),
        undoes: non_empty(UNDOES_TRAILER),
        redoes: non_empty(REDOES_TRAILER),
    }
}

/// The newest change (not an undo/redo) that no live undo has undone. History is newest first.
pub fn undo_target(history: &[SnapshotMeta]) -> Option<String> {
    let reversed = reversed_undos(history);
    let undone: Vec<&str> = history
        .iter()
        .filter_map(|e| {
            e.undoes
                .as_deref()
                .filter(|_| !reversed.contains(&e.id.as_str()))
        })
        .collect();
    for e in history {
        if matches!(e.kind.as_deref(), Some("undo") | Some("redo")) {
            continue;
        }
        if !undone.contains(&e.id.as_str()) {
            return Some(e.id.clone());
        }
    }
    None
}

/// The newest live undo, reachable only through undo/redo commits (a new change clears redo).
pub fn redo_target(history: &[SnapshotMeta]) -> Option<String> {
    let reversed = reversed_undos(history);
    for e in history {
        match e.kind.as_deref() {
            Some("redo") => continue,
            Some("undo") => {
                if reversed.contains(&e.id.as_str()) {
                    continue;
                }
                return Some(e.id.clone());
            }
            _ => return None,
        }
    }
    None
}

fn reversed_undos(history: &[SnapshotMeta]) -> Vec<&str> {
    history.iter().filter_map(|e| e.redoes.as_deref()).collect()
}

fn short(oid: &str) -> String {
    oid.chars().take(7).collect()
}

fn walk(dir: &Path, root: &Path, out: &mut Vec<TreeNode>) -> Result<(), IntegrationError> {
    for entry in fs::read_dir(dir).map_err(io)? {
        let full = dir.join(entry.map_err(io)?.file_name());
        let stat = fs::metadata(&full).map_err(io)?;
        let rel = full
            .strip_prefix(root)
            .unwrap_or(&full)
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");
        if stat.is_dir() {
            out.push(TreeNode {
                path: rel,
                is_directory: true,
                size: 0,
            });
            walk(&full, root, out)?;
        } else {
            out.push(TreeNode {
                path: rel,
                is_directory: false,
                size: stat.len(),
            });
        }
    }
    Ok(())
}

impl WorkspacePort for DiskWorkspace {
    fn list_tree(&self, rel_path: Option<&str>) -> Result<Vec<TreeNode>, IntegrationError> {
        let _guard = self.guard();
        let base = match rel_path {
            None => self.work_dir.clone(),
            Some(rel) => self.sandbox(rel)?,
        };
        if !base.exists() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        walk(&base, &self.work_dir, &mut out)?;
        Ok(out)
    }

    fn read_file(&self, rel_path: &str) -> Result<Option<String>, IntegrationError> {
        let _guard = self.guard();
        let abs = self.sandbox(rel_path)?;
        if !abs.exists() {
            return Ok(None);
        }
        Ok(fs::read(&abs)
            .ok()
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned()))
    }

    fn write_file(&self, rel_path: &str, content: &str) -> Result<(), IntegrationError> {
        let _guard = self.guard();
        let abs = self.sandbox(rel_path)?;
        if let Some(parent) = abs.parent() {
            fs::create_dir_all(parent).map_err(io)?;
        }
        fs::write(&abs, content).map_err(io)
    }

    fn remove_file(&self, rel_path: &str) -> Result<(), IntegrationError> {
        let _guard = self.guard();
        let abs = self.sandbox(rel_path)?;
        if abs.exists() {
            fs::remove_file(&abs).map_err(io)?;
        }
        Ok(())
    }

    fn exists(&self, rel_path: &str) -> Result<bool, IntegrationError> {
        let _guard = self.guard();
        Ok(self
            .sandbox(rel_path)
            .map(|abs| abs.exists())
            .unwrap_or(false))
    }

    fn snapshot(&self, reason: &str) -> Result<Option<String>, IntegrationError> {
        let _guard = self.guard();
        let Some(git) = &self.git else {
            return Ok(None);
        };
        git.add_all()?;
        git.commit(
            &format!("snapshot: {reason}"),
            &[(KIND_TRAILER, "snapshot".to_string())],
        )
    }

    fn list_snapshots(&self, limit: Option<&str>) -> Result<Vec<SnapshotMeta>, IntegrationError> {
        let _guard = self.guard();
        Ok(self.list_snapshots_unlocked(limit))
    }

    fn history_state(&self) -> Result<HistoryState, IntegrationError> {
        let _guard = self.guard();
        let entries = self.list_snapshots_unlocked(None);
        Ok(HistoryState {
            can_undo: undo_target(&entries).is_some(),
            can_redo: redo_target(&entries).is_some(),
        })
    }

    fn changes_at(&self, oid: &str) -> Result<Vec<HistoryFileChange>, IntegrationError> {
        let _guard = self.guard();
        let Some(git) = &self.git else {
            return Ok(Vec::new());
        };
        if !git.is_commit(oid) {
            return Ok(Vec::new());
        }
        let parent = git.parent_of(oid);
        let mut files = git.changed_files(oid, &pathspecs())?;
        files.sort();
        Ok(files
            .into_iter()
            .map(|path| HistoryFileChange {
                before: parent.as_ref().and_then(|p| git.show(p, &path)),
                after: git.show(oid, &path),
                path,
            })
            .collect())
    }

    fn restore(&self, oid: &str) -> Result<Option<String>, IntegrationError> {
        let _guard = self.guard();
        let Some(git) = &self.git else {
            return Ok(None);
        };
        if !git.is_commit(oid) {
            return Ok(None);
        }
        self.stage_app_tree(git, Some(oid))?;
        self.commit_history_step(
            git,
            &format!("restore: {}", short(oid)),
            &[
                (KIND_TRAILER, "restore".to_string()),
                (RESTORES_TRAILER, oid.to_string()),
            ],
        )
    }

    fn undo(&self) -> Result<Option<String>, IntegrationError> {
        let _guard = self.guard();
        let Some(git) = &self.git else {
            return Ok(None);
        };
        let Some(target) = undo_target(&self.list_snapshots_unlocked(None)) else {
            return Ok(None);
        };
        self.stage_app_tree(git, git.parent_of(&target).as_deref())?;
        self.commit_history_step(
            git,
            &format!("undo: {}", short(&target)),
            &[(KIND_TRAILER, "undo".to_string()), (UNDOES_TRAILER, target)],
        )
    }

    fn redo(&self) -> Result<Option<String>, IntegrationError> {
        let _guard = self.guard();
        let Some(git) = &self.git else {
            return Ok(None);
        };
        let Some(target) = redo_target(&self.list_snapshots_unlocked(None)) else {
            return Ok(None);
        };
        self.stage_app_tree(git, git.parent_of(&target).as_deref())?;
        self.commit_history_step(
            git,
            &format!("redo: {}", short(&target)),
            &[(KIND_TRAILER, "redo".to_string()), (REDOES_TRAILER, target)],
        )
    }
}
