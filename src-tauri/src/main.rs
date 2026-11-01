use std::{
    collections::HashMap,
    ffi::CString,
    fs::{self, File},
    io::{Read, Write},
    os::fd::FromRawFd,
    path::{Component, Path, PathBuf},
    sync::Mutex,
};

use pulldown_cmark::{Options, Parser, html};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_dialog::DialogExt;

const LAST_PROJECT_FILE: &str = "last-project-v1";
const LEGACY_LAST_DOCUMENT_FILE: &str = "last-document-path";
const IGNORED_NAMES: &[&str] = &[".git", "node_modules", "target", "dist"];

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProjectDto {
    project_id: u64,
    root: String,
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
    root_fd: File,
    identity: u64,
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
/// Lock ordering is ProjectAuthority (brief identity lookup only), then this
/// recovery lock. Project locks are always released before recovery I/O.
#[derive(Default)]
struct RecoveryAuthority(Mutex<()>);

impl ProjectAuthority {
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
        state.project = Some(ActiveProject {
            root: root.clone(),
            root_fd,
            identity,
            documents: HashMap::new(),
            paths: HashMap::new(),
        });
        Ok(ProjectDto {
            project_id: identity,
            root: root.display().to_string(),
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

#[tauri::command]
fn write_recovery_buffer(
    app: AppHandle,
    authority: State<'_, ProjectAuthority>,
    recovery: State<'_, RecoveryAuthority>,
    project_id: u64,
    buffer: RecoveryBuffer,
) -> Result<RecoveryWriteResult, String> {
    let root = recovery_project_root(&authority, project_id)?;
    let _transaction = recovery
        .0
        .lock()
        .map_err(|_| "Recovery transaction state is unavailable".to_string())?;
    let store = RecoveryStore::for_root(&app, &root)?;
    write_recovery_transaction(&store, &root, buffer)
}

#[tauri::command]
fn write_recovery_manifest(
    app: AppHandle,
    authority: State<'_, ProjectAuthority>,
    recovery: State<'_, RecoveryAuthority>,
    project_id: u64,
    active_path: Option<String>,
) -> Result<(), String> {
    let root = recovery_project_root(&authority, project_id)?;
    if active_path
        .as_deref()
        .is_some_and(|path| validate_relative(path, true).is_err())
    {
        return Err("Invalid active recovery path".into());
    }
    let _transaction = recovery
        .0
        .lock()
        .map_err(|_| "Recovery transaction state is unavailable".to_string())?;
    let store = RecoveryStore::for_root(&app, &root)?;
    let mut manifest = store.read_bound(&root)?.unwrap_or(RecoveryManifest {
        version: RECOVERY_VERSION,
        root: root.clone(),
        project_key: RecoveryStore::project_key(&root),
        active_path: None,
        buffers: Vec::new(),
    });
    if manifest.root != root {
        return Err("Recovery root does not match the active project".into());
    }
    manifest.active_path = active_path;
    store.write(&manifest)
}

#[tauri::command]
fn delete_recovery_project(
    app: AppHandle,
    authority: State<'_, ProjectAuthority>,
    recovery: State<'_, RecoveryAuthority>,
    project_id: u64,
) -> Result<(), String> {
    let root = recovery_project_root(&authority, project_id)?;
    let _transaction = recovery
        .0
        .lock()
        .map_err(|_| "Recovery transaction state is unavailable".to_string())?;
    RecoveryStore::for_root(&app, &root)?.clear()
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
        if let Err(error) = commit_staged(&parent, &temporary, target, create_new) {
            cleanup_staging(&parent, &temporary);
            return Err(error);
        }
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

// Recovery snapshots are bound to the backend-issued document identity. Keep
// that identity when rebuilding the in-memory registry so the next recovery
// sequence remains bound to the same manifest entry rather than being rejected
// as a different document after restart.
fn activate_recovered_document(
    state: &mut ProjectState,
    relative_path: PathBuf,
    file_identity: FileIdentity,
    content: &str,
    write_revision: u64,
    document_id: u64,
) -> Result<u64, String> {
    if document_id == 0 {
        return Err("Recovery document identity is invalid".into());
    }
    if let Some(existing) = state
        .project
        .as_ref()
        .and_then(|project| project.paths.get(&relative_path).copied())
    {
        return (existing == document_id)
            .then_some(existing)
            .ok_or_else(|| "Recovery path is already bound to another document".to_string());
    }
    let project = state
        .project
        .as_ref()
        .ok_or_else(|| "No project is open".to_string())?;
    if project.documents.contains_key(&document_id) {
        return Err("Recovery document identity is already bound to another path".into());
    }
    // Keep subsequent opaque IDs ahead of the recovered binding. A max-value
    // legacy ID can still safely wrap to a currently unused nonzero value.
    state.next_identity = state.next_identity.max(document_id);
    let project = state.project.as_mut().expect("project was checked above");
    project.paths.insert(relative_path.clone(), document_id);
    project.documents.insert(
        document_id,
        ActiveDocument {
            relative_path,
            identity: document_id,
            file_identity,
            write_revision,
            baseline_hash: hash_content(content),
        },
    );
    Ok(document_id)
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
    if revision <= document.write_revision {
        return Err("A newer document revision was already saved".into());
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
) -> Result<Option<ProjectCandidateDto>, String> {
    let Some(selected) = app.dialog().file().blocking_pick_folder() else {
        return Ok(None);
    };
    let path = selected
        .into_path()
        .map_err(|e| format!("Could not use selected project: {e}"))?;
    let root = canonical_project_root(&path)?;
    let mut state = authority
        .0
        .lock()
        .map_err(|_| "Project state is unavailable".to_string())?;
    let candidate_id = ProjectAuthority::next_id(&mut state);
    state.pending_project = Some((candidate_id, root.clone()));
    Ok(Some(ProjectCandidateDto {
        candidate_id,
        root: root.display().to_string(),
    }))
}

#[tauri::command]
fn activate_picked_project(
    app: AppHandle,
    authority: State<'_, ProjectAuthority>,
    recovery: State<'_, RecoveryAuthority>,
    candidate_id: u64,
    discard_previous_recovery: bool,
) -> Result<ProjectDto, String> {
    let root = {
        let mut state = authority
            .0
            .lock()
            .map_err(|_| "Project state is unavailable".to_string())?;
        let (id, root) = state
            .pending_project
            .take()
            .ok_or_else(|| "No project selection is pending".to_string())?;
        if id != candidate_id {
            return Err("The selected project is stale".into());
        }
        root
    };
    let previous = authority
        .0
        .lock()
        .map_err(|_| "Project state is unavailable".to_string())?
        .project
        .as_ref()
        .map(|project| project.root.display().to_string());
    let project = authority.activate_project(root.clone())?;
    ProjectStore::for_app(&app)?.write(&root, None)?;
    // Discard is irreversible only after the candidate became the active
    // project and its persisted project state was accepted.
    if discard_previous_recovery {
        if let Some(previous) = previous {
            let _transaction = recovery
                .0
                .lock()
                .map_err(|_| "Recovery transaction state is unavailable".to_string())?;
            RecoveryStore::for_root(&app, &previous)?.clear()?;
        }
    }
    Ok(project)
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
    authority: State<'_, ProjectAuthority>,
    project_id: u64,
    relative_path: String,
) -> Result<DocumentDto, String> {
    let relative = validate_relative(&relative_path, true)?;
    let mut state = authority
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
    let (content, file_identity) = read_file_at(&project.root_fd, &relative)?;
    let id = activate_document(&mut state, relative.clone(), file_identity, &content, 0)?;
    let project = state.project.as_ref().expect("project remains active");
    let document = project.documents.get(&id).expect("registered document");
    Ok(document_dto(project, document, content))
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

fn save_active(
    app: &AppHandle,
    authority: &ProjectAuthority,
    project_id: u64,
    document_id: u64,
    revision: u64,
    content: &str,
) -> Result<DocumentDto, String> {
    let mut state = authority
        .0
        .lock()
        .map_err(|_| "Project state is unavailable".to_string())?;
    let project = state
        .project
        .as_ref()
        .ok_or_else(|| "No project is open".to_string())?;
    if project.identity != project_id {
        return Err("The project changed before save".into());
    }
    write_document_revision(
        state.project.as_mut().expect("project"),
        document_id,
        revision,
        content,
    )?;
    let project = state.project.as_ref().expect("project");
    let document = project
        .documents
        .get(&document_id)
        .expect("registered document");
    ProjectStore::for_app(app)?.write(&project.root, Some(&document.relative_path))?;
    Ok(document_dto(project, document, content.into()))
}

#[tauri::command]
fn save_document(
    app: AppHandle,
    authority: State<'_, ProjectAuthority>,
    project_id: u64,
    document_id: u64,
    revision: u64,
    content: String,
) -> Result<DocumentDto, String> {
    save_active(
        &app,
        &authority,
        project_id,
        document_id,
        revision,
        &content,
    )
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

#[tauri::command]
async fn save_document_as(
    app: AppHandle,
    authority: State<'_, ProjectAuthority>,
    project_id: u64,
    revision: u64,
    content: String,
) -> Result<Option<DocumentDto>, String> {
    let root = {
        let state = authority
            .0
            .lock()
            .map_err(|_| "Project state is unavailable".to_string())?;
        let project = state
            .project
            .as_ref()
            .ok_or_else(|| "No project is open".to_string())?;
        if project.identity != project_id {
            return Err("The project changed before Save As".into());
        }
        project.root.clone()
    };
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
    let mut state = authority
        .0
        .lock()
        .map_err(|_| "Project state is unavailable".to_string())?;
    let project = state
        .project
        .as_ref()
        .ok_or_else(|| "No project is open".to_string())?;
    if project.identity != project_id
        || project.root != root
        || project.paths.contains_key(&relative)
    {
        return Err("Save As target is already authorized or project changed".into());
    }
    let file_identity = write_file_at(&project.root_fd, &relative, &content, None, true)?;
    let id = activate_document(
        &mut state,
        relative.clone(),
        file_identity,
        &content,
        revision,
    )?;
    let project = state.project.as_ref().expect("project");
    let document = project.documents.get(&id).expect("document");
    ProjectStore::for_app(&app)?.write(&project.root, Some(&relative))?;
    Ok(Some(document_dto(project, document, content)))
}

#[tauri::command]
fn autosave_document(
    authority: State<'_, ProjectAuthority>,
    project_id: u64,
    document_id: u64,
    revision: u64,
    content: String,
) -> Result<DocumentDto, String> {
    let mut state = authority
        .0
        .lock()
        .map_err(|_| "Project state is unavailable".to_string())?;
    let project = state
        .project
        .as_ref()
        .ok_or_else(|| "No project is open".to_string())?;
    if project.identity != project_id {
        return Err("The project changed before autosave".into());
    }
    write_document_revision(
        state.project.as_mut().expect("project"),
        document_id,
        revision,
        &content,
    )?;
    let project = state.project.as_ref().expect("project");
    let document = project.documents.get(&document_id).expect("document");
    Ok(document_dto(project, document, content))
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
            let mut state = authority
                .0
                .lock()
                .map_err(|_| "Project state is unavailable".to_string())?;
            let active_project = state
                .project
                .as_ref()
                .ok_or_else(|| "No project is open".to_string())?;
            if let Ok((content, file_identity)) = read_file_at(&active_project.root_fd, &relative) {
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

#[tauri::command]
fn restore_last_project(
    app: AppHandle,
    authority: State<'_, ProjectAuthority>,
    recovery: State<'_, RecoveryAuthority>,
) -> Result<Option<RestoreDto>, String> {
    let store = ProjectStore::for_app(&app)?;
    if let Some(stored) = store.read() {
        // Recovery is keyed by the canonical root recorded for the last active
        // project; a different project can never overwrite or be restored as it.
        if let Ok(root) = canonical_project_root(Path::new(&stored.root)) {
            let canonical = root.display().to_string();
            let _transaction = recovery
                .0
                .lock()
                .map_err(|_| "Recovery transaction state is unavailable".to_string())?;
            let recovery_store = RecoveryStore::for_root(&app, &canonical)?;
            if let Ok(Some(manifest)) = recovery_store.read_bound(&canonical) {
                // Corrupt, stale, or misbound recovery never activates another root;
                // disk-backed stored-project restore below is the safe fallback.
                if let Ok(Some(restored)) =
                    restore_recovery(&recovery_store, &authority, manifest, &root)
                {
                    return Ok(Some(restored));
                }
            }
        }
        let restored = restore_from_state(&app, &authority, stored)?;
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
    let root = recovery_project_root(&authority, project_id)?;
    let _transaction = recovery
        .0
        .lock()
        .map_err(|_| "Recovery transaction state is unavailable".to_string())?;
    terminal_discard(
        || {
            RecoveryStore::for_root(&app, &root)?.clear()?;
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
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::{SystemTime, UNIX_EPOCH};
    fn temp(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
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
        let outside = temp("outside").join("sentinel.md");
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
        assert_eq!(clean.document_id, Some(11));
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
        write_recovery_transaction(&store, &root_text, next).unwrap();
        let updated = store.read_bound(&root_text).unwrap().unwrap();
        let metadata = updated
            .buffers
            .iter()
            .find(|entry| entry.relative_path.as_deref() == Some("a.md"))
            .unwrap();
        assert_eq!(metadata.sequence, 12);
        assert_eq!(
            metadata.baseline_hash.as_deref(),
            Some(current_baseline.as_str())
        );

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
}
