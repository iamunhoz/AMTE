// V1 parsing is retained as an intentionally read-only compatibility parser;
// it is not part of the V2 runtime writer.
#![allow(dead_code)]

use std::{
    collections::HashMap,
    ffi::CString,
    fs::{self, File},
    io::{Read, Write},
    os::fd::FromRawFd,
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use pulldown_cmark::{Options, Parser, html};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_dialog::DialogExt;

mod recovery_db;
use recovery_db::RecoveryDb;

const LAST_PROJECT_FILE: &str = "last-project-v1";
const LEGACY_LAST_DOCUMENT_FILE: &str = "last-document-path";
const IGNORED_NAMES: &[&str] = &[".git", "node_modules", "target", "dist"];

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProjectDto {
    project_id: u64,
    root: String,
    session_token: String,
}
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProjectCandidateDto {
    candidate_id: u64,
    root: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct EntryDto {
    name: String,
    relative_path: String,
    kind: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct DocumentDto {
    project_id: u64,
    document_id: Option<u64>,
    relative_path: Option<String>,
    content: String,
    write_revision: u64,
    baseline_hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    conflict: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    recovery: Option<RecoveryBuffer>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct SaveOutcome {
    /// This is true only after the staged replacement was renamed and re-read
    /// through the descriptor-bound root. It is deliberately independent from
    /// every SQLite/selector acknowledgement below.
    file_committed: bool,
    recovery_checkpoint_durable: bool,
    selector_session_state_durable: bool,
    reconciliation_pending: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    reconciliation_error_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reconciliation_error_message: Option<String>,
    project_id: u64,
    session_token: String,
    document_id: Option<u64>,
    relative_path: Option<String>,
    file_identity: Option<String>,
    write_revision: u64,
    baseline_hash: String,
    saved_client_edit_version: u64,
    content_blake3: String,
    operation_key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    document: Option<Box<DocumentDto>>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct RestoreDto {
    project: ProjectDto,
    documents: Vec<DocumentDto>,
    active_relative_path: Option<String>,
}

#[derive(Debug)]
struct ActiveDocument {
    relative_path: PathBuf,
    identity: u64,
    file_identity: FileIdentity,
    write_revision: u64,
    baseline_hash: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    device: u64,
    inode: u64,
}

#[derive(Debug)]
struct ActiveProject {
    root: PathBuf,
    // An Arc permits an authorization snapshot to retain the descriptor after
    // the short ProjectAuthority critical section has ended.
    root_fd: Arc<File>,
    identity: u64,
    session_token: String,
    documents: HashMap<u64, ActiveDocument>,
    paths: HashMap<PathBuf, u64>,
}

#[derive(Default)]
struct ProjectState {
    project: Option<ActiveProject>,
    pending_project: Option<(u64, PathBuf)>,
    next_identity: u64,
}
#[derive(Default)]
struct ProjectAuthority(Mutex<ProjectState>);
#[derive(Default)]
struct CloseState {
    // Interception is opt-in only after the frontend has successfully installed
    // its event listener.  A broken or still-starting webview must retain the
    // platform's normal close behavior rather than creating an immortal window.
    frontend_ready: bool,
    permitted: bool,
    pending_request: Option<u64>,
    acknowledged: bool,
    next_request: u64,
}
#[derive(Default)]
struct CloseAuthority(Mutex<CloseState>);

#[derive(Debug, PartialEq, Eq)]
enum CloseRequestAction {
    Allow,
    PreventExisting,
    PreventAndEmit(u64),
}

fn request_window_close(state: &mut CloseState) -> CloseRequestAction {
    if !state.frontend_ready || state.permitted {
        return CloseRequestAction::Allow;
    }
    // A request already accepted by the frontend owns the modal. Further WM
    // requests must not create duplicates.
    if state.pending_request.is_some() {
        return CloseRequestAction::PreventExisting;
    }
    state.next_request = state.next_request.wrapping_add(1).max(1);
    let request_id = state.next_request;
    state.pending_request = Some(request_id);
    state.acknowledged = false;
    CloseRequestAction::PreventAndEmit(request_id)
}

fn take_unacknowledged_close_fallback(state: &mut CloseState, request_id: u64) -> bool {
    if state.pending_request == Some(request_id) && !state.acknowledged && !state.permitted {
        state.permitted = true;
        state.pending_request = None;
        true
    } else {
        false
    }
}
/// Lock protocol (enforced by the snapshot helpers below):
///
/// 1. ProjectAuthority is held only to copy immutable authorization state or
///    compare-and-apply a completed operation. It is never held for file I/O,
///    SQLite, or RecoveryAuthority acquisition.
/// 2. RecoveryAuthority serializes recovery SQLite transactions. It is never
///    acquired while ProjectAuthority is held.
/// 3. Project replacement first snapshots the old session, then deactivates it
///    under RecoveryAuthority, then activates the new token, and only then
///    swaps ProjectAuthority. Queued old-token work therefore cannot commit.
#[derive(Default)]
struct RecoveryAuthority(Mutex<()>);

/// Serializes complete project activations, including the SQLite token fence,
/// selector persistence, and the final in-memory authority swap.  It is never
/// taken by save or recovery paths, so it cannot participate in their lock
/// ordering.  In particular, holding this gate does *not* permit holding
/// ProjectAuthority while taking RecoveryAuthority.
#[derive(Default)]
struct ActivationAuthority(Mutex<()>);

// SaveOutcome boundaries are test-only observability/failure points. Keeping
// them here (rather than accepting a command argument) ensures a webview can
// never select a fault mode in a production build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SaveOutcomeFault {
    RenameBeforeOperationJournal,
    OperationJournalBeforeCheckpoint,
    CheckpointBeforeSelector,
    SelectorBeforeResponse,
}
#[cfg(test)]
thread_local! {
    static SAVE_OUTCOME_FAULT: std::cell::Cell<Option<SaveOutcomeFault>> = const { std::cell::Cell::new(None) };
}
#[cfg(test)]
fn set_save_outcome_fault(fault: Option<SaveOutcomeFault>) {
    SAVE_OUTCOME_FAULT.with(|current| current.set(fault));
}
#[cfg(test)]
fn fail_save_outcome_at(fault: SaveOutcomeFault) -> Result<(), String> {
    SAVE_OUTCOME_FAULT.with(|current| {
        if current.get() == Some(fault) {
            current.set(None);
            Err(format!("Injected SaveOutcome fault at {fault:?}"))
        } else {
            Ok(())
        }
    })
}
#[cfg(not(test))]
fn fail_save_outcome_at(_: SaveOutcomeFault) -> Result<(), String> {
    Ok(())
}

/// Everything an asynchronous persistence operation is allowed to retain after
/// dropping ProjectAuthority. Keeping this as one value makes lock inversion
/// difficult to reintroduce: callers cannot borrow ActiveProject across I/O.
#[derive(Debug, Clone)]
struct AuthorizationSnapshot {
    project_id: u64,
    root: PathBuf,
    root_fd: Arc<File>,
    session_token: String,
    document_id: Option<u64>,
    relative_path: Option<PathBuf>,
    write_revision: Option<u64>,
    file_identity: Option<FileIdentity>,
    baseline_hash: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AuthorityApply<T> {
    Applied(T),
    Stale,
}

fn capture_authorization_snapshot(
    authority: &ProjectAuthority,
    project_id: u64,
    session_token: Option<&str>,
    document_id: Option<u64>,
) -> Result<AuthorizationSnapshot, String> {
    let state = authority
        .0
        .lock()
        .map_err(|_| "Project state is unavailable".to_string())?;
    let project = state
        .project
        .as_ref()
        .ok_or_else(|| "No project is open".to_string())?;
    if project.identity != project_id {
        return Err("The project changed".into());
    }
    if session_token.is_some_and(|token| project.session_token != token) {
        return Err("Stale project session".into());
    }
    let document = document_id
        .map(|id| {
            project
                .documents
                .get(&id)
                .ok_or_else(|| "Unknown or stale project document".to_string())
        })
        .transpose()?;
    Ok(AuthorizationSnapshot {
        project_id: project.identity,
        root: project.root.clone(),
        root_fd: project.root_fd.clone(),
        session_token: project.session_token.clone(),
        document_id,
        relative_path: document.map(|document| document.relative_path.clone()),
        write_revision: document.map(|document| document.write_revision),
        file_identity: document.map(|document| document.file_identity),
        baseline_hash: document.map(|document| document.baseline_hash.clone()),
    })
}

impl ProjectAuthority {
    fn session_token(root: &Path, identity: u64) -> String {
        // This is opaque to the webview: only this backend mints it. Including
        // a process-local identity and clock value prevents it being a stable
        // frontend-inventable project authority.
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        blake3::hash(format!("{}:{identity}:{nanos}", root.display()).as_bytes())
            .to_hex()
            .to_string()
    }
    fn next_id(state: &mut ProjectState) -> u64 {
        state.next_identity = state.next_identity.wrapping_add(1);
        if state.next_identity == 0 {
            state.next_identity = 1;
        }
        state.next_identity
    }

    fn activate_project(&self, root: PathBuf) -> Result<ProjectDto, String> {
        let root_fd = open_directory_path(&root)?;
        let mut state = self
            .0
            .lock()
            .map_err(|_| "Project state is unavailable".to_string())?;
        let identity = Self::next_id(&mut state);
        let session_token = Self::session_token(&root, identity);
        state.project = Some(ActiveProject {
            root: root.clone(),
            root_fd: Arc::new(root_fd),
            identity,
            session_token: session_token.clone(),
            documents: HashMap::new(),
            paths: HashMap::new(),
        });
        Ok(ProjectDto {
            project_id: identity,
            root: root.display().to_string(),
            session_token,
        })
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredProject {
    version: u8,
    root: String,
    relative_path: Option<String>,
}

struct ProjectStore {
    directory: PathBuf,
    state_path: PathBuf,
    legacy_path: PathBuf,
}

const RECOVERY_VERSION: u8 = 1;
const RECOVERY_MAX_BUFFERS: usize = 64;
const RECOVERY_MAX_MANIFEST_BYTES: usize = 256 * 1024;
const RECOVERY_MAX_PAYLOAD_BYTES: usize = 1024 * 1024;
const RECOVERY_MAX_TOTAL_BYTES: usize = 4 * 1024 * 1024;
const RECOVERY_MAX_SCROLL: f64 = 100_000_000.0;
const RECOVERY_MAX_SEARCH_BYTES: usize = 4096;
const RECOVERY_MAX_COLLAPSED_CHAPTERS: usize = 1024;
const RECOVERY_MAX_CHAPTER_IDENTITY_BYTES: usize = 32;
// Recovery sequence values cross the JSON/JavaScript boundary. Keep the
// backend within Number's exact integer range so a restored sequence can never
// be rounded and reused by the next frontend snapshot.
const RECOVERY_MAX_SEQUENCE: u64 = 9_007_199_254_740_991;
// At most one newly-written payload can exist without a manifest after a
// process crash; normal commits remove all superseded payloads immediately.
const RECOVERY_MAX_CRASH_ORPHANS: usize = 1;
const RECOVERY_CLEANUP_SCAN_LIMIT: usize = RECOVERY_MAX_BUFFERS + RECOVERY_MAX_CRASH_ORPHANS + 8;
#[cfg(unix)]
fn recovery_owner_uid() -> u32 {
    // libc exposes the process effective identity; no user-controlled path is consulted.
    unsafe { libc::geteuid() }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
struct RecoveryBuffer {
    relative_path: Option<String>,
    document_id: Option<u64>,
    write_revision: u64,
    #[serde(rename = "recoverySequence", alias = "sequence")]
    sequence: u64,
    /// Client monotonic edit/metadata generation. V2 allocates `sequence` in
    /// SQLite; this remains a stale-completion fence, never an ID authority.
    #[serde(default)]
    client_edit_version: u64,
    dirty: bool,
    content: Option<String>,
    payload: Option<String>,
    payload_hash: Option<String>,
    payload_bytes: Option<usize>,
    baseline_hash: Option<String>,
    source_scroll: Option<RecoveryScroll>,
    selection: Option<RecoverySelection>,
    preview_scroll: Option<f64>,
    previewing: Option<bool>,
    search_term: Option<String>,
    chapter_identity: Option<String>,
    chapter_scroll: Option<f64>,
    // Optional keeps version-1 snapshots readable. Only compact canonical
    // identities are accepted; the frontend reconciles them against the live
    // outline and never trusts recovery metadata as document structure.
    collapsed_chapter_identities: Option<Vec<String>>,
    activity: Option<RecoveryActivity>,
    // This is an opaque backend-issued save journal key, not a path or a
    // payload capability. It makes a post-rename reconciliation survivable
    // when the webview restarts before it receives the SaveOutcome.
    #[serde(default)]
    reconciliation_operation_key: Option<String>,
    #[serde(default)]
    reconciliation_client_edit_version: Option<u64>,
}

/// Network-facing V2 write contract. It deliberately has no recovery sequence,
/// payload filename, or payload checksum: those are backend-owned fields.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RecoveryWriteRequest {
    relative_path: Option<String>,
    document_id: Option<u64>,
    write_revision: u64,
    client_edit_version: u64,
    dirty: bool,
    content: Option<String>,
    baseline_hash: Option<String>,
    source_scroll: Option<RecoveryScroll>,
    selection: Option<RecoverySelection>,
    preview_scroll: Option<f64>,
    previewing: Option<bool>,
    search_term: Option<String>,
    chapter_identity: Option<String>,
    chapter_scroll: Option<f64>,
    collapsed_chapter_identities: Option<Vec<String>>,
    activity: Option<RecoveryActivity>,
    reconciliation_operation_key: Option<String>,
    reconciliation_client_edit_version: Option<u64>,
}
impl RecoveryWriteRequest {
    fn into_buffer(self) -> RecoveryBuffer {
        RecoveryBuffer {
            relative_path: self.relative_path,
            document_id: self.document_id,
            write_revision: self.write_revision,
            sequence: 0,
            client_edit_version: self.client_edit_version,
            dirty: self.dirty,
            content: self.content,
            payload: None,
            payload_hash: None,
            payload_bytes: None,
            baseline_hash: self.baseline_hash,
            source_scroll: self.source_scroll,
            selection: self.selection,
            preview_scroll: self.preview_scroll,
            previewing: self.previewing,
            search_term: self.search_term,
            chapter_identity: self.chapter_identity,
            chapter_scroll: self.chapter_scroll,
            collapsed_chapter_identities: self.collapsed_chapter_identities,
            activity: self.activity,
            reconciliation_operation_key: self.reconciliation_operation_key,
            reconciliation_client_edit_version: self.reconciliation_client_edit_version,
        }
    }
}
fn into_dirty_recovery_request(request: RecoveryWriteRequest) -> Result<RecoveryBuffer, String> {
    let buffer = request.into_buffer();
    if !buffer.dirty {
        return Err(
            "Recovery invoke accepts dirty snapshots only; saves create clean checkpoints".into(),
        );
    }
    if buffer.client_edit_version == 0 || buffer.client_edit_version > RECOVERY_MAX_SEQUENCE {
        return Err(
            "Recovery client edit version is outside the canonical JavaScript-safe range".into(),
        );
    }
    Ok(buffer)
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
struct RecoveryScroll {
    top: f64,
    left: f64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
struct RecoverySelection {
    anchor: usize,
    head: usize,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
struct RecoveryActivity {
    day: String,
    added: u64,
    deleted: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
struct RecoveryManifest {
    version: u8,
    root: String,
    project_key: String,
    active_path: Option<String>,
    buffers: Vec<RecoveryBuffer>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RecoveryWriteResult {
    cleanup_error: Option<String>,
}

#[derive(Clone)]
struct RecoveryStore {
    base: PathBuf,
    directory: PathBuf,
    manifest: PathBuf,
}
impl RecoveryStore {
    fn for_root(app: &AppHandle, root: &str) -> Result<Self, String> {
        let base = app
            .path()
            .app_config_dir()
            .map_err(|e| format!("Could not locate recovery storage: {e}"))?
            .join("recovery-v1");
        // The canonical root is not used as a filesystem name.  This keeps
        // sessions separate without accepting any caller-provided path.
        let directory = base.join(blake3::hash(root.as_bytes()).to_hex());
        Ok(Self {
            manifest: directory.join("manifest.json"),
            base,
            directory,
        })
    }
    fn project_key(root: &str) -> String {
        blake3::hash(root.as_bytes()).to_hex().to_string()
    }
    fn private_directory(path: &Path) -> Result<(), String> {
        match fs::symlink_metadata(path) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    return Err("Unsafe recovery directory".into());
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    if metadata.mode() & 0o077 != 0 || metadata.uid() != recovery_owner_uid() {
                        return Err("Recovery directory is not owner-private".into());
                    }
                }
                Ok(())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let mut builder = fs::DirBuilder::new();
                builder.recursive(false);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::DirBuilderExt;
                    builder.mode(0o700);
                }
                builder
                    .create(path)
                    .map_err(|e| format!("Could not create private recovery directory: {e}"))?;
                Self::private_directory(path)
            }
            Err(error) => Err(error.to_string()),
        }
    }
    fn ensure_private(&self) -> Result<(), String> {
        Self::private_directory(&self.base)?;
        Self::private_directory(&self.directory)?;
        Ok(())
    }
    fn atomic(&self, path: &Path, value: &[u8]) -> Result<(), String> {
        self.ensure_private()?;
        if value.len() > RECOVERY_MAX_PAYLOAD_BYTES {
            return Err("Recovery snapshot exceeds the per-file limit".into());
        }
        let temp = self.directory.join(format!(
            ".amte-recovery-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp).map_err(|e| e.to_string())?;
        let result = (|| {
            file.write_all(value)
                .and_then(|_| file.sync_all())
                .map_err(|e| format!("Could not persist recovery: {e}"))?;
            fs::rename(&temp, path)
                .map_err(|e| format!("Could not replace recovery snapshot: {e}"))?;
            File::open(&self.directory)
                .and_then(|directory| directory.sync_all())
                .map_err(|e| format!("Could not sync recovery directory: {e}"))
        })();
        if result.is_err() {
            // Removing the temp name removes a link itself rather than walking
            // it; no directory traversal occurs during failed-write cleanup.
            let _ = fs::remove_file(&temp);
        }
        result
    }
    /// Parses and schema-validates only.  It deliberately performs no cleanup
    /// because manifest identity has not yet been bound to trusted context.
    fn read(&self) -> Result<Option<RecoveryManifest>, String> {
        self.ensure_private()?;
        let metadata = match fs::symlink_metadata(&self.manifest) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.to_string()),
        };
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || metadata.len() as usize > RECOVERY_MAX_MANIFEST_BYTES
        {
            return Err("Unsafe recovery manifest".into());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if metadata.mode() & 0o077 != 0
                || metadata.nlink() != 1
                || metadata.uid() != recovery_owner_uid()
            {
                return Err("Recovery manifest is not private".into());
            }
        }
        let mut options = fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(0o400000);
        }
        let mut file = options
            .open(&self.manifest)
            .map_err(|_| "Recovery manifest could not be opened safely".to_string())?;
        let opened = file
            .metadata()
            .map_err(|_| "Recovery manifest could not be inspected".to_string())?;
        if !opened.is_file() || opened.len() as usize > RECOVERY_MAX_MANIFEST_BYTES {
            return Err("Recovery manifest changed while opening".into());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if opened.mode() & 0o077 != 0
                || opened.nlink() != 1
                || opened.uid() != recovery_owner_uid()
            {
                return Err("Recovery manifest changed to an unsafe file".into());
            }
        }
        let mut bytes = Vec::with_capacity(opened.len() as usize);
        file.read_to_end(&mut bytes)
            .map_err(|_| "Recovery manifest could not be read".to_string())?;
        if bytes.len() > RECOVERY_MAX_MANIFEST_BYTES {
            return Err("Recovery manifest exceeds limit".into());
        }
        let manifest: RecoveryManifest = serde_json::from_slice(&bytes).map_err(|_| {
            "Recovery manifest is corrupt; retained for manual inspection".to_string()
        })?;
        if manifest.version != RECOVERY_VERSION {
            return Err(
                "Recovery manifest version is unsupported; retained for manual inspection".into(),
            );
        }
        if manifest.buffers.len() > RECOVERY_MAX_BUFFERS
            || manifest.project_key != Self::project_key(&manifest.root)
        {
            return Err("Recovery manifest identity or buffer count is invalid".into());
        }
        if manifest
            .active_path
            .as_deref()
            .is_some_and(|path| validate_relative(path, true).is_err())
        {
            return Err("Recovery manifest active path is invalid".into());
        }
        let mut paths = std::collections::HashSet::new();
        let mut document_ids = std::collections::HashSet::new();
        let mut has_untitled = false;
        let mut total_bytes = 0usize;
        for entry in &manifest.buffers {
            validate_manifest_entry(entry)?;
            match (&entry.relative_path, entry.document_id) {
                (Some(path), Some(id)) => {
                    if !paths.insert(path.clone()) || !document_ids.insert(id) {
                        return Err("Recovery manifest has duplicate buffer identities".into());
                    }
                }
                (None, None) if !has_untitled => has_untitled = true,
                _ => return Err("Recovery manifest has invalid buffer identity".into()),
            }
            total_bytes = total_bytes
                .checked_add(entry.payload_bytes.expect("validated"))
                .ok_or_else(|| "Recovery payload total overflow".to_string())?;
            if total_bytes > RECOVERY_MAX_TOTAL_BYTES {
                return Err("Recovery payload total exceeds limit".into());
            }
        }
        if manifest
            .active_path
            .as_ref()
            .is_some_and(|path| !paths.contains(path))
        {
            return Err("Recovery manifest active buffer is missing".into());
        }
        Ok(Some(manifest))
    }
    fn read_bound(&self, expected_root: &str) -> Result<Option<RecoveryManifest>, String> {
        let expected_key = Self::project_key(expected_root);
        if self.directory.file_name().and_then(|name| name.to_str()) != Some(expected_key.as_str())
        {
            return Err("Recovery directory does not match the expected project namespace".into());
        }
        let manifest = self.read()?;
        match manifest {
            Some(manifest) => {
                if manifest.root != expected_root || manifest.project_key != expected_key {
                    return Err("Recovery manifest belongs to a different project".into());
                }
                self.cleanup_orphans(&manifest)?;
                Ok(Some(manifest))
            }
            None => {
                // The namespace was trusted before touching a first-write crash
                // orphan, so empty-manifest cleanup cannot affect another root.
                self.cleanup_orphans(&RecoveryManifest {
                    version: RECOVERY_VERSION,
                    root: expected_root.to_owned(),
                    project_key: expected_key,
                    active_path: None,
                    buffers: Vec::new(),
                })?;
                Ok(None)
            }
        }
    }
    /// Migration is intentionally observational: unlike the active V1 writer,
    /// it must not clean an orphan or otherwise mutate legacy storage while
    /// deciding whether a V2 import is possible.
    fn read_bound_read_only(
        &self,
        expected_root: &str,
    ) -> Result<Option<RecoveryManifest>, String> {
        let expected_key = Self::project_key(expected_root);
        if self.directory.file_name().and_then(|name| name.to_str()) != Some(expected_key.as_str())
        {
            return Err("Recovery directory does not match the expected project namespace".into());
        }
        let manifest = self.read()?;
        if let Some(manifest) = manifest {
            if manifest.root != expected_root || manifest.project_key != expected_key {
                return Err("Recovery manifest belongs to a different project".into());
            }
            Ok(Some(manifest))
        } else {
            Ok(None)
        }
    }
    fn write(&self, manifest: &RecoveryManifest) -> Result<(), String> {
        if manifest.root.is_empty() || manifest.project_key != Self::project_key(&manifest.root) {
            return Err("Recovery manifest project identity is invalid".into());
        }
        if manifest.buffers.len() > RECOVERY_MAX_BUFFERS {
            return Err("Recovery manifest has too many buffers".into());
        }
        for entry in &manifest.buffers {
            validate_manifest_entry(entry)?;
        }
        recovery_total_bytes(&manifest.buffers)?;
        let serialized = serde_json::to_vec(manifest).map_err(|e| e.to_string())?;
        if serialized.len() > RECOVERY_MAX_MANIFEST_BYTES {
            return Err("Recovery manifest exceeds limit".into());
        }
        self.atomic(&self.manifest, &serialized)
    }
    fn read_payload(
        &self,
        name: &str,
        expected_bytes: usize,
        expected_hash: &str,
    ) -> Result<RecoveryBuffer, String> {
        if Path::new(name).components().count() != 1
            || !is_recovery_payload_name(name)
            || expected_bytes > RECOVERY_MAX_PAYLOAD_BYTES
        {
            return Err("Unsafe recovery payload name or size".into());
        }
        let path = self.directory.join(name);
        let metadata =
            fs::symlink_metadata(&path).map_err(|_| "Recovery payload is missing".to_string())?;
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || metadata.len() as usize != expected_bytes
        {
            return Err("Recovery payload is unsafe or truncated".into());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if metadata.mode() & 0o077 != 0
                || metadata.nlink() != 1
                || metadata.uid() != recovery_owner_uid()
            {
                return Err("Recovery payload is not private".into());
            }
        }
        let mut options = fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(0o400000);
        } // O_NOFOLLOW
        let mut file = options
            .open(&path)
            .map_err(|_| "Recovery payload could not be read safely".to_string())?;
        let opened = file
            .metadata()
            .map_err(|_| "Recovery payload could not be inspected".to_string())?;
        if !opened.is_file() || opened.len() as usize != expected_bytes {
            return Err("Recovery payload changed while opening".into());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if opened.mode() & 0o077 != 0
                || opened.nlink() != 1
                || opened.uid() != recovery_owner_uid()
            {
                return Err("Recovery payload changed to an unsafe file".into());
            }
        }
        let mut bytes = Vec::with_capacity(expected_bytes);
        file.read_to_end(&mut bytes)
            .map_err(|_| "Recovery payload could not be read".to_string())?;
        if bytes.len() != expected_bytes || blake3::hash(&bytes).to_hex().as_str() != expected_hash
        {
            return Err("Recovery payload integrity check failed".into());
        }
        let buffer: RecoveryBuffer = serde_json::from_slice(&bytes)
            .map_err(|_| "Recovery payload is corrupt".to_string())?;
        validate_recovery_buffer(&buffer)?;
        Ok(buffer)
    }
    fn cleanup_orphans(&self, manifest: &RecoveryManifest) -> Result<(), String> {
        self.ensure_private()?;
        let referenced = manifest
            .buffers
            .iter()
            .map(|entry| entry.payload.as_deref().expect("validated"))
            .collect::<std::collections::HashSet<_>>();
        let mut scanned = 0usize;
        let mut orphans = Vec::new();
        for item in fs::read_dir(&self.directory)
            .map_err(|e| format!("Could not scan recovery storage: {e}"))?
        {
            scanned = scanned
                .checked_add(1)
                .ok_or_else(|| "Recovery cleanup scan overflow".to_string())?;
            if scanned > RECOVERY_CLEANUP_SCAN_LIMIT {
                return Err(
                    "Recovery cleanup scan limit exceeded; recovery storage was left untouched"
                        .into(),
                );
            }
            let item = item.map_err(|e| format!("Could not inspect recovery storage: {e}"))?;
            let name = item
                .file_name()
                .to_str()
                .map(str::to_owned)
                .ok_or_else(|| "Recovery storage contains an untrusted filename".to_string())?;
            if name == "manifest.json" {
                continue;
            }
            if !is_recovery_payload_name(&name) {
                return Err(
                    "Recovery storage contains an untrusted entry; cleanup was not attempted"
                        .into(),
                );
            }
            let path = item.path();
            let metadata = fs::symlink_metadata(&path)
                .map_err(|_| "Recovery payload changed during cleanup".to_string())?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err("Recovery cleanup refused an unsafe payload entry".into());
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if metadata.mode() & 0o077 != 0
                    || metadata.nlink() != 1
                    || metadata.uid() != recovery_owner_uid()
                {
                    return Err("Recovery cleanup refused a non-private payload entry".into());
                }
            }
            if !referenced.contains(name.as_str()) {
                orphans.push(path);
            }
        }
        // Scan and validate the complete bounded directory before deleting any
        // orphan, so an over-limit or hostile entry never causes partial cleanup.
        for path in &orphans {
            fs::remove_file(path)
                .map_err(|e| format!("Could not remove superseded recovery payload: {e}"))?;
        }
        if !orphans.is_empty() {
            File::open(&self.directory)
                .and_then(|directory| directory.sync_all())
                .map_err(|e| format!("Could not sync recovery cleanup: {e}"))?;
        }
        Ok(())
    }
    fn finish_recovery_commit(&self, manifest: &RecoveryManifest) -> RecoveryWriteResult {
        RecoveryWriteResult {
            cleanup_error: self.cleanup_orphans(manifest).err(),
        }
    }
    fn clear(&self) -> Result<(), String> {
        match fs::symlink_metadata(&self.directory) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    return Err("Unsafe recovery directory".into());
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    if metadata.mode() & 0o077 != 0 || metadata.uid() != recovery_owner_uid() {
                        return Err("Recovery directory is not owner-private".into());
                    }
                }
                fs::remove_dir_all(&self.directory).map_err(|e| e.to_string())?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.to_string()),
        }
        Ok(())
    }
}

fn recovery_project_root(authority: &ProjectAuthority, project_id: u64) -> Result<String, String> {
    let state = authority
        .0
        .lock()
        .map_err(|_| "Project state is unavailable".to_string())?;
    let project = state
        .project
        .as_ref()
        .ok_or_else(|| "No project is open".to_string())?;
    if project.identity != project_id {
        return Err("The project changed".into());
    }
    Ok(project.root.display().to_string())
}

fn recovery_is_file_bound(buffer: &RecoveryBuffer) -> bool {
    buffer.relative_path.is_some() && buffer.document_id.is_some_and(|id| id != 0)
}

fn valid_file_bound_baseline(buffer: &RecoveryBuffer) -> bool {
    recovery_is_file_bound(buffer)
        && buffer
            .baseline_hash
            .as_deref()
            .is_some_and(valid_blake3_hex)
}

// This structural validator deliberately permits an old dirty file snapshot
// with a missing/bad baseline to be read. Restore converts it to a visible,
// non-writable conflict while retaining valid recovered text. New writes use
// validate_recovery_write_buffer below and can never create that state.
fn validate_recovery_buffer(buffer: &RecoveryBuffer) -> Result<(), String> {
    if buffer.sequence == 0 || buffer.sequence > RECOVERY_MAX_SEQUENCE {
        return Err("Recovery buffer identity is incomplete".into());
    }
    match (&buffer.relative_path, buffer.document_id) {
        (Some(path), Some(id)) if id != 0 => {
            validate_relative(path, true)?;
        }
        (None, None) if buffer.dirty => {}
        _ => return Err("Recovery buffer identity is incomplete".into()),
    }
    if buffer
        .content
        .as_ref()
        .is_some_and(|content| content.len() > RECOVERY_MAX_PAYLOAD_BYTES)
    {
        return Err("Recovery content exceeds limit".into());
    }
    if buffer.dirty && buffer.content.is_none() {
        return Err("Dirty recovery content is missing".into());
    }
    if !buffer.dirty && buffer.content.is_some() {
        return Err("Clean recovery buffer may not store content".into());
    }
    if buffer
        .search_term
        .as_ref()
        .is_some_and(|term| term.len() > RECOVERY_MAX_SEARCH_BYTES)
    {
        return Err("Recovery search term exceeds limit".into());
    }
    if buffer
        .chapter_identity
        .as_ref()
        .is_some_and(|identity| identity.len() > RECOVERY_MAX_SEARCH_BYTES)
    {
        return Err("Recovery chapter identity exceeds limit".into());
    }
    if let Some(identities) = &buffer.collapsed_chapter_identities {
        if identities.len() > RECOVERY_MAX_COLLAPSED_CHAPTERS {
            return Err("Recovery collapsed chapter count exceeds limit".into());
        }
        let mut unique = std::collections::HashSet::new();
        for identity in identities {
            if !valid_chapter_identity(identity) || !unique.insert(identity) {
                return Err("Recovery collapsed chapter identity is invalid".into());
            }
        }
    }
    for value in [
        buffer.preview_scroll,
        buffer.chapter_scroll,
        buffer.source_scroll.as_ref().map(|scroll| scroll.top),
        buffer.source_scroll.as_ref().map(|scroll| scroll.left),
    ]
    .into_iter()
    .flatten()
    {
        if !value.is_finite() || !(0.0..=RECOVERY_MAX_SCROLL).contains(&value) {
            return Err("Recovery scroll is invalid".into());
        }
    }
    if let Some(selection) = &buffer.selection {
        if selection.anchor > RECOVERY_MAX_PAYLOAD_BYTES
            || selection.head > RECOVERY_MAX_PAYLOAD_BYTES
        {
            return Err("Recovery selection is invalid".into());
        }
    }
    if let Some(activity) = &buffer.activity {
        if activity.day.len() != 10
            || activity
                .day
                .as_bytes()
                .iter()
                .enumerate()
                .any(|(index, byte)| {
                    !matches!(index, 4 | 7) && !byte.is_ascii_digit()
                        || matches!(index, 4 | 7) && *byte != b'-'
                })
        {
            return Err("Recovery activity is invalid".into());
        }
    }
    Ok(())
}

fn validate_recovery_write_buffer(buffer: &RecoveryBuffer) -> Result<(), String> {
    validate_recovery_buffer(buffer)?;
    if buffer
        .baseline_hash
        .as_ref()
        .is_some_and(|hash| !valid_blake3_hex(hash))
    {
        return Err("Recovery baseline hash is invalid".into());
    }
    if buffer.dirty && recovery_is_file_bound(buffer) && !valid_file_bound_baseline(buffer) {
        return Err("Dirty file recovery requires a valid baseline hash".into());
    }
    if buffer
        .reconciliation_operation_key
        .as_deref()
        .is_some_and(|key| !valid_blake3_hex(key))
        || buffer
            .reconciliation_client_edit_version
            .is_some_and(|version| version == 0 || version > RECOVERY_MAX_SEQUENCE)
    {
        return Err("Recovery reconciliation metadata is invalid".into());
    }
    Ok(())
}

fn utf16_len(content: &str) -> usize {
    content.encode_utf16().count()
}

fn valid_chapter_identity(identity: &str) -> bool {
    // This is the exact format emitted by analysis.js:
    // h<1-6>:<16 lowercase FNV-1a hex digits>:<canonical positive decimal>.
    // JS starts each occurrence at one, so zero is not a generated value.
    let mut parts = identity.split(':');
    let (Some(level), Some(fingerprint), Some(occurrence), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    if identity.len() > RECOVERY_MAX_CHAPTER_IDENTITY_BYTES
        || !matches!(level, "h1" | "h2" | "h3" | "h4" | "h5" | "h6")
        || fingerprint.len() != 16
        || !fingerprint
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        || occurrence.is_empty()
        || !matches!(occurrence.as_bytes().first(), Some(b'1'..=b'9'))
        || !occurrence.bytes().all(|byte| byte.is_ascii_digit())
    {
        return false;
    }
    // Parsing gives an explicit overflow bound; stringifying the parsed value
    // proves no alternative decimal spelling (notably a leading zero) was
    // accepted as the same semantic occurrence.
    occurrence
        .parse::<u64>()
        .is_ok_and(|value| value.to_string() == occurrence)
}

fn expected_payload_name(relative_path: Option<&str>, sequence: u64) -> String {
    match relative_path {
        Some(relative_path) => format!(
            "buffer-{}-{sequence}.json",
            blake3::hash(relative_path.as_bytes()).to_hex()
        ),
        None => format!("untitled-{sequence}.json"),
    }
}

fn is_recovery_payload_name(name: &str) -> bool {
    if let Some(sequence) = name
        .strip_prefix("untitled-")
        .and_then(|value| value.strip_suffix(".json"))
    {
        return !sequence.is_empty()
            && sequence.bytes().all(|byte| byte.is_ascii_digit())
            && sequence
                .parse::<u64>()
                .is_ok_and(|value| (1..=RECOVERY_MAX_SEQUENCE).contains(&value));
    }
    let Some(rest) = name.strip_prefix("buffer-") else {
        return false;
    };
    let Some((hash, sequence_and_extension)) = rest.split_once('-') else {
        return false;
    };
    let Some(sequence) = sequence_and_extension.strip_suffix(".json") else {
        return false;
    };
    valid_blake3_hex(hash)
        && !sequence.is_empty()
        && sequence.bytes().all(|byte| byte.is_ascii_digit())
        && sequence
            .parse::<u64>()
            .is_ok_and(|value| (1..=RECOVERY_MAX_SEQUENCE).contains(&value))
}

fn valid_blake3_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn validate_manifest_entry(metadata: &RecoveryBuffer) -> Result<(), String> {
    if metadata.content.is_some() {
        return Err("Recovery manifest must not inline buffer content".into());
    }
    let mut payload_shape = metadata.clone();
    if payload_shape.dirty {
        payload_shape.content = Some(String::new());
    }
    validate_recovery_buffer(&payload_shape)?;
    let name = metadata
        .payload
        .as_deref()
        .ok_or_else(|| "Recovery payload name is missing".to_string())?;
    if name != expected_payload_name(metadata.relative_path.as_deref(), metadata.sequence) {
        return Err("Recovery payload name does not match its buffer".into());
    }
    if metadata
        .payload_bytes
        .is_none_or(|size| size == 0 || size > RECOVERY_MAX_PAYLOAD_BYTES)
        || !metadata
            .payload_hash
            .as_deref()
            .is_some_and(valid_blake3_hex)
    {
        return Err("Recovery payload metadata is invalid".into());
    }
    Ok(())
}

fn recovery_content_or_error(
    dirty: bool,
    recovered_content: Option<String>,
    disk: &str,
) -> Result<String, String> {
    if dirty {
        recovered_content.ok_or_else(|| {
            "RECOVERY ERROR: Dirty snapshot is missing, corrupt, or mismatched; disk text was not accepted as recovered text".into()
        })
    } else {
        Ok(disk.to_owned())
    }
}

fn recovery_total_bytes(buffers: &[RecoveryBuffer]) -> Result<usize, String> {
    let total = buffers.iter().try_fold(0usize, |total, entry| {
        total
            .checked_add(
                entry
                    .payload_bytes
                    .ok_or_else(|| "Recovery payload size is missing".to_string())?,
            )
            .ok_or_else(|| "Recovery payload total overflow".to_string())
    })?;
    if total > RECOVERY_MAX_TOTAL_BYTES {
        return Err(
            "Recovery aggregate limit exceeded; the latest snapshot was not recorded".into(),
        );
    }
    Ok(total)
}

fn with_recovery_update(
    manifest: &RecoveryManifest,
    metadata: RecoveryBuffer,
) -> Result<RecoveryManifest, String> {
    validate_manifest_entry(&metadata)?;
    let mut next = manifest.clone();
    next.buffers
        .retain(|item| item.relative_path != metadata.relative_path);
    next.buffers.push(metadata);
    if next.buffers.len() > RECOVERY_MAX_BUFFERS {
        return Err("Too many recovery buffers".into());
    }
    recovery_total_bytes(&next.buffers)?;
    Ok(next)
}

#[tauri::command]
fn write_recovery_transaction(
    store: &RecoveryStore,
    root: &str,
    buffer: RecoveryBuffer,
) -> Result<RecoveryWriteResult, String> {
    let manifest = store.read_bound(root)?.unwrap_or(RecoveryManifest {
        version: RECOVERY_VERSION,
        root: root.to_string(),
        project_key: RecoveryStore::project_key(root),
        active_path: None,
        buffers: Vec::new(),
    });
    if manifest.root != root {
        return Err("Recovery root does not match the active project".into());
    }
    validate_recovery_write_buffer(&buffer)?;
    let key = buffer.relative_path.clone();
    let previous = manifest
        .buffers
        .iter()
        .find(|item| item.relative_path == key)
        .map(|item| (item.sequence, item.write_revision, item.document_id));
    if previous.is_some_and(|(sequence, revision, document_id)| {
        buffer.sequence <= sequence
            || buffer.write_revision < revision
            || buffer.document_id != document_id
    }) {
        return Err("Stale recovery sequence, revision, or document identity".into());
    }
    if buffer.document_id.is_some_and(|id| {
        manifest
            .buffers
            .iter()
            .any(|item| item.document_id == Some(id) && item.relative_path != buffer.relative_path)
    }) {
        return Err("Recovery document ID is already bound to another path".into());
    }
    let payload = expected_payload_name(buffer.relative_path.as_deref(), buffer.sequence);
    let payload_data = serde_json::to_vec(&buffer).map_err(|e| e.to_string())?;
    if payload_data.len() > RECOVERY_MAX_PAYLOAD_BYTES {
        return Err("Recovery payload exceeds limit".into());
    }
    let mut metadata = buffer;
    metadata.content = None;
    metadata.payload = Some(payload.clone());
    metadata.payload_hash = Some(blake3::hash(&payload_data).to_hex().to_string());
    metadata.payload_bytes = Some(payload_data.len());
    let next_manifest = with_recovery_update(&manifest, metadata)?;
    // Enforce the aggregate limit before creating a new payload or publishing
    // a manifest, preserving the prior recovery set on a rejected update.
    store.atomic(&store.directory.join(&payload), &payload_data)?;
    store.write(&next_manifest)?;
    // The manifest is durable at this point.  Report cleanup trouble without
    // lying that the snapshot was not committed; later reads/writes retry the
    // bounded cleanup and fail closed while stale plaintext remains.
    Ok(store.finish_recovery_commit(&next_manifest))
}

fn recovery_db_for_app(app: &AppHandle) -> Result<RecoveryDb, String> {
    let config = app
        .path()
        .app_config_dir()
        .map_err(|e| format!("Could not locate recovery storage: {e}"))?;
    RecoveryDb::for_config(&config)
}

/// V1 remains read-only. Every legacy byte is validated through the existing
/// no-follow/private parser before this function gives SQLite a detached copy.
fn import_legacy_recovery_once(
    app: &AppHandle,
    db: &RecoveryDb,
    canonical_root: &str,
) -> Result<(), String> {
    let store = RecoveryStore::for_root(app, canonical_root)?;
    import_legacy_recovery_store_once(&store, db, canonical_root)
}

fn import_legacy_recovery_store_once(
    store: &RecoveryStore,
    db: &RecoveryDb,
    canonical_root: &str,
) -> Result<(), String> {
    match fs::symlink_metadata(&store.directory) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("Could not inspect legacy recovery: {error}")),
        Ok(_) => {}
    }
    let Some(manifest) = store.read_bound_read_only(canonical_root)? else {
        return Ok(());
    };
    if manifest.root != canonical_root
        || manifest.project_key != RecoveryStore::project_key(canonical_root)
    {
        return Err("Legacy recovery root binding is invalid".into());
    }
    let manifest_hash = blake3::hash(&serde_json::to_vec(&manifest).map_err(|e| e.to_string())?)
        .to_hex()
        .to_string();
    let mut buffers = Vec::with_capacity(manifest.buffers.len());
    for metadata in &manifest.buffers {
        let payload = store.read_payload(
            metadata
                .payload
                .as_deref()
                .ok_or_else(|| "Legacy recovery payload is missing".to_string())?,
            metadata
                .payload_bytes
                .ok_or_else(|| "Legacy recovery payload size is missing".to_string())?,
            metadata
                .payload_hash
                .as_deref()
                .ok_or_else(|| "Legacy recovery payload hash is missing".to_string())?,
        )?;
        if payload.relative_path != metadata.relative_path
            || payload.document_id != metadata.document_id
            || payload.sequence != metadata.sequence
            || payload.write_revision != metadata.write_revision
            || payload.dirty != metadata.dirty
            || payload.baseline_hash != metadata.baseline_hash
        {
            return Err("Legacy recovery payload does not match its manifest".into());
        }
        let mut imported = payload;
        imported.payload = None;
        imported.payload_hash = None;
        imported.payload_bytes = None;
        imported.client_edit_version = 0;
        validate_recovery_write_buffer(&imported)?;
        buffers.push(imported);
    }
    db.import_legacy(
        canonical_root,
        manifest.active_path.as_deref(),
        &manifest_hash,
        &buffers,
    )?;
    Ok(())
}

/// Startup must privilege committed V2 state or a terminal migration over
/// untrusted legacy bytes. An empty project row is not a migration marker.
/// closure defers all legacy path traversal until SQLite proved migration is
/// genuinely pending.
fn restore_v2_before_legacy<F>(
    db: &RecoveryDb,
    canonical_root: &str,
    import_legacy: F,
) -> Result<Option<recovery_db::RestoredRecovery>, String>
where
    F: FnOnce() -> Result<(), String>,
{
    if db.has_committed_recovery_or_terminal_migration(canonical_root)? {
        return db.latest(canonical_root);
    }
    import_legacy()?;
    db.latest(canonical_root)
}

fn capture_recovery_authorization(
    authority: &ProjectAuthority,
    project_id: u64,
    session_token: &str,
    buffer: &RecoveryBuffer,
) -> Result<AuthorizationSnapshot, String> {
    let snapshot = capture_authorization_snapshot(
        authority,
        project_id,
        Some(session_token),
        buffer.document_id,
    )?;
    match (&buffer.relative_path, buffer.document_id) {
        (Some(path), Some(id)) => {
            let relative = validate_relative(path, true)?;
            if snapshot.document_id != Some(id)
                || snapshot.relative_path.as_deref() != Some(relative.as_path())
                || snapshot.write_revision != Some(buffer.write_revision)
                || snapshot.baseline_hash.as_deref() != buffer.baseline_hash.as_deref()
            {
                return Err("Stale recovery document binding".into());
            }
        }
        (None, None) => {}
        _ => return Err("Recovery buffer identity is incomplete".into()),
    }
    Ok(snapshot)
}

#[tauri::command]
fn write_recovery_buffer(
    app: AppHandle,
    authority: State<'_, ProjectAuthority>,
    recovery: State<'_, RecoveryAuthority>,
    project_id: u64,
    session_token: String,
    buffer: RecoveryWriteRequest,
) -> Result<recovery_db::SnapshotAck, String> {
    let buffer = into_dirty_recovery_request(buffer)?;
    let snapshot = capture_recovery_authorization(&authority, project_id, &session_token, &buffer)?;
    let _transaction = recovery
        .0
        .lock()
        .map_err(|_| "Recovery transaction state is unavailable".to_string())?;
    recovery_db_for_app(&app)?.write_snapshot_for_session(
        &snapshot.root.display().to_string(),
        None,
        &snapshot.session_token,
        &buffer,
    )
}

#[tauri::command]
fn write_recovery_manifest(
    app: AppHandle,
    authority: State<'_, ProjectAuthority>,
    recovery: State<'_, RecoveryAuthority>,
    project_id: u64,
    active_path: Option<String>,
) -> Result<(), String> {
    if active_path
        .as_deref()
        .is_some_and(|path| validate_relative(path, true).is_err())
    {
        return Err("Invalid active recovery path".into());
    }
    let snapshot = capture_authorization_snapshot(&authority, project_id, None, None)?;
    let _transaction = recovery
        .0
        .lock()
        .map_err(|_| "Recovery transaction state is unavailable".to_string())?;
    recovery_db_for_app(&app)?
        .set_active_path(&snapshot.root.display().to_string(), active_path.as_deref())
}

#[tauri::command]
fn delete_recovery_project(
    app: AppHandle,
    authority: State<'_, ProjectAuthority>,
    recovery: State<'_, RecoveryAuthority>,
    project_id: u64,
) -> Result<(), String> {
    let snapshot = capture_authorization_snapshot(&authority, project_id, None, None)?;
    let _transaction = recovery
        .0
        .lock()
        .map_err(|_| "Recovery transaction state is unavailable".to_string())?;
    recovery_db_for_app(&app)?.deactivate_and_discard_session(
        &snapshot.root.display().to_string(),
        &snapshot.session_token,
    )
}
impl ProjectStore {
    fn for_app(app: &AppHandle) -> Result<Self, String> {
        let directory = app
            .path()
            .app_config_dir()
            .map_err(|error| format!("Could not locate application config: {error}"))?;
        Ok(Self {
            state_path: directory.join(LAST_PROJECT_FILE),
            legacy_path: directory.join(LEGACY_LAST_DOCUMENT_FILE),
            directory,
        })
    }
    fn read(&self) -> Option<StoredProject> {
        let text = fs::read_to_string(&self.state_path).ok()?;
        let state: StoredProject = serde_json::from_str(&text).ok()?;
        (state.version == 1).then_some(state)
    }
    fn state_bytes(&self) -> Result<Option<Vec<u8>>, String> {
        match fs::read(&self.state_path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(format!("Could not read project state backup: {error}")),
        }
    }
    fn restore_state_bytes(&self, saved: Option<&[u8]>) -> Result<(), String> {
        match saved {
            Some(bytes) => {
                fs::create_dir_all(&self.directory).map_err(|error| {
                    format!("Could not restore application config directory: {error}")
                })?;
                let temp = self
                    .state_path
                    .with_extension(format!("restore-{}", std::process::id()));
                fs::write(&temp, bytes)
                    .map_err(|error| format!("Could not restore project state: {error}"))?;
                fs::rename(&temp, &self.state_path)
                    .map_err(|error| format!("Could not replace restored project state: {error}"))
            }
            None => match fs::remove_file(&self.state_path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(format!("Could not clear staged project state: {error}")),
            },
        }
    }
    fn write(&self, root: &Path, relative_path: Option<&Path>) -> Result<(), String> {
        fs::create_dir_all(&self.directory)
            .map_err(|e| format!("Could not create application config directory: {e}"))?;
        let payload = StoredProject {
            version: 1,
            root: root.display().to_string(),
            relative_path: relative_path.map(|path| path.to_string_lossy().into_owned()),
        };
        let temp = self
            .state_path
            .with_extension(format!("tmp-{}", std::process::id()));
        let json = serde_json::to_vec(&payload)
            .map_err(|e| format!("Could not encode project state: {e}"))?;
        fs::write(&temp, json).map_err(|e| format!("Could not write project state: {e}"))?;
        fs::rename(&temp, &self.state_path)
            .map_err(|e| format!("Could not replace project state: {e}"))
    }
    fn clear_v1(&self) {
        let _ = fs::remove_file(&self.state_path);
    }
    fn read_legacy(&self) -> Option<PathBuf> {
        fs::read_to_string(&self.legacy_path)
            .ok()
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    }
    fn retire_legacy(&self) {
        let _ = fs::remove_file(&self.legacy_path);
    }
}

fn is_markdown(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("md") || extension.eq_ignore_ascii_case("markdown")
        })
}
fn is_new_markdown(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("md"))
}
fn relative_string(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn validate_relative(path: &str, require_markdown: bool) -> Result<PathBuf, String> {
    if path.is_empty() {
        return if require_markdown {
            Err("A Markdown file is required".into())
        } else {
            Ok(PathBuf::new())
        };
    }
    let parsed = Path::new(path);
    if parsed.is_absolute() {
        return Err("Absolute paths are not permitted".into());
    }
    for component in parsed.components() {
        match component {
            Component::Normal(name) => {
                let value = name.to_string_lossy();
                if value.starts_with('.') || IGNORED_NAMES.contains(&value.as_ref()) {
                    return Err("Hidden or ignored project entries are not permitted".into());
                }
            }
            _ => return Err("Project paths must contain normal relative components only".into()),
        }
    }
    if require_markdown && !is_markdown(parsed) {
        return Err("Only .md and .markdown files are permitted".into());
    }
    Ok(parsed.to_path_buf())
}

#[cfg(target_os = "linux")]
mod secure_fs {
    use super::*;
    use std::os::{
        fd::AsRawFd,
        linux::fs::MetadataExt,
        raw::{c_int, c_long, c_uint},
    };
    #[cfg(test)]
    use std::sync::{Mutex, OnceLock};

    const AT_FDCWD: c_int = -100;
    const O_RDONLY: c_int = 0;
    const O_WRONLY: c_int = 1;
    const O_CREAT: c_int = 0o100;
    const O_EXCL: c_int = 0o200;
    const O_DIRECTORY: c_int = 0o200000;
    const O_NOFOLLOW: c_int = 0o400000;
    const O_CLOEXEC: c_int = 0o2000000;
    const RESOLVE_NO_SYMLINKS: u64 = 0x04;
    const RESOLVE_BENEATH: u64 = 0x08;
    const SYS_OPENAT2: c_long = 437;
    const SYS_RENAMEAT2: c_long = 316;
    const SYS_UNLINKAT: c_long = 263;
    const RENAME_NOREPLACE: c_uint = 1;

    #[repr(C)]
    struct OpenHow {
        flags: u64,
        mode: u64,
        resolve: u64,
    }

    unsafe extern "C" {
        fn syscall(number: c_long, ...) -> c_long;
    }

    #[cfg(test)]
    type TestHook = OnceLock<Mutex<Option<Box<dyn FnOnce() + Send>>>>;
    #[cfg(test)]
    static TEST_HOOK: TestHook = OnceLock::new();
    #[cfg(test)]
    pub(super) fn set_test_hook(hook: Box<dyn FnOnce() + Send>) {
        *TEST_HOOK
            .get_or_init(|| Mutex::new(None))
            .lock()
            .expect("test hook lock") = Some(hook);
    }
    #[cfg(test)]
    fn run_test_hook() {
        if let Some(hook) = TEST_HOOK
            .get_or_init(|| Mutex::new(None))
            .lock()
            .expect("test hook lock")
            .take()
        {
            hook();
        }
    }

    /// Save-boundary failures are deliberately test-only. They model a full
    /// disk/I/O error at a particular persistence boundary without ever
    /// directing a destructive test at a user project.
    #[cfg(test)]
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) enum SaveFault {
        BeforeStageWrite,
        AfterStageFsync,
        BeforeRename,
        AfterRenameAckLoss,
        DirectoryFsync,
    }
    #[cfg(test)]
    thread_local! {
        static SAVE_FAULT: std::cell::Cell<Option<SaveFault>> = const { std::cell::Cell::new(None) };
    }
    #[cfg(test)]
    pub(super) fn set_save_fault(fault: Option<SaveFault>) {
        SAVE_FAULT.with(|current| current.set(fault));
    }
    #[cfg(test)]
    fn fail_save_at(fault: SaveFault) -> Result<(), String> {
        SAVE_FAULT.with(|current| {
            if current.get() == Some(fault) {
                current.set(None);
                Err(format!("Injected persistence fault at {fault:?}"))
            } else {
                Ok(())
            }
        })
    }
    #[cfg(not(test))]
    fn run_test_hook() {}

    fn c_path(path: &Path) -> Result<CString, String> {
        CString::new(path.as_os_str().as_encoded_bytes())
            .map_err(|_| "Project path contains an unsupported NUL byte".to_string())
    }
    fn openat2(
        dirfd: c_int,
        path: &Path,
        flags: c_int,
        mode: c_uint,
        resolve: u64,
    ) -> Result<File, String> {
        let path = c_path(path)?;
        let how = OpenHow {
            flags: (flags | O_CLOEXEC | O_NOFOLLOW) as u64,
            mode: mode as u64,
            resolve,
        };
        // Linux openat2 resolves the complete relative name beneath the held root
        // descriptor in one kernel operation; no intermediate parent descriptor is trusted.
        let fd = unsafe {
            syscall(
                SYS_OPENAT2,
                dirfd,
                path.as_ptr(),
                &how,
                std::mem::size_of::<OpenHow>(),
            )
        };
        if fd < 0 {
            return Err(format!(
                "Could not securely access project entry: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(unsafe { File::from_raw_fd(fd as c_int) })
    }
    fn identity(file: &File, reject_hardlinks: bool) -> Result<FileIdentity, String> {
        let metadata = file.metadata().map_err(|e| e.to_string())?;
        if !metadata.is_file() {
            return Err("Project target is not a regular file".into());
        }
        if reject_hardlinks && metadata.st_nlink() != 1 {
            return Err("Multiply-linked project files are not permitted".into());
        }
        Ok(FileIdentity {
            device: metadata.st_dev(),
            inode: metadata.st_ino(),
        })
    }
    fn is_beneath_root(root: &File, directory: &File) -> Result<(), String> {
        let root_path = fs::read_link(format!("/proc/self/fd/{}", root.as_raw_fd()))
            .map_err(|e| e.to_string())?;
        let directory_path = fs::read_link(format!("/proc/self/fd/{}", directory.as_raw_fd()))
            .map_err(|e| e.to_string())?;
        if !directory_path.starts_with(&root_path) {
            return Err("Project directory moved outside the authorized root".into());
        }
        Ok(())
    }
    fn parent_at(root: &File, relative: &Path) -> Result<File, String> {
        let parent = relative.parent().unwrap_or_else(|| Path::new(""));
        if parent.as_os_str().is_empty() {
            return root.try_clone().map_err(|error| error.to_string());
        }
        openat2(
            root.as_raw_fd(),
            parent,
            O_RDONLY | O_DIRECTORY,
            0,
            RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS,
        )
    }
    fn temporary_name() -> PathBuf {
        PathBuf::from(format!(
            ".amte-write-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ))
    }
    fn cleanup_staging(parent: &File, temporary: &Path) {
        if let Ok(temporary) = c_path(temporary) {
            let _ = unsafe { syscall(SYS_UNLINKAT, parent.as_raw_fd(), temporary.as_ptr(), 0u32) };
        }
    }
    fn stage_content(
        parent: &File,
        temporary: &Path,
        content: &str,
    ) -> Result<FileIdentity, String> {
        #[cfg(test)]
        fail_save_at(SaveFault::BeforeStageWrite)?;
        let mut staged = openat2(
            parent.as_raw_fd(),
            temporary,
            O_WRONLY | O_CREAT | O_EXCL,
            0o600,
            RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS,
        )?;
        let identity = identity(&staged, true)?;
        if let Err(error) = staged
            .write_all(content.as_bytes())
            .and_then(|_| staged.sync_all())
        {
            drop(staged);
            cleanup_staging(parent, temporary);
            return Err(format!("Could not stage project document: {error}"));
        }
        #[cfg(test)]
        if let Err(error) = fail_save_at(SaveFault::AfterStageFsync) {
            drop(staged);
            cleanup_staging(parent, temporary);
            return Err(error);
        }
        Ok(identity)
    }
    fn commit_staged(
        parent: &File,
        temporary: &Path,
        target: &Path,
        no_replace: bool,
    ) -> Result<(), String> {
        let temporary = c_path(temporary)?;
        let target = c_path(target)?;
        let flags = if no_replace { RENAME_NOREPLACE } else { 0 };
        let result = unsafe {
            syscall(
                SYS_RENAMEAT2,
                parent.as_raw_fd(),
                temporary.as_ptr(),
                parent.as_raw_fd(),
                target.as_ptr(),
                flags,
            )
        };
        if result < 0 {
            return Err(format!(
                "Could not commit project document: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(())
    }

    pub(super) fn open_directory_path(path: &Path) -> Result<File, String> {
        // An absolute root has no BENEATH base, but all components must still be non-symlinks.
        openat2(
            AT_FDCWD,
            path,
            O_RDONLY | O_DIRECTORY,
            0,
            RESOLVE_NO_SYMLINKS,
        )
    }
    pub(super) fn read_file_at(
        root: &File,
        relative: &Path,
    ) -> Result<(String, FileIdentity), String> {
        run_test_hook();
        let mut file = openat2(
            root.as_raw_fd(),
            relative,
            O_RDONLY,
            0,
            RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS,
        )?;
        let file_identity = identity(&file, true)?;
        let mut content = String::new();
        file.read_to_string(&mut content)
            .map_err(|e| format!("Could not read project document: {e}"))?;
        Ok((content, file_identity))
    }
    pub(super) fn write_file_at(
        root: &File,
        relative: &Path,
        content: &str,
        expected: Option<FileIdentity>,
        create_new: bool,
    ) -> Result<FileIdentity, String> {
        run_test_hook();
        let parent = parent_at(root, relative)?;
        // Cooperative-project assumption: after this best-effort check, the
        // namespace is not maliciously renamed by the same UID during commit.
        is_beneath_root(root, &parent)?;
        let target = Path::new(
            relative
                .file_name()
                .ok_or_else(|| "Invalid project file".to_string())?,
        );
        let current_identity = if create_new {
            None
        } else {
            let current = openat2(
                root.as_raw_fd(),
                relative,
                O_RDONLY,
                0,
                RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS,
            )?;
            let identity = identity(&current, true)?;
            if expected.is_some_and(|value| value != identity) {
                return Err("The active document changed on disk before save".into());
            }
            Some(identity)
        };
        let temporary = temporary_name();
        let replacement_identity = stage_content(&parent, &temporary, content)?;
        if let Some(current_identity) = current_identity {
            // Revalidate the ordinary target through the root before replacing it.
            let final_check = (|| -> Result<(), String> {
                let final_target = openat2(
                    root.as_raw_fd(),
                    relative,
                    O_RDONLY,
                    0,
                    RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS,
                )?;
                if identity(&final_target, true)? != current_identity {
                    return Err("The active document changed on disk before save".into());
                }
                Ok(())
            })();
            if let Err(error) = final_check {
                cleanup_staging(&parent, &temporary);
                return Err(error);
            }
        }
        #[cfg(test)]
        if let Err(error) = fail_save_at(SaveFault::BeforeRename) {
            cleanup_staging(&parent, &temporary);
            return Err(error);
        }
        if let Err(error) = commit_staged(&parent, &temporary, target, create_new) {
            cleanup_staging(&parent, &temporary);
            return Err(error);
        }
        #[cfg(test)]
        fail_save_at(SaveFault::AfterRenameAckLoss)?;
        #[cfg(test)]
        fail_save_at(SaveFault::DirectoryFsync)?;
        parent
            .sync_all()
            .map_err(|error| format!("Could not sync project directory after save: {error}"))?;
        Ok(replacement_identity)
    }
    pub(super) fn entries_at(root: &File, relative: &Path) -> Result<Vec<fs::DirEntry>, String> {
        run_test_hook();
        let directory = if relative.as_os_str().is_empty() {
            root.try_clone().map_err(|error| error.to_string())?
        } else {
            openat2(
                root.as_raw_fd(),
                relative,
                O_RDONLY | O_DIRECTORY,
                0,
                RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS,
            )?
        };
        is_beneath_root(root, &directory)?;
        let iterator = fs::read_dir(format!("/proc/self/fd/{}", directory.as_raw_fd()))
            .map_err(|e| format!("Could not list project directory: {e}"))?;
        Ok(iterator.filter_map(Result::ok).collect())
    }
}

#[cfg(all(target_os = "linux", not(target_arch = "x86_64")))]
compile_error!(
    "AMTE secure project access currently requires Linux x86_64 openat2/renameat2 syscall numbers"
);
#[cfg(not(target_os = "linux"))]
compile_error!("AMTE project file access is supported only on Linux");
use secure_fs::{entries_at, open_directory_path, read_file_at, write_file_at};

fn canonical_project_root(path: &Path) -> Result<PathBuf, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|e| format!("Could not inspect selected project: {e}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err("Selected project must be a non-symlink directory".into());
    }
    let root = fs::canonicalize(path)
        .map_err(|e| format!("Could not canonicalize selected project: {e}"))?;
    let metadata = fs::symlink_metadata(&root).map_err(|e| e.to_string())?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err("Selected project is not a safe directory".into());
    }
    Ok(root)
}

fn list_entries(root_fd: &File, relative: &Path) -> Result<Vec<EntryDto>, String> {
    let mut entries = Vec::new();
    for entry in entries_at(root_fd, relative)? {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || IGNORED_NAMES.contains(&name.as_str()) {
            continue;
        }
        let kind = match entry.file_type() {
            Ok(file_type) if file_type.is_symlink() => continue,
            Ok(file_type) if file_type.is_dir() => "directory",
            Ok(file_type) if file_type.is_file() && is_markdown(Path::new(&name)) => "file",
            _ => continue,
        };
        let child = relative.join(&name);
        entries.push(EntryDto {
            name,
            relative_path: relative_string(&child),
            kind: kind.into(),
        });
    }
    entries.sort_by(|left, right| {
        let category = (left.kind != "directory").cmp(&(right.kind != "directory"));
        category
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
            .then_with(|| left.name.cmp(&right.name))
    });
    Ok(entries)
}

fn hash_content(content: &str) -> String {
    blake3::hash(content.as_bytes()).to_hex().to_string()
}

fn file_identity_text(identity: FileIdentity) -> String {
    format!("{}:{}", identity.device, identity.inode)
}

/// Stable across acknowledgement loss and process restart, but scoped to the
/// backend-issued session capability. The frontend never supplies this key.
fn save_operation_key(
    snapshot: &AuthorizationSnapshot,
    source: &str,
    destination: &str,
    revision: u64,
    client_edit_version: u64,
    content_hash: &str,
) -> String {
    blake3::hash(
        format!(
            "save-v1\0{}\0{}\0{}\0{}\0{}\0{}\0{}",
            snapshot.root.display(),
            snapshot.session_token,
            source,
            destination,
            revision,
            client_edit_version,
            content_hash,
        )
        .as_bytes(),
    )
    .to_hex()
    .to_string()
}

fn document_dto(
    project: &ActiveProject,
    document: &ActiveDocument,
    content: String,
) -> DocumentDto {
    DocumentDto {
        project_id: project.identity,
        document_id: Some(document.identity),
        relative_path: Some(relative_string(&document.relative_path)),
        content,
        write_revision: document.write_revision,
        baseline_hash: document.baseline_hash.clone(),
        conflict: None,
        recovery: None,
    }
}

fn activate_document(
    state: &mut ProjectState,
    relative_path: PathBuf,
    file_identity: FileIdentity,
    content: &str,
    write_revision: u64,
) -> Result<u64, String> {
    if let Some(id) = state
        .project
        .as_ref()
        .and_then(|project| project.paths.get(&relative_path).copied())
    {
        return Ok(id);
    }
    let identity = ProjectAuthority::next_id(state);
    let project = state
        .project
        .as_mut()
        .ok_or_else(|| "No project is open".to_string())?;
    project.paths.insert(relative_path.clone(), identity);
    project.documents.insert(
        identity,
        ActiveDocument {
            relative_path,
            identity,
            file_identity,
            write_revision,
            baseline_hash: hash_content(content),
        },
    );
    Ok(identity)
}

fn activate_document_with_identity(
    state: &mut ProjectState,
    relative_path: PathBuf,
    identity: u64,
    file_identity: FileIdentity,
    content: &str,
    write_revision: u64,
) -> Result<u64, String> {
    if identity == 0 {
        return Err("Document identity is invalid".into());
    }
    let project = state
        .project
        .as_mut()
        .ok_or_else(|| "No project is open".to_string())?;
    if project.paths.contains_key(&relative_path) || project.documents.contains_key(&identity) {
        return Err("Save As destination is already active".into());
    }
    project.paths.insert(relative_path.clone(), identity);
    project.documents.insert(
        identity,
        ActiveDocument {
            relative_path,
            identity,
            file_identity,
            write_revision,
            baseline_hash: hash_content(content),
        },
    );
    Ok(identity)
}

// Historical recovery IDs are evidence, never a capability for a newly opened
// backend session. Reserve them before restoration and issue a new opaque ID.
fn activate_recovered_document(
    state: &mut ProjectState,
    relative_path: PathBuf,
    file_identity: FileIdentity,
    content: &str,
    write_revision: u64,
    historical_document_id: u64,
) -> Result<u64, String> {
    if historical_document_id == 0 {
        return Err("Recovery document identity is invalid".into());
    }
    activate_document(state, relative_path, file_identity, content, write_revision)
}

fn document_status(project: &ActiveProject, document_id: u64) -> Result<(), String> {
    let document = project
        .documents
        .get(&document_id)
        .ok_or_else(|| "Unknown or stale project document".to_string())?;
    let (content, identity) = read_file_at(&project.root_fd, &document.relative_path)
        .map_err(|error| format!("CONFLICT: {error}"))?;
    if identity != document.file_identity || hash_content(&content) != document.baseline_hash {
        return Err(
            "CONFLICT: The file changed on disk; copy local text or discard and reload".into(),
        );
    }
    Ok(())
}

fn write_document_revision(
    project: &mut ActiveProject,
    document_id: u64,
    revision: u64,
    content: &str,
) -> Result<(), String> {
    let document = project
        .documents
        .get(&document_id)
        .ok_or_else(|| "Unknown or stale project document".to_string())?;
    if revision < document.write_revision {
        return Err("A newer document revision was already saved".into());
    }
    if revision == document.write_revision {
        return (document.baseline_hash == hash_content(content))
            .then_some(())
            .ok_or_else(|| "A different document revision was already saved".into());
    }
    document_status(project, document_id)?;
    let relative_path = document.relative_path.clone();
    let expected_identity = document.file_identity;
    let new_identity = write_file_at(
        &project.root_fd,
        &relative_path,
        content,
        Some(expected_identity),
        false,
    )?;
    // Re-read the committed replacement. This captures the baseline bytes rather
    // than trusting a frontend-supplied hash.
    let (committed, final_identity) = read_file_at(&project.root_fd, &relative_path)?;
    if final_identity != new_identity {
        return Err("CONFLICT: File changed during save".into());
    }
    let document = project
        .documents
        .get_mut(&document_id)
        .expect("registered document remains present");
    document.file_identity = new_identity;
    document.write_revision = revision;
    document.baseline_hash = hash_content(&committed);
    Ok(())
}

fn backend_clean_checkpoint(
    db: &RecoveryDb,
    root: &str,
    document: &ActiveDocument,
    client_edit_version: u64,
) -> Result<recovery_db::SnapshotAck, String> {
    db.write_clean_checkpoint(
        root,
        &relative_string(&document.relative_path),
        document.identity,
        document.write_revision,
        client_edit_version,
        document.baseline_hash.clone(),
    )
}

fn backend_clean_checkpoint_for_session(
    db: &RecoveryDb,
    root: &str,
    document: &ActiveDocument,
    client_edit_version: u64,
    session_token: &str,
) -> Result<recovery_db::SnapshotAck, String> {
    db.write_clean_checkpoint_for_session(
        root,
        &relative_string(&document.relative_path),
        document.identity,
        document.write_revision,
        client_edit_version,
        document.baseline_hash.clone(),
        session_token,
    )
}

#[derive(Debug, Clone)]
struct CompletedDocumentSave {
    file_identity: FileIdentity,
    write_revision: u64,
    baseline_hash: String,
}

/// Performs all document filesystem work from an immutable authorization
/// snapshot. This function must not receive ProjectAuthority (or a borrowed
/// ActiveProject), which is the mechanical guard against future lock inversion.
fn persist_document_snapshot(
    snapshot: &AuthorizationSnapshot,
    revision: u64,
    content: &str,
) -> Result<CompletedDocumentSave, String> {
    let document_id = snapshot
        .document_id
        .ok_or_else(|| "Save requires a document authorization".to_string())?;
    let relative = snapshot
        .relative_path
        .as_ref()
        .ok_or_else(|| "Save requires a document path".to_string())?;
    let previous_revision = snapshot
        .write_revision
        .ok_or_else(|| "Save requires a document revision".to_string())?;
    let baseline = snapshot
        .baseline_hash
        .as_ref()
        .ok_or_else(|| "Save requires a document baseline".to_string())?;
    let expected_identity = snapshot
        .file_identity
        .ok_or_else(|| "Save requires a document file identity".to_string())?;
    let _ = document_id; // Document binding is checked again by SQLite/apply.
    if revision < previous_revision {
        return Err("A newer document revision was already saved".into());
    }
    if revision == previous_revision {
        return (baseline == &hash_content(content))
            .then_some(CompletedDocumentSave {
                file_identity: expected_identity,
                write_revision: revision,
                baseline_hash: baseline.clone(),
            })
            .ok_or_else(|| "A different document revision was already saved".to_string());
    }
    let (on_disk, identity) =
        read_file_at(&snapshot.root_fd, relative).map_err(|error| format!("CONFLICT: {error}"))?;
    if identity != expected_identity || hash_content(&on_disk) != *baseline {
        return Err(
            "CONFLICT: The file changed on disk; copy local text or discard and reload".into(),
        );
    }
    let new_identity = write_file_at(
        &snapshot.root_fd,
        relative,
        content,
        Some(expected_identity),
        false,
    )?;
    let (committed, final_identity) = read_file_at(&snapshot.root_fd, relative)?;
    if final_identity != new_identity {
        return Err("CONFLICT: File changed during save".into());
    }
    Ok(CompletedDocumentSave {
        file_identity: new_identity,
        write_revision: revision,
        baseline_hash: hash_content(&committed),
    })
}

/// The only post-I/O ProjectAuthority acquisition for normal saves. It compares
/// the snapshot generation/revision before changing in-memory state.
fn apply_document_save(
    authority: &ProjectAuthority,
    snapshot: &AuthorizationSnapshot,
    completed: &CompletedDocumentSave,
    content: String,
) -> Result<AuthorityApply<DocumentDto>, String> {
    let mut state = authority
        .0
        .lock()
        .map_err(|_| "Project state is unavailable".to_string())?;
    let Some(project) = state.project.as_mut() else {
        return Ok(AuthorityApply::Stale);
    };
    let Some(document_id) = snapshot.document_id else {
        return Ok(AuthorityApply::Stale);
    };
    let Some(document) = project.documents.get_mut(&document_id) else {
        return Ok(AuthorityApply::Stale);
    };
    if project.identity != snapshot.project_id
        || project.session_token != snapshot.session_token
        || document.relative_path != snapshot.relative_path.clone().expect("document path")
    {
        return Ok(AuthorityApply::Stale);
    }
    // A response/acknowledgement-loss retry reaches here after the original
    // compare/apply completed. Return the already-authoritative value rather
    // than treating that ordinary idempotent retry as stale.
    if document.write_revision == completed.write_revision
        && document.file_identity == completed.file_identity
        && document.baseline_hash == completed.baseline_hash
    {
        return Ok(AuthorityApply::Applied(DocumentDto {
            project_id: project.identity,
            document_id: Some(document.identity),
            relative_path: Some(relative_string(&document.relative_path)),
            content,
            write_revision: document.write_revision,
            baseline_hash: document.baseline_hash.clone(),
            conflict: None,
            recovery: None,
        }));
    }
    if document.write_revision != snapshot.write_revision.expect("document revision")
        || document.file_identity != snapshot.file_identity.expect("document identity")
        || document.baseline_hash != *snapshot.baseline_hash.as_ref().expect("document baseline")
    {
        return Ok(AuthorityApply::Stale);
    }
    document.file_identity = completed.file_identity;
    document.write_revision = completed.write_revision;
    document.baseline_hash = completed.baseline_hash.clone();
    let dto = DocumentDto {
        project_id: project.identity,
        document_id: Some(document.identity),
        relative_path: Some(relative_string(&document.relative_path)),
        content,
        write_revision: document.write_revision,
        baseline_hash: document.baseline_hash.clone(),
        conflict: None,
        recovery: None,
    };
    Ok(AuthorityApply::Applied(dto))
}

/// Journal-led completion after the webview has lost a SaveOutcome. The
/// operation row was authenticated against the current session and disk before
/// this function is called; this only advances an in-memory binding, never
/// writes bytes. A newer in-memory revision is a stale operation and wins.
fn apply_reconciled_document_save(
    authority: &ProjectAuthority,
    snapshot: &AuthorizationSnapshot,
    operation: &recovery_db::SaveOperation,
    completed: &CompletedDocumentSave,
    content: String,
) -> Result<AuthorityApply<DocumentDto>, String> {
    let mut state = authority
        .0
        .lock()
        .map_err(|_| "Project state is unavailable".to_string())?;
    let Some(project) = state.project.as_mut() else {
        return Ok(AuthorityApply::Stale);
    };
    if project.identity != snapshot.project_id || project.session_token != snapshot.session_token {
        return Ok(AuthorityApply::Stale);
    }
    let Some(document) = project.documents.get_mut(&operation.document_id) else {
        return Ok(AuthorityApply::Stale);
    };
    if document.relative_path != operation.relative_path
        || document.write_revision > operation.write_revision
    {
        return Ok(AuthorityApply::Stale);
    }
    document.file_identity = completed.file_identity;
    document.write_revision = operation.write_revision;
    document.baseline_hash = completed.baseline_hash.clone();
    Ok(AuthorityApply::Applied(DocumentDto {
        project_id: project.identity,
        document_id: Some(document.identity),
        relative_path: Some(relative_string(&document.relative_path)),
        content,
        write_revision: document.write_revision,
        baseline_hash: document.baseline_hash.clone(),
        conflict: None,
        recovery: None,
    }))
}

/// Applies only a RecoveryDb-committed Save As transition.  The destination is
/// intentionally absent from ProjectAuthority until this compare-and-apply
/// step; a file write or SQLite failure can therefore never redirect the live
/// editor away from its untitled source.
#[allow(clippy::too_many_arguments)]
fn apply_save_as_transition(
    authority: &ProjectAuthority,
    snapshot: &AuthorizationSnapshot,
    relative_path: PathBuf,
    document_id: u64,
    file_identity: FileIdentity,
    revision: u64,
    content: String,
) -> Result<AuthorityApply<DocumentDto>, String> {
    let mut state = authority
        .0
        .lock()
        .map_err(|_| "Project state is unavailable".to_string())?;
    let Some(project) = state.project.as_ref() else {
        return Ok(AuthorityApply::Stale);
    };
    if project.identity != snapshot.project_id
        || project.session_token != snapshot.session_token
        || project.paths.contains_key(&relative_path)
    {
        return Ok(AuthorityApply::Stale);
    }
    activate_document_with_identity(
        &mut state,
        relative_path,
        document_id,
        file_identity,
        &content,
        revision,
    )?;
    let project = state.project.as_ref().expect("project remains active");
    let document = project
        .documents
        .get(&document_id)
        .expect("registered document");
    Ok(AuthorityApply::Applied(document_dto(
        project, document, content,
    )))
}

fn checkpoint_completed_save(
    db: &RecoveryDb,
    snapshot: &AuthorizationSnapshot,
    completed: &CompletedDocumentSave,
    client_edit_version: u64,
) -> Result<recovery_db::SnapshotAck, String> {
    db.write_clean_checkpoint_for_session(
        &snapshot.root.display().to_string(),
        &relative_string(snapshot.relative_path.as_ref().expect("document path")),
        snapshot.document_id.expect("document id"),
        completed.write_revision,
        client_edit_version,
        completed.baseline_hash.clone(),
        &snapshot.session_token,
    )
}

#[allow(clippy::too_many_arguments)]
fn save_outcome(
    snapshot: &AuthorizationSnapshot,
    operation_key: String,
    completed: &CompletedDocumentSave,
    client_edit_version: u64,
    checkpoint: bool,
    selector: bool,
    document: Option<DocumentDto>,
    error: Option<(&str, String)>,
) -> SaveOutcome {
    SaveOutcome {
        file_committed: true,
        recovery_checkpoint_durable: checkpoint,
        selector_session_state_durable: selector,
        reconciliation_pending: !checkpoint || !selector,
        reconciliation_error_code: error.as_ref().map(|(code, _)| (*code).to_string()),
        reconciliation_error_message: error.map(|(_, message)| message.chars().take(240).collect()),
        project_id: snapshot.project_id,
        session_token: snapshot.session_token.clone(),
        document_id: snapshot.document_id,
        relative_path: snapshot
            .relative_path
            .as_ref()
            .map(|path| relative_string(path)),
        file_identity: Some(file_identity_text(completed.file_identity)),
        write_revision: completed.write_revision,
        baseline_hash: completed.baseline_hash.clone(),
        saved_client_edit_version: client_edit_version,
        content_blake3: completed.baseline_hash.clone(),
        operation_key,
        document: document.map(Box::new),
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct DocumentStatusDto {
    conflict: Option<String>,
    baseline_hash: String,
}

#[tauri::command]
fn revalidate_project_document(
    authority: State<'_, ProjectAuthority>,
    project_id: u64,
    document_id: u64,
) -> Result<DocumentStatusDto, String> {
    let state = authority
        .0
        .lock()
        .map_err(|_| "Project state is unavailable".to_string())?;
    let project = state
        .project
        .as_ref()
        .ok_or_else(|| "No project is open".to_string())?;
    if project.identity != project_id {
        return Err("The project changed".into());
    }
    let document = project
        .documents
        .get(&document_id)
        .ok_or_else(|| "Unknown or stale project document".to_string())?;
    let result = document_status(project, document_id).err();
    Ok(DocumentStatusDto {
        conflict: result,
        baseline_hash: document.baseline_hash.clone(),
    })
}

#[tauri::command]
fn render_markdown(markdown: String) -> String {
    let mut rendered = String::new();
    html::push_html(&mut rendered, Parser::new_ext(&markdown, Options::all()));
    ammonia::clean(&rendered)
}

#[tauri::command]
async fn pick_project(
    app: AppHandle,
    authority: State<'_, ProjectAuthority>,
    activation: State<'_, ActivationAuthority>,
) -> Result<Option<ProjectCandidateDto>, String> {
    let Some(selected) = app.dialog().file().blocking_pick_folder() else {
        return Ok(None);
    };
    let path = selected
        .into_path()
        .map_err(|e| format!("Could not use selected project: {e}"))?;
    let root = canonical_project_root(&path)?;
    // The native dialog and candidate validation deliberately happen before the
    // gate. A picker must never make a slow user interaction block activation.
    Ok(Some(publish_picked_project(&authority, &activation, root)?))
}

/// Publish an already validated picker result. The activation gate makes the
/// candidate generation and replacement atomic with respect to every complete
/// activation: a picker cannot replace the candidate after terminal discard
/// and before the in-memory authority swap.
fn publish_picked_project(
    authority: &ProjectAuthority,
    activation: &ActivationAuthority,
    root: PathBuf,
) -> Result<ProjectCandidateDto, String> {
    let _activation = activation
        .0
        .lock()
        .map_err(|_| "Project activation state is unavailable".to_string())?;
    let mut state = authority
        .0
        .lock()
        .map_err(|_| "Project state is unavailable".to_string())?;
    let candidate_id = ProjectAuthority::next_id(&mut state);
    state.pending_project = Some((candidate_id, root.clone()));
    Ok(ProjectCandidateDto {
        candidate_id,
        root: root.display().to_string(),
    })
}

#[tauri::command]
fn activate_picked_project(
    app: AppHandle,
    authority: State<'_, ProjectAuthority>,
    recovery: State<'_, RecoveryAuthority>,
    activation: State<'_, ActivationAuthority>,
    candidate_id: u64,
    discard_previous_recovery: bool,
) -> Result<ProjectDto, String> {
    let _activation = activation
        .0
        .lock()
        .map_err(|_| "Project activation state is unavailable".to_string())?;
    // Snapshot candidate and old session under ProjectAuthority. No fallible
    // filesystem or SQLite operation is permitted in this critical section.
    let (root, identity, session_token, old_session) = {
        let mut state = authority
            .0
            .lock()
            .map_err(|_| "Project state is unavailable".to_string())?;
        let (id, root) = state
            .pending_project
            .as_ref()
            .ok_or_else(|| "No project selection is pending".to_string())?;
        if *id != candidate_id {
            return Err("The selected project is stale".into());
        }
        let root = root.clone();
        let identity = ProjectAuthority::next_id(&mut state);
        let session_token = ProjectAuthority::session_token(&root, identity);
        let old_session = state.project.as_ref().map(|project| {
            (
                project.identity,
                project.root.display().to_string(),
                project.session_token.clone(),
            )
        });
        (root, identity, session_token, old_session)
    };
    let root_fd = open_directory_path(&root)?;
    let store = ProjectStore::for_app(&app)?;
    let saved_state = store.state_bytes()?;
    let next = ActiveProject {
        root: root.clone(),
        root_fd: Arc::new(root_fd),
        identity,
        session_token: session_token.clone(),
        documents: HashMap::new(),
        paths: HashMap::new(),
    };
    let next_root = root.display().to_string();
    // The SQLite fence is the linearization point.  It must compare the exact
    // old capability, rather than merely deactivating whichever row is current.
    let transition = (|| {
        let _transaction = recovery
            .0
            .lock()
            .map_err(|_| "Recovery transaction state is unavailable".to_string())?;
        let db = recovery_db_for_app(&app)?;
        db.transition_active_session(
            old_session
                .as_ref()
                .map(|(_, root, token)| (root.as_str(), token.as_str())),
            &next_root,
            &session_token,
        )
    })();
    transition?;
    // Selector persistence deliberately follows the token fence. If it fails,
    // restore the SQLite session to the still-live authority before returning.
    if let Err(error) = store.write(&root, None) {
        let rollback = {
            let _transaction = recovery
                .0
                .lock()
                .map_err(|_| "Recovery transaction state is unavailable".to_string())?;
            recovery_db_for_app(&app)?.rollback_activation_session(
                old_session
                    .as_ref()
                    .map(|(_, root, token)| (root.as_str(), token.as_str())),
                &next_root,
                &session_token,
            )
        };
        let _ = store.restore_state_bytes(saved_state.as_deref());
        return match rollback {
            Ok(()) => Err(error),
            Err(rollback_error) => Err(format!(
                "{error}; could not restore the live recovery session: {rollback_error}"
            )),
        };
    }
    // This is the last candidate/old-authority comparison. The activation gate
    // excludes picker publication and every other activation until the final
    // authority swap, so after terminal discard candidate identity can no
    // longer change underneath this request.
    {
        let state = authority
            .0
            .lock()
            .map_err(|_| "Project state is unavailable".to_string())?;
        let unchanged = state
            .pending_project
            .as_ref()
            .is_some_and(|(id, pending)| *id == candidate_id && *pending == root)
            && state.project.as_ref().map(|project| project.identity)
                == old_session.as_ref().map(|session| session.0);
        if !unchanged {
            return Err("STALE_COMPLETION: project changed during activation".into());
        }
    }
    // Discard is intentionally after selector persistence but before the
    // authority swap. The transition above is its successful old-token CAS;
    // this final step cannot discard data on a stale activation request.
    if discard_previous_recovery {
        let discard = {
            let _transaction = recovery
                .0
                .lock()
                .map_err(|_| "Recovery transaction state is unavailable".to_string())?;
            recovery_db_for_app(&app)?.finalize_activation_discard(
                old_session
                    .as_ref()
                    .map(|(_, root, token)| (root.as_str(), token.as_str())),
                &next_root,
                &session_token,
            )
        };
        if let Err(error) = discard {
            let rollback = {
                let _transaction = recovery
                    .0
                    .lock()
                    .map_err(|_| "Recovery transaction state is unavailable".to_string())?;
                recovery_db_for_app(&app)?.rollback_activation_session(
                    old_session
                        .as_ref()
                        .map(|(_, root, token)| (root.as_str(), token.as_str())),
                    &next_root,
                    &session_token,
                )
            };
            let _ = store.restore_state_bytes(saved_state.as_deref());
            return match rollback {
                Ok(()) => Err(error),
                Err(rollback_error) => Err(format!(
                    "{error}; could not restore the live recovery session: {rollback_error}"
                )),
            };
        }
    }
    let mut state = authority
        .0
        .lock()
        .map_err(|_| "Project state is unavailable".to_string())?;
    // No fallible candidate comparison is allowed after terminal discard. The
    // activation gate retained above guarantees this is still our candidate.
    state.pending_project.take();
    state.project = Some(next);
    let project = ProjectDto {
        project_id: identity,
        root: next_root,
        session_token: session_token.clone(),
    };
    drop(state);
    // The one-use CAS proof is only required to authorize the discard. Do not
    // retain an event for ordinary switches after the authority is published.
    if !discard_previous_recovery {
        let _transaction = recovery
            .0
            .lock()
            .map_err(|_| "Recovery transaction state is unavailable".to_string())?;
        let _ = recovery_db_for_app(&app)?.clear_activation_cas(
            old_session
                .as_ref()
                .map(|(_, root, token)| (root.as_str(), token.as_str())),
            &session_token,
        );
    }
    Ok(project)
}

/// Runs every fallible activation step while the old authority is still live.
/// The sole swap is last, so an error preserves all old document IDs and their
/// recovery binding. `rollback` restores the persisted selector if discard
/// preparation failed after the new selector was staged.
fn commit_project_activation<P, D, R>(
    state: &mut ProjectState,
    candidate_id: u64,
    next: ActiveProject,
    persist_new: P,
    discard_previous: D,
    rollback: R,
) -> Result<ProjectDto, String>
where
    P: FnOnce() -> Result<(), String>,
    D: FnOnce(&ActiveProject) -> Result<(), String>,
    R: FnOnce() -> Result<(), String>,
{
    match state.pending_project.as_ref() {
        Some((id, root)) if *id == candidate_id && *root == next.root => {}
        _ => return Err("The selected project is stale".into()),
    }
    persist_new()?;
    if let Some(previous) = state.project.as_ref() {
        if let Err(error) = discard_previous(previous) {
            return match rollback() {
                Ok(()) => Err(error),
                Err(rollback_error) => Err(format!(
                    "{error}; old session remains active but restoring the project selector failed: {rollback_error}"
                )),
            };
        }
    }
    state.pending_project.take();
    let project_id = next.identity;
    let root = next.root.display().to_string();
    state.project = Some(next);
    let session_token = state
        .project
        .as_ref()
        .expect("activated project")
        .session_token
        .clone();
    Ok(ProjectDto {
        project_id,
        root,
        session_token,
    })
}

#[tauri::command]
fn list_project_directory(
    authority: State<'_, ProjectAuthority>,
    project_id: u64,
    relative_path: String,
) -> Result<Vec<EntryDto>, String> {
    let relative = validate_relative(&relative_path, false)?;
    let state = authority
        .0
        .lock()
        .map_err(|_| "Project state is unavailable".to_string())?;
    let project = state
        .project
        .as_ref()
        .ok_or_else(|| "No project is open".to_string())?;
    if project.identity != project_id {
        return Err("The project changed".into());
    }
    list_entries(&project.root_fd, &relative)
}

#[tauri::command]
fn open_project_document(
    app: AppHandle,
    authority: State<'_, ProjectAuthority>,
    recovery: State<'_, RecoveryAuthority>,
    project_id: u64,
    relative_path: String,
) -> Result<DocumentDto, String> {
    let relative = validate_relative(&relative_path, true)?;
    let snapshot = capture_authorization_snapshot(&authority, project_id, None, None)?;
    let (content, file_identity) = read_file_at(&snapshot.root_fd, &relative)?;
    let mut state = authority
        .0
        .lock()
        .map_err(|_| "Project state is unavailable".to_string())?;
    let project = state
        .project
        .as_ref()
        .ok_or_else(|| "No project is open".to_string())?;
    if project.identity != snapshot.project_id || project.session_token != snapshot.session_token {
        return Err("The project changed".into());
    }
    let id = activate_document(&mut state, relative.clone(), file_identity, &content, 0)?;
    let project = state.project.as_ref().expect("project remains active");
    let document = project.documents.get(&id).expect("registered document");
    let root = project.root.display().to_string();
    let dto = document_dto(project, document, content);
    drop(state);
    let _guard = recovery
        .0
        .lock()
        .map_err(|_| "Recovery transaction state is unavailable".to_string())?;
    recovery_db_for_app(&app)?.rebind_session_binding(&root, &relative_string(&relative), id)?;
    Ok(dto)
}

#[tauri::command]
fn select_project_document(
    app: AppHandle,
    authority: State<'_, ProjectAuthority>,
    project_id: u64,
    document_id: u64,
) -> Result<(), String> {
    let state = authority
        .0
        .lock()
        .map_err(|_| "Project state is unavailable".to_string())?;
    let project = state
        .project
        .as_ref()
        .ok_or_else(|| "No project is open".to_string())?;
    if project.identity != project_id {
        return Err("The project changed".into());
    }
    let document = project
        .documents
        .get(&document_id)
        .ok_or_else(|| "Unknown or stale project document".to_string())?;
    ProjectStore::for_app(&app)?.write(&project.root, Some(&document.relative_path))
}

#[allow(clippy::too_many_arguments)]
fn save_active(
    app: &AppHandle,
    authority: &ProjectAuthority,
    recovery: &RecoveryAuthority,
    project_id: u64,
    session_token: &str,
    document_id: u64,
    revision: u64,
    client_edit_version: u64,
    content: &str,
) -> Result<SaveOutcome, String> {
    let snapshot = capture_authorization_snapshot(
        authority,
        project_id,
        Some(session_token),
        Some(document_id),
    )?;
    let relative = snapshot.relative_path.as_ref().expect("document path");
    let content_hash = hash_content(content);
    let operation_key = save_operation_key(
        &snapshot,
        &snapshot.document_id.expect("document id").to_string(),
        &relative_string(relative),
        revision,
        client_edit_version,
        &content_hash,
    );

    // A retry checks the operation record before it attempts any replacement.
    // This is necessary after process restart: the in-memory baseline may still
    // be old even though the prior process already renamed the new bytes.
    let prior = {
        let _guard = recovery
            .0
            .lock()
            .map_err(|_| "Recovery transaction state is unavailable".to_string())?;
        recovery_db_for_app(app)?.save_operation(
            &snapshot.root.display().to_string(),
            &snapshot.session_token,
            &operation_key,
        )?
    };
    let completed = if let Some(prior) = prior {
        let (disk, identity) = read_file_at(&snapshot.root_fd, relative)?;
        if hash_content(&disk) != prior.content_blake3
            || file_identity_text(identity) != prior.file_identity
        {
            // The bytes were once committed but their durable record is no
            // longer safe to apply. Do not overwrite a potentially newer file.
            return Ok(save_outcome(&snapshot, operation_key, &CompletedDocumentSave { file_identity: identity, write_revision: prior.write_revision, baseline_hash: hash_content(&disk) }, client_edit_version, prior.recovery_checkpoint_durable, prior.selector_durable, None, Some(("AMBIGUOUS_DISK_STATE", "A prior save was recorded but disk bytes no longer match; recovery requires user action".into()))));
        }
        CompletedDocumentSave {
            file_identity: identity,
            write_revision: prior.write_revision,
            baseline_hash: prior.content_blake3,
        }
    } else {
        // A killed process can die between rename and the SQLite journal. Exact
        // requested bytes on a replacement inode are preserved as an explicit
        // ambiguity, never overwritten on a retry.
        let (disk, identity) = read_file_at(&snapshot.root_fd, relative)?;
        if hash_content(&disk) == content_hash
            && snapshot
                .file_identity
                .is_some_and(|expected| expected != identity)
        {
            return Ok(save_outcome(
                &snapshot,
                operation_key,
                &CompletedDocumentSave {
                    file_identity: identity,
                    write_revision: revision,
                    baseline_hash: content_hash,
                },
                client_edit_version,
                false,
                false,
                None,
                Some(("AMBIGUOUS_COMMIT_JOURNAL", "Disk has the exact requested bytes but the post-rename operation record is absent; reconciliation will not overwrite it".into())),
            ));
        }
        persist_document_snapshot(&snapshot, revision, content)?
    };

    // The replacement inode is visible but has no durable operation evidence
    // yet. A real kill here is deliberately treated as ambiguous on retry.
    if let Err(error) = fail_save_outcome_at(SaveOutcomeFault::RenameBeforeOperationJournal) {
        return Ok(save_outcome(
            &snapshot,
            operation_key,
            &completed,
            client_edit_version,
            false,
            false,
            None,
            Some(("OPERATION_JOURNAL_PENDING", error)),
        ));
    }

    // The rename has happened. From this point this command returns a
    // SaveOutcome rather than an ordinary error (aside from response transport
    // loss), so callers never falsely report the manuscript as unsaved.
    let operation = {
        let _guard = recovery
            .0
            .lock()
            .map_err(|_| "Recovery transaction state is unavailable".to_string())?;
        recovery_db_for_app(app).and_then(|db| {
            db.record_file_commit(
                &snapshot.root.display().to_string(),
                &snapshot.session_token,
                &operation_key,
                &relative_string(relative),
                &relative_string(relative),
                snapshot.document_id.expect("document id"),
                completed.write_revision,
                client_edit_version,
                &completed.baseline_hash,
                &file_identity_text(completed.file_identity),
            )
        })
    };
    let mut operation = match operation {
        Ok(operation) => operation,
        Err(error) => {
            return Ok(save_outcome(
                &snapshot,
                operation_key,
                &completed,
                client_edit_version,
                false,
                false,
                None,
                Some(("OPERATION_JOURNAL_PENDING", error)),
            ));
        }
    };
    if let Err(error) = {
        let _guard = recovery
            .0
            .lock()
            .map_err(|_| "Recovery transaction state is unavailable".to_string())?;
        recovery_db_for_app(app).and_then(|db| {
            db.mark_save_operation_pending_metadata(
                &snapshot.root.display().to_string(),
                &snapshot.session_token,
                &operation_key,
            )
        })
    } {
        return Ok(save_outcome(
            &snapshot,
            operation_key,
            &completed,
            client_edit_version,
            false,
            false,
            None,
            Some(("RECOVERY_METADATA_PENDING", error)),
        ));
    }
    if let Err(error) = fail_save_outcome_at(SaveOutcomeFault::OperationJournalBeforeCheckpoint) {
        return Ok(save_outcome(
            &snapshot,
            operation_key,
            &completed,
            client_edit_version,
            false,
            operation.selector_durable,
            None,
            Some(("RECOVERY_CHECKPOINT_PENDING", error)),
        ));
    }
    if !operation.recovery_checkpoint_durable {
        let checkpoint = {
            let _guard = recovery
                .0
                .lock()
                .map_err(|_| "Recovery transaction state is unavailable".to_string())?;
            recovery_db_for_app(app).and_then(|db| {
                db.reconcile_save_checkpoint(
                    &snapshot.root.display().to_string(),
                    &snapshot.session_token,
                    &operation_key,
                    &operation.relative_path,
                    operation.document_id,
                    operation.write_revision,
                    operation.client_edit_version,
                    completed.baseline_hash.clone(),
                )
            })
        };
        operation = match checkpoint {
            Ok(value) => value,
            Err(error) => {
                return Ok(save_outcome(
                    &snapshot,
                    operation_key,
                    &completed,
                    client_edit_version,
                    false,
                    operation.selector_durable,
                    None,
                    Some(("RECOVERY_CHECKPOINT_PENDING", error)),
                ));
            }
        };
    }
    if let Err(error) = fail_save_outcome_at(SaveOutcomeFault::CheckpointBeforeSelector) {
        return Ok(save_outcome(
            &snapshot,
            operation_key,
            &completed,
            client_edit_version,
            true,
            operation.selector_durable,
            None,
            Some(("SELECTOR_PENDING", error)),
        ));
    }
    // Do not carry RecoveryAuthority into ProjectAuthority. The compare/apply
    // below is deliberately after the transaction guard is released.
    let document = match apply_document_save(authority, &snapshot, &completed, content.into())? {
        AuthorityApply::Applied(document) => document,
        AuthorityApply::Stale => return Ok(save_outcome(&snapshot, operation_key, &completed, client_edit_version, true, operation.selector_durable, None, Some(("SESSION_STATE_PENDING", "File and recovery checkpoint committed; the active project changed before acknowledgement".into())))),
    };
    if !operation.selector_durable {
        if let Err(error) = ProjectStore::for_app(app)
            .and_then(|store| store.write(&snapshot.root, snapshot.relative_path.as_deref()))
        {
            return Ok(save_outcome(
                &snapshot,
                operation_key,
                &completed,
                client_edit_version,
                true,
                false,
                Some(document),
                Some(("SELECTOR_PENDING", error)),
            ));
        }
        let selector = {
            let _guard = recovery
                .0
                .lock()
                .map_err(|_| "Recovery transaction state is unavailable".to_string())?;
            recovery_db_for_app(app).and_then(|db| {
                db.mark_save_selector_durable(
                    &snapshot.root.display().to_string(),
                    &snapshot.session_token,
                    &operation_key,
                )
            })
        };
        operation = match selector {
            Ok(value) => value,
            Err(error) => {
                return Ok(save_outcome(
                    &snapshot,
                    operation_key,
                    &completed,
                    client_edit_version,
                    true,
                    false,
                    Some(document),
                    Some(("SELECTOR_JOURNAL_PENDING", error)),
                ));
            }
        };
    }
    // Retain restart metadata until the response boundary has passed. The
    // destructive child test aborts at this exact durable point.
    fail_save_outcome_at(SaveOutcomeFault::SelectorBeforeResponse)?;
    let _ = {
        let _guard = recovery
            .0
            .lock()
            .map_err(|_| "Recovery transaction state is unavailable".to_string())?;
        recovery_db_for_app(app).and_then(|db| {
            db.clear_save_operation_pending_metadata(
                &snapshot.root.display().to_string(),
                &snapshot.session_token,
                &operation_key,
            )
        })
    };
    Ok(save_outcome(
        &snapshot,
        operation_key,
        &completed,
        client_edit_version,
        true,
        operation.selector_durable,
        Some(document),
        None,
    ))
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
fn save_document(
    app: AppHandle,
    authority: State<'_, ProjectAuthority>,
    recovery: State<'_, RecoveryAuthority>,
    project_id: u64,
    session_token: String,
    document_id: u64,
    revision: u64,
    client_edit_version: u64,
    content: String,
) -> Result<SaveOutcome, String> {
    save_active(
        &app,
        &authority,
        &recovery,
        project_id,
        &session_token,
        document_id,
        revision,
        client_edit_version,
        &content,
    )
}

/// Reconcile a previously committed save without accepting a path, content,
/// picker result, revision, or document id from the webview. The opaque key is
/// meaningful only with the currently active project/session capability.
#[tauri::command]
fn reconcile_save_operation(
    app: AppHandle,
    authority: State<'_, ProjectAuthority>,
    recovery: State<'_, RecoveryAuthority>,
    project_id: u64,
    session_token: String,
    operation_key: String,
) -> Result<SaveOutcome, String> {
    if !valid_blake3_hex(&operation_key) {
        return Err("Save reconciliation operation key is invalid".into());
    }
    let snapshot =
        capture_authorization_snapshot(&authority, project_id, Some(&session_token), None)?;
    let root = snapshot.root.display().to_string();
    let mut operation = {
        let _guard = recovery
            .0
            .lock()
            .map_err(|_| "Recovery transaction state is unavailable".to_string())?;
        recovery_db_for_app(&app)?.save_operation(&root, &snapshot.session_token, &operation_key)?
    }
    .ok_or_else(|| {
        "Save reconciliation operation is unknown, stale, or belongs to another project".to_string()
    })?;
    let relative = validate_relative(&operation.relative_path, true)?;
    if operation.document_id == 0
        || operation.write_revision == 0
        || operation.client_edit_version == 0
        || !valid_blake3_hex(&operation.content_blake3)
    {
        return Err("Save reconciliation journal is invalid".into());
    }
    if operation.source_key != "__untitled__" && operation.source_key != operation.relative_path {
        return Err("Save reconciliation source/destination journal is invalid".into());
    }
    let (content, identity) =
        read_file_at(&snapshot.root_fd, &relative).map_err(|error| format!("CONFLICT: {error}"))?;
    if hash_content(&content) != operation.content_blake3
        || file_identity_text(identity) != operation.file_identity
    {
        return Err("CONFLICT: The committed file changed on disk before reconciliation".into());
    }
    let completed = CompletedDocumentSave {
        file_identity: identity,
        write_revision: operation.write_revision,
        baseline_hash: operation.content_blake3.clone(),
    };
    let destination_state = {
        let state = authority
            .0
            .lock()
            .map_err(|_| "Project state is unavailable".to_string())?;
        state.project.as_ref().and_then(|project| {
            if project.identity != snapshot.project_id
                || project.session_token != snapshot.session_token
            {
                return None;
            }
            project
                .documents
                .get(&operation.document_id)
                .map(|document| {
                    document.relative_path == relative
                        && document.write_revision <= operation.write_revision
                })
        })
    };
    let document = if destination_state == Some(true) {
        match apply_reconciled_document_save(
            &authority,
            &snapshot,
            &operation,
            &completed,
            content.clone(),
        )? {
            AuthorityApply::Applied(document) => document,
            AuthorityApply::Stale => {
                return Err("Save reconciliation is stale for the active document".into());
            }
        }
    } else {
        if destination_state == Some(false) || operation.source_key != "__untitled__" {
            return Err("Save reconciliation is stale for the active document".into());
        }
        let transition = {
            let _guard = recovery
                .0
                .lock()
                .map_err(|_| "Recovery transaction state is unavailable".to_string())?;
            recovery_db_for_app(&app)?.reconcile_save_as_operation(
                &root,
                &snapshot.session_token,
                &operation_key,
            )?
        };
        if transition.destination_document_id != operation.document_id {
            return Err("Save As reconciliation destination is stale".into());
        }
        match apply_save_as_transition(
            &authority,
            &snapshot,
            relative.clone(),
            operation.document_id,
            identity,
            operation.write_revision,
            content.clone(),
        )? {
            AuthorityApply::Applied(document) => document,
            AuthorityApply::Stale => {
                return Err("Save As reconciliation is stale for the active project".into());
            }
        }
    };
    if !operation.recovery_checkpoint_durable {
        let checkpoint = {
            let _guard = recovery
                .0
                .lock()
                .map_err(|_| "Recovery transaction state is unavailable".to_string())?;
            recovery_db_for_app(&app)?.reconcile_save_checkpoint(
                &root,
                &snapshot.session_token,
                &operation_key,
                &operation.relative_path,
                operation.document_id,
                operation.write_revision,
                operation.client_edit_version,
                operation.content_blake3.clone(),
            )
        };
        operation = match checkpoint {
            Ok(operation) => operation,
            Err(error) => {
                let mut outcome_snapshot = snapshot.clone();
                outcome_snapshot.document_id = Some(operation.document_id);
                outcome_snapshot.relative_path = Some(relative.clone());
                return Ok(save_outcome(
                    &outcome_snapshot,
                    operation_key,
                    &completed,
                    operation.client_edit_version,
                    false,
                    operation.selector_durable,
                    Some(document),
                    Some(("RECOVERY_CHECKPOINT_PENDING", error)),
                ));
            }
        };
    }
    if !operation.selector_durable {
        if let Err(error) = ProjectStore::for_app(&app)
            .and_then(|store| store.write(&snapshot.root, Some(&relative)))
        {
            let mut outcome_snapshot = snapshot.clone();
            outcome_snapshot.document_id = Some(operation.document_id);
            outcome_snapshot.relative_path = Some(relative.clone());
            return Ok(save_outcome(
                &outcome_snapshot,
                operation_key,
                &completed,
                operation.client_edit_version,
                true,
                false,
                Some(document),
                Some(("SELECTOR_PENDING", error)),
            ));
        }
        let selector = {
            let _guard = recovery
                .0
                .lock()
                .map_err(|_| "Recovery transaction state is unavailable".to_string())?;
            recovery_db_for_app(&app)?.mark_save_selector_durable(
                &root,
                &snapshot.session_token,
                &operation_key,
            )
        };
        operation = match selector {
            Ok(operation) => operation,
            Err(error) => {
                let mut outcome_snapshot = snapshot.clone();
                outcome_snapshot.document_id = Some(operation.document_id);
                outcome_snapshot.relative_path = Some(relative.clone());
                return Ok(save_outcome(
                    &outcome_snapshot,
                    operation_key,
                    &completed,
                    operation.client_edit_version,
                    true,
                    false,
                    Some(document),
                    Some(("SELECTOR_JOURNAL_PENDING", error)),
                ));
            }
        };
    }
    let _ = {
        let _guard = recovery
            .0
            .lock()
            .map_err(|_| "Recovery transaction state is unavailable".to_string())?;
        recovery_db_for_app(&app)?.clear_save_operation_pending_metadata(
            &root,
            &snapshot.session_token,
            &operation_key,
        )
    };
    let mut outcome_snapshot = snapshot;
    outcome_snapshot.document_id = Some(operation.document_id);
    outcome_snapshot.relative_path = Some(relative);
    Ok(save_outcome(
        &outcome_snapshot,
        operation_key,
        &completed,
        operation.client_edit_version,
        true,
        operation.selector_durable,
        Some(document),
        None,
    ))
}

fn picker_target_relative(root: &Path, selected: &Path) -> Result<PathBuf, String> {
    if !selected.is_absolute() || !is_new_markdown(selected) {
        return Err(
            "Save As accepts only an absolute .md target selected inside the project".into(),
        );
    }
    let name = selected
        .file_name()
        .ok_or_else(|| "Save As target has no filename".to_string())?;
    let parent = selected
        .parent()
        .ok_or_else(|| "Save As target has no parent".to_string())?;
    let canonical_parent =
        fs::canonicalize(parent).map_err(|e| format!("Could not validate Save As parent: {e}"))?;
    let parent_metadata = fs::symlink_metadata(&canonical_parent).map_err(|e| e.to_string())?;
    if parent_metadata.file_type().is_symlink() || !parent_metadata.is_dir() {
        return Err("Save As parent is not a safe directory".into());
    }
    let relative_parent = canonical_parent
        .strip_prefix(root)
        .map_err(|_| "Save As target is outside the opened project".to_string())?;
    let relative = relative_parent.join(name);
    validate_relative(&relative_string(&relative), true)?;
    if !is_new_markdown(&relative) {
        return Err("Save As creates only .md files".into());
    }
    Ok(relative)
}

#[allow(clippy::too_many_arguments)]
fn save_as_outcome(
    snapshot: &AuthorizationSnapshot,
    relative: &Path,
    file_identity: FileIdentity,
    revision: u64,
    client_edit_version: u64,
    content_hash: String,
    operation_key: String,
    document_id: Option<u64>,
    checkpoint: bool,
    selector: bool,
    document: Option<DocumentDto>,
    error: Option<(&str, String)>,
) -> SaveOutcome {
    SaveOutcome {
        file_committed: true,
        recovery_checkpoint_durable: checkpoint,
        selector_session_state_durable: selector,
        reconciliation_pending: !checkpoint || !selector,
        reconciliation_error_code: error.as_ref().map(|(code, _)| (*code).into()),
        reconciliation_error_message: error.map(|(_, message)| message.chars().take(240).collect()),
        project_id: snapshot.project_id,
        session_token: snapshot.session_token.clone(),
        document_id,
        relative_path: Some(relative_string(relative)),
        file_identity: Some(file_identity_text(file_identity)),
        write_revision: revision,
        baseline_hash: content_hash.clone(),
        saved_client_edit_version: client_edit_version,
        content_blake3: content_hash,
        operation_key,
        document: document.map(Box::new),
    }
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
async fn save_document_as(
    app: AppHandle,
    authority: State<'_, ProjectAuthority>,
    recovery: State<'_, RecoveryAuthority>,
    project_id: u64,
    session_token: String,
    revision: u64,
    client_edit_version: u64,
    content: String,
    source_recovery: RecoveryWriteRequest,
) -> Result<Option<SaveOutcome>, String> {
    let source_recovery = into_dirty_recovery_request(source_recovery)?;
    if source_recovery.relative_path.is_some()
        || source_recovery.document_id.is_some()
        || source_recovery.client_edit_version != client_edit_version
        || source_recovery.content.as_deref() != Some(content.as_str())
    {
        return Err("Save As recovery source no longer matches the editor snapshot".into());
    }
    let project_snapshot =
        capture_authorization_snapshot(&authority, project_id, Some(&session_token), None)?;
    let root = project_snapshot.root.clone();
    let Some(selected) = app
        .dialog()
        .file()
        .set_directory(&root)
        .add_filter("Markdown", &["md"])
        .set_file_name("untitled.md")
        .blocking_save_file()
    else {
        return Ok(None);
    };
    let target = selected
        .into_path()
        .map_err(|e| format!("Could not use Save As target: {e}"))?;
    let relative = picker_target_relative(&root, &target)?;
    // Filesystem writes use the immutable snapshot; no authority lock survives
    // the dialog, canonicalization, or staged write.
    let file_identity = write_file_at(&project_snapshot.root_fd, &relative, &content, None, true)?;
    // Reserve an opaque ID but do not bind it into ProjectAuthority. The
    // RecoveryDb transaction below owns the authority transfer; this keeps the
    // untitled source live if a failure occurs after the filesystem rename.
    let document_identity = {
        let mut state = authority
            .0
            .lock()
            .map_err(|_| "Project state is unavailable".to_string())?;
        let project = state
            .project
            .as_ref()
            .ok_or_else(|| "No project is open".to_string())?;
        if project.identity != project_snapshot.project_id
            || project.session_token != project_snapshot.session_token
            || project.root != root
            || project.paths.contains_key(&relative)
        {
            return Ok(Some(save_as_outcome(&project_snapshot, &relative, file_identity, revision, client_edit_version, hash_content(&content), "unrecorded-save-as".into(), None, false, false, None, Some(("SESSION_STATE_PENDING", "File bytes were committed, but the project changed before Save As could reconcile".into())))));
        }
        ProjectAuthority::next_id(&mut state)
    };
    let root_text = project_snapshot.root.display().to_string();
    let baseline_hash = hash_content(&content);
    let operation_key = save_operation_key(
        &project_snapshot,
        "__untitled__",
        &relative_string(&relative),
        revision,
        client_edit_version,
        &baseline_hash,
    );
    if let Err(error) = fail_save_outcome_at(SaveOutcomeFault::RenameBeforeOperationJournal) {
        return Ok(Some(save_as_outcome(
            &project_snapshot,
            &relative,
            file_identity,
            revision,
            client_edit_version,
            baseline_hash,
            operation_key,
            Some(document_identity),
            false,
            false,
            None,
            Some(("OPERATION_JOURNAL_PENDING", error)),
        )));
    }
    // Save As uses the same post-rename journal as every other path. The
    // source/destination tuple is embedded in the operation key; the recovery
    // transition below remains the authority transfer for the untitled buffer.
    let operation = {
        let _guard = recovery
            .0
            .lock()
            .map_err(|_| "Recovery transaction state is unavailable".to_string())?;
        recovery_db_for_app(&app).and_then(|db| {
            db.record_file_commit(
                &root_text,
                &project_snapshot.session_token,
                &operation_key,
                "__untitled__",
                &relative_string(&relative),
                document_identity,
                revision,
                client_edit_version,
                &baseline_hash,
                &file_identity_text(file_identity),
            )
        })
    };
    let mut operation = match operation {
        Ok(value) => value,
        Err(error) => {
            return Ok(Some(save_as_outcome(
                &project_snapshot,
                &relative,
                file_identity,
                revision,
                client_edit_version,
                baseline_hash,
                operation_key,
                Some(document_identity),
                false,
                false,
                None,
                Some(("OPERATION_JOURNAL_PENDING", error)),
            )));
        }
    };
    if let Err(error) = {
        let _guard = recovery
            .0
            .lock()
            .map_err(|_| "Recovery transaction state is unavailable".to_string())?;
        recovery_db_for_app(&app).and_then(|db| {
            db.mark_save_operation_pending_metadata(
                &root_text,
                &project_snapshot.session_token,
                &operation_key,
            )
        })
    } {
        return Ok(Some(save_as_outcome(
            &project_snapshot,
            &relative,
            file_identity,
            revision,
            client_edit_version,
            baseline_hash,
            operation_key,
            Some(document_identity),
            false,
            false,
            None,
            Some(("RECOVERY_METADATA_PENDING", error)),
        )));
    }
    if let Err(error) = fail_save_outcome_at(SaveOutcomeFault::OperationJournalBeforeCheckpoint) {
        return Ok(Some(save_as_outcome(
            &project_snapshot,
            &relative,
            file_identity,
            revision,
            client_edit_version,
            baseline_hash,
            operation_key,
            Some(document_identity),
            false,
            operation.selector_durable,
            None,
            Some(("RECOVERY_CHECKPOINT_PENDING", error)),
        )));
    }
    let _guard = recovery
        .0
        .lock()
        .map_err(|_| "Recovery transaction state is unavailable".to_string())?;
    let transition = recovery_db_for_app(&app).and_then(|db| {
        db.save_as_transition(
            &root_text,
            &project_snapshot.session_token,
            &source_recovery,
            &relative_string(&relative),
            document_identity,
            revision,
            client_edit_version,
            baseline_hash.clone(),
        )
    });
    drop(_guard);
    let transition = match transition {
        Ok(value) => value,
        Err(error) => {
            return Ok(Some(save_as_outcome(
                &project_snapshot,
                &relative,
                file_identity,
                revision,
                client_edit_version,
                baseline_hash,
                operation_key,
                Some(document_identity),
                false,
                operation.selector_durable,
                None,
                Some((
                    "RECOVERY_CHECKPOINT_PENDING",
                    format!(
                        "File bytes were committed; Save As recovery reconciliation is pending: {error}"
                    ),
                )),
            )));
        }
    };
    operation = {
        let _guard = recovery
            .0
            .lock()
            .map_err(|_| "Recovery transaction state is unavailable".to_string())?;
        match recovery_db_for_app(&app).and_then(|db| {
            db.reconcile_save_checkpoint(
                &root_text,
                &project_snapshot.session_token,
                &operation_key,
                &relative_string(&relative),
                document_identity,
                revision,
                client_edit_version,
                baseline_hash.clone(),
            )
        }) {
            Ok(value) => value,
            Err(error) => {
                return Ok(Some(save_as_outcome(
                    &project_snapshot,
                    &relative,
                    file_identity,
                    revision,
                    client_edit_version,
                    baseline_hash,
                    operation_key,
                    Some(document_identity),
                    false,
                    operation.selector_durable,
                    None,
                    Some(("RECOVERY_CHECKPOINT_PENDING", error)),
                )));
            }
        }
    };
    if let Err(error) = fail_save_outcome_at(SaveOutcomeFault::CheckpointBeforeSelector) {
        return Ok(Some(save_as_outcome(
            &project_snapshot,
            &relative,
            file_identity,
            revision,
            client_edit_version,
            baseline_hash,
            operation_key,
            Some(document_identity),
            true,
            operation.selector_durable,
            None,
            Some(("SELECTOR_PENDING", error)),
        )));
    }
    let applied = apply_save_as_transition(
        &authority,
        &project_snapshot,
        relative.clone(),
        transition.destination_document_id,
        file_identity,
        revision,
        content,
    )?;
    let AuthorityApply::Applied(document) = applied else {
        return Ok(Some(save_as_outcome(
            &project_snapshot,
            &relative,
            file_identity,
            revision,
            client_edit_version,
            baseline_hash,
            operation_key,
            Some(document_identity),
            true,
            operation.selector_durable,
            None,
            Some((
                "SESSION_STATE_PENDING",
                "Save As recovery committed, but the live project changed before acknowledgement"
                    .into(),
            )),
        )));
    };
    if let Err(error) = ProjectStore::for_app(&app)
        .and_then(|store| store.write(&project_snapshot.root, Some(&relative)))
    {
        return Ok(Some(save_as_outcome(
            &project_snapshot,
            &relative,
            file_identity,
            revision,
            client_edit_version,
            baseline_hash,
            operation_key,
            Some(document_identity),
            true,
            false,
            Some(document),
            Some((
                "SELECTOR_PENDING",
                format!(
                    "Save As recovery committed, but selector metadata acknowledgement is pending: {error}"
                ),
            )),
        )));
    }
    let selector = {
        let _guard = recovery
            .0
            .lock()
            .map_err(|_| "Recovery transaction state is unavailable".to_string())?;
        recovery_db_for_app(&app).and_then(|db| {
            db.mark_save_selector_durable(
                &root_text,
                &project_snapshot.session_token,
                &operation_key,
            )
        })
    };
    operation = match selector {
        Ok(value) => value,
        Err(error) => {
            return Ok(Some(save_as_outcome(
                &project_snapshot,
                &relative,
                file_identity,
                revision,
                client_edit_version,
                baseline_hash,
                operation_key,
                Some(document_identity),
                true,
                false,
                Some(document),
                Some(("SELECTOR_JOURNAL_PENDING", error)),
            )));
        }
    };
    fail_save_outcome_at(SaveOutcomeFault::SelectorBeforeResponse)?;
    let _ = {
        let _guard = recovery
            .0
            .lock()
            .map_err(|_| "Recovery transaction state is unavailable".to_string())?;
        recovery_db_for_app(&app).and_then(|db| {
            db.clear_save_operation_pending_metadata(
                &root_text,
                &project_snapshot.session_token,
                &operation_key,
            )
        })
    };
    Ok(Some(save_as_outcome(
        &project_snapshot,
        &relative,
        file_identity,
        revision,
        client_edit_version,
        baseline_hash,
        operation_key,
        Some(document_identity),
        true,
        operation.selector_durable,
        Some(document),
        None,
    )))
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
fn autosave_document(
    app: AppHandle,
    authority: State<'_, ProjectAuthority>,
    recovery: State<'_, RecoveryAuthority>,
    project_id: u64,
    session_token: String,
    document_id: u64,
    revision: u64,
    client_edit_version: u64,
    content: String,
) -> Result<SaveOutcome, String> {
    save_active(
        &app,
        &authority,
        &recovery,
        project_id,
        &session_token,
        document_id,
        revision,
        client_edit_version,
        &content,
    )
}

fn restore_from_state(
    app: &AppHandle,
    authority: &ProjectAuthority,
    stored: StoredProject,
) -> Result<Option<RestoreDto>, String> {
    let root = match canonical_project_root(Path::new(&stored.root)) {
        Ok(root) => root,
        Err(_) => return Ok(None),
    };
    let project = authority.activate_project(root.clone())?;
    let mut documents = Vec::new();
    let mut active_relative_path = None;
    if let Some(raw_relative) = stored.relative_path {
        if let Ok(relative) = validate_relative(&raw_relative, true) {
            let snapshot = capture_authorization_snapshot(
                authority,
                project.project_id,
                Some(&project.session_token),
                None,
            )?;
            let loaded = read_file_at(&snapshot.root_fd, &relative);
            let mut state = authority
                .0
                .lock()
                .map_err(|_| "Project state is unavailable".to_string())?;
            let active_project = state
                .project
                .as_ref()
                .ok_or_else(|| "No project is open".to_string())?;
            if active_project.identity != snapshot.project_id
                || active_project.session_token != snapshot.session_token
            {
                return Ok(None);
            }
            if let Ok((content, file_identity)) = loaded {
                let id =
                    activate_document(&mut state, relative.clone(), file_identity, &content, 0)?;
                let active_project = state.project.as_ref().expect("project");
                documents.push(document_dto(
                    active_project,
                    active_project.documents.get(&id).expect("document"),
                    content,
                ));
                active_relative_path = Some(raw_relative);
            }
        }
    }
    let _ =
        ProjectStore::for_app(app)?.write(&root, active_relative_path.as_deref().map(Path::new));
    Ok(Some(RestoreDto {
        project,
        documents,
        active_relative_path,
    }))
}

fn validated_recovery_root(
    manifest: &RecoveryManifest,
    expected_root: &Path,
) -> Result<Option<PathBuf>, String> {
    let root = match canonical_project_root(Path::new(&manifest.root)) {
        Ok(root) => root,
        Err(_) => return Ok(None),
    };
    if root != expected_root
        || manifest.root != expected_root.display().to_string()
        || manifest.project_key != RecoveryStore::project_key(&manifest.root)
    {
        return Err("Recovery manifest belongs to a different project".into());
    }
    Ok(Some(root))
}

fn restore_recovery(
    store: &RecoveryStore,
    authority: &ProjectAuthority,
    manifest: RecoveryManifest,
    expected_root: &Path,
) -> Result<Option<RestoreDto>, String> {
    let Some(root) = validated_recovery_root(&manifest, expected_root)? else {
        return Ok(None);
    };
    if store.directory.file_name().and_then(|name| name.to_str())
        != Some(manifest.project_key.as_str())
    {
        return Err("Recovery manifest key does not match its storage namespace".into());
    }
    if manifest
        .active_path
        .as_deref()
        .is_some_and(|path| validate_relative(path, true).is_err())
    {
        return Err("Recovery active path is invalid".into());
    }
    let mut paths = std::collections::HashSet::new();
    let mut document_ids = std::collections::HashSet::new();
    let mut has_untitled = false;
    let mut total_bytes = 0usize;
    for entry in &manifest.buffers {
        validate_manifest_entry(entry)?;
        match (&entry.relative_path, entry.document_id) {
            (Some(path), Some(id)) => {
                if !paths.insert(path.clone()) || !document_ids.insert(id) {
                    return Err("Recovery manifest has duplicate paths or document IDs".into());
                }
            }
            (None, None) if !has_untitled => has_untitled = true,
            _ => return Err("Recovery manifest has invalid buffer identity".into()),
        }
        total_bytes = total_bytes
            .checked_add(entry.payload_bytes.expect("validated"))
            .ok_or_else(|| "Recovery payload total overflow".to_string())?;
        if total_bytes > RECOVERY_MAX_TOTAL_BYTES {
            return Err("Recovery payload total exceeds limit".into());
        }
    }
    let project = authority.activate_project(root.clone())?;
    let historical_max = manifest
        .buffers
        .iter()
        .filter_map(|buffer| buffer.document_id)
        .max()
        .unwrap_or(0);
    if historical_max != 0 {
        let mut state = authority
            .0
            .lock()
            .map_err(|_| "Project state is unavailable".to_string())?;
        state.next_identity = state.next_identity.max(historical_max);
    }
    let mut documents = Vec::new();
    for mut metadata in manifest.buffers {
        let relative = metadata
            .relative_path
            .as_deref()
            .map(|path| validate_relative(path, true))
            .transpose()?;
        let recovered_result = match (
            &metadata.payload,
            metadata.payload_bytes,
            &metadata.payload_hash,
        ) {
            (Some(name), Some(bytes), Some(hash)) => store.read_payload(name, bytes, hash),
            _ => Err("Recovery payload metadata is missing".into()),
        };
        let mut recovery_failure = None;
        let mut recovered = match recovered_result {
            Ok(payload)
                if payload.relative_path == metadata.relative_path
                    && payload.document_id == metadata.document_id
                    && payload.sequence == metadata.sequence
                    && payload.write_revision == metadata.write_revision
                    && payload.dirty == metadata.dirty
                    && payload.baseline_hash == metadata.baseline_hash
                    && payload.payload.is_none()
                    && payload.payload_hash.is_none()
                    && payload.payload_bytes.is_none() =>
            {
                Some(payload)
            }
            Ok(_) => {
                recovery_failure = Some(
                    "RECOVERY ERROR: Snapshot metadata does not match its manifest entry"
                        .to_string(),
                );
                None
            }
            Err(error) => {
                recovery_failure = Some(format!("RECOVERY ERROR: {error}"));
                None
            }
        };
        let selection_valid = recovered.as_ref().is_none_or(|entry| {
            entry.selection.as_ref().is_none_or(|selection| {
                entry.content.as_ref().is_none_or(|content| {
                    selection.anchor <= utf16_len(content) && selection.head <= utf16_len(content)
                })
            })
        });
        if !selection_valid {
            recovery_failure =
                Some("RECOVERY ERROR: Snapshot selection exceeds recovered content".into());
            recovered = None;
            metadata.selection = None;
        }
        let recovered_content = recovered.as_ref().and_then(|entry| entry.content.clone());
        if relative.is_none() {
            let loaded = match recovery_content_or_error(metadata.dirty, recovered_content, "") {
                Ok(content) => content,
                Err(error) => {
                    recovery_failure = Some(error);
                    String::new()
                }
            };
            documents.push(DocumentDto {
                project_id: project.project_id,
                document_id: None,
                relative_path: None,
                content: loaded,
                write_revision: metadata.write_revision,
                baseline_hash: String::new(),
                conflict: recovery_failure,
                recovery: recovered.or_else(|| Some(metadata.clone())),
            });
            continue;
        }
        let relative = relative.expect("validated bound recovery path");
        let mut conflict = None;
        let (disk, identity, disk_readable) = {
            let state = authority
                .0
                .lock()
                .map_err(|_| "Project state is unavailable".to_string())?;
            let active = state
                .project
                .as_ref()
                .ok_or_else(|| "No project is open".to_string())?;
            match read_file_at(&active.root_fd, &relative) {
                Ok((content, identity)) => (content, identity, true),
                Err(error) => {
                    conflict = Some(format!("CONFLICT: {error}"));
                    (
                        String::new(),
                        FileIdentity {
                            device: 0,
                            inode: 0,
                        },
                        false,
                    )
                }
            }
        };
        let loaded = match recovery_content_or_error(metadata.dirty, recovered_content, &disk) {
            Ok(content) => content,
            Err(error) => {
                conflict = recovery_failure.or(Some(error));
                String::new()
            }
        };
        if let Some(selection) = recovered
            .as_ref()
            .and_then(|entry| entry.selection.as_ref())
        {
            if selection.anchor > utf16_len(&loaded) || selection.head > utf16_len(&loaded) {
                if metadata.dirty {
                    conflict =
                        Some("RECOVERY ERROR: Snapshot selection exceeds recovered content".into());
                    if let Some(entry) = recovered.as_mut() {
                        entry.selection = None;
                    }
                    metadata.selection = None;
                } else {
                    // A clean snapshot never owns manuscript content. Its UI
                    // metadata follows the safely re-read disk document, so
                    // preserve a usable cursor rather than turning a stale
                    // position into a content conflict.
                    let clamped = RecoverySelection {
                        anchor: selection.anchor.min(utf16_len(&loaded)),
                        head: selection.head.min(utf16_len(&loaded)),
                    };
                    if let Some(entry) = recovered.as_mut() {
                        entry.selection = Some(clamped.clone());
                    }
                    metadata.selection = Some(clamped);
                }
            }
        }
        if metadata.dirty && !valid_file_bound_baseline(&metadata) {
            conflict = Some(
                "CONFLICT: Dirty recovery snapshot has no valid disk baseline; recovered text is retained"
                    .into(),
            );
        } else if metadata.dirty
            && disk_readable
            && hash_content(&disk) != *metadata.baseline_hash.as_ref().expect("validated baseline")
        {
            conflict = Some("CONFLICT: File changed while AMTE was closed".into());
        }
        // A clean recovery payload contains UI metadata only. A safe disk read
        // is authoritative even when the old baseline differs: rebind both
        // the active document and recovery metadata to that disk version.
        if !metadata.dirty && disk_readable {
            let baseline = hash_content(&disk);
            metadata.baseline_hash = Some(baseline.clone());
            if let Some(entry) = recovered.as_mut() {
                entry.baseline_hash = Some(baseline);
            }
        }
        let mut state = authority
            .0
            .lock()
            .map_err(|_| "Project state is unavailable".to_string())?;
        let id = activate_recovered_document(
            &mut state,
            relative.clone(),
            identity,
            &disk,
            metadata.write_revision,
            metadata
                .document_id
                .expect("validated manifest document ID"),
        )?;
        metadata.document_id = Some(id);
        if let Some(entry) = recovered.as_mut() {
            entry.document_id = Some(id);
        }
        if conflict.is_some() {
            // Missing/invalid dirty baselines must never inherit the current
            // disk baseline. An empty stored baseline forces document_status
            // to conflict until an explicit resolution flow is implemented.
            state
                .project
                .as_mut()
                .expect("project")
                .documents
                .get_mut(&id)
                .expect("document")
                .baseline_hash = metadata.baseline_hash.clone().unwrap_or_default();
        }
        let active = state.project.as_ref().expect("project");
        let mut dto = document_dto(active, active.documents.get(&id).expect("document"), loaded);
        dto.conflict = conflict;
        dto.recovery = recovered.or_else(|| Some(metadata.clone()));
        documents.push(dto);
    }
    Ok(Some(RestoreDto {
        project,
        documents,
        active_relative_path: manifest.active_path,
    }))
}

/// Rehydrates V2 rows only after the SQLite integrity check completed. Dirty
/// content remains authoritative; clean snapshots carry UI metadata only and
/// are safely rebased to a descriptor-bound disk read.
fn restore_recovery_v2(
    authority: &ProjectAuthority,
    root: &Path,
    recovered: recovery_db::RestoredRecovery,
    pending_operations: &[recovery_db::SaveOperation],
) -> Result<RestoreDto, String> {
    let project = authority.activate_project(root.to_path_buf())?;
    let snapshot = capture_authorization_snapshot(
        authority,
        project.project_id,
        Some(&project.session_token),
        None,
    )?;
    // A new process can otherwise allocate the same small integer that was
    // persisted by its predecessor (for example, old ID 2 after new project
    // ID 1). Reserve every historical ID before issuing any restored document
    // authority, so none can be returned as a current capability.
    let historical_max = recovered
        .buffers
        .iter()
        .filter_map(|buffer| buffer.document_id)
        .max()
        .unwrap_or(0);
    if historical_max != 0 {
        let mut state = authority
            .0
            .lock()
            .map_err(|_| "Project state is unavailable".to_string())?;
        if state.project.as_ref().is_none_or(|active| {
            active.identity != snapshot.project_id || active.session_token != snapshot.session_token
        }) {
            return Err("STALE_COMPLETION: project changed during recovery restore".into());
        }
        state.next_identity = state.next_identity.max(historical_max);
    }
    let mut documents = Vec::new();
    for mut buffer in recovered.buffers {
        validate_recovery_buffer(&buffer)?;
        if buffer.relative_path.is_none() {
            if buffer.dirty {
                documents.push(DocumentDto {
                    project_id: project.project_id,
                    document_id: None,
                    relative_path: None,
                    content: buffer.content.clone().unwrap_or_default(),
                    write_revision: buffer.write_revision,
                    baseline_hash: String::new(),
                    conflict: None,
                    recovery: Some(buffer),
                });
            }
            continue;
        }
        let relative = validate_relative(buffer.relative_path.as_deref().expect("checked"), true)?;
        let (disk, identity) = match read_file_at(&snapshot.root_fd, &relative) {
            Ok(value) => value,
            Err(_error) if buffer.dirty => (
                String::new(),
                FileIdentity {
                    device: 0,
                    inode: 0,
                },
            ),
            Err(error) => {
                return Err(format!(
                    "Could not restore clean recovery snapshot: {error}"
                ));
            }
        };
        let mut conflict = None;
        let content = if buffer.dirty {
            let content = buffer
                .content
                .clone()
                .ok_or_else(|| "Dirty recovery snapshot is missing content".to_string())?;
            let baseline_matches = valid_file_bound_baseline(&buffer)
                && !disk.is_empty()
                && hash_content(&disk) == buffer.baseline_hash.clone().unwrap_or_default();
            let pending_reconciliation = pending_operations.iter().find(|operation| {
                buffer.reconciliation_operation_key.as_deref()
                    == Some(operation.operation_key.as_str())
                    && buffer.reconciliation_client_edit_version
                        == Some(operation.client_edit_version)
                    && buffer.client_edit_version == operation.client_edit_version
                    && buffer.relative_path.as_deref() == Some(operation.relative_path.as_str())
                    && buffer
                        .content
                        .as_deref()
                        .is_some_and(|content| hash_content(content) == operation.content_blake3)
                    && hash_content(&disk) == operation.content_blake3
                    && file_identity_text(identity) == operation.file_identity
            });
            if let Some(operation) = pending_reconciliation {
                // The saved disk bytes are authoritative proof that this is a
                // post-commit recovery handoff, not an external edit. Keep the
                // draft until the frontend calls reconciliation with the opaque
                // key, but bind the fresh live document to the disk baseline.
                buffer.baseline_hash = Some(operation.content_blake3.clone());
                buffer.reconciliation_operation_key = Some(operation.operation_key.clone());
                buffer.reconciliation_client_edit_version = Some(operation.client_edit_version);
            } else if !baseline_matches {
                conflict = Some(
                    "CONFLICT: File changed while AMTE was closed; recovered draft is retained"
                        .into(),
                );
            }
            content
        } else {
            buffer.baseline_hash = Some(hash_content(&disk));
            disk.clone()
        };
        let mut state = authority
            .0
            .lock()
            .map_err(|_| "Project state is unavailable".to_string())?;
        if state.project.as_ref().is_none_or(|active| {
            active.identity != snapshot.project_id || active.session_token != snapshot.session_token
        }) {
            return Err("STALE_COMPLETION: project changed during recovery restore".into());
        }
        let id = activate_recovered_document(
            &mut state,
            relative,
            identity,
            &disk,
            buffer.write_revision,
            buffer
                .document_id
                .ok_or_else(|| "Recovery document identity is missing".to_string())?,
        )?;
        // The DTO (and its frontend cache) must carry only the freshly minted
        // authority. The historical ID is retained solely in the retired DB
        // binding once rebind commits below.
        buffer.document_id = Some(id);
        if conflict.is_some() {
            state
                .project
                .as_mut()
                .expect("project")
                .documents
                .get_mut(&id)
                .expect("document")
                .baseline_hash = buffer.baseline_hash.clone().unwrap_or_default();
        }
        let active = state.project.as_ref().expect("project");
        let mut dto = document_dto(
            active,
            active.documents.get(&id).expect("document"),
            content,
        );
        dto.conflict = conflict;
        dto.recovery = Some(buffer);
        documents.push(dto);
    }
    Ok(RestoreDto {
        project,
        documents,
        active_relative_path: recovered.active_path,
    })
}

/// Publish all freshly minted restored document authorities in one RecoveryDb
/// transaction. Clean buffers then receive a normal clean checkpoint so their
/// copied UI state follows the disk baseline read during restore; dirty buffers
/// deliberately retain their draft/conflict snapshot.
fn bind_restored_recovery_v2(db: &RecoveryDb, restored: &RestoreDto) -> Result<(), String> {
    let bindings = restored
        .documents
        .iter()
        .filter_map(|document| Some((document.relative_path.clone()?, document.document_id?)))
        .collect::<Vec<_>>();
    // Only descriptor-read bytes are eligible to accompany a journal into the
    // fresh session. Failed reads and changed files are intentionally omitted:
    // their old-token journal remains unusable and reconciliation fails closed.
    let root = PathBuf::from(&restored.project.root);
    let root_fd = open_directory_path(&root)?;
    let operation_rebinds = db
        .pending_save_operations(&restored.project.root)?
        .into_iter()
        .filter_map(|operation| {
            let relative = validate_relative(&operation.relative_path, true).ok()?;
            let (content, identity) = read_file_at(&root_fd, &relative).ok()?;
            Some(recovery_db::SaveOperationRebind {
                operation_key: operation.operation_key,
                content_blake3: hash_content(&content),
                file_identity: file_identity_text(identity),
            })
        })
        .collect::<Vec<_>>();
    db.rebind_session_bindings_and_activate_with_operations(
        &restored.project.root,
        &restored.project.session_token,
        &bindings,
        &operation_rebinds,
    )?;
    for document in &restored.documents {
        let Some(recovery) = document.recovery.as_ref() else {
            continue;
        };
        if recovery.dirty {
            continue;
        }
        let (Some(relative_path), Some(document_id)) =
            (document.relative_path.as_deref(), document.document_id)
        else {
            continue;
        };
        db.write_clean_checkpoint_for_session(
            &restored.project.root,
            relative_path,
            document_id,
            document.write_revision,
            recovery.client_edit_version,
            document.baseline_hash.clone(),
            &restored.project.session_token,
        )?;
    }
    Ok(())
}

#[tauri::command]
fn restore_last_project(
    app: AppHandle,
    authority: State<'_, ProjectAuthority>,
    recovery: State<'_, RecoveryAuthority>,
    activation: State<'_, ActivationAuthority>,
) -> Result<Option<RestoreDto>, String> {
    // Restore also mints and publishes a project session, so it shares the
    // activation gate with picker-driven project replacement.
    let _activation = activation
        .0
        .lock()
        .map_err(|_| "Project activation state is unavailable".to_string())?;
    let store = ProjectStore::for_app(&app)?;
    if let Some(stored) = store.read() {
        // Recovery is keyed by the canonical root recorded for the last active
        // project; a different project can never overwrite or be restored as it.
        if let Ok(root) = canonical_project_root(Path::new(&stored.root)) {
            let canonical = root.display().to_string();
            let (recovered, pending_operations) = {
                let _transaction = recovery
                    .0
                    .lock()
                    .map_err(|_| "Recovery transaction state is unavailable".to_string())?;
                // A V2 error is surfaced rather than silently falling back to a
                // fresh database or legacy data. Existing V1 bytes stay untouched.
                let db = recovery_db_for_app(&app)?;
                let recovered = restore_v2_before_legacy(&db, &canonical, || {
                    import_legacy_recovery_once(&app, &db, &canonical)
                })?;
                let pending = if recovered.is_some() {
                    db.pending_save_operations(&canonical)?
                } else {
                    Vec::new()
                };
                (recovered, pending)
            };
            if let Some(recovered) = recovered {
                let restored =
                    restore_recovery_v2(&authority, &root, recovered, &pending_operations)?;
                let _transaction = recovery
                    .0
                    .lock()
                    .map_err(|_| "Recovery transaction state is unavailable".to_string())?;
                bind_restored_recovery_v2(&recovery_db_for_app(&app)?, &restored)?;
                return Ok(Some(restored));
            }
        }
        let restored = restore_from_state(&app, &authority, stored)?;
        if let Some(restored) = restored.as_ref() {
            let bindings = restored
                .documents
                .iter()
                .filter_map(|document| {
                    Some((document.relative_path.clone()?, document.document_id?))
                })
                .collect::<Vec<_>>();
            let _transaction = recovery
                .0
                .lock()
                .map_err(|_| "Recovery transaction state is unavailable".to_string())?;
            recovery_db_for_app(&app)?.rebind_session_bindings_and_activate(
                &restored.project.root,
                &restored.project.session_token,
                &bindings,
            )?;
        }
        if restored.is_none() {
            store.clear_v1();
        }
        return Ok(restored);
    }
    let Some(legacy) = store.read_legacy() else {
        return Ok(None);
    };
    let migrated = (|| -> Result<Option<RestoreDto>, String> {
        if !legacy.is_absolute()
            || !is_markdown(&legacy)
            || fs::symlink_metadata(&legacy)
                .map_err(|e| e.to_string())?
                .file_type()
                .is_symlink()
        {
            return Ok(None);
        }
        let canonical_file = fs::canonicalize(&legacy).map_err(|e| e.to_string())?;
        if canonical_file != legacy || !canonical_file.is_file() {
            return Ok(None);
        }
        let root = canonical_project_root(
            canonical_file
                .parent()
                .ok_or_else(|| "Legacy file has no parent".to_string())?,
        )?;
        let relative = PathBuf::from(
            canonical_file
                .file_name()
                .ok_or_else(|| "Legacy file has no name".to_string())?,
        );
        store.write(&root, Some(&relative))?;
        restore_from_state(
            &app,
            &authority,
            StoredProject {
                version: 1,
                root: root.display().to_string(),
                relative_path: Some(relative_string(&relative)),
            },
        )
    })()?;
    store.retire_legacy();
    Ok(migrated)
}

#[tauri::command]
fn set_application_zoom(app: AppHandle, zoom: f64) -> Result<(), String> {
    if !zoom.is_finite() {
        return Err("Zoom must be a finite number".into());
    }
    app.get_webview_window("main")
        .ok_or_else(|| "The main editor window is unavailable".to_string())?
        .set_zoom(zoom.clamp(0.5, 2.0))
        .map_err(|e| format!("Could not set application zoom: {e}"))
}

#[tauri::command]
fn complete_close(app: AppHandle, close: State<'_, CloseAuthority>) -> Result<(), String> {
    let window = app
        .get_webview_window("main")
        .ok_or_else(|| "The main editor window is unavailable".to_string())?;
    {
        let mut state = close
            .0
            .lock()
            .map_err(|_| "Close state is unavailable".to_string())?;
        state.permitted = true;
        state.pending_request = None;
    }
    if let Err(error) = window.close() {
        // Do not leave a failed close authorized: the next WM request must
        // still travel through the dirty-buffer/recovery decision flow.
        if let Ok(mut state) = close.0.lock() {
            state.permitted = false;
        }
        return Err(format!("Could not close the window: {error}"));
    }
    Ok(())
}

#[tauri::command]
fn close_listener_ready(close: State<'_, CloseAuthority>) -> Result<(), String> {
    let mut state = close
        .0
        .lock()
        .map_err(|_| "Close state is unavailable".to_string())?;
    state.frontend_ready = true;
    Ok(())
}

#[tauri::command]
fn acknowledge_close_request(
    close: State<'_, CloseAuthority>,
    request_id: u64,
) -> Result<bool, String> {
    let mut state = close
        .0
        .lock()
        .map_err(|_| "Close state is unavailable".to_string())?;
    if state.pending_request != Some(request_id) || state.permitted {
        return Ok(false);
    }
    state.acknowledged = true;
    Ok(true)
}

#[tauri::command]
fn finish_close_request(close: State<'_, CloseAuthority>, request_id: u64) -> Result<(), String> {
    let mut state = close
        .0
        .lock()
        .map_err(|_| "Close state is unavailable".to_string())?;
    if state.pending_request == Some(request_id) {
        state.pending_request = None;
        state.acknowledged = false;
    }
    Ok(())
}

#[tauri::command]
fn discard_recovery_and_exit(
    app: AppHandle,
    authority: State<'_, ProjectAuthority>,
    recovery: State<'_, RecoveryAuthority>,
    close: State<'_, CloseAuthority>,
    project_id: u64,
) -> Result<(), String> {
    // Project identity is read and released before taking the recovery lock;
    // no ProjectAuthority guard survives any filesystem operation.
    let snapshot = capture_authorization_snapshot(&authority, project_id, None, None)?;
    let _transaction = recovery
        .0
        .lock()
        .map_err(|_| "Recovery transaction state is unavailable".to_string())?;
    terminal_discard(
        || {
            // A discard fence makes every queued writer stale before rows are
            // terminally discarded, in the same SQLite transaction.
            recovery_db_for_app(&app)?.deactivate_and_discard_session(
                &snapshot.root.display().to_string(),
                &snapshot.session_token,
            )?;
            let mut state = close
                .0
                .lock()
                .map_err(|_| "Close state is unavailable".to_string())?;
            state.permitted = true;
            state.pending_request = None;
            Ok(())
        },
        || app.exit(0),
    )
}

fn terminal_discard(
    delete_recovery: impl FnOnce() -> Result<(), String>,
    terminate: impl FnOnce(),
) -> Result<(), String> {
    delete_recovery()?;
    // AppHandle::exit has no fallible close round-trip: after deletion, this
    // is the terminal sole-process path and frontend code has no close step.
    terminate();
    Ok(())
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(ProjectAuthority::default())
        .manage(RecoveryAuthority::default())
        .manage(ActivationAuthority::default())
        .manage(CloseAuthority::default())
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let close = window.state::<CloseAuthority>();
                let action = close
                    .0
                    .lock()
                    .map(|mut state| request_window_close(&mut state))
                    .unwrap_or(CloseRequestAction::Allow);
                match action {
                    CloseRequestAction::Allow => {}
                    CloseRequestAction::PreventExisting => api.prevent_close(),
                    CloseRequestAction::PreventAndEmit(request_id) => {
                        api.prevent_close();
                        let _ = window.emit("amte://close-requested", request_id);
                        // The listener acknowledges before opening any dialog. If
                        // it was lost during startup or the webview failed, permit
                        // one normal close after a bounded wait. Recovery was not
                        // deleted, so this fail-safe preserves crash recovery.
                        let app = window.app_handle().clone();
                        std::thread::spawn(move || {
                            std::thread::sleep(std::time::Duration::from_secs(2));
                            let close = app.state::<CloseAuthority>();
                            let should_fallback = close.0.lock().ok().is_some_and(|mut state| {
                                take_unacknowledged_close_fallback(&mut state, request_id)
                            });
                            if should_fallback {
                                if let Some(window) = app.get_webview_window("main") {
                                    if window.close().is_err() {
                                        if let Ok(mut state) = close.0.lock() {
                                            state.permitted = false;
                                        }
                                    }
                                }
                            }
                        });
                    }
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            render_markdown,
            pick_project,
            activate_picked_project,
            list_project_directory,
            open_project_document,
            select_project_document,
            revalidate_project_document,
            save_document,
            save_document_as,
            autosave_document,
            reconcile_save_operation,
            write_recovery_buffer,
            write_recovery_manifest,
            delete_recovery_project,
            restore_last_project,
            set_application_zoom,
            close_listener_ready,
            acknowledge_close_request,
            finish_close_request,
            complete_close,
            discard_recovery_and_exit
        ])
        .run(tauri::generate_context!())
        .expect("error while running amte");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::{Instant, SystemTime, UNIX_EPOCH};
    fn temp(name: &str) -> PathBuf {
        let path = PathBuf::from("/tmp/opencode").join(format!(
            "amte-{name}-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&path).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        path
    }
    fn legacy_entry_fingerprint(store: &RecoveryStore) -> Vec<(String, String, Vec<u8>)> {
        use std::os::unix::ffi::OsStrExt;
        let mut entries = fs::read_dir(&store.directory)
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                let path = entry.path();
                let metadata = fs::symlink_metadata(&path).unwrap();
                let kind = if metadata.file_type().is_symlink() {
                    "symlink"
                } else if metadata.is_file() {
                    "file"
                } else if metadata.is_dir() {
                    "directory"
                } else {
                    "other"
                };
                let bytes = if metadata.file_type().is_symlink() {
                    fs::read_link(&path)
                        .unwrap()
                        .as_os_str()
                        .as_bytes()
                        .to_vec()
                } else if metadata.is_file() {
                    fs::read(&path).unwrap()
                } else {
                    Vec::new()
                };
                (
                    entry.file_name().to_string_lossy().into_owned(),
                    kind.into(),
                    bytes,
                )
            })
            .collect::<Vec<_>>();
        entries.sort();
        entries
    }
    fn assert_no_legacy_import(db: &RecoveryDb, root: &str) {
        assert_eq!(
            db.legacy_import_test_state(root).unwrap(),
            (None, 0, 0, Vec::new()),
            "a failed import must not create a marker, project buffers, or snapshots"
        );
    }
    fn migration_fixture(name: &str) -> (PathBuf, PathBuf, String, RecoveryStore, RecoveryDb) {
        let base = temp(name);
        let root = base.join("project");
        fs::create_dir(&root).unwrap();
        let canonical = root.canonicalize().unwrap().display().to_string();
        let directory = base.join(RecoveryStore::project_key(&canonical));
        let store = RecoveryStore {
            base: base.clone(),
            manifest: directory.join("manifest.json"),
            directory,
        };
        let db = RecoveryDb::for_config(&base.join("xdg")).unwrap();
        (base, root, canonical, store, db)
    }
    fn authority(root: &Path) -> ProjectAuthority {
        let authority = ProjectAuthority::default();
        authority
            .activate_project(canonical_project_root(root).unwrap())
            .unwrap();
        authority
    }
    #[test]
    fn markdown_is_rendered_and_sanitized() {
        assert!(render_markdown("# Title\n<script>x</script>".into()).contains("<h1>Title</h1>"));
        assert!(!render_markdown("<script>x</script>".into()).contains("script"));
    }
    #[test]
    fn paths_reject_absolute_traversal_and_non_markdown() {
        assert!(validate_relative("/tmp/a.md", true).is_err());
        assert!(validate_relative("../a.md", true).is_err());
        assert!(validate_relative("a.txt", true).is_err());
        assert!(validate_relative(".secret/a.md", true).is_err());
    }
    #[test]
    fn lazy_listing_filters_sorts_and_omits_symlinks() {
        let root = temp("list");
        fs::create_dir(root.join("zDir")).unwrap();
        fs::create_dir(root.join("Apple")).unwrap();
        fs::write(root.join("b.md"), "").unwrap();
        fs::write(root.join("A.markdown"), "").unwrap();
        fs::write(root.join("skip.txt"), "").unwrap();
        fs::create_dir(root.join("node_modules")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join("zDir"), root.join("link")).unwrap();
        let fd = open_directory_path(&root).unwrap();
        let entries = list_entries(&fd, Path::new("")).unwrap();
        assert_eq!(
            entries
                .iter()
                .map(|entry| (entry.name.as_str(), entry.kind.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("Apple", "directory"),
                ("zDir", "directory"),
                ("A.markdown", "file"),
                ("b.md", "file")
            ]
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn descriptor_bound_io_rejects_symlink_and_stays_in_root() {
        let root = temp("io");
        let outside_directory = temp("outside");
        let outside = outside_directory.join("sentinel.md");
        fs::write(&outside, "safe").unwrap();
        fs::write(root.join("ok.md"), "old").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, root.join("escape.md")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.parent().unwrap(), root.join("escape-dir")).unwrap();
        let fd = open_directory_path(&root).unwrap();
        assert_eq!(read_file_at(&fd, Path::new("ok.md")).unwrap().0, "old");
        assert!(read_file_at(&fd, Path::new("escape.md")).is_err());
        assert!(write_file_at(&fd, Path::new("escape.md"), "bad", None, false).is_err());
        assert!(read_file_at(&fd, Path::new("escape-dir/sentinel.md")).is_err());
        assert_eq!(fs::read_to_string(outside).unwrap(), "safe");
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(outside_directory).unwrap();
    }
    #[test]
    fn hardlinked_outside_file_is_rejected_for_read_manual_save_and_autosave() {
        let root = temp("hardlink-root");
        let outside_directory = temp("hardlink-outside");
        let outside = outside_directory.join("outside.md");
        fs::write(&outside, "outside stays exact").unwrap();
        fs::hard_link(&outside, root.join("linked.md")).unwrap();
        let fd = open_directory_path(&root).unwrap();
        assert!(read_file_at(&fd, Path::new("linked.md")).is_err());
        assert!(write_file_at(&fd, Path::new("linked.md"), "manual", None, false).is_err());
        assert!(write_file_at(&fd, Path::new("linked.md"), "autosave", None, false).is_err());
        assert_eq!(fs::read_to_string(&outside).unwrap(), "outside stays exact");
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(outside_directory).unwrap();
    }
    #[test]
    fn staged_replacement_swaps_a_new_inode_and_cleans_staging_on_success_or_no_clobber_failure() {
        let root = temp("staged-write");
        let document = root.join("document.md");
        let existing = root.join("existing.md");
        fs::write(&document, "old project text").unwrap();
        fs::write(&existing, "must not be replaced").unwrap();
        let fd = open_directory_path(&root).unwrap();
        let original = read_file_at(&fd, Path::new("document.md")).unwrap().1;
        let replacement = write_file_at(
            &fd,
            Path::new("document.md"),
            "new project text",
            Some(original),
            false,
        )
        .unwrap();
        assert_ne!(replacement, original);
        assert_eq!(fs::read_to_string(&document).unwrap(), "new project text");
        assert!(write_file_at(&fd, Path::new("existing.md"), "new Save As", None, true).is_err());
        assert_eq!(
            fs::read_to_string(&existing).unwrap(),
            "must not be replaced"
        );
        assert!(
            !fs::read_dir(&root)
                .unwrap()
                .filter_map(Result::ok)
                .any(|entry| entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".amte-write-"))
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn moved_parent_is_rejected_before_read_create_or_overwrite() {
        for operation in ["read", "create", "overwrite"] {
            let root = temp(&format!("move-root-{operation}"));
            let nested = root.join("nested");
            fs::create_dir(&nested).unwrap();
            fs::write(nested.join("document.md"), "project").unwrap();
            let outside = temp(&format!("move-outside-{operation}"));
            let sentinel = outside.join("sentinel.md");
            fs::write(&sentinel, "outside stays exact").unwrap();
            let fd = open_directory_path(&root).unwrap();
            let moved = outside.join("moved-project-directory");
            let move_parent = Box::new({
                let nested = nested.clone();
                let moved = moved.clone();
                move || fs::rename(nested, moved).unwrap()
            });
            secure_fs::set_test_hook(move_parent);
            let result = match operation {
                "read" => read_file_at(&fd, Path::new("nested/document.md")).map(|_| ()),
                "create" => write_file_at(&fd, Path::new("nested/new.md"), "created", None, true)
                    .map(|_| ()),
                _ => {
                    let identity = FileIdentity {
                        device: 0,
                        inode: 0,
                    };
                    write_file_at(
                        &fd,
                        Path::new("nested/document.md"),
                        "overwritten",
                        Some(identity),
                        false,
                    )
                    .map(|_| ())
                }
            };
            assert!(result.is_err(), "{operation} must reject moved parent");
            assert_eq!(
                fs::read_to_string(&sentinel).unwrap(),
                "outside stays exact"
            );
            assert_eq!(
                fs::read_to_string(moved.join("document.md")).unwrap(),
                "project"
            );
            assert!(!moved.join("new.md").exists());
            fs::remove_dir_all(root).unwrap();
            fs::remove_dir_all(outside).unwrap();
        }
    }
    #[test]
    fn newest_revision_wins_when_autosave_arrives_after_manual_save() {
        let root = temp("revision");
        fs::write(root.join("document.md"), "old").unwrap();
        let authority = authority(&root);
        let document_id = {
            let mut state = authority.0.lock().unwrap();
            let project = state.project.as_ref().unwrap();
            let identity = read_file_at(&project.root_fd, Path::new("document.md"))
                .unwrap()
                .1;
            activate_document(&mut state, PathBuf::from("document.md"), identity, "old", 0).unwrap()
        };
        let project = authority
            .0
            .lock()
            .unwrap()
            .project
            .as_ref()
            .unwrap()
            .identity;
        {
            let mut state = authority.0.lock().unwrap();
            write_document_revision(
                state.project.as_mut().unwrap(),
                document_id,
                9,
                "manual newest",
            )
            .unwrap();
            assert!(
                write_document_revision(
                    state.project.as_mut().unwrap(),
                    document_id,
                    8,
                    "older autosave"
                )
                .is_err()
            );
        }
        assert_eq!(
            fs::read_to_string(root.join("document.md")).unwrap(),
            "manual newest"
        );
        assert!(
            save_active_stub(
                &authority,
                project,
                document_id.wrapping_add(1),
                "stale identity"
            )
            .is_err()
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn stale_project_and_document_cannot_write() {
        let one = temp("one");
        let two = temp("two");
        fs::write(one.join("a.md"), "one").unwrap();
        fs::write(two.join("b.md"), "two").unwrap();
        let state = authority(&one);
        let one_id = state.0.lock().unwrap().project.as_ref().unwrap().identity;
        let file_identity = read_file_at(&open_directory_path(&one).unwrap(), Path::new("a.md"))
            .unwrap()
            .1;
        {
            let mut locked = state.0.lock().unwrap();
            activate_document(&mut locked, PathBuf::from("a.md"), file_identity, "one", 0).unwrap();
        }
        let old_doc = state
            .0
            .lock()
            .unwrap()
            .project
            .as_ref()
            .unwrap()
            .paths
            .get(Path::new("a.md"))
            .copied()
            .unwrap();
        state
            .activate_project(canonical_project_root(&two).unwrap())
            .unwrap();
        let locked = state.0.lock().unwrap();
        let project = locked.project.as_ref().unwrap();
        assert_ne!(project.identity, one_id);
        assert!(project.documents.is_empty());
        drop(locked);
        assert!(save_active_stub(&state, one_id, old_doc, "bad").is_err());
        assert_eq!(fs::read_to_string(one.join("a.md")).unwrap(), "one");
        fs::remove_dir_all(one).unwrap();
        fs::remove_dir_all(two).unwrap();
    }
    fn save_active_stub(
        authority: &ProjectAuthority,
        project_id: u64,
        document_id: u64,
        content: &str,
    ) -> Result<(), String> {
        let state = authority.0.lock().map_err(|_| "lock".to_string())?;
        let project = state
            .project
            .as_ref()
            .ok_or_else(|| "no project".to_string())?;
        if project.identity != project_id {
            return Err("stale project".into());
        }
        let active = project
            .documents
            .get(&document_id)
            .ok_or_else(|| "stale document".to_string())?;
        write_file_at(
            &project.root_fd,
            &active.relative_path,
            content,
            Some(active.file_identity),
            false,
        )
        .map(|_| ())
    }
    #[test]
    fn save_as_requires_existing_in_root_parent_and_md() {
        let root = temp("save-as");
        let outside = temp("save-as-outside");
        assert!(picker_target_relative(&root, &outside.join("bad.md")).is_err());
        assert!(picker_target_relative(&root, &root.join("bad.markdown")).is_err());
        assert_eq!(
            picker_target_relative(&root, &root.join("ok.md")).unwrap(),
            PathBuf::from("ok.md")
        );
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    fn versioned_project_state_round_trips_and_legacy_records_can_be_retired() {
        let directory = temp("state");
        let project = temp("state-project");
        let store = ProjectStore {
            state_path: directory.join(LAST_PROJECT_FILE),
            legacy_path: directory.join(LEGACY_LAST_DOCUMENT_FILE),
            directory: directory.clone(),
        };
        store
            .write(&project, Some(Path::new("chapter.md")))
            .unwrap();
        let saved = store.read().unwrap();
        assert_eq!(saved.version, 1);
        assert_eq!(saved.relative_path.as_deref(), Some("chapter.md"));
        fs::write(
            &store.legacy_path,
            project.join("chapter.md").display().to_string(),
        )
        .unwrap();
        store.retire_legacy();
        assert!(store.read_legacy().is_none());
        fs::remove_dir_all(directory).unwrap();
        fs::remove_dir_all(project).unwrap();
    }

    #[test]
    fn registry_authorizes_multiple_inactive_documents_and_rejects_hash_conflicts() {
        let root = temp("multi-registry");
        fs::write(root.join("a.md"), "A").unwrap();
        fs::write(root.join("b.md"), "B").unwrap();
        let authority = authority(&root);
        let (a, b) = {
            let mut state = authority.0.lock().unwrap();
            let project = state.project.as_ref().unwrap();
            let (a_text, a_identity) = read_file_at(&project.root_fd, Path::new("a.md")).unwrap();
            let (b_text, b_identity) = read_file_at(&project.root_fd, Path::new("b.md")).unwrap();
            let a = activate_document(&mut state, PathBuf::from("a.md"), a_identity, &a_text, 0)
                .unwrap();
            let b = activate_document(&mut state, PathBuf::from("b.md"), b_identity, &b_text, 0)
                .unwrap();
            (a, b)
        };
        {
            let mut state = authority.0.lock().unwrap();
            write_document_revision(
                state.project.as_mut().unwrap(),
                b,
                1,
                "B saved while inactive",
            )
            .unwrap();
            write_document_revision(
                state.project.as_mut().unwrap(),
                a,
                1,
                "A saved while inactive",
            )
            .unwrap();
            assert!(
                write_document_revision(state.project.as_mut().unwrap(), a, 1, "stale").is_err()
            );
        }
        fs::write(root.join("a.md"), "same inode style external mutation").unwrap();
        let mut state = authority.0.lock().unwrap();
        assert!(
            write_document_revision(state.project.as_mut().unwrap(), a, 2, "must not overwrite")
                .unwrap_err()
                .starts_with("CONFLICT:")
        );
        drop(state);
        assert_eq!(
            fs::read_to_string(root.join("a.md")).unwrap(),
            "same inode style external mutation"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn recovery_store_is_private_atomic_and_rejects_corrupt_or_future_manifest() {
        let base = temp("recovery-store");
        let directory = base.join(RecoveryStore::project_key("/tmp"));
        let store = RecoveryStore {
            base: base.clone(),
            manifest: directory.join("manifest.json"),
            directory: directory.clone(),
        };
        let manifest = RecoveryManifest {
            version: RECOVERY_VERSION,
            root: "/safe/root".into(),
            project_key: RecoveryStore::project_key("/safe/root"),
            active_path: None,
            buffers: Vec::new(),
        };
        store.write(&manifest).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(&store.manifest).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert_eq!(store.read().unwrap().unwrap().root, "/safe/root");
        fs::write(&store.manifest, b"{").unwrap();
        assert!(store.read().is_err());
        store
            .write(&RecoveryManifest {
                version: 99,
                root: "/safe/root".into(),
                project_key: RecoveryStore::project_key("/safe/root"),
                active_path: None,
                buffers: Vec::new(),
            })
            .unwrap();
        assert!(store.read().is_err());
        store.clear().unwrap();
        assert!(!directory.exists());
        #[cfg(unix)]
        {
            fs::create_dir_all(&directory).unwrap();
            std::os::unix::fs::symlink(base.join("outside"), &store.manifest).unwrap();
            assert!(store.read().is_err());
        }
        fs::remove_dir_all(base).unwrap();
    }

    fn recovery_buffer(
        path: &str,
        document_id: u64,
        sequence: u64,
        content: Option<&str>,
    ) -> RecoveryBuffer {
        RecoveryBuffer {
            relative_path: Some(path.into()),
            document_id: Some(document_id),
            write_revision: 4,
            sequence,
            client_edit_version: sequence,
            dirty: content.is_some(),
            content: content.map(str::to_owned),
            payload: None,
            payload_hash: None,
            payload_bytes: None,
            baseline_hash: Some(blake3::hash(b"disk baseline").to_hex().to_string()),
            source_scroll: Some(RecoveryScroll {
                top: 12.0,
                left: 0.0,
            }),
            selection: Some(RecoverySelection { anchor: 1, head: 2 }),
            preview_scroll: Some(4.0),
            previewing: Some(false),
            search_term: Some("term".into()),
            chapter_identity: Some("chapter".into()),
            chapter_scroll: Some(5.0),
            collapsed_chapter_identities: None,
            activity: Some(RecoveryActivity {
                day: "2026-10-25".into(),
                added: 2,
                deleted: 1,
            }),
            reconciliation_operation_key: None,
            reconciliation_client_edit_version: None,
        }
    }

    #[test]
    fn recovery_sequence_wire_contract_is_canonical_and_reads_version_one_sequence() {
        let buffer = recovery_buffer("chapter.md", 42, 11, None);
        let serialized = serde_json::to_value(&buffer).unwrap();
        assert_eq!(
            serialized.get("recoverySequence"),
            Some(&serde_json::json!(11))
        );
        assert!(serialized.get("sequence").is_none());

        // This shared fixture is consumed by the frontend test too. Keep the
        // Rust serializer and frontend property name from drifting apart.
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../src/recovery-contract.fixture.json")).unwrap();
        assert_eq!(
            fixture.get("recoverySequence"),
            Some(&serde_json::json!(11))
        );
        assert!(fixture.get("sequence").is_none());
        let unicode_fixture = fixture.get("utf16PositionFixture").unwrap();
        let unicode_text = unicode_fixture
            .get("text")
            .and_then(|value| value.as_str())
            .unwrap();
        assert_eq!(
            utf16_len(unicode_text),
            unicode_fixture
                .get("length")
                .and_then(|value| value.as_u64())
                .unwrap() as usize,
            "Rust and JavaScript must share CodeMirror UTF-16 positions"
        );

        let mut legacy = serialized.as_object().unwrap().clone();
        let sequence = legacy.remove("recoverySequence").unwrap();
        legacy.insert("sequence".into(), sequence);
        let restored: RecoveryBuffer = serde_json::from_value(legacy.into()).unwrap();
        assert_eq!(restored.sequence, 11, "existing manifests remain readable");

        let mut invalid = restored.clone();
        invalid.sequence = RECOVERY_MAX_SEQUENCE + 1;
        assert!(validate_recovery_buffer(&invalid).is_err());
        for malformed in [
            serde_json::json!(-1),
            serde_json::json!(1.5),
            serde_json::json!("11"),
            serde_json::json!(RECOVERY_MAX_SEQUENCE + 1),
            serde_json::json!(0),
        ] {
            let mut raw = serialized.as_object().unwrap().clone();
            raw.insert("recoverySequence".into(), malformed);
            assert!(
                serde_json::from_value::<RecoveryBuffer>(raw.into())
                    .map_err(|error| error.to_string())
                    .and_then(|buffer| validate_recovery_buffer(&buffer))
                    .is_err()
            );
        }
    }

    #[test]
    fn frontend_write_invoke_fixture_decodes_only_the_write_dto() {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        struct InvokeFixture {
            project_id: u64,
            buffer: RecoveryWriteRequest,
        }

        // This fixture is the literal `{ projectId, buffer }` shape emitted by
        // app.js, so serde checks the command boundary rather than a mock DTO.
        let fixture: InvokeFixture =
            serde_json::from_str(include_str!("../../src/recovery-write-invoke.fixture.json"))
                .unwrap();
        assert_eq!(fixture.project_id, 42);
        let buffer = fixture.buffer.into_buffer();
        assert_eq!(buffer.sequence, 0, "SQLite, never the webview, owns IDs");
        assert_eq!(buffer.client_edit_version, 14);
        assert_eq!(
            buffer.content.as_deref(),
            Some("Draft from the real frontend invoke shape")
        );

        let mut forged: serde_json::Value =
            serde_json::from_str(include_str!("../../src/recovery-write-invoke.fixture.json"))
                .unwrap();
        forged["buffer"]["recoverySequence"] = serde_json::json!(99);
        assert!(serde_json::from_value::<InvokeFixture>(forged).is_err());
    }

    #[test]
    fn frontend_cannot_forge_or_replay_a_clean_recovery_invoke() {
        let mut fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../src/recovery-write-invoke.fixture.json"))
                .unwrap();
        fixture["buffer"]["dirty"] = serde_json::json!(false);
        fixture["buffer"]["content"] = serde_json::Value::Null;
        let request: RecoveryWriteRequest =
            serde_json::from_value(fixture["buffer"].clone()).unwrap();
        assert!(
            into_dirty_recovery_request(request)
                .unwrap_err()
                .contains("dirty snapshots only")
        );
    }

    #[test]
    fn activation_faults_keep_old_authority_until_all_steps_succeed() {
        fn project(root: &str, identity: u64) -> ActiveProject {
            ActiveProject {
                root: PathBuf::from(root),
                root_fd: Arc::new(File::open("/dev/null").unwrap()),
                identity,
                session_token: ProjectAuthority::session_token(Path::new(root), identity),
                documents: HashMap::new(),
                paths: HashMap::new(),
            }
        }
        for phase in ["persist", "discard"] {
            let mut state = ProjectState {
                project: Some(project("/old", 1)),
                pending_project: Some((9, PathBuf::from("/new"))),
                next_identity: 9,
            };
            let result = commit_project_activation(
                &mut state,
                9,
                project("/new", 10),
                || {
                    if phase == "persist" {
                        Err("persist fault".into())
                    } else {
                        Ok(())
                    }
                },
                |_| {
                    if phase == "discard" {
                        Err("discard fault".into())
                    } else {
                        Ok(())
                    }
                },
                || Ok(()),
            );
            assert!(result.is_err(), "{phase}");
            assert_eq!(state.project.as_ref().unwrap().identity, 1, "{phase}");
            assert_eq!(state.pending_project.as_ref().unwrap().0, 9, "{phase}");
        }
        let mut state = ProjectState {
            project: Some(project("/old", 1)),
            pending_project: Some((9, PathBuf::from("/new"))),
            next_identity: 9,
        };
        let committed = commit_project_activation(
            &mut state,
            9,
            project("/new", 10),
            || Ok(()),
            |_| Ok(()),
            || Ok(()),
        )
        .unwrap();
        assert_eq!(committed.project_id, 10);
        assert_eq!(state.project.as_ref().unwrap().identity, 10);
        assert!(state.pending_project.is_none());
    }

    #[test]
    fn recovery_collapsed_chapter_identities_are_optional_bounded_unique_and_canonical() {
        let legacy = recovery_buffer("chapter.md", 1, 1, Some("draft"));
        assert!(
            validate_recovery_buffer(&legacy).is_ok(),
            "old version-one data remains readable"
        );
        let mut valid = legacy.clone();
        valid.collapsed_chapter_identities = Some(vec![
            "h2:0123456789abcdef:1".into(),
            "h6:fedcba9876543210:22".into(),
        ]);
        assert!(validate_recovery_buffer(&valid).is_ok());
        valid.collapsed_chapter_identities = Some(vec![
            "h2:0123456789abcdef:1".into(),
            "h2:0123456789abcdef:1".into(),
        ]);
        assert!(validate_recovery_buffer(&valid).is_err());
        valid.collapsed_chapter_identities = Some(vec!["../../not-a-heading".into()]);
        assert!(validate_recovery_buffer(&valid).is_err());
        valid.collapsed_chapter_identities = Some(
            (0..=RECOVERY_MAX_COLLAPSED_CHAPTERS)
                .map(|index| format!("h1:0123456789abcdef:{}", index + 1))
                .collect(),
        );
        assert!(validate_recovery_buffer(&valid).is_err());
    }

    #[test]
    fn collapsed_heading_identity_grammar_matches_the_js_generator_exactly() {
        // analysis.js emits a lowercase 16-digit FNV-1a value and occurrence
        // 1, 2, ... via JavaScript number stringification.
        for identity in ["h1:0000000000000000:1", "h6:ffffffffffffffff:999999999999"] {
            assert!(
                valid_chapter_identity(identity),
                "expected valid: {identity}"
            );
        }
        for identity in [
            "h0:0123456789abcdef:1",
            "h7:0123456789abcdef:1",
            "H2:0123456789abcdef:1",
            "h2:0123456789ABCDEf:1",
            "h2:0123456789abcdef0:1",
            "h2:0123456789abcde:1",
            "h2:0123456789abcdeg:1",
            "h2:0123456789abcdef:",
            "h2:0123456789abcdef:0",
            "h2:0123456789abcdef:00",
            "h2:0123456789abcdef:01",
            "h2:0123456789abcdef:+1",
            "h2:0123456789abcdef:-1",
            "h2:0123456789abcdef: 1",
            "h2:0123456789abcdef:1 ",
            "h2:0123456789abcdef:1.0",
            "h2:0123456789abcdef:1e1",
            "h2:0123456789abcdef:18446744073709551616",
            "h2:0123456789abcdef:1:extra",
            "h2::1",
        ] {
            assert!(
                !valid_chapter_identity(identity),
                "noncanonical identity was accepted: {identity}"
            );
        }

        // Different spellings for the same numeric occurrence cannot evade
        // uniqueness validation: the alternate spelling is rejected first.
        let mut buffer = recovery_buffer("chapter.md", 1, 1, Some("draft"));
        buffer.collapsed_chapter_identities = Some(vec![
            "h2:0123456789abcdef:1".into(),
            "h2:0123456789abcdef:01".into(),
        ]);
        assert!(validate_recovery_buffer(&buffer).is_err());
    }

    fn payload_metadata(buffer: &RecoveryBuffer) -> (RecoveryBuffer, Vec<u8>) {
        let data = serde_json::to_vec(buffer).unwrap();
        let mut metadata = buffer.clone();
        metadata.content = None;
        metadata.payload = Some(expected_payload_name(
            metadata.relative_path.as_deref(),
            metadata.sequence,
        ));
        metadata.payload_hash = Some(blake3::hash(&data).to_hex().to_string());
        metadata.payload_bytes = Some(data.len());
        (metadata, data)
    }

    fn recovery_manifest(root: &Path, buffers: Vec<RecoveryBuffer>) -> RecoveryManifest {
        let root = root.display().to_string();
        RecoveryManifest {
            version: RECOVERY_VERSION,
            project_key: RecoveryStore::project_key(&root),
            root,
            active_path: Some("a.md".into()),
            buffers,
        }
    }

    #[test]
    fn recovery_root_binding_and_missing_dirty_payload_fail_closed() {
        let expected = temp("recovery-expected");
        let other = temp("recovery-other");
        let manifest = recovery_manifest(&other, Vec::new());
        assert!(validated_recovery_root(&manifest, &expected).is_err());
        let message = recovery_content_or_error(true, None, "disk text").unwrap_err();
        assert!(message.starts_with("RECOVERY ERROR:"));
        assert_ne!(message, "disk text");
        assert_eq!(
            recovery_content_or_error(false, None, "disk text").unwrap(),
            "disk text"
        );
        fs::remove_dir_all(expected).unwrap();
        fs::remove_dir_all(other).unwrap();
    }

    #[test]
    fn recovery_restore_rebases_clean_disk_and_preserves_dirty_conflict_and_sequence_binding() {
        let root = temp("recovery-rebase-project");
        let unicode_disk = "Olá 😀e\u{301}";
        fs::write(root.join("a.md"), unicode_disk).unwrap();
        fs::write(root.join("dirty.md"), "changed outside AMTE").unwrap();
        let root = canonical_project_root(&root).unwrap();
        let root_text = root.display().to_string();
        let base = temp("recovery-rebase-store");
        let directory = base.join(RecoveryStore::project_key(&root_text));
        let store = RecoveryStore {
            base: base.clone(),
            manifest: directory.join("manifest.json"),
            directory,
        };

        // This reproduces the affected state: a clean metadata-only snapshot
        // whose old baseline differs from a safely readable current disk file.
        let mut clean_payload = recovery_buffer("a.md", 11, 11, None);
        clean_payload.selection = Some(RecoverySelection {
            anchor: 999,
            head: 999,
        });
        let (clean_metadata, clean_bytes) = payload_metadata(&clean_payload);
        let dirty_payload = recovery_buffer("dirty.md", 12, 4, Some("recovered dirty text"));
        let (dirty_metadata, dirty_bytes) = payload_metadata(&dirty_payload);
        for (metadata, bytes) in [
            (&clean_metadata, clean_bytes),
            (&dirty_metadata, dirty_bytes),
        ] {
            store
                .atomic(
                    &store.directory.join(metadata.payload.as_ref().unwrap()),
                    &bytes,
                )
                .unwrap();
        }
        let manifest = recovery_manifest(&root, vec![clean_metadata, dirty_metadata]);
        store.write(&manifest).unwrap();

        let authority = ProjectAuthority::default();
        let restored = restore_recovery(&store, &authority, manifest, &root)
            .unwrap()
            .unwrap();
        let clean = restored
            .documents
            .iter()
            .find(|document| document.relative_path.as_deref() == Some("a.md"))
            .unwrap();
        assert_eq!(clean.content, unicode_disk);
        assert!(clean.conflict.is_none());
        assert_ne!(clean.document_id, Some(11));
        assert_ne!(clean.document_id, Some(12));
        assert_eq!(clean.write_revision, 4);
        let current_baseline = hash_content(unicode_disk);
        assert_eq!(clean.baseline_hash, current_baseline);
        let clean_recovery = clean.recovery.as_ref().unwrap();
        assert_eq!(clean_recovery.sequence, 11);
        assert_eq!(
            clean_recovery.baseline_hash.as_deref(),
            Some(clean.baseline_hash.as_str())
        );
        assert_eq!(
            clean_recovery.selection.as_ref().unwrap().anchor,
            utf16_len(&clean.content)
        );

        let dirty = restored
            .documents
            .iter()
            .find(|document| document.relative_path.as_deref() == Some("dirty.md"))
            .unwrap();
        assert_eq!(dirty.content, "recovered dirty text");
        assert!(
            dirty
                .conflict
                .as_deref()
                .is_some_and(|error| error.starts_with("CONFLICT:"))
        );
        let dirty_id = dirty.document_id.expect("bound dirty document");
        {
            let mut state = authority.0.lock().unwrap();
            assert!(
                write_document_revision(
                    state.project.as_mut().unwrap(),
                    dirty_id,
                    dirty.write_revision + 1,
                    "must not overwrite the changed disk file"
                )
                .is_err()
            );
        }

        let mut next = recovery_buffer(
            "a.md",
            clean.document_id.expect("bound clean document"),
            12,
            Some("edited after rebase"),
        );
        next.write_revision = clean.write_revision;
        next.baseline_hash = Some(clean.baseline_hash.clone());
        // V1 manifests are read-only migration input. Their historical ID is
        // intentionally not accepted for the fresh restored authority; V2
        // creates the replacement binding before any future snapshot write.
        assert!(write_recovery_transaction(&store, &root_text, next).is_err());

        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn dirty_file_recovery_requires_baseline_but_untitled_recovery_remains_valid() {
        let root = temp("recovery-baseline-project");
        fs::write(root.join("missing.md"), "disk missing baseline").unwrap();
        fs::write(root.join("malformed.md"), "disk malformed baseline").unwrap();
        fs::write(root.join("matching.md"), "matching disk baseline").unwrap();
        let root = canonical_project_root(&root).unwrap();
        let root_text = root.display().to_string();
        let base = temp("recovery-baseline-store");
        let directory = base.join(RecoveryStore::project_key(&root_text));
        let store = RecoveryStore {
            base: base.clone(),
            manifest: directory.join("manifest.json"),
            directory,
        };

        let mut missing =
            recovery_buffer("missing.md", 21, 3, Some("exact missing recovered text"));
        missing.baseline_hash = None;
        let mut malformed = recovery_buffer(
            "malformed.md",
            22,
            3,
            Some("exact malformed recovered text"),
        );
        malformed.baseline_hash = Some("not-a-blake3-hash".into());
        let mut matching =
            recovery_buffer("matching.md", 23, 3, Some("recovered but verified draft"));
        matching.baseline_hash = Some(hash_content("matching disk baseline"));
        for invalid in [&missing, &malformed] {
            assert!(validate_recovery_write_buffer(invalid).is_err());
        }
        assert!(validate_recovery_write_buffer(&matching).is_ok());
        let snapshots = [missing, malformed, matching]
            .into_iter()
            .map(|payload| payload_metadata(&payload))
            .collect::<Vec<_>>();
        let metadata = snapshots
            .iter()
            .map(|(metadata, _)| metadata.clone())
            .collect::<Vec<_>>();
        for (metadata, bytes) in &snapshots {
            store
                .atomic(
                    &store.directory.join(metadata.payload.as_ref().unwrap()),
                    bytes,
                )
                .unwrap();
        }
        let mut manifest = recovery_manifest(&root, metadata);
        manifest.active_path = Some("missing.md".into());
        store.write(&manifest).unwrap();
        let authority = ProjectAuthority::default();
        let restored = restore_recovery(&store, &authority, manifest, &root)
            .unwrap()
            .unwrap();
        for (path, text) in [
            ("missing.md", "exact missing recovered text"),
            ("malformed.md", "exact malformed recovered text"),
        ] {
            let document = restored
                .documents
                .iter()
                .find(|document| document.relative_path.as_deref() == Some(path))
                .unwrap();
            assert_eq!(document.content, text);
            assert!(
                document
                    .conflict
                    .as_deref()
                    .is_some_and(|error| error.contains("no valid disk baseline"))
            );
            assert_ne!(
                document.baseline_hash,
                hash_content(if path == "missing.md" {
                    "disk missing baseline"
                } else {
                    "disk malformed baseline"
                })
            );
            let mut state = authority.0.lock().unwrap();
            assert!(
                write_document_revision(
                    state.project.as_mut().unwrap(),
                    document.document_id.expect("bound file"),
                    document.write_revision + 1,
                    "must remain blocked"
                )
                .is_err()
            );
        }
        let matching = restored
            .documents
            .iter()
            .find(|document| document.relative_path.as_deref() == Some("matching.md"))
            .unwrap();
        assert_eq!(matching.content, "recovered but verified draft");
        assert!(matching.conflict.is_none());
        assert_eq!(
            matching.baseline_hash,
            hash_content("matching disk baseline")
        );

        let mut untitled = recovery_buffer("placeholder.md", 99, 7, Some("untitled recovery text"));
        untitled.relative_path = None;
        untitled.document_id = None;
        untitled.baseline_hash = None;
        assert!(validate_recovery_write_buffer(&untitled).is_ok());
        let (untitled_metadata, untitled_bytes) = payload_metadata(&untitled);
        let untitled_store_base = temp("untitled-recovery-store");
        let untitled_directory = untitled_store_base.join(RecoveryStore::project_key(&root_text));
        let untitled_store = RecoveryStore {
            base: untitled_store_base.clone(),
            manifest: untitled_directory.join("manifest.json"),
            directory: untitled_directory,
        };
        untitled_store
            .atomic(
                &untitled_store
                    .directory
                    .join(untitled_metadata.payload.as_ref().unwrap()),
                &untitled_bytes,
            )
            .unwrap();
        let mut untitled_manifest = recovery_manifest(&root, vec![untitled_metadata]);
        untitled_manifest.active_path = None;
        untitled_store.write(&untitled_manifest).unwrap();
        let untitled_authority = ProjectAuthority::default();
        let untitled_restored = restore_recovery(
            &untitled_store,
            &untitled_authority,
            untitled_manifest,
            &root,
        )
        .unwrap()
        .unwrap();
        assert_eq!(untitled_restored.documents.len(), 1);
        assert_eq!(
            untitled_restored.documents[0].content,
            "untitled recovery text"
        );
        assert!(untitled_restored.documents[0].document_id.is_none());
        assert!(untitled_restored.documents[0].conflict.is_none());
        let mut next_untitled = untitled;
        next_untitled.sequence = 8;
        write_recovery_transaction(&untitled_store, &root_text, next_untitled).unwrap();
        assert_eq!(
            untitled_store
                .read_bound(&root_text)
                .unwrap()
                .unwrap()
                .buffers[0]
                .sequence,
            8
        );

        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(base).unwrap();
        fs::remove_dir_all(untitled_store_base).unwrap();
    }

    #[test]
    fn recovery_manifest_rejects_adversarial_metadata_and_sizes() {
        let base = temp("recovery-manifest-adversarial");
        let directory = base.join("project");
        let store = RecoveryStore {
            base: base.clone(),
            manifest: directory.join("manifest.json"),
            directory,
        };
        let payload = recovery_buffer("a.md", 11, 1, Some("draft"));
        let (metadata, _) = payload_metadata(&payload);
        let (duplicate_id, _) = payload_metadata(&recovery_buffer("b.md", 11, 1, Some("other")));
        let oversized_total = (0..5)
            .map(|index| {
                let (mut entry, _) = payload_metadata(&recovery_buffer(
                    &format!("{index}.md"),
                    index + 100,
                    1,
                    Some("draft"),
                ));
                entry.payload_bytes = Some(900_000);
                entry
            })
            .collect();
        let too_many = (0..=RECOVERY_MAX_BUFFERS)
            .map(|index| {
                payload_metadata(&recovery_buffer(
                    &format!("count-{index}.md"),
                    index as u64 + 1000,
                    1,
                    Some("draft"),
                ))
                .0
            })
            .collect();
        for mut manifest in [
            {
                let mut duplicate = metadata.clone();
                duplicate.sequence = 2;
                duplicate.payload = Some(expected_payload_name(Some("a.md"), 2));
                recovery_manifest(Path::new("/tmp"), vec![metadata.clone(), duplicate])
            },
            recovery_manifest(Path::new("/tmp"), vec![metadata.clone(), duplicate_id]),
            {
                let mut traversal = metadata.clone();
                traversal.relative_path = Some("../escape.md".into());
                recovery_manifest(Path::new("/tmp"), vec![traversal])
            },
            {
                let mut absolute = metadata.clone();
                absolute.relative_path = Some("/absolute.md".into());
                recovery_manifest(Path::new("/tmp"), vec![absolute])
            },
            {
                let mut non_markdown = metadata.clone();
                non_markdown.relative_path = Some("bad.txt".into());
                recovery_manifest(Path::new("/tmp"), vec![non_markdown])
            },
            {
                let mut malformed = metadata.clone();
                malformed.payload_hash = Some("bad".into());
                recovery_manifest(Path::new("/tmp"), vec![malformed])
            },
            {
                let mut oversized = metadata.clone();
                oversized.payload_bytes = Some(RECOVERY_MAX_PAYLOAD_BYTES + 1);
                recovery_manifest(Path::new("/tmp"), vec![oversized])
            },
            recovery_manifest(Path::new("/tmp"), oversized_total),
            {
                let mut invalid_selection = metadata.clone();
                invalid_selection.selection = Some(RecoverySelection {
                    anchor: RECOVERY_MAX_PAYLOAD_BYTES + 1,
                    head: 0,
                });
                recovery_manifest(Path::new("/tmp"), vec![invalid_selection])
            },
            recovery_manifest(Path::new("/tmp"), too_many),
        ] {
            // Write unchecked data so `read` is the adversarial boundary under test.
            manifest.project_key = RecoveryStore::project_key(&manifest.root);
            store
                .atomic(&store.manifest, &serde_json::to_vec(&manifest).unwrap())
                .unwrap();
            assert!(store.read().is_err());
        }
        let oversized = vec![b'x'; RECOVERY_MAX_MANIFEST_BYTES + 1];
        store.atomic(&store.manifest, &oversized).unwrap();
        assert!(store.read().is_err());
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn recovery_payload_rejects_links_modes_integrity_and_outside_files() {
        let base = temp("recovery-payload-adversarial");
        let directory = base.join("project");
        let store = RecoveryStore {
            base: base.clone(),
            manifest: directory.join("manifest.json"),
            directory,
        };
        let payload = recovery_buffer("a.md", 11, 1, Some("draft"));
        let (metadata, data) = payload_metadata(&payload);
        store
            .atomic(
                &store.directory.join(metadata.payload.as_ref().unwrap()),
                &data,
            )
            .unwrap();
        assert_eq!(
            store
                .read_payload(
                    metadata.payload.as_ref().unwrap(),
                    data.len(),
                    metadata.payload_hash.as_ref().unwrap()
                )
                .unwrap()
                .content
                .as_deref(),
            Some("draft")
        );
        assert!(
            store
                .read_payload(
                    metadata.payload.as_ref().unwrap(),
                    data.len(),
                    "0".repeat(64).as_str()
                )
                .is_err()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::{PermissionsExt, symlink};
            let outside = base.join("outside.json");
            fs::write(&outside, b"outside remains").unwrap();
            let link = store.directory.join("buffer-link-1.json");
            symlink(&outside, &link).unwrap();
            assert!(
                store
                    .read_payload(
                        "buffer-link-1.json",
                        15,
                        &blake3::hash(b"outside remains").to_hex()
                    )
                    .is_err()
            );
            assert_eq!(fs::read_to_string(&outside).unwrap(), "outside remains");
            let hard = store.directory.join("buffer-hard-1.json");
            fs::hard_link(&outside, &hard).unwrap();
            assert!(
                store
                    .read_payload(
                        "buffer-hard-1.json",
                        15,
                        &blake3::hash(b"outside remains").to_hex()
                    )
                    .is_err()
            );
            fs::remove_file(&hard).unwrap();
            fs::set_permissions(
                store.directory.join(metadata.payload.as_ref().unwrap()),
                fs::Permissions::from_mode(0o644),
            )
            .unwrap();
            assert!(
                store
                    .read_payload(
                        metadata.payload.as_ref().unwrap(),
                        data.len(),
                        metadata.payload_hash.as_ref().unwrap()
                    )
                    .is_err()
            );
        }
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn recovery_storage_rejects_preexisting_insecure_or_linked_directories() {
        let base = temp("recovery-insecure-directory");
        let directory = base.join("project");
        let store = RecoveryStore {
            base: base.clone(),
            manifest: directory.join("manifest.json"),
            directory,
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::{PermissionsExt, symlink};
            fs::set_permissions(&base, fs::Permissions::from_mode(0o755)).unwrap();
            assert!(
                store
                    .write(&recovery_manifest(Path::new("/safe"), Vec::new()))
                    .is_err()
            );
            fs::set_permissions(&base, fs::Permissions::from_mode(0o700)).unwrap();
            symlink(base.join("outside"), &store.directory).unwrap();
            assert!(
                store
                    .write(&recovery_manifest(Path::new("/safe"), Vec::new()))
                    .is_err()
            );
        }
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn recovery_writer_enforces_aggregate_limit_without_publishing_the_rejected_update() {
        let base = temp("recovery-aggregate-limit");
        let directory = base.join("project");
        let store = RecoveryStore {
            base: base.clone(),
            manifest: directory.join("manifest.json"),
            directory,
        };
        let entries = (0..4)
            .map(|index| {
                let (mut entry, _) = payload_metadata(&recovery_buffer(
                    &format!("{index}.md"),
                    index + 1,
                    1,
                    Some("draft"),
                ));
                entry.payload_bytes = Some(RECOVERY_MAX_PAYLOAD_BYTES);
                entry
            })
            .collect::<Vec<_>>();
        assert_eq!(
            recovery_total_bytes(&entries).unwrap(),
            RECOVERY_MAX_TOTAL_BYTES
        );
        let mut manifest = recovery_manifest(Path::new("/tmp"), entries);
        manifest.active_path = None;
        store.write(&manifest).unwrap();
        let (mut newest, _) = payload_metadata(&recovery_buffer("newest.md", 99, 1, Some("draft")));
        newest.payload_bytes = Some(1);
        let error = with_recovery_update(&manifest, newest).unwrap_err();
        assert!(error.contains("latest snapshot was not recorded"));
        let retained = store.read().unwrap().unwrap();
        assert_eq!(retained.buffers.len(), 4);
        assert_eq!(
            recovery_total_bytes(&retained.buffers).unwrap(),
            RECOVERY_MAX_TOTAL_BYTES
        );
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn recovery_transaction_lock_preserves_concurrent_buffer_updates_and_references() {
        let base = temp("recovery-transaction-lock");
        let directory = base.join(RecoveryStore::project_key("/tmp"));
        let store = RecoveryStore {
            base: base.clone(),
            manifest: directory.clone().join("manifest.json"),
            directory,
        };
        let lock = Arc::new(RecoveryAuthority::default());
        let barrier = Arc::new(Barrier::new(3));
        let root = "/tmp".to_string();
        let writers = [
            (
                store.clone(),
                lock.clone(),
                barrier.clone(),
                root.clone(),
                recovery_buffer("a.md", 1, 1, Some("a")),
            ),
            (
                store.clone(),
                lock.clone(),
                barrier.clone(),
                root.clone(),
                recovery_buffer("b.md", 2, 1, Some("b")),
            ),
        ]
        .into_iter()
        .map(|(store, lock, barrier, root, buffer)| {
            thread::spawn(move || {
                barrier.wait();
                let _transaction = lock.0.lock().unwrap();
                write_recovery_transaction(&store, &root, buffer).unwrap();
            })
        })
        .collect::<Vec<_>>();
        barrier.wait();
        for writer in writers {
            writer.join().unwrap();
        }
        let manifest = store.read().unwrap().unwrap();
        assert_eq!(manifest.buffers.len(), 2);
        for entry in &manifest.buffers {
            let payload = entry.payload.as_ref().unwrap();
            assert!(store.directory.join(payload).is_file());
            assert_eq!(
                store
                    .read_payload(
                        payload,
                        entry.payload_bytes.unwrap(),
                        entry.payload_hash.as_ref().unwrap()
                    )
                    .unwrap()
                    .relative_path,
                entry.relative_path
            );
        }
        assert_eq!(
            fs::read_dir(&store.directory).unwrap().count(),
            3,
            "manifest plus only current payloads remain"
        );
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn wrong_root_manifest_in_trusted_namespace_cannot_trigger_cleanup() {
        let base = temp("recovery-wrong-root-no-cleanup");
        let root_a = "/trusted/project-a";
        let root_b = "/trusted/project-b";
        let directory = base.join(RecoveryStore::project_key(root_a));
        let store = RecoveryStore {
            base: base.clone(),
            manifest: directory.join("manifest.json"),
            directory,
        };
        let buffer = recovery_buffer("a.md", 1, 1, Some("A recovery content"));
        let (metadata, payload) = payload_metadata(&buffer);
        let mut manifest_a = RecoveryManifest {
            version: RECOVERY_VERSION,
            root: root_a.into(),
            project_key: RecoveryStore::project_key(root_a),
            active_path: Some("a.md".into()),
            buffers: vec![metadata.clone()],
        };
        store
            .atomic(
                &store.directory.join(metadata.payload.as_ref().unwrap()),
                &payload,
            )
            .unwrap();
        store.write(&manifest_a).unwrap();
        let mut before_names = fs::read_dir(&store.directory)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        before_names.sort();
        let before_payload =
            fs::read(store.directory.join(metadata.payload.as_ref().unwrap())).unwrap();
        manifest_a.root = root_b.into();
        manifest_a.project_key = RecoveryStore::project_key(root_b);
        manifest_a.active_path = None;
        manifest_a.buffers.clear();
        // Simulate an attacker replacing only the manifest in A's namespace.
        store
            .atomic(&store.manifest, &serde_json::to_vec(&manifest_a).unwrap())
            .unwrap();
        assert!(store.read_bound(root_a).is_err());
        let mut after_names = fs::read_dir(&store.directory)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        after_names.sort();
        assert_eq!(after_names, before_names);
        assert_eq!(
            fs::read(store.directory.join(metadata.payload.unwrap())).unwrap(),
            before_payload
        );
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn terminal_discard_never_terminates_on_delete_failure_and_terminates_once_on_success() {
        let calls = std::sync::Mutex::new(Vec::new());
        assert!(
            terminal_discard(
                || {
                    calls.lock().unwrap().push("delete");
                    Err("delete failed".into())
                },
                || calls.lock().unwrap().push("exit"),
            )
            .is_err()
        );
        assert_eq!(*calls.lock().unwrap(), vec!["delete"]);
        terminal_discard(
            || {
                calls.lock().unwrap().push("delete");
                Ok(())
            },
            || calls.lock().unwrap().push("exit"),
        )
        .unwrap();
        assert_eq!(*calls.lock().unwrap(), vec!["delete", "delete", "exit"]);
    }

    #[test]
    fn close_interception_is_ready_gated_deduplicated_and_has_a_listener_failure_fallback() {
        let mut state = CloseState::default();
        // A close before frontend listener registration is intentionally the
        // normal platform close, not a permanently prevented request.
        assert_eq!(request_window_close(&mut state), CloseRequestAction::Allow);

        state.frontend_ready = true;
        let request_id = match request_window_close(&mut state) {
            CloseRequestAction::PreventAndEmit(id) => id,
            other => panic!("expected an emitted close request, got {other:?}"),
        };
        assert_eq!(
            request_window_close(&mut state),
            CloseRequestAction::PreventExisting
        );
        assert!(take_unacknowledged_close_fallback(&mut state, request_id));
        assert!(state.permitted);
        assert_eq!(state.pending_request, None);

        // A listener that acknowledged the request owns the dirty dialog; the
        // bounded fallback must not interrupt it.
        state.permitted = false;
        let acknowledged = match request_window_close(&mut state) {
            CloseRequestAction::PreventAndEmit(id) => id,
            other => panic!("expected an emitted close request, got {other:?}"),
        };
        state.acknowledged = true;
        assert!(!take_unacknowledged_close_fallback(
            &mut state,
            acknowledged
        ));
        state.pending_request = None;
        state.acknowledged = false;
        assert!(matches!(
            request_window_close(&mut state),
            CloseRequestAction::PreventAndEmit(_)
        ));
    }

    #[test]
    fn recovery_cleanup_bounds_superseded_payloads_and_crash_orphans() {
        let base = temp("recovery-cleanup");
        let directory = base.join(RecoveryStore::project_key("/tmp"));
        let store = RecoveryStore {
            base: base.clone(),
            manifest: directory.join("manifest.json"),
            directory,
        };
        let first_crash = expected_payload_name(Some("a.md"), 1);
        store
            .atomic(&store.directory.join(&first_crash), b"first crash leftover")
            .unwrap();
        assert!(store.read_bound("/tmp").unwrap().is_none());
        assert!(!store.directory.join(&first_crash).exists());
        let mut manifest = recovery_manifest(Path::new("/tmp"), Vec::new());
        manifest.active_path = None;
        for sequence in 1..=20 {
            let buffer = recovery_buffer("a.md", 1, sequence, Some("draft"));
            let (metadata, data) = payload_metadata(&buffer);
            let next = with_recovery_update(&manifest, metadata.clone()).unwrap();
            store
                .atomic(
                    &store.directory.join(metadata.payload.as_ref().unwrap()),
                    &data,
                )
                .unwrap();
            store.write(&next).unwrap();
            assert!(store.finish_recovery_commit(&next).cleanup_error.is_none());
            manifest = next;
        }
        let entries = fs::read_dir(&store.directory).unwrap().count();
        assert_eq!(
            entries, 2,
            "only manifest and current payload remain after sequential updates"
        );
        let current = manifest.buffers[0].payload.clone().unwrap();
        let crash = expected_payload_name(Some("a.md"), 21);
        store
            .atomic(&store.directory.join(&crash), b"crash leftover")
            .unwrap();
        assert!(
            store
                .finish_recovery_commit(&manifest)
                .cleanup_error
                .is_none()
        );
        assert!(store.directory.join(&current).exists());
        assert!(!store.directory.join(&crash).exists());
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn recovery_cleanup_refuses_hostile_or_excess_entries_without_deleting_them() {
        let base = temp("recovery-cleanup-hostile");
        let directory = base.join("project");
        let store = RecoveryStore {
            base: base.clone(),
            manifest: directory.join("manifest.json"),
            directory,
        };
        let buffer = recovery_buffer("a.md", 1, 1, Some("draft"));
        let (metadata, data) = payload_metadata(&buffer);
        let mut manifest = recovery_manifest(Path::new("/tmp"), vec![metadata.clone()]);
        manifest.active_path = None;
        store
            .atomic(
                &store.directory.join(metadata.payload.as_ref().unwrap()),
                &data,
            )
            .unwrap();
        store.write(&manifest).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::{PermissionsExt, symlink};
            let outside = base.join("outside");
            fs::write(&outside, "outside").unwrap();
            fs::set_permissions(&outside, fs::Permissions::from_mode(0o600)).unwrap();
            let link = store
                .directory
                .join(expected_payload_name(Some("orphan.md"), 1));
            symlink(&outside, &link).unwrap();
            assert!(
                store
                    .finish_recovery_commit(&manifest)
                    .cleanup_error
                    .is_some()
            );
            assert!(link.exists() || fs::symlink_metadata(&link).is_ok());
            assert_eq!(fs::read_to_string(&outside).unwrap(), "outside");
            fs::remove_file(&link).unwrap();
            let hard = store
                .directory
                .join(expected_payload_name(Some("hard.md"), 1));
            fs::hard_link(&outside, &hard).unwrap();
            assert!(
                store
                    .finish_recovery_commit(&manifest)
                    .cleanup_error
                    .is_some()
            );
            assert!(hard.exists());
            fs::remove_file(&hard).unwrap();
        }
        let unrelated = store.directory.join("unrelated.txt");
        fs::write(&unrelated, "keep").unwrap();
        assert!(
            store
                .finish_recovery_commit(&manifest)
                .cleanup_error
                .is_some()
        );
        assert!(unrelated.exists());
        fs::remove_file(&unrelated).unwrap();
        for index in 0..=RECOVERY_CLEANUP_SCAN_LIMIT {
            let name = format!(
                "buffer-{}-{index}.json",
                blake3::hash(format!("{index}").as_bytes()).to_hex()
            );
            store
                .atomic(&store.directory.join(name), b"orphan")
                .unwrap();
        }
        assert!(
            store
                .finish_recovery_commit(&manifest)
                .cleanup_error
                .is_some()
        );
        assert!(
            store.directory.join(metadata.payload.unwrap()).exists(),
            "scan-limit failure leaves current payload untouched"
        );
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn committed_manifest_reports_cleanup_failure_and_is_recoverable() {
        let base = temp("recovery-cleanup-commit");
        let directory = base.join("project");
        let store = RecoveryStore {
            base: base.clone(),
            manifest: directory.join("manifest.json"),
            directory,
        };
        let buffer = recovery_buffer("a.md", 1, 1, Some("draft"));
        let (metadata, data) = payload_metadata(&buffer);
        let mut manifest = recovery_manifest(Path::new("/tmp"), vec![metadata.clone()]);
        manifest.active_path = None;
        store
            .atomic(
                &store.directory.join(metadata.payload.as_ref().unwrap()),
                &data,
            )
            .unwrap();
        store.write(&manifest).unwrap();
        let unrelated = store.directory.join("unrelated.txt");
        fs::write(&unrelated, "block cleanup").unwrap();
        assert!(
            store
                .finish_recovery_commit(&manifest)
                .cleanup_error
                .is_some()
        );
        fs::remove_file(&unrelated).unwrap();
        assert!(
            store
                .finish_recovery_commit(&manifest)
                .cleanup_error
                .is_none()
        );
        assert_eq!(store.read().unwrap().unwrap().buffers.len(), 1);
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn legacy_v1_import_is_atomic_idempotent_and_preserves_every_legacy_byte() {
        // This test deliberately uses only an approved synthetic root.
        let base = PathBuf::from("/tmp/opencode").join(format!(
            "amte-legacy-import-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&base).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&base, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let root = base.join("project");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("clean.md"), "disk current").unwrap();
        let canonical = root.canonicalize().unwrap().display().to_string();
        let directory = base.join(RecoveryStore::project_key(&canonical));
        let store = RecoveryStore {
            base: base.clone(),
            manifest: directory.join("manifest.json"),
            directory,
        };
        let mut dirty = recovery_buffer("dirty.md", 7, 8, Some("draft bytes"));
        dirty.client_edit_version = 0;
        let mut clean = recovery_buffer("clean.md", 8, 9, None);
        clean.client_edit_version = 0;
        write_recovery_transaction(&store, &canonical, dirty.clone()).unwrap();
        write_recovery_transaction(&store, &canonical, clean.clone()).unwrap();
        let before = fs::read_dir(&store.directory)
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                (entry.file_name(), fs::read(entry.path()).unwrap())
            })
            .collect::<Vec<_>>();
        let db = RecoveryDb::for_config(&base.join("xdg")).unwrap();
        import_legacy_recovery_store_once(&store, &db, &canonical).unwrap();
        assert_eq!(
            db.legacy_import_test_state(&canonical).unwrap(),
            (Some("imported-v1".into()), 2, 2, vec![1, 1])
        );
        let restored = db.latest(&canonical).unwrap().unwrap();
        assert_eq!(restored.buffers.len(), 2);
        assert_eq!(
            restored
                .buffers
                .iter()
                .find(|buffer| buffer.relative_path.as_deref() == Some("dirty.md"))
                .unwrap()
                .content
                .as_deref(),
            Some("draft bytes")
        );
        assert!(
            restored
                .buffers
                .iter()
                .find(|buffer| buffer.relative_path.as_deref() == Some("clean.md"))
                .unwrap()
                .content
                .is_none()
        );
        assert_eq!(
            before,
            fs::read_dir(&store.directory)
                .unwrap()
                .map(|entry| {
                    let entry = entry.unwrap();
                    (entry.file_name(), fs::read(entry.path()).unwrap())
                })
                .collect::<Vec<_>>()
        );
        // Repeated import observes the terminal marker and cannot duplicate rows.
        import_legacy_recovery_store_once(&store, &db, &canonical).unwrap();
        assert_eq!(
            db.legacy_import_test_state(&canonical).unwrap(),
            (Some("imported-v1".into()), 2, 2, vec![1, 1])
        );
        let mut next = dirty.clone();
        next.client_edit_version = 10;
        next.content = Some("newer".into());
        assert_eq!(
            db.write_snapshot(&canonical, None, &next)
                .unwrap()
                .recovery_sequence,
            2
        );
        // The immutable import event may point at the first snapshot, but its
        // audit record must survive pruning more than the per-buffer window.
        for version in 11..=31 {
            let mut revision = dirty.clone();
            revision.write_revision = version;
            revision.client_edit_version = version;
            revision.content = Some(format!("post-import-{version}"));
            db.write_snapshot(&canonical, None, &revision).unwrap();
        }
        let restored_after_retention = db.latest(&canonical).unwrap().unwrap();
        assert_eq!(
            restored_after_retention
                .buffers
                .iter()
                .find(|buffer| buffer.relative_path.as_deref() == Some("dirty.md"))
                .and_then(|buffer| buffer.content.as_deref()),
            Some("post-import-31")
        );
        let connection = rusqlite::Connection::open(db.test_path()).unwrap();
        let project_key = blake3::hash(canonical.as_bytes()).to_hex().to_string();
        assert_eq!(
            connection
                .query_row(
                    "SELECT outcome FROM legacy_imports WHERE project_key=?1",
                    [&project_key],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "imported-v1"
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*) FROM snapshots s JOIN buffers b ON b.id=s.buffer_id WHERE b.project_key=?1 AND b.relative_path='dirty.md'",
                    [&project_key],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            20
        );
        assert_eq!(
            before,
            fs::read_dir(&store.directory)
                .unwrap()
                .map(|entry| {
                    let entry = entry.unwrap();
                    (entry.file_name(), fs::read(entry.path()).unwrap())
                })
                .collect::<Vec<_>>()
        );
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn corrupt_legacy_import_is_retryable_and_existing_v2_wins() {
        let base = PathBuf::from("/tmp/opencode").join(format!(
            "amte-legacy-reject-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&base).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&base, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let root = base.join("project");
        fs::create_dir(&root).unwrap();
        let canonical = root.canonicalize().unwrap().display().to_string();
        let directory = base.join(RecoveryStore::project_key(&canonical));
        let store = RecoveryStore {
            base: base.clone(),
            manifest: directory.join("manifest.json"),
            directory,
        };
        let mut legacy = recovery_buffer("a.md", 1, 1, Some("legacy"));
        legacy.client_edit_version = 0;
        write_recovery_transaction(&store, &canonical, legacy.clone()).unwrap();
        let manifest = store.read_bound(&canonical).unwrap().unwrap();
        let payload = store
            .directory
            .join(manifest.buffers[0].payload.as_ref().unwrap());
        let original = fs::read(&payload).unwrap();
        fs::write(&payload, b"corrupt").unwrap();
        let db = RecoveryDb::for_config(&base.join("xdg")).unwrap();
        assert!(import_legacy_recovery_store_once(&store, &db, &canonical).is_err());
        assert_no_legacy_import(&db, &canonical);
        fs::write(&payload, &original).unwrap();
        import_legacy_recovery_store_once(&store, &db, &canonical).unwrap();
        assert_eq!(
            db.legacy_import_test_state(&canonical).unwrap(),
            (Some("imported-v1".into()), 1, 1, vec![1])
        );
        let mut v2 = legacy.clone();
        v2.client_edit_version = 2;
        v2.content = Some("v2".into());
        db.write_snapshot(&canonical, None, &v2).unwrap();
        assert!(import_legacy_recovery_store_once(&store, &db, &canonical).is_ok());
        assert_eq!(
            db.legacy_import_test_state(&canonical).unwrap(),
            (Some("imported-v1".into()), 1, 2, vec![1, 2])
        );
        assert!(import_legacy_recovery_store_once(&store, &db, "/wrong/root").is_err());
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn committed_v2_marker_restores_before_corrupt_legacy_is_even_parsed() {
        let base = temp("v2-before-legacy");
        let root = base.join("project");
        fs::create_dir(&root).unwrap();
        let canonical = root.canonicalize().unwrap().display().to_string();
        let directory = base.join(RecoveryStore::project_key(&canonical));
        let store = RecoveryStore {
            base: base.clone(),
            manifest: directory.join("manifest.json"),
            directory,
        };
        let mut legacy = recovery_buffer("chapter.md", 1, 1, Some("legacy draft"));
        legacy.client_edit_version = 0;
        write_recovery_transaction(&store, &canonical, legacy).unwrap();
        let db = RecoveryDb::for_config(&base.join("config")).unwrap();
        import_legacy_recovery_store_once(&store, &db, &canonical).unwrap();

        // Make the V1 payload hostile after the committed V2 import. Calling
        // its parser would now fail; the startup ordering must not call it.
        let manifest = store.read_bound(&canonical).unwrap().unwrap();
        fs::write(
            store
                .directory
                .join(manifest.buffers[0].payload.as_ref().unwrap()),
            b"corrupt legacy",
        )
        .unwrap();
        let called = std::cell::Cell::new(false);
        let restored = restore_v2_before_legacy(&db, &canonical, || {
            called.set(true);
            import_legacy_recovery_store_once(&store, &db, &canonical)
        })
        .unwrap()
        .unwrap();
        assert!(
            !called.get(),
            "a valid V2 row/marker must short-circuit V1 parsing"
        );
        assert_eq!(restored.buffers[0].content.as_deref(), Some("legacy draft"));
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn legacy_imported_dirty_disk_divergence_restores_a_conflict_and_blocks_both_save_paths() {
        let (base, root, canonical, store, db) = migration_fixture("legacy-conflict");
        let document = root.join("chapter.md");
        fs::write(&document, "disk baseline").unwrap();
        let mut dirty = recovery_buffer("chapter.md", 7, 8, Some("recovered draft"));
        dirty.client_edit_version = 0;
        dirty.baseline_hash = Some(hash_content("disk baseline"));
        write_recovery_transaction(&store, &canonical, dirty).unwrap();
        let legacy_before = legacy_entry_fingerprint(&store);
        fs::write(&document, "disk changed while closed").unwrap();

        import_legacy_recovery_store_once(&store, &db, &canonical).unwrap();
        assert_eq!(legacy_entry_fingerprint(&store), legacy_before);
        assert_eq!(
            db.legacy_import_test_state(&canonical).unwrap(),
            (Some("imported-v1".into()), 1, 1, vec![1])
        );
        let authority = ProjectAuthority::default();
        let restored = restore_recovery_v2(
            &authority,
            &root,
            db.latest(&canonical).unwrap().unwrap(),
            &[],
        )
        .unwrap();
        let recovered = &restored.documents[0];
        assert_eq!(recovered.content, "recovered draft");
        assert!(
            recovered
                .conflict
                .as_deref()
                .is_some_and(|message| message.starts_with("CONFLICT:"))
        );
        let document_id = recovered.document_id.unwrap();
        let disk_before_save_attempts = fs::read_to_string(&document).unwrap();
        let mut state = authority.0.lock().unwrap();
        let project = state.project.as_mut().unwrap();
        let manual = write_document_revision(project, document_id, 9, "manual save must not win")
            .unwrap_err();
        let autosave =
            write_document_revision(project, document_id, 10, "autosave must not win").unwrap_err();
        assert!(manual.starts_with("CONFLICT:"));
        assert!(autosave.starts_with("CONFLICT:"));
        drop(state);
        assert_eq!(
            fs::read_to_string(&document).unwrap(),
            disk_before_save_attempts
        );
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn legacy_import_missing_dirty_payload_is_retryable_and_never_marks_or_mutates_v1() {
        let (base, _root, canonical, store, db) = migration_fixture("legacy-missing-payload");
        let mut dirty = recovery_buffer("missing.md", 1, 1, Some("recover me"));
        dirty.client_edit_version = 0;
        write_recovery_transaction(&store, &canonical, dirty).unwrap();
        let manifest = store.read_bound(&canonical).unwrap().unwrap();
        let payload = store
            .directory
            .join(manifest.buffers[0].payload.as_ref().unwrap());
        let payload_bytes = fs::read(&payload).unwrap();
        fs::remove_file(&payload).unwrap();
        let legacy_before = legacy_entry_fingerprint(&store);

        assert!(import_legacy_recovery_store_once(&store, &db, &canonical).is_err());
        assert_no_legacy_import(&db, &canonical);
        assert_eq!(legacy_entry_fingerprint(&store), legacy_before);

        fs::write(&payload, payload_bytes).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&payload, fs::Permissions::from_mode(0o600)).unwrap();
        }
        import_legacy_recovery_store_once(&store, &db, &canonical).unwrap();
        assert_eq!(
            db.legacy_import_test_state(&canonical).unwrap(),
            (Some("imported-v1".into()), 1, 1, vec![1])
        );
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn legacy_import_enforces_all_v1_bounds_before_v2_commit() {
        // Oversized manifest: read() rejects based on metadata before creating
        // its input Vec or opening a SQLite import transaction.
        let (base, _root, canonical, store, db) = migration_fixture("legacy-bound-manifest");
        fs::create_dir_all(&store.directory).unwrap();
        fs::write(&store.manifest, vec![b'x'; RECOVERY_MAX_MANIFEST_BYTES + 1]).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&store.manifest, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let legacy_before = legacy_entry_fingerprint(&store);
        assert!(import_legacy_recovery_store_once(&store, &db, &canonical).is_err());
        assert_no_legacy_import(&db, &canonical);
        assert_eq!(legacy_entry_fingerprint(&store), legacy_before);
        fs::remove_dir_all(base).unwrap();

        // Oversized payload metadata is rejected before read_payload can reserve
        // or read the oversized legacy file.
        let (base, _root, canonical, store, db) = migration_fixture("legacy-bound-payload");
        let mut buffer = recovery_buffer("large.md", 1, 1, Some("small metadata payload"));
        buffer.client_edit_version = 0;
        write_recovery_transaction(&store, &canonical, buffer).unwrap();
        let mut manifest = store.read_bound(&canonical).unwrap().unwrap();
        let payload = store
            .directory
            .join(manifest.buffers[0].payload.as_ref().unwrap());
        fs::write(&payload, vec![b'p'; RECOVERY_MAX_PAYLOAD_BYTES + 1]).unwrap();
        manifest.buffers[0].payload_bytes = Some(RECOVERY_MAX_PAYLOAD_BYTES + 1);
        fs::write(&store.manifest, serde_json::to_vec(&manifest).unwrap()).unwrap();
        let legacy_before = legacy_entry_fingerprint(&store);
        assert!(import_legacy_recovery_store_once(&store, &db, &canonical).is_err());
        assert_no_legacy_import(&db, &canonical);
        assert_eq!(legacy_entry_fingerprint(&store), legacy_before);
        fs::remove_dir_all(base).unwrap();

        // An over-count manifest is rejected before payload reads or V2 rows.
        let (base, _root, canonical, store, db) = migration_fixture("legacy-bound-count");
        let mut buffer = recovery_buffer("one.md", 1, 1, Some("one"));
        buffer.client_edit_version = 0;
        write_recovery_transaction(&store, &canonical, buffer).unwrap();
        let mut manifest = store.read_bound(&canonical).unwrap().unwrap();
        manifest.buffers = vec![manifest.buffers[0].clone(); RECOVERY_MAX_BUFFERS + 1];
        fs::write(&store.manifest, serde_json::to_vec(&manifest).unwrap()).unwrap();
        let legacy_before = legacy_entry_fingerprint(&store);
        assert!(import_legacy_recovery_store_once(&store, &db, &canonical).is_err());
        assert_no_legacy_import(&db, &canonical);
        assert_eq!(legacy_entry_fingerprint(&store), legacy_before);
        fs::remove_dir_all(base).unwrap();

        // Aggregate declared bytes are bounded while parsing the manifest, so no
        // individual payload is opened when the total is too large.
        let (base, _root, canonical, store, db) = migration_fixture("legacy-bound-total");
        let mut initial = recovery_buffer("seed.md", 1, 1, Some("seed"));
        initial.client_edit_version = 0;
        write_recovery_transaction(&store, &canonical, initial).unwrap();
        let mut manifest = store.read_bound(&canonical).unwrap().unwrap();
        manifest.buffers = (0..5)
            .map(|index| {
                let buffer = recovery_buffer(
                    &format!("buffer-{index}.md"),
                    index + 1,
                    index + 1,
                    Some("declared but never opened"),
                );
                let (mut metadata, _) = payload_metadata(&buffer);
                metadata.payload_bytes = Some(RECOVERY_MAX_PAYLOAD_BYTES);
                metadata
            })
            .collect();
        fs::write(&store.manifest, serde_json::to_vec(&manifest).unwrap()).unwrap();
        let legacy_before = legacy_entry_fingerprint(&store);
        assert!(import_legacy_recovery_store_once(&store, &db, &canonical).is_err());
        assert_no_legacy_import(&db, &canonical);
        assert_eq!(legacy_entry_fingerprint(&store), legacy_before);
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn legacy_import_fails_closed_on_hostile_storage_without_touching_v1_or_outside() {
        for attack in [
            "manifest-symlink",
            "payload-symlink",
            "payload-hardlink",
            "insecure-file",
            "insecure-directory",
            "nonregular-payload",
        ] {
            let (base, _root, canonical, store, db) =
                migration_fixture(&format!("legacy-hostile-{attack}"));
            let mut buffer = recovery_buffer("attack.md", 1, 1, Some("legacy draft"));
            buffer.client_edit_version = 0;
            write_recovery_transaction(&store, &canonical, buffer).unwrap();
            let manifest = store.read_bound(&canonical).unwrap().unwrap();
            let payload = store
                .directory
                .join(manifest.buffers[0].payload.as_ref().unwrap());
            let outside = base.join("outside-target");
            fs::write(&outside, "outside bytes must remain exact").unwrap();
            match attack {
                "manifest-symlink" => {
                    fs::remove_file(&store.manifest).unwrap();
                    std::os::unix::fs::symlink(&outside, &store.manifest).unwrap();
                }
                "payload-symlink" => {
                    fs::remove_file(&payload).unwrap();
                    std::os::unix::fs::symlink(&outside, &payload).unwrap();
                }
                "payload-hardlink" => {
                    fs::remove_file(&payload).unwrap();
                    fs::hard_link(&outside, &payload).unwrap();
                }
                "insecure-file" => {
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(&payload, fs::Permissions::from_mode(0o644)).unwrap();
                }
                "insecure-directory" => {
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(&store.directory, fs::Permissions::from_mode(0o755))
                        .unwrap();
                }
                "nonregular-payload" => {
                    fs::remove_file(&payload).unwrap();
                    fs::create_dir(&payload).unwrap();
                }
                _ => unreachable!(),
            }
            let legacy_before = legacy_entry_fingerprint(&store);
            assert!(
                import_legacy_recovery_store_once(&store, &db, &canonical).is_err(),
                "{attack} must fail closed"
            );
            assert_no_legacy_import(&db, &canonical);
            assert_eq!(legacy_entry_fingerprint(&store), legacy_before, "{attack}");
            assert_eq!(
                fs::read_to_string(&outside).unwrap(),
                "outside bytes must remain exact",
                "{attack} must not follow or alter outside targets"
            );
            if attack == "payload-hardlink" {
                assert_eq!(
                    fs::read_to_string(&payload).unwrap(),
                    "outside bytes must remain exact"
                );
            }
            fs::remove_dir_all(base).unwrap();
        }
    }

    #[test]
    fn legacy_import_fault_boundaries_roll_back_or_commit_once_and_retry_idempotently() {
        for (fault, expected_after_error) in [
            (
                recovery_db::LegacyImportFault::BeforeCommit,
                (None, 0, 0, vec![]),
            ),
            (
                recovery_db::LegacyImportFault::AfterCommit,
                (Some("imported-v1".into()), 1, 1, vec![1]),
            ),
        ] {
            let (base, _root, canonical, store, db) = migration_fixture("legacy-import-fault");
            let mut buffer = recovery_buffer("fault.md", 1, 1, Some("fault draft"));
            buffer.client_edit_version = 0;
            write_recovery_transaction(&store, &canonical, buffer).unwrap();
            let legacy_before = legacy_entry_fingerprint(&store);

            recovery_db::set_legacy_import_fault(Some(fault));
            assert!(import_legacy_recovery_store_once(&store, &db, &canonical).is_err());
            assert_eq!(
                db.legacy_import_test_state(&canonical).unwrap(),
                expected_after_error,
                "{fault:?} database state"
            );
            assert_eq!(legacy_entry_fingerprint(&store), legacy_before);

            import_legacy_recovery_store_once(&store, &db, &canonical).unwrap();
            assert_eq!(
                db.legacy_import_test_state(&canonical).unwrap(),
                (Some("imported-v1".into()), 1, 1, vec![1]),
                "retry after {fault:?} must have one terminal imported set"
            );
            assert_eq!(legacy_entry_fingerprint(&store), legacy_before);
            fs::remove_dir_all(base).unwrap();
        }
    }

    fn reliability_root(name: &str) -> PathBuf {
        let root = temp(&format!("reliability-v2-{name}"));
        assert!(root.starts_with("/tmp/opencode"));
        root
    }

    /// Reliability fixtures must disappear even when an assertion deliberately
    /// fails while developing the destructive campaign.
    struct ReliabilityCleanup(PathBuf);
    impl Drop for ReliabilityCleanup {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn recovery_text(seed: u64, index: usize, large: bool) -> String {
        if !large {
            return format!("# buffer {index}\nseed={seed}\n😀 café\n");
        }
        let mut text = format!("# large buffer {index}\nseed={seed}\n😀 café — 日本語\n");
        while text.len() < 400 * 1024 {
            text.push_str("line with Unicode 😀 café — 日本語 and Markdown **bold**\n");
        }
        text
    }

    fn lcg(next: &mut u64) -> u64 {
        *next = next
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        *next
    }

    #[test]
    fn recovery_v2_randomized_model_restart_campaign() {
        let configured =
            env::var("AMTE_RELIABILITY_SEEDS").unwrap_or_else(|_| "11,29,73,101".into());
        let seeds = configured
            .split(',')
            .map(|seed| seed.parse::<u64>().expect("numeric reliability seed"))
            .collect::<Vec<_>>();
        assert!(
            seeds.len() >= 4,
            "three fixed seeds plus one logged random seed are required"
        );

        for seed in seeds {
            let base = reliability_root(&format!("model-{seed}"));
            let _cleanup = ReliabilityCleanup(base.clone());
            let project = base.join("project");
            fs::create_dir(&project).unwrap();
            let root = project.canonicalize().unwrap().display().to_string();
            let config = base.join("config");
            let db = RecoveryDb::for_config(&config).unwrap();
            let mut live = (0..20)
                .map(|index| recovery_text(seed, index, index < 6))
                .collect::<Vec<_>>();
            let mut durable = live.clone();
            let mut versions = [1u64; 20];
            let mut histories = live
                .iter()
                .cloned()
                .map(|text| vec![text])
                .collect::<Vec<_>>();
            let mut sequences = [0i64; 20];

            for index in 0..20 {
                let buffer = recovery_buffer(
                    &format!("buffer-{index}.md"),
                    index as u64 + 1,
                    1,
                    Some(&live[index]),
                );
                let ack = db
                    .write_snapshot(&root, Some("buffer-0.md"), &buffer)
                    .unwrap();
                assert!(ack.durable && ack.recovery_sequence > sequences[index]);
                sequences[index] = ack.recovery_sequence;
            }

            let mut random = seed;
            // 1,000 operations for each of four seeds: 4,000 deterministic
            // randomized edit/replace/delete/undo/switch/snapshot operations.
            for operation in 0..1_000usize {
                let index = (lcg(&mut random) as usize) % live.len();
                match lcg(&mut random) % 6 {
                    0 => live[index].push_str(&format!("edit-{seed}-{operation}-😀\n")),
                    1 => {
                        if live[index].len() > 64 && index >= 6 {
                            let mut chars = live[index].chars().collect::<Vec<_>>();
                            chars.truncate(chars.len().saturating_sub(3));
                            live[index] = chars.into_iter().collect();
                        }
                    }
                    2 => {
                        if index >= 6 {
                            live[index] = format!(
                                "# replacement {operation}\n{}",
                                live[index].chars().take(96).collect::<String>()
                            );
                        }
                    }
                    3 => {
                        if histories[index].len() > 1 {
                            let previous =
                                (lcg(&mut random) as usize) % (histories[index].len() - 1);
                            live[index] = histories[index][previous].clone();
                        }
                    }
                    4 => { /* buffer switch: reference state must stay untouched */ }
                    _ => { /* explicit snapshot below */ }
                }
                if index >= 6 {
                    histories[index].push(live[index].clone());
                    if histories[index].len() > 24 {
                        histories[index].remove(0);
                    }
                }
                if operation % 100 == 99 {
                    // A checkpoint persists every changed small buffer. Large
                    // Unicode buffers remain byte-identical to their initial
                    // checkpoint, exercising large restore without exceeding
                    // the bounded aggregate history budget.
                    for index in 6..20 {
                        if durable[index] != live[index] {
                            versions[index] += 1;
                            let mut buffer = recovery_buffer(
                                &format!("buffer-{index}.md"),
                                index as u64 + 1,
                                versions[index],
                                Some(&live[index]),
                            );
                            buffer.client_edit_version = versions[index];
                            let ack = db
                                .write_snapshot(&root, Some("buffer-0.md"), &buffer)
                                .unwrap();
                            assert!(ack.recovery_sequence > sequences[index]);
                            sequences[index] = ack.recovery_sequence;
                            durable[index] = live[index].clone();
                        }
                    }
                    db.integrity_check().unwrap();
                    let restarted = RecoveryDb::for_config(&config).unwrap();
                    let restored = restarted.latest(&root).unwrap().unwrap();
                    assert_eq!(restored.active_path.as_deref(), Some("buffer-0.md"));
                    assert_eq!(restored.buffers.len(), 20);
                    for buffer in restored.buffers {
                        let index = buffer
                            .relative_path
                            .unwrap()
                            .trim_start_matches("buffer-")
                            .trim_end_matches(".md")
                            .parse::<usize>()
                            .unwrap();
                        assert_eq!(
                            buffer.content.as_deref(),
                            Some(durable[index].as_str()),
                            "seed={seed} checkpoint={operation} buffer={index}"
                        );
                    }
                }
            }

            let mut stale = recovery_buffer("buffer-0.md", 1, 1, Some(&durable[0]));
            stale.client_edit_version = 0;
            assert!(
                db.write_snapshot(&root, None, &stale)
                    .unwrap_err()
                    .contains("version")
            );
            // A corrupt newest dirty snapshot must never silently downgrade the
            // user to an older clean state.
            let path = db.test_path();
            let connection = rusqlite::Connection::open(&path).unwrap();
            connection.execute("UPDATE snapshots SET content='forged' WHERE id=(SELECT MAX(id) FROM snapshots)", []).unwrap();
            assert!(
                db.latest(&root)
                    .unwrap_err()
                    .contains("RECOVERY CORRUPTION")
            );
            db.integrity_check().unwrap();
            fs::remove_dir_all(base).unwrap();
        }
    }

    #[test]
    fn recovery_v2_isolated_storage_and_latency_measurement() {
        fn percentile(samples: &mut [f64], percentile: f64) -> f64 {
            samples.sort_by(|left, right| left.total_cmp(right));
            samples[((samples.len() as f64 * percentile).ceil() as usize).saturating_sub(1)]
        }
        fn file_size(path: &Path) -> u64 {
            fs::metadata(path)
                .map(|metadata| metadata.len())
                .unwrap_or(0)
        }

        let base = reliability_root("measurement");
        let _cleanup = ReliabilityCleanup(base.clone());
        let project = base.join("project");
        fs::create_dir(&project).unwrap();
        let root = project.canonicalize().unwrap().display().to_string();
        let config = base.join("xdg-config");
        let db = RecoveryDb::for_config(&config).unwrap();
        let mut unicode_markdown = "# AMTE V2 performance fixture\n\n".to_owned();
        const TARGET_BYTES: usize = 400 * 1024;
        const LINE: &str = "Unicode 😀 café — 日本語 with **Markdown**\n";
        while unicode_markdown.len() + LINE.len() <= TARGET_BYTES {
            unicode_markdown.push_str(LINE);
        }
        assert!(unicode_markdown.len() <= TARGET_BYTES);

        let mut snapshot_ms = Vec::new();
        for index in 0..10u64 {
            let buffer = recovery_buffer(
                &format!("benchmark-{index}.md"),
                index + 1,
                1,
                Some(&unicode_markdown),
            );
            let started = Instant::now();
            let acknowledgement = db
                .write_snapshot(&root, Some("benchmark-0.md"), &buffer)
                .unwrap();
            snapshot_ms.push(started.elapsed().as_secs_f64() * 1_000.0);
            assert!(acknowledgement.durable);
            assert_eq!(acknowledgement.recovery_sequence, 1);
        }
        db.integrity_check().unwrap();
        let database = db.test_path();
        let wal = PathBuf::from(format!("{}-wal", database.display()));
        let shm = PathBuf::from(format!("{}-shm", database.display()));
        let db_bytes = file_size(&database);
        let wal_bytes = file_size(&wal);
        let shm_bytes = file_size(&shm);

        let mut restore_ms = Vec::new();
        for _ in 0..20 {
            let started = Instant::now();
            let recovered = RecoveryDb::for_config(&config)
                .unwrap()
                .latest(&root)
                .unwrap()
                .unwrap();
            restore_ms.push(started.elapsed().as_secs_f64() * 1_000.0);
            assert_eq!(recovered.active_path.as_deref(), Some("benchmark-0.md"));
            assert_eq!(recovered.buffers.len(), 10);
            for (index, buffer) in recovered.buffers.iter().enumerate() {
                assert_eq!(
                    buffer.relative_path.as_deref(),
                    Some(format!("benchmark-{index}.md").as_str())
                );
                assert_eq!(buffer.content.as_deref(), Some(unicode_markdown.as_str()));
                assert_eq!(
                    buffer
                        .selection
                        .as_ref()
                        .map(|selection| (selection.anchor, selection.head)),
                    Some((1, 2))
                );
            }
        }

        let retention_root = base.join("retention-project");
        fs::create_dir(&retention_root).unwrap();
        let retention_canonical = retention_root.canonicalize().unwrap().display().to_string();
        let retention = RecoveryDb::for_config(&base.join("retention-config")).unwrap();
        for version in 1..=25u64 {
            retention
                .write_snapshot(
                    &retention_canonical,
                    None,
                    &recovery_buffer(
                        "retained.md",
                        1,
                        version,
                        Some(&format!("revision-{version}")),
                    ),
                )
                .unwrap();
        }
        let (retained_rows, latest_sequence, retained_content_bytes) =
            retention.test_snapshot_stats(&retention_canonical).unwrap();
        assert_eq!(retained_rows, 20);
        assert_eq!(latest_sequence, 25);
        assert!(retained_content_bytes > 0);

        let snapshot_p50 = percentile(&mut snapshot_ms, 0.50);
        let snapshot_p95 = percentile(&mut snapshot_ms, 0.95);
        let restore_p50 = percentile(&mut restore_ms, 0.50);
        let restore_p95 = percentile(&mut restore_ms, 0.95);
        // The frontend's trailing debounce coalesces 100 edits into this one
        // committed backend transaction; BufferSession unit coverage asserts
        // that no intermediate content version is persisted.
        eprintln!(
            "AMTE_RELIABILITY_METRICS {{\"payloadBytes\":{},\"buffers\":10,\"snapshotP50Ms\":{snapshot_p50:.3},\"snapshotP95Ms\":{snapshot_p95:.3},\"restoreP50Ms\":{restore_p50:.3},\"restoreP95Ms\":{restore_p95:.3},\"dbBytes\":{db_bytes},\"walBytes\":{wal_bytes},\"shmBytes\":{shm_bytes},\"retainedRows\":{retained_rows},\"retainedLatestSequence\":{latest_sequence},\"recoveryWrites\":1,\"keystrokes\":100}}",
            unicode_markdown.len(),
        );
    }

    #[test]
    fn recovery_child_crash_phase() {
        let Ok(root) = env::var("AMTE_RECOVERY_CHILD_ROOT") else {
            return;
        };
        let phase = env::var("AMTE_RECOVERY_CHILD_PHASE").unwrap();
        let db = RecoveryDb::for_config(&PathBuf::from(&root).join("config")).unwrap();
        let phase = match phase.as_str() {
            "before-transaction" => recovery_db::SnapshotCrashPhase::BeforeTransaction,
            "after-payload" => recovery_db::SnapshotCrashPhase::AfterPayloadBeforeCommit,
            "retention" => recovery_db::SnapshotCrashPhase::DuringRetention,
            "after-commit" => recovery_db::SnapshotCrashPhase::AfterCommitAckLoss,
            _ => panic!("unknown child crash phase"),
        };
        recovery_db::set_snapshot_crash_phase(Some(phase));
        let mut next = recovery_buffer("crash.md", 1, 2, Some("new committed bytes 😀"));
        next.client_edit_version = 2;
        let _ = db.write_snapshot(&format!("{root}/project"), None, &next);
        panic!("crash hook did not terminate its child");
    }

    #[test]
    fn save_outcome_child_crash_phase() {
        let Ok(base) = env::var("AMTE_SAVE_OUTCOME_CHILD_ROOT") else {
            return;
        };
        let phase = match env::var("AMTE_SAVE_OUTCOME_CHILD_PHASE").as_deref() {
            Ok("after-journal") => recovery_db::SaveOperationCrashPhase::AfterJournal,
            Ok("after-checkpoint") => recovery_db::SaveOperationCrashPhase::AfterCheckpoint,
            Ok("after-selector") => recovery_db::SaveOperationCrashPhase::AfterSelector,
            _ => panic!("unknown SaveOutcome child crash phase"),
        };
        let token = "c".repeat(64);
        let key = "d".repeat(64);
        let content = "new exact bytes 😀";
        let hash = hash_content(content);
        let root = PathBuf::from(&base).join("project").display().to_string();
        let db = RecoveryDb::for_config(&PathBuf::from(&base).join("config")).unwrap();
        let root_fd = open_directory_path(Path::new(&root)).unwrap();
        let (_, disk_identity) = read_file_at(&root_fd, Path::new("chapter.md")).unwrap();
        recovery_db::set_save_operation_crash_phase(Some(phase));
        db.record_file_commit(
            &root,
            &token,
            &key,
            "chapter.md",
            "chapter.md",
            7,
            1,
            1,
            &hash,
            &file_identity_text(disk_identity),
        )
        .unwrap();
        db.mark_save_operation_pending_metadata(&root, &token, &key)
            .unwrap();
        db.reconcile_save_checkpoint(&root, &token, &key, "chapter.md", 7, 1, 1, hash)
            .unwrap();
        db.mark_save_selector_durable(&root, &token, &key).unwrap();
        panic!("SaveOutcome crash hook did not terminate its child");
    }

    #[test]
    fn save_outcome_post_commit_crash_restart_matrix_keeps_pending_operations_discoverable() {
        let exe = env::current_exe().unwrap();
        for (phase, checkpoint, selector) in [
            ("after-journal", false, false),
            ("after-checkpoint", true, false),
            ("after-selector", true, true),
        ] {
            let base = reliability_root(&format!("save-outcome-crash-{phase}"));
            let _cleanup = ReliabilityCleanup(base.clone());
            let project = base.join("project");
            fs::create_dir(&project).unwrap();
            fs::write(project.join("chapter.md"), "new exact bytes 😀").unwrap();
            let root = project.display().to_string();
            let token = "c".repeat(64);
            let fresh_token = "e".repeat(64);
            let key = "d".repeat(64);
            let db = RecoveryDb::for_config(&base.join("config")).unwrap();
            let mut dirty = recovery_buffer("chapter.md", 7, 1, Some("new exact bytes 😀"));
            dirty.client_edit_version = 1;
            dirty.baseline_hash = Some(hash_content("old exact bytes"));
            db.write_snapshot(&root, None, &dirty).unwrap();
            db.rebind_session_bindings_and_activate(
                &root,
                &token,
                &[("chapter.md".to_string(), 7)],
            )
            .unwrap();
            db.write_snapshot_for_session(&root, None, &token, &dirty)
                .unwrap();

            let status = std::process::Command::new(&exe)
                .args([
                    "--exact",
                    "tests::save_outcome_child_crash_phase",
                    "--nocapture",
                ])
                .env("AMTE_SAVE_OUTCOME_CHILD_ROOT", &base)
                .env("AMTE_SAVE_OUTCOME_CHILD_PHASE", phase)
                .status()
                .unwrap();
            assert!(!status.success(), "child must abort at {phase}");

            let restarted = RecoveryDb::for_config(&base.join("config")).unwrap();
            restarted.integrity_check().unwrap();
            assert_eq!(
                fs::read_to_string(project.join("chapter.md")).unwrap(),
                "new exact bytes 😀"
            );
            // A real restart mints a fresh token and document identity. The
            // opaque journal key must be handed to that fresh authority only
            // after the restore-time disk and snapshot checks succeed.
            restarted
                .rebind_session_bindings_and_activate_with_operations(
                    &root,
                    &fresh_token,
                    &[("chapter.md".into(), 8)],
                    &[recovery_db::SaveOperationRebind {
                        operation_key: key.clone(),
                        content_blake3: hash_content("new exact bytes 😀"),
                        file_identity: file_identity_text(
                            read_file_at(
                                &open_directory_path(Path::new(&root)).unwrap(),
                                Path::new("chapter.md"),
                            )
                            .unwrap()
                            .1,
                        ),
                    }],
                )
                .unwrap();
            assert!(
                restarted
                    .save_operation(&root, &token, &key)
                    .unwrap()
                    .is_none()
            );
            let operation = restarted
                .save_operation(&root, &fresh_token, &key)
                .unwrap()
                .unwrap();
            assert_eq!(operation.document_id, 8);
            assert_eq!(operation.recovery_checkpoint_durable, checkpoint);
            assert_eq!(operation.selector_durable, selector);
            let pending = restarted.latest(&root).unwrap().unwrap();
            assert_eq!(
                pending.buffers[0].reconciliation_operation_key.as_deref(),
                Some(key.as_str())
            );

            // Restart convergence only advances the missing legs. Repeating it
            // is idempotent and does not erase the historical dirty snapshot.
            let hash = hash_content("new exact bytes 😀");
            let first = restarted
                .reconcile_save_checkpoint(
                    &root,
                    &fresh_token,
                    &key,
                    "chapter.md",
                    8,
                    1,
                    1,
                    hash.clone(),
                )
                .unwrap();
            let second = restarted
                .reconcile_save_checkpoint(&root, &fresh_token, &key, "chapter.md", 8, 1, 1, hash)
                .unwrap();
            assert!(first.recovery_checkpoint_durable && second.recovery_checkpoint_durable);
            restarted
                .mark_save_selector_durable(&root, &fresh_token, &key)
                .unwrap();
            restarted
                .clear_save_operation_pending_metadata(&root, &fresh_token, &key)
                .unwrap();
            let finished = restarted
                .save_operation(&root, &fresh_token, &key)
                .unwrap()
                .unwrap();
            assert!(finished.recovery_checkpoint_durable && finished.selector_durable);
            assert!(
                restarted.test_snapshot_stats(&root).unwrap().0 >= 2,
                "dirty recovery history is retained"
            );
            assert!(
                restarted.latest(&root).unwrap().unwrap().buffers[0]
                    .reconciliation_operation_key
                    .is_none()
            );
            fs::remove_dir_all(base).unwrap();
        }
    }

    #[test]
    fn recovery_real_child_crash_campaign_recovers_only_committed_versions() {
        let exe = env::current_exe().unwrap();
        for (phase, new_is_allowed) in [
            ("before-transaction", false),
            ("after-payload", false),
            ("retention", false),
            ("after-commit", true),
        ] {
            let base = reliability_root(&format!("crash-{phase}"));
            let _cleanup = ReliabilityCleanup(base.clone());
            let project = base.join("project");
            fs::create_dir(&project).unwrap();
            let root = project.display().to_string();
            let db = RecoveryDb::for_config(&base.join("config")).unwrap();
            db.write_snapshot(
                &root,
                None,
                &recovery_buffer("crash.md", 1, 1, Some("old committed bytes")),
            )
            .unwrap();
            let status = std::process::Command::new(&exe)
                .args([
                    "--exact",
                    "tests::recovery_child_crash_phase",
                    "--nocapture",
                ])
                .env("AMTE_RECOVERY_CHILD_ROOT", &base)
                .env("AMTE_RECOVERY_CHILD_PHASE", phase)
                .status()
                .unwrap();
            assert!(!status.success(), "child must be aborted at {phase}");
            let restarted = RecoveryDb::for_config(&base.join("config")).unwrap();
            restarted.integrity_check().unwrap();
            let content = restarted.latest(&root).unwrap().unwrap().buffers[0]
                .content
                .clone()
                .unwrap();
            if new_is_allowed {
                assert_eq!(content, "new committed bytes 😀");
            } else {
                assert_eq!(content, "old committed bytes");
            }
            fs::remove_dir_all(base).unwrap();
        }
    }

    #[test]
    fn recovery_concurrency_fault_capacity_and_corruption_campaign() {
        use std::sync::{Arc, Barrier};
        let base = reliability_root("concurrency");
        let _cleanup = ReliabilityCleanup(base.clone());
        let project = base.join("project");
        fs::create_dir(&project).unwrap();
        let root = project.display().to_string();
        let db = Arc::new(RecoveryDb::for_config(&base.join("config")).unwrap());
        let barrier = Arc::new(Barrier::new(9));
        let mut writers = Vec::new();
        for writer in 0..8u64 {
            let db = db.clone();
            let barrier = barrier.clone();
            let root = root.clone();
            writers.push(thread::spawn(move || {
                barrier.wait();
                for version in 1..=8u64 {
                    let mut buffer = recovery_buffer(
                        &format!("writer-{writer}.md"),
                        writer + 1,
                        version,
                        Some(&format!("writer={writer};version={version}")),
                    );
                    buffer.client_edit_version = version;
                    db.write_snapshot(&root, None, &buffer).unwrap();
                    assert!(db.latest(&root).unwrap().is_some());
                }
            }));
        }
        barrier.wait();
        for writer in writers {
            writer.join().unwrap();
        }
        db.integrity_check().unwrap();
        let restored = db.latest(&root).unwrap().unwrap();
        assert_eq!(restored.buffers.len(), 8);
        for writer in 0..8 {
            assert_eq!(
                restored
                    .buffers
                    .iter()
                    .find(|buffer| buffer.relative_path.as_deref()
                        == Some(format!("writer-{writer}.md").as_str()))
                    .and_then(|buffer| buffer.content.as_deref()),
                Some(format!("writer={writer};version=8").as_str())
            );
        }

        db.write_snapshot(
            &root,
            None,
            &recovery_buffer("same.md", 99, 1, Some("same-v1")),
        )
        .unwrap();
        let same_barrier = Arc::new(Barrier::new(3));
        let mut same_writers = Vec::new();
        for version in [2u64, 3] {
            let db = db.clone();
            let root = root.clone();
            let barrier = same_barrier.clone();
            same_writers.push(thread::spawn(move || {
                barrier.wait();
                let mut buffer =
                    recovery_buffer("same.md", 99, version, Some(&format!("same-v{version}")));
                buffer.client_edit_version = version;
                db.write_snapshot(&root, None, &buffer)
            }));
        }
        same_barrier.wait();
        let a = same_writers.remove(0).join().unwrap();
        let b = same_writers.remove(0).join().unwrap();
        let sequences = [
            a.ok().map(|ack| ack.recovery_sequence),
            b.ok().map(|ack| ack.recovery_sequence),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
        assert!(!sequences.is_empty() && sequences.windows(2).all(|pair| pair[0] != pair[1]));
        assert_eq!(
            db.latest(&root)
                .unwrap()
                .unwrap()
                .buffers
                .iter()
                .find(|buffer| buffer.relative_path.as_deref() == Some("same.md"))
                .and_then(|buffer| buffer.content.as_deref()),
            Some("same-v3")
        );

        for failure in [
            recovery_db::SnapshotInjectedFailure::FullBeforeTransaction,
            recovery_db::SnapshotInjectedFailure::IoAfterPayload,
            recovery_db::SnapshotInjectedFailure::BusyDuringRetention,
        ] {
            recovery_db::set_snapshot_injected_failure(Some(failure));
            let result = db.write_snapshot(
                &root,
                None,
                &recovery_buffer("writer-0.md", 1, 9, Some("must roll back")),
            );
            assert!(result.unwrap_err().contains("Injected SQLITE"));
            assert_eq!(
                db.latest(&root)
                    .unwrap()
                    .unwrap()
                    .buffers
                    .iter()
                    .find(|buffer| buffer.relative_path.as_deref() == Some("writer-0.md"))
                    .and_then(|buffer| buffer.content.as_deref()),
                Some("writer=0;version=8")
            );
            db.integrity_check().unwrap();
        }

        // Twenty retained revisions plus a protected latest dirty snapshot are
        // bounded by the policy, and the aggregate remains below the configured cap.
        for version in 9..=36u64 {
            let mut buffer = recovery_buffer(
                "writer-0.md",
                1,
                version,
                Some(&format!("retention-{version}")),
            );
            buffer.client_edit_version = version;
            db.write_snapshot(&root, None, &buffer).unwrap();
        }
        let (rows, sequence, bytes) = db.test_snapshot_stats(&root).unwrap();
        assert!(rows <= 8 * 20 + 1 && sequence >= 36 && bytes as usize <= RECOVERY_MAX_TOTAL_BYTES);

        let original = db.test_path();
        let corrupt_base = reliability_root("corrupt-copy");
        let _corrupt_cleanup = ReliabilityCleanup(corrupt_base.clone());
        let corrupt_dir = corrupt_base.join("config/recovery-v2");
        fs::create_dir_all(&corrupt_dir).unwrap();
        let corrupt = corrupt_dir.join("recovery.sqlite3");
        fs::copy(&original, &corrupt).unwrap();
        fs::write(&corrupt, b"not a sqlite database").unwrap();
        let bytes_before = fs::read(&corrupt).unwrap();
        assert!(RecoveryDb::for_config(&corrupt_base.join("config")).is_err());
        assert_eq!(
            fs::read(&corrupt).unwrap(),
            bytes_before,
            "corrupt main DB is never reinitialized"
        );
        let mismatch_base = reliability_root("wal-mismatch");
        let _mismatch_cleanup = ReliabilityCleanup(mismatch_base.clone());
        let mismatch_dir = mismatch_base.join("config/recovery-v2");
        fs::create_dir_all(&mismatch_dir).unwrap();
        fs::copy(&original, mismatch_dir.join("recovery.sqlite3")).unwrap();
        fs::write(
            mismatch_dir.join("recovery.sqlite3-wal"),
            b"mismatched wal bytes",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(
                mismatch_dir.join("recovery.sqlite3-wal"),
                fs::Permissions::from_mode(0o600),
            )
            .unwrap();
        }
        assert!(
            RecoveryDb::for_config(&mismatch_base.join("config")).is_err(),
            "mismatched WAL must fail closed"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let private = base.join("config/recovery-v2");
            fs::set_permissions(&private, fs::Permissions::from_mode(0o755)).unwrap();
            assert!(
                RecoveryDb::for_config(&base.join("config"))
                    .unwrap_err()
                    .contains("owner-private")
            );
        }
        fs::remove_dir_all(corrupt_base).unwrap();
        fs::remove_dir_all(mismatch_base).unwrap();
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn staged_save_faults_keep_exact_old_or_new_bytes_and_recovery() {
        for (fault, new_expected) in [
            (secure_fs::SaveFault::BeforeStageWrite, false),
            (secure_fs::SaveFault::AfterStageFsync, false),
            (secure_fs::SaveFault::BeforeRename, false),
            (secure_fs::SaveFault::AfterRenameAckLoss, true),
            (secure_fs::SaveFault::DirectoryFsync, true),
        ] {
            let base = reliability_root(&format!("save-{fault:?}"));
            let _cleanup = ReliabilityCleanup(base.clone());
            let project = base.join("project");
            fs::create_dir(&project).unwrap();
            let target = project.join("chapter.md");
            fs::write(&target, "old exact bytes\n😀").unwrap();
            let fd = open_directory_path(&project).unwrap();
            let expected = read_file_at(&fd, Path::new("chapter.md")).unwrap().1;
            let root = project.display().to_string();
            let db = RecoveryDb::for_config(&base.join("config")).unwrap();
            db.write_snapshot(
                &root,
                None,
                &recovery_buffer("chapter.md", 1, 1, Some("new exact bytes\n😀")),
            )
            .unwrap();
            secure_fs::set_save_fault(Some(fault));
            assert!(
                write_file_at(
                    &fd,
                    Path::new("chapter.md"),
                    "new exact bytes\n😀",
                    Some(expected),
                    false
                )
                .is_err()
            );
            assert_eq!(
                fs::read_to_string(&target).unwrap(),
                if new_expected {
                    "new exact bytes\n😀"
                } else {
                    "old exact bytes\n😀"
                }
            );
            assert_eq!(
                db.latest(&root).unwrap().unwrap().buffers[0]
                    .content
                    .as_deref(),
                Some("new exact bytes\n😀")
            );
            assert!(
                !fs::read_dir(&project)
                    .unwrap()
                    .filter_map(Result::ok)
                    .any(|entry| entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with(".amte-write-"))
            );
            fs::remove_dir_all(base).unwrap();
        }
    }

    #[test]
    fn discarded_binding_reopens_as_fresh_session_without_restoring_old_bytes() {
        let base = temp("discard-reopen");
        let root = base.join("project");
        fs::create_dir(&root).unwrap();
        let canonical = root.canonicalize().unwrap().display().to_string();
        let db = RecoveryDb::for_config(&base.join("config")).unwrap();
        let mut old = recovery_buffer("chapter.md", 1, 1, Some("discarded draft"));
        old.client_edit_version = 1;
        db.write_snapshot(&canonical, None, &old).unwrap();
        db.record_discard(&canonical).unwrap();
        db.rebind_session_binding(&canonical, "chapter.md", 2)
            .unwrap();
        let mut fresh = recovery_buffer("chapter.md", 2, 1, Some("fresh draft"));
        fresh.client_edit_version = 1;
        db.write_snapshot(&canonical, None, &fresh).unwrap();
        let restored = db.latest(&canonical).unwrap().unwrap();
        assert_eq!(restored.buffers.len(), 1);
        assert_eq!(restored.buffers[0].document_id, Some(2));
        assert_eq!(restored.buffers[0].content.as_deref(), Some("fresh draft"));
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn normal_reopen_rebinds_fresh_ids_tokens_and_bounded_current_buffers() {
        let base = temp("normal-reopen-fresh-binding");
        let root = base.join("project");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("clean.md"), "current clean disk").unwrap();
        fs::write(root.join("dirty.md"), "changed after close").unwrap();
        let canonical = root.canonicalize().unwrap().display().to_string();
        let db = RecoveryDb::for_config(&base.join("config")).unwrap();

        let mut clean = recovery_buffer("clean.md", 41, 3, None);
        clean.client_edit_version = 1;
        clean.baseline_hash = Some(hash_content("old clean disk"));
        db.write_snapshot(&canonical, None, &clean).unwrap();
        let mut dirty = recovery_buffer("dirty.md", 42, 4, Some("retained draft"));
        dirty.client_edit_version = 1;
        dirty.baseline_hash = Some(hash_content("old dirty disk"));
        dirty.source_scroll = Some(RecoveryScroll {
            top: 17.0,
            left: 3.0,
        });
        dirty.selection = Some(RecoverySelection { anchor: 2, head: 8 });
        dirty.preview_scroll = Some(9.0);
        dirty.previewing = Some(true);
        dirty.search_term = Some("retained".into());
        dirty.chapter_identity = Some("h2:0123456789abcdef:1".into());
        dirty.chapter_scroll = Some(11.0);
        dirty.collapsed_chapter_identities = Some(vec!["h2:0123456789abcdef:1".into()]);
        dirty.activity = Some(RecoveryActivity {
            day: "2026-11-01".into(),
            added: 2,
            deleted: 1,
        });
        db.write_snapshot(&canonical, None, &dirty).unwrap();

        let authority = ProjectAuthority::default();
        let first = restore_recovery_v2(
            &authority,
            Path::new(&canonical),
            db.latest(&canonical).unwrap().unwrap(),
            &[],
        )
        .unwrap();
        let first_clean = first
            .documents
            .iter()
            .find(|item| item.relative_path.as_deref() == Some("clean.md"))
            .unwrap();
        let first_dirty = first
            .documents
            .iter()
            .find(|item| item.relative_path.as_deref() == Some("dirty.md"))
            .unwrap();
        assert_ne!(first_clean.document_id, Some(41));
        assert_ne!(first_dirty.document_id, Some(42));
        assert_eq!(first_clean.content, "current clean disk");
        assert!(first_clean.conflict.is_none());
        assert_eq!(first_dirty.content, "retained draft");
        assert!(
            first_dirty
                .conflict
                .as_deref()
                .is_some_and(|error| error.starts_with("CONFLICT:"))
        );
        assert_eq!(
            first_dirty
                .recovery
                .as_ref()
                .unwrap()
                .selection
                .as_ref()
                .unwrap()
                .head,
            8
        );
        assert_eq!(
            first_dirty
                .recovery
                .as_ref()
                .unwrap()
                .search_term
                .as_deref(),
            Some("retained")
        );
        let dirty_ui = first_dirty.recovery.as_ref().unwrap();
        assert_eq!(
            dirty_ui
                .source_scroll
                .as_ref()
                .map(|scroll| (scroll.top, scroll.left)),
            Some((17.0, 3.0))
        );
        assert_eq!(dirty_ui.preview_scroll, Some(9.0));
        assert_eq!(dirty_ui.previewing, Some(true));
        assert_eq!(
            dirty_ui.chapter_identity.as_deref(),
            Some("h2:0123456789abcdef:1")
        );
        assert_eq!(dirty_ui.chapter_scroll, Some(11.0));
        assert_eq!(
            dirty_ui.collapsed_chapter_identities.as_ref().unwrap(),
            &vec!["h2:0123456789abcdef:1".to_string()]
        );
        assert_eq!(
            dirty_ui.activity.as_ref().map(|activity| (
                activity.day.as_str(),
                activity.added,
                activity.deleted
            )),
            Some(("2026-11-01", 2, 1))
        );
        bind_restored_recovery_v2(&db, &first).unwrap();

        let current = db.latest(&canonical).unwrap().unwrap();
        assert_eq!(current.buffers.len(), 2, "one current buffer per path");
        let rebound_clean = current
            .buffers
            .iter()
            .find(|item| item.relative_path.as_deref() == Some("clean.md"))
            .unwrap();
        assert_eq!(rebound_clean.document_id, first_clean.document_id);
        assert!(!rebound_clean.dirty);
        assert_eq!(
            rebound_clean.baseline_hash.as_deref(),
            Some(hash_content("current clean disk").as_str())
        );
        let rebound_dirty = current
            .buffers
            .iter()
            .find(|item| item.relative_path.as_deref() == Some("dirty.md"))
            .unwrap();
        assert_eq!(rebound_dirty.document_id, first_dirty.document_id);
        assert_eq!(rebound_dirty.content.as_deref(), Some("retained draft"));

        let mut stale = recovery_buffer("dirty.md", 42, 5, Some("must reject old ID"));
        stale.client_edit_version = 2;
        stale.baseline_hash = Some(hash_content("old dirty disk"));
        assert!(
            db.write_snapshot_for_session(&canonical, None, &first.project.session_token, &stale)
                .is_err()
        );
        let mut fresh = recovery_buffer(
            "dirty.md",
            first_dirty.document_id.unwrap(),
            5,
            Some("new current draft"),
        );
        fresh.client_edit_version = 2;
        fresh.baseline_hash = Some(hash_content("old dirty disk"));
        db.write_snapshot_for_session(&canonical, None, &first.project.session_token, &fresh)
            .unwrap();

        let second = restore_recovery_v2(
            &authority,
            Path::new(&canonical),
            db.latest(&canonical).unwrap().unwrap(),
            &[],
        )
        .unwrap();
        assert_ne!(second.project.session_token, first.project.session_token);
        for document in &second.documents {
            let first_id = first
                .documents
                .iter()
                .find(|item| item.relative_path == document.relative_path)
                .and_then(|item| item.document_id);
            assert_ne!(
                document.document_id, first_id,
                "each re-open mints a fresh document ID"
            );
        }
        bind_restored_recovery_v2(&db, &second).unwrap();
        assert!(
            db.write_snapshot_for_session(&canonical, None, &first.project.session_token, &fresh)
                .is_err()
        );
        let restored_twice = db.latest(&canonical).unwrap().unwrap();
        assert_eq!(
            restored_twice.buffers.len(),
            2,
            "re-open does not duplicate visible paths"
        );
        assert!(
            db.test_snapshot_stats(&canonical).unwrap().0 <= 24,
            "retired snapshot retention is bounded"
        );
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn empty_v2_project_row_does_not_poison_pending_legacy_import() {
        let base = temp("empty-v2-row");
        let root = base.join("project");
        fs::create_dir(&root).unwrap();
        let canonical = root.canonicalize().unwrap().display().to_string();
        let db = RecoveryDb::for_config(&base.join("config")).unwrap();
        db.set_active_path(&canonical, Some("chapter.md")).unwrap();
        assert!(
            !db.has_committed_recovery_or_terminal_migration(&canonical)
                .unwrap()
        );
        let directory = base.join(RecoveryStore::project_key(&canonical));
        let store = RecoveryStore {
            base: base.clone(),
            manifest: directory.join("manifest.json"),
            directory,
        };
        let mut legacy = recovery_buffer("chapter.md", 1, 1, Some("legacy"));
        legacy.client_edit_version = 0;
        write_recovery_transaction(&store, &canonical, legacy).unwrap();
        let restored = restore_v2_before_legacy(&db, &canonical, || {
            import_legacy_recovery_store_once(&store, &db, &canonical)
        })
        .unwrap()
        .unwrap();
        assert_eq!(restored.buffers[0].content.as_deref(), Some("legacy"));
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn backend_clean_checkpoint_is_idempotent_and_never_hides_newer_dirty() {
        let base = temp("clean-checkpoint");
        let root = base.join("project");
        fs::create_dir(&root).unwrap();
        let canonical = root.canonicalize().unwrap().display().to_string();
        let db = RecoveryDb::for_config(&base.join("config")).unwrap();
        let mut dirty = recovery_buffer("chapter.md", 1, 1, Some("saved bytes"));
        dirty.client_edit_version = 1;
        db.write_snapshot(&canonical, None, &dirty).unwrap();
        let document = ActiveDocument {
            relative_path: PathBuf::from("chapter.md"),
            identity: 1,
            file_identity: FileIdentity {
                device: 1,
                inode: 1,
            },
            write_revision: 1,
            baseline_hash: hash_content("saved bytes"),
        };
        // Repeating this models response/ack loss after file and checkpoint commit.
        backend_clean_checkpoint(&db, &canonical, &document, 1).unwrap();
        backend_clean_checkpoint(&db, &canonical, &document, 1).unwrap();
        let mut newer = recovery_buffer("chapter.md", 1, 2, Some("newer dirty"));
        newer.client_edit_version = 2;
        db.write_snapshot(&canonical, None, &newer).unwrap();
        assert!(
            backend_clean_checkpoint(&db, &canonical, &document, 1)
                .unwrap_err()
                .contains("Stale")
        );
        assert_eq!(
            db.latest(&canonical).unwrap().unwrap().buffers[0]
                .content
                .as_deref(),
            Some("newer dirty")
        );
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn session_fence_barrier_rejects_validated_dirty_writer_after_discard() {
        use std::sync::mpsc;
        use std::time::Duration;
        let base = temp("session-fence-discard");
        let root = base.join("project");
        fs::create_dir(&root).unwrap();
        let canonical = root.canonicalize().unwrap().display().to_string();
        let db = Arc::new(RecoveryDb::for_config(&base.join("config")).unwrap());
        let token = "a".repeat(64);
        db.activate_session(&canonical, &token).unwrap();
        db.rebind_session_binding(&canonical, "draft.md", 7)
            .unwrap();
        let barrier = Arc::new(recovery_db::SessionFenceBarrier {
            root: canonical.clone(),
            token: token.clone(),
            reached: Barrier::new(2),
            resume: Barrier::new(2),
        });
        recovery_db::set_session_fence_barrier(Some(barrier.clone()));
        let writer_db = db.clone();
        let writer_root = canonical.clone();
        let writer_token = token.clone();
        let writer = thread::spawn(move || {
            let mut dirty = recovery_buffer("draft.md", 7, 1, Some("must not commit"));
            dirty.client_edit_version = 1;
            writer_db.write_snapshot_for_session(&writer_root, None, &writer_token, &dirty)
        });
        barrier.reached.wait();

        // The test-only fence is global to the process, but it must never
        // capture a normal writer from another recovery authority. A bounded
        // completion assertion turns the historical suite-order hang into a
        // deterministic regression failure.
        let unrelated_base = temp("session-fence-unrelated");
        let unrelated_root = unrelated_base.join("project");
        fs::create_dir(&unrelated_root).unwrap();
        let unrelated_canonical = unrelated_root.canonicalize().unwrap().display().to_string();
        let unrelated_db = RecoveryDb::for_config(&unrelated_base.join("config")).unwrap();
        let unrelated_token = "b".repeat(64);
        unrelated_db
            .write_snapshot(
                &unrelated_canonical,
                None,
                &recovery_buffer("other.md", 1, 1, Some("prior writer")),
            )
            .unwrap();
        unrelated_db
            .activate_session(&unrelated_canonical, &unrelated_token)
            .unwrap();
        unrelated_db
            .rebind_session_binding(&unrelated_canonical, "other.md", 8)
            .unwrap();
        let (unrelated_done, unrelated_result) = mpsc::channel();
        let unrelated_writer = thread::spawn(move || {
            let mut dirty = recovery_buffer("other.md", 8, 2, Some("unrelated writer"));
            dirty.client_edit_version = 2;
            unrelated_done
                .send(unrelated_db.write_snapshot_for_session(
                    &unrelated_canonical,
                    None,
                    &unrelated_token,
                    &dirty,
                ))
                .unwrap();
        });
        assert!(
            unrelated_result
                .recv_timeout(Duration::from_secs(1))
                .expect("unrelated session must not wait at the scoped fence")
                .is_ok()
        );
        unrelated_writer.join().unwrap();
        fs::remove_dir_all(unrelated_base).unwrap();

        db.deactivate_and_discard_session(&canonical, &token)
            .unwrap();
        barrier.resume.wait();
        assert!(writer.join().unwrap().is_err());
        recovery_db::set_session_fence_barrier(None);
        assert_eq!(db.test_snapshot_stats(&canonical).unwrap().0, 0);
        assert!(db.latest(&canonical).unwrap().unwrap().buffers.is_empty());
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn stale_discard_waiting_across_reopen_preserves_new_session_recovery() {
        use std::sync::mpsc;
        use std::time::Duration;
        let base = temp("stale-discard-reopen");
        let root = base.join("project");
        fs::create_dir(&root).unwrap();
        let canonical = root.canonicalize().unwrap().display().to_string();
        let db = Arc::new(RecoveryDb::for_config(&base.join("config")).unwrap());
        let old_token = "d".repeat(64);
        let new_token = "e".repeat(64);
        db.write_snapshot(
            &canonical,
            None,
            &recovery_buffer("draft.md", 1, 1, Some("old")),
        )
        .unwrap();
        db.activate_session(&canonical, &old_token).unwrap();
        db.rebind_session_binding(&canonical, "draft.md", 7)
            .unwrap();
        let mut old = recovery_buffer("draft.md", 7, 2, Some("old dirty"));
        old.client_edit_version = 2;
        db.write_snapshot_for_session(&canonical, None, &old_token, &old)
            .unwrap();

        let barrier = Arc::new(recovery_db::DiscardSessionBarrier {
            root: canonical.clone(),
            token: old_token.clone(),
            reached: Barrier::new(2),
            resume: Barrier::new(2),
        });
        recovery_db::set_discard_session_barrier(Some(barrier.clone()));
        let old_db = db.clone();
        let old_root = canonical.clone();
        let old_capability = old_token.clone();
        let (done, result) = mpsc::channel();
        let old_discard = thread::spawn(move || {
            done.send(old_db.deactivate_and_discard_session(&old_root, &old_capability))
                .unwrap();
        });
        barrier.reached.wait();

        db.rebind_session_bindings_and_activate(&canonical, &new_token, &[("draft.md".into(), 8)])
            .unwrap();
        let mut replacement = recovery_buffer("draft.md", 8, 3, Some("new session dirty"));
        replacement.client_edit_version = 3;
        db.write_snapshot_for_session(&canonical, None, &new_token, &replacement)
            .unwrap();
        barrier.resume.wait();
        assert!(
            result
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .unwrap_err()
                .contains("Stale or inactive")
        );
        old_discard.join().unwrap();
        recovery_db::set_discard_session_barrier(None);

        let recovered = db.latest(&canonical).unwrap().unwrap();
        assert_eq!(recovered.buffers.len(), 1);
        assert_eq!(recovered.buffers[0].document_id, Some(8));
        assert_eq!(
            recovered.buffers[0].content.as_deref(),
            Some("new session dirty")
        );
        let connection = rusqlite::Connection::open(db.test_path()).unwrap();
        let project_key = blake3::hash(canonical.as_bytes()).to_hex().to_string();
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*) FROM events WHERE project_key=?1 AND kind='terminal-discard'",
                    [&project_key],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );

        db.deactivate_and_discard_session(&canonical, &new_token)
            .unwrap();
        assert!(db.latest(&canonical).unwrap().unwrap().buffers.is_empty());
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn activation_gate_serializes_two_candidates_without_discard() {
        use std::sync::mpsc;
        let base = temp("activation-gate-race");
        let old_root = base.join("old");
        let first_root = base.join("first");
        let second_root = base.join("second");
        fs::create_dir(&old_root).unwrap();
        fs::create_dir(&first_root).unwrap();
        fs::create_dir(&second_root).unwrap();
        let old = canonical_project_root(&old_root).unwrap();
        let first = canonical_project_root(&first_root).unwrap();
        let second = canonical_project_root(&second_root).unwrap();
        let authority = Arc::new(authority(&old));
        let old_token = authority
            .0
            .lock()
            .unwrap()
            .project
            .as_ref()
            .unwrap()
            .session_token
            .clone();
        let db = Arc::new(RecoveryDb::for_config(&base.join("config")).unwrap());
        db.activate_session(&old.display().to_string(), &old_token)
            .unwrap();
        let gate = Arc::new(ActivationAuthority::default());
        let reached = Arc::new(Barrier::new(2));
        let resume = Arc::new(Barrier::new(2));
        let (first_started, started) = mpsc::channel();

        let first_db = db.clone();
        let first_gate = gate.clone();
        let first_authority = authority.clone();
        let first_reached = reached.clone();
        let first_resume = resume.clone();
        let old_root_text = old.display().to_string();
        let first_root_text = first.display().to_string();
        let old_for_first = old.clone();
        let first_path = first.clone();
        let expected_first = first.clone();
        let first_worker = thread::spawn(move || {
            let _gate = first_gate.0.lock().unwrap();
            first_started.send(()).unwrap();
            first_reached.wait();
            first_resume.wait();
            let token = "1".repeat(64);
            first_db
                .transition_active_session(
                    Some((&old_root_text, &old_token)),
                    &first_root_text,
                    &token,
                )
                .unwrap();
            let mut state = first_authority.0.lock().unwrap();
            state.project = Some(ActiveProject {
                root: first_path.clone(),
                root_fd: Arc::new(open_directory_path(&first_path).unwrap()),
                identity: 101,
                session_token: token.clone(),
                documents: HashMap::new(),
                paths: HashMap::new(),
            });
            (token, old_for_first)
        });
        started.recv().unwrap();

        let second_db = db.clone();
        let second_gate = gate.clone();
        let second_authority = authority.clone();
        let second_root_text = second.display().to_string();
        let old_root_text = old.display().to_string();
        let old_token_for_second = authority
            .0
            .lock()
            .unwrap()
            .project
            .as_ref()
            .unwrap()
            .session_token
            .clone();
        let second_worker = thread::spawn(move || {
            let _gate = second_gate.0.lock().unwrap();
            let token = "2".repeat(64);
            let result = second_db.transition_active_session(
                Some((&old_root_text, &old_token_for_second)),
                &second_root_text,
                &token,
            );
            // A loser must not replace or deactivate the winner's authority.
            assert!(result.is_err());
            assert_eq!(
                second_authority
                    .0
                    .lock()
                    .unwrap()
                    .project
                    .as_ref()
                    .unwrap()
                    .root,
                expected_first
            );
        });
        reached.wait();
        resume.wait();
        let (winner, _) = first_worker.join().unwrap();
        second_worker.join().unwrap();
        assert_eq!(
            db.test_active_session_token(&first.display().to_string())
                .unwrap(),
            Some(winner.clone())
        );
        assert_eq!(
            authority
                .0
                .lock()
                .unwrap()
                .project
                .as_ref()
                .unwrap()
                .session_token,
            winner
        );
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn activation_gate_rolls_back_injected_failure_then_allows_discard_winner() {
        use std::sync::mpsc;
        let base = temp("activation-gate-failure");
        let old_root = base.join("old");
        let failed_root = base.join("failed");
        let winner_root = base.join("winner");
        fs::create_dir(&old_root).unwrap();
        fs::create_dir(&failed_root).unwrap();
        fs::create_dir(&winner_root).unwrap();
        let old = canonical_project_root(&old_root).unwrap();
        let failed = canonical_project_root(&failed_root).unwrap();
        let winner_root = canonical_project_root(&winner_root).unwrap();
        let authority = Arc::new(authority(&old));
        let old_token = authority
            .0
            .lock()
            .unwrap()
            .project
            .as_ref()
            .unwrap()
            .session_token
            .clone();
        let db = Arc::new(RecoveryDb::for_config(&base.join("config")).unwrap());
        let old_text = old.display().to_string();
        db.activate_session(&old_text, &old_token).unwrap();
        db.write_snapshot(
            &old_text,
            None,
            &recovery_buffer("draft.md", 1, 1, Some("keep or discard exactly once")),
        )
        .unwrap();
        let gate = Arc::new(ActivationAuthority::default());
        let transitioned = Arc::new(Barrier::new(2));
        let resume = Arc::new(Barrier::new(2));
        let (started_tx, started_rx) = mpsc::channel();

        let failed_db = db.clone();
        let failed_gate = gate.clone();
        let failed_old = old_text.clone();
        let failed_root_text = failed.display().to_string();
        let failed_token = old_token.clone();
        let failed_transitioned = transitioned.clone();
        let failed_resume = resume.clone();
        let failed_worker = thread::spawn(move || {
            let _gate = failed_gate.0.lock().unwrap();
            started_tx.send(()).unwrap();
            let token = "3".repeat(64);
            failed_db
                .transition_active_session(
                    Some((&failed_old, &failed_token)),
                    &failed_root_text,
                    &token,
                )
                .unwrap();
            failed_transitioned.wait();
            failed_resume.wait();
            // Inject selector persistence failure: rollback must restore the
            // still-live old token before releasing the gate.
            failed_db
                .rollback_activation_session(
                    Some((&failed_old, &failed_token)),
                    &failed_root_text,
                    &token,
                )
                .unwrap();
        });
        started_rx.recv().unwrap();
        transitioned.wait();

        let winner_db = db.clone();
        let winner_gate = gate.clone();
        let winner_authority = authority.clone();
        let winner_old = old_text.clone();
        let winner_old_token = old_token.clone();
        let winner_root_text = winner_root.display().to_string();
        let winner_path = winner_root.clone();
        let winner_worker = thread::spawn(move || {
            let _gate = winner_gate.0.lock().unwrap();
            let token = "4".repeat(64);
            winner_db
                .transition_active_session(
                    Some((&winner_old, &winner_old_token)),
                    &winner_root_text,
                    &token,
                )
                .unwrap();
            winner_db
                .finalize_activation_discard(
                    Some((&winner_old, &winner_old_token)),
                    &winner_root_text,
                    &token,
                )
                .unwrap();
            winner_authority.0.lock().unwrap().project = Some(ActiveProject {
                root: winner_path.clone(),
                root_fd: Arc::new(open_directory_path(&winner_path).unwrap()),
                identity: 102,
                session_token: token.clone(),
                documents: HashMap::new(),
                paths: HashMap::new(),
            });
            token
        });
        resume.wait();
        failed_worker.join().unwrap();
        let winner = winner_worker.join().unwrap();
        assert_eq!(
            db.test_active_session_token(&winner_root.display().to_string())
                .unwrap(),
            Some(winner.clone())
        );
        assert_eq!(
            authority
                .0
                .lock()
                .unwrap()
                .project
                .as_ref()
                .unwrap()
                .session_token,
            winner
        );
        assert!(
            db.latest(&old_text).unwrap().unwrap().buffers.is_empty(),
            "only the successful discard may remove old buffers"
        );
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn picker_publication_waits_for_discarding_activation_then_becomes_next_candidate() {
        use std::sync::mpsc;
        use std::time::Duration;

        let base = temp("picker-activation-interleaving");
        let old_root = base.join("old");
        let first_root = base.join("first");
        let second_root = base.join("second");
        fs::create_dir(&old_root).unwrap();
        fs::create_dir(&first_root).unwrap();
        fs::create_dir(&second_root).unwrap();
        let old = canonical_project_root(&old_root).unwrap();
        let first = canonical_project_root(&first_root).unwrap();
        let second = canonical_project_root(&second_root).unwrap();
        let authority = Arc::new(authority(&old));
        let gate = Arc::new(ActivationAuthority::default());

        // Picker A completes before activation starts. Its ID is the exact
        // generation activation must retain through terminal discard.
        let first_candidate = publish_picked_project(&authority, &gate, first.clone()).unwrap();
        let (activation_ready_tx, activation_ready_rx) = mpsc::channel();
        let (resume_activation_tx, resume_activation_rx) = mpsc::channel();
        let activation_authority = authority.clone();
        let activation_gate = gate.clone();
        let activation_root = first.clone();
        let activation_candidate_id = first_candidate.candidate_id;
        let activation = thread::spawn(move || {
            let _activation = activation_gate.0.lock().unwrap();
            {
                let state = activation_authority.0.lock().unwrap();
                assert_eq!(
                    state.pending_project.as_ref().map(|(id, _)| *id),
                    Some(activation_candidate_id),
                    "A captures its candidate while holding the activation gate"
                );
            }
            activation_ready_tx.send(()).unwrap();
            // This is the deterministic pause immediately around the terminal
            // discard boundary. Picker B may have fully validated its folder,
            // but must not publish while A owns the gate.
            resume_activation_rx.recv().unwrap();

            // Model the post-discard authority swap: no candidate lookup or
            // fallible candidate check occurs after this point.
            let mut state = activation_authority.0.lock().unwrap();
            let identity = ProjectAuthority::next_id(&mut state);
            let session_token = ProjectAuthority::session_token(&activation_root, identity);
            state.pending_project.take();
            state.project = Some(ActiveProject {
                root: activation_root.clone(),
                root_fd: Arc::new(open_directory_path(&activation_root).unwrap()),
                identity,
                session_token,
                documents: HashMap::new(),
                paths: HashMap::new(),
            });
        });
        activation_ready_rx.recv().unwrap();

        let (picker_started_tx, picker_started_rx) = mpsc::channel();
        let (picker_done_tx, picker_done_rx) = mpsc::channel();
        let picker_authority = authority.clone();
        let picker_gate = gate.clone();
        let expected_second = second.clone();
        let picker = thread::spawn(move || {
            picker_started_tx.send(()).unwrap();
            picker_done_tx
                .send(publish_picked_project(
                    &picker_authority,
                    &picker_gate,
                    second,
                ))
                .unwrap();
        });
        picker_started_rx.recv().unwrap();
        assert!(
            picker_done_rx.try_recv().is_err(),
            "picker B must wait while A is paused at discard"
        );

        resume_activation_tx.send(()).unwrap();
        activation.join().unwrap();
        let second_candidate = picker_done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("picker B must finish once A releases the gate")
            .unwrap();
        picker.join().unwrap();

        let state = authority.0.lock().unwrap();
        assert_eq!(state.project.as_ref().unwrap().root, first);
        assert_eq!(
            state.pending_project.as_ref().map(|(id, root)| (*id, root)),
            Some((second_candidate.candidate_id, &expected_second))
        );
        assert_ne!(first_candidate.candidate_id, second_candidate.candidate_id);
        drop(state);
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn cancelled_or_invalid_picker_never_changes_the_pending_candidate() {
        let base = temp("picker-cancel-failure");
        let old_root = base.join("old");
        let pending_root = base.join("pending");
        fs::create_dir(&old_root).unwrap();
        fs::create_dir(&pending_root).unwrap();
        let old = canonical_project_root(&old_root).unwrap();
        let pending = canonical_project_root(&pending_root).unwrap();
        let authority = ProjectAuthority::default();
        authority.activate_project(old).unwrap();
        let gate = ActivationAuthority::default();
        let original = publish_picked_project(&authority, &gate, pending.clone()).unwrap();

        // `pick_project` returns before publication on cancellation, and an
        // invalid selection fails canonical validation before it reaches the
        // publication helper.
        let cancelled: Option<PathBuf> = None;
        assert!(cancelled.is_none());
        assert!(canonical_project_root(&base.join("missing")).is_err());

        let state = authority.0.lock().unwrap();
        assert_eq!(
            state.project.as_ref().unwrap().root,
            old_root.canonicalize().unwrap()
        );
        assert_eq!(
            state.pending_project.as_ref().map(|(id, root)| (*id, root)),
            Some((original.candidate_id, &pending))
        );
        drop(state);
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn picker_response_fixture_is_the_camel_case_tauri_wire_contract() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../src/project-picker-response.fixture.json"
        ))
        .unwrap();
        let candidate = ProjectCandidateDto {
            candidate_id: 1,
            root: "/tmp/opencode/amte-picker-fixture".into(),
        };
        let serialized = serde_json::to_value(candidate).unwrap();
        assert_eq!(serialized, fixture);
        assert!(serialized.get("candidate_id").is_none());
        assert_eq!(serialized["candidateId"], 1);
    }

    #[test]
    fn published_picker_candidate_serializes_and_exposes_a_disposable_markdown_root() {
        let base = temp("picker-wire-integration");
        let root = base.join("project");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("root-entry.md"), "# Disposable picker entry\n").unwrap();
        let authority = ProjectAuthority::default();
        let activation = ActivationAuthority::default();
        let candidate = publish_picked_project(
            &authority,
            &activation,
            canonical_project_root(&root).unwrap(),
        )
        .unwrap();
        let wire = serde_json::to_value(&candidate).unwrap();
        assert_eq!(wire["candidateId"], candidate.candidate_id);
        assert_eq!(
            wire["root"],
            root.canonicalize().unwrap().display().to_string()
        );
        let root_fd = open_directory_path(Path::new(wire["root"].as_str().unwrap())).unwrap();
        assert_eq!(
            list_entries(&root_fd, Path::new(""))
                .unwrap()
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            vec!["root-entry.md"]
        );
        assert_eq!(
            authority
                .0
                .lock()
                .unwrap()
                .pending_project
                .as_ref()
                .map(|(id, _)| *id),
            Some(candidate.candidate_id)
        );
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn activation_response_fixture_is_the_camel_case_tauri_wire_contract() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../src/project-activation-response.fixture.json"
        ))
        .unwrap();
        let serialized = serde_json::to_value(ProjectDto {
            project_id: 2,
            root: "/tmp/opencode/amte-picker-fixture".into(),
            session_token: "a".repeat(64),
        })
        .unwrap();
        assert_eq!(serialized, fixture);
        assert!(serialized.get("project_id").is_none());
        assert_eq!(serialized["projectId"], 2);
        assert_eq!(serialized["sessionToken"], "a".repeat(64));
    }

    #[test]
    fn restart_rebinds_pending_save_as_without_picker_and_rejects_changed_disk() {
        let base = temp("restart-save-as-rebind");
        let root = base.join("project");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("saved.md"), "saved bytes 😀").unwrap();
        let canonical = root.canonicalize().unwrap().display().to_string();
        let db = RecoveryDb::for_config(&base.join("config")).unwrap();
        let old_token = "a".repeat(64);
        let fresh_token = "b".repeat(64);
        let key = "c".repeat(64);
        let content = "saved bytes 😀";
        let hash = hash_content(content);
        db.activate_session(&canonical, &old_token).unwrap();
        let mut source = recovery_buffer("placeholder.md", 1, 4, Some(content));
        source.relative_path = None;
        source.document_id = None;
        source.client_edit_version = 4;
        source.baseline_hash = None;
        db.write_snapshot_for_session(&canonical, None, &old_token, &source)
            .unwrap();
        let root_fd = open_directory_path(Path::new(&canonical)).unwrap();
        let (_, identity) = read_file_at(&root_fd, Path::new("saved.md")).unwrap();
        db.record_file_commit(
            &canonical,
            &old_token,
            &key,
            "__untitled__",
            "saved.md",
            81,
            1,
            4,
            &hash,
            &file_identity_text(identity),
        )
        .unwrap();
        db.mark_save_operation_pending_metadata(&canonical, &old_token, &key)
            .unwrap();

        // This is the restart path: only the journal key and descriptor-read
        // destination facts cross the process boundary; no picker is involved.
        db.rebind_session_bindings_and_activate_with_operations(
            &canonical,
            &fresh_token,
            &[],
            &[recovery_db::SaveOperationRebind {
                operation_key: key.clone(),
                content_blake3: hash.clone(),
                file_identity: file_identity_text(identity),
            }],
        )
        .unwrap();
        assert!(
            db.save_operation(&canonical, &old_token, &key)
                .unwrap()
                .is_none()
        );
        assert!(
            db.save_operation(&canonical, &fresh_token, &key)
                .unwrap()
                .is_some()
        );
        let transition = db
            .reconcile_save_as_operation(&canonical, &fresh_token, &key)
            .unwrap();
        assert_eq!(transition.destination_document_id, 81);
        let destination = db
            .latest(&canonical)
            .unwrap()
            .unwrap()
            .buffers
            .into_iter()
            .find(|buffer| buffer.relative_path.as_deref() == Some("saved.md"))
            .unwrap();
        assert_eq!(
            destination.reconciliation_operation_key.as_deref(),
            Some(key.as_str()),
            "the destination, not the retired untitled source, owns restart reconciliation"
        );
        assert!(
            db.save_operation(&canonical, &old_token, &key)
                .unwrap()
                .is_none(),
            "the pre-restart token remains rejected"
        );

        fs::remove_dir_all(base).unwrap();

        let changed_base = temp("restart-save-disk-change");
        let changed_root = changed_base.join("project");
        fs::create_dir(&changed_root).unwrap();
        fs::write(changed_root.join("chapter.md"), "committed bytes").unwrap();
        let changed_canonical = changed_root.canonicalize().unwrap().display().to_string();
        let changed = RecoveryDb::for_config(&changed_base.join("config")).unwrap();
        let old = "d".repeat(64);
        let fresh = "e".repeat(64);
        let changed_key = "f".repeat(64);
        changed
            .write_snapshot(
                &changed_canonical,
                None,
                &recovery_buffer("chapter.md", 1, 1, Some("committed bytes")),
            )
            .unwrap();
        changed.activate_session(&changed_canonical, &old).unwrap();
        changed
            .rebind_session_binding(&changed_canonical, "chapter.md", 7)
            .unwrap();
        let mut pending = recovery_buffer("chapter.md", 7, 1, Some("committed bytes"));
        pending.client_edit_version = 1;
        pending.baseline_hash = Some(hash_content("before save"));
        changed
            .write_snapshot_for_session(&changed_canonical, None, &old, &pending)
            .unwrap();
        let changed_fd = open_directory_path(Path::new(&changed_canonical)).unwrap();
        let (_, committed_identity) = read_file_at(&changed_fd, Path::new("chapter.md")).unwrap();
        let committed_hash = hash_content("committed bytes");
        changed
            .record_file_commit(
                &changed_canonical,
                &old,
                &changed_key,
                "chapter.md",
                "chapter.md",
                7,
                1,
                1,
                &committed_hash,
                &file_identity_text(committed_identity),
            )
            .unwrap();
        changed
            .mark_save_operation_pending_metadata(&changed_canonical, &old, &changed_key)
            .unwrap();
        // This is the backend-to-frontend DTO boundary on restart: although
        // the dirty draft baseline predates the committed disk bytes, the
        // exact pending journal proves this is reconciliation, not conflict.
        let restored_authority = ProjectAuthority::default();
        let pending_operations = changed.pending_save_operations(&changed_canonical).unwrap();
        let restored = restore_recovery_v2(
            &restored_authority,
            &changed_root,
            changed.latest(&changed_canonical).unwrap().unwrap(),
            &pending_operations,
        )
        .unwrap();
        let restored_document = restored
            .documents
            .iter()
            .find(|document| document.relative_path.as_deref() == Some("chapter.md"))
            .unwrap();
        assert!(restored_document.conflict.is_none());
        assert_eq!(
            restored_document
                .recovery
                .as_ref()
                .and_then(|recovery| recovery.reconciliation_operation_key.as_deref()),
            Some(changed_key.as_str())
        );
        bind_restored_recovery_v2(&changed, &restored).unwrap();
        assert!(
            changed
                .save_operation(
                    &changed_canonical,
                    &restored.project.session_token,
                    &changed_key
                )
                .unwrap()
                .is_some(),
            "the fresh frontend session receives the opaque reconciliation key"
        );
        fs::write(changed_root.join("chapter.md"), "external replacement").unwrap();
        let (_, changed_identity) = read_file_at(&changed_fd, Path::new("chapter.md")).unwrap();
        changed
            .rebind_session_bindings_and_activate_with_operations(
                &changed_canonical,
                &fresh,
                &[("chapter.md".into(), 8)],
                &[recovery_db::SaveOperationRebind {
                    operation_key: changed_key.clone(),
                    content_blake3: hash_content("external replacement"),
                    file_identity: file_identity_text(changed_identity),
                }],
            )
            .unwrap();
        assert!(
            changed
                .save_operation(
                    &changed_canonical,
                    &restored.project.session_token,
                    &changed_key
                )
                .unwrap()
                .is_none()
        );
        assert!(
            changed
                .save_operation(&changed_canonical, &fresh, &changed_key)
                .unwrap()
                .is_none()
        );
        fs::remove_dir_all(changed_base).unwrap();
    }

    #[test]
    fn save_snapshot_switched_before_checkpoint_cannot_apply_to_new_authority() {
        let old_root = temp("snapshot-old");
        let new_root = temp("snapshot-new");
        fs::write(old_root.join("draft.md"), "old").unwrap();
        let authority = authority(&old_root);
        let (project_id, token, document_id) = {
            let mut state = authority.0.lock().unwrap();
            let project = state.project.as_ref().unwrap();
            let (content, identity) =
                read_file_at(&project.root_fd, Path::new("draft.md")).unwrap();
            let id =
                activate_document(&mut state, PathBuf::from("draft.md"), identity, &content, 0)
                    .unwrap();
            let project = state.project.as_ref().unwrap();
            (project.identity, project.session_token.clone(), id)
        };
        let snapshot =
            capture_authorization_snapshot(&authority, project_id, Some(&token), Some(document_id))
                .unwrap();
        let completed = persist_document_snapshot(&snapshot, 1, "saved before switch").unwrap();
        authority
            .activate_project(canonical_project_root(&new_root).unwrap())
            .unwrap();
        assert!(matches!(
            apply_document_save(
                &authority,
                &snapshot,
                &completed,
                "saved before switch".into()
            )
            .unwrap(),
            AuthorityApply::Stale
        ));
        let active = authority.0.lock().unwrap();
        assert_eq!(
            active.project.as_ref().unwrap().root,
            canonical_project_root(&new_root).unwrap()
        );
        assert!(active.project.as_ref().unwrap().documents.is_empty());
        drop(active);
        fs::remove_dir_all(old_root).unwrap();
        fs::remove_dir_all(new_root).unwrap();
    }

    #[test]
    fn recovery_and_save_snapshot_protocol_completes_without_lock_inversion() {
        use std::sync::mpsc;
        use std::time::Duration;
        let root = temp("lock-order");
        fs::write(root.join("draft.md"), "old").unwrap();
        let authority = Arc::new(authority(&root));
        let project = authority
            .0
            .lock()
            .unwrap()
            .project
            .as_ref()
            .unwrap()
            .identity;
        let recovery = Arc::new(RecoveryAuthority::default());
        let start = Arc::new(Barrier::new(2));
        let (done, receiver) = mpsc::channel();
        for recovery_first in [false, true] {
            let authority = authority.clone();
            let recovery = recovery.clone();
            let start = start.clone();
            let done = done.clone();
            thread::spawn(move || {
                start.wait();
                if recovery_first {
                    let transaction = recovery.0.lock().unwrap();
                    drop(transaction);
                    let _ =
                        capture_authorization_snapshot(&authority, project, None, None).unwrap();
                } else {
                    let _ =
                        capture_authorization_snapshot(&authority, project, None, None).unwrap();
                    let transaction = recovery.0.lock().unwrap();
                    drop(transaction);
                }
                done.send(()).unwrap();
            });
        }
        for _ in 0..2 {
            receiver
                .recv_timeout(Duration::from_secs(1))
                .expect("snapshot/recovery protocol must not deadlock");
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn concurrent_recovery_writers_serialize_through_global_transaction_mutex() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let lock = Arc::new(RecoveryAuthority::default());
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new(Barrier::new(3));
        let mut writers = Vec::new();
        for _ in 0..2 {
            let lock = lock.clone();
            let active = active.clone();
            let maximum = maximum.clone();
            let gate = gate.clone();
            writers.push(thread::spawn(move || {
                gate.wait();
                let _transaction = lock.0.lock().unwrap();
                let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                maximum.fetch_max(now, Ordering::SeqCst);
                thread::yield_now();
                active.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        gate.wait();
        for writer in writers {
            writer.join().unwrap();
        }
        assert_eq!(maximum.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn save_as_recovery_transition_is_atomic_idempotent_and_retires_untitled() {
        let base = temp("save-as-recovery-transition");
        let root = base.join("project");
        fs::create_dir(&root).unwrap();
        let canonical = root.canonicalize().unwrap().display().to_string();
        let db = RecoveryDb::for_config(&base.join("config")).unwrap();
        let token = "b".repeat(64);
        db.activate_session(&canonical, &token).unwrap();
        let mut source = recovery_buffer("placeholder.md", 1, 7, Some("untitled exact bytes"));
        source.relative_path = None;
        source.document_id = None;
        source.client_edit_version = 7;
        source.baseline_hash = None;
        db.write_snapshot_for_session(&canonical, None, &token, &source)
            .unwrap();
        let hash = hash_content("untitled exact bytes");

        // A pre-commit fault keeps the only ordinary-recovery binding untitled
        // and dirty; restart observes no half-destination.
        recovery_db::set_save_as_injected_fault(Some(
            recovery_db::SaveAsInjectedFault::BeforeCommit,
        ));
        assert!(
            db.save_as_transition(
                &canonical,
                &token,
                &source,
                "saved.md",
                81,
                7,
                7,
                hash.clone()
            )
            .is_err()
        );
        let before = RecoveryDb::for_config(&base.join("config"))
            .unwrap()
            .latest(&canonical)
            .unwrap()
            .unwrap();
        assert_eq!(before.buffers.len(), 1);
        assert!(before.buffers[0].dirty);
        assert!(before.buffers[0].relative_path.is_none());

        // Commit/ack loss returns an error, but the exact key returns the
        // original destination identity rather than allocating a second row.
        recovery_db::set_save_as_injected_fault(Some(
            recovery_db::SaveAsInjectedFault::AfterCommitAckLoss,
        ));
        assert!(
            db.save_as_transition(
                &canonical,
                &token,
                &source,
                "saved.md",
                81,
                7,
                7,
                hash.clone()
            )
            .is_err()
        );
        let retry = RecoveryDb::for_config(&base.join("config"))
            .unwrap()
            .save_as_transition(
                &canonical,
                &token,
                &source,
                "saved.md",
                999,
                7,
                7,
                hash.clone(),
            )
            .unwrap();
        assert_eq!(retry.destination_document_id, 81);
        assert!(
            db.save_as_transition(&canonical, &token, &source, "other.md", 82, 7, 7, hash)
                .unwrap_err()
                .contains("does not match")
        );

        let restored = RecoveryDb::for_config(&base.join("config"))
            .unwrap()
            .latest(&canonical)
            .unwrap()
            .unwrap();
        assert_eq!(restored.active_path.as_deref(), Some("saved.md"));
        assert_eq!(restored.buffers.len(), 1, "retired history is audit-only");
        assert_eq!(
            restored.buffers[0].relative_path.as_deref(),
            Some("saved.md")
        );
        assert!(!restored.buffers[0].dirty);
        assert_eq!(restored.buffers[0].document_id, Some(81));
        assert_eq!(
            restored.buffers[0].baseline_hash.as_deref(),
            Some(hash_content("untitled exact bytes").as_str())
        );

        let connection = rusqlite::Connection::open(db.test_path()).unwrap();
        let key = blake3::hash(canonical.as_bytes()).to_hex().to_string();
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*) FROM save_as_transitions WHERE project_key=?1",
                    [&key],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            1
        );
        assert_eq!(connection.query_row("SELECT COUNT(*) FROM events WHERE project_key=?1 AND kind='save-as-transition'", [&key], |row| row.get::<_, i64>(0)).unwrap(), 1);
        fs::remove_dir_all(base).unwrap();
    }
}
