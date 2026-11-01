//! Recovery V2's only runtime writer.  The v1 JSON store remains read-only
//! migration input in `main.rs`; this module never removes or edits it.
use std::{
    ffi::{CString, OsStr},
    fs, io,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

#[cfg(unix)]
use std::os::{
    fd::{AsRawFd, FromRawFd},
    unix::ffi::OsStrExt,
};

#[cfg(test)]
use std::cell::Cell;
#[cfg(test)]
use std::sync::{Arc, Barrier, Mutex, OnceLock};

use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::{
    RECOVERY_MAX_BUFFERS, RECOVERY_MAX_PAYLOAD_BYTES, RECOVERY_MAX_TOTAL_BYTES, RecoveryBuffer,
    valid_blake3_hex, validate_recovery_write_buffer,
};

const SCHEMA_VERSION: i64 = 6;
const RETAIN_PER_BUFFER: i64 = 20;
// Retired bindings are audit records, not a second live recovery set. Keep a
// bounded project-wide window of their payload rows; the retired binding rows
// themselves continue to identify the old authority without being eligible for
// normal restore or writes.
const RETAIN_RETIRED_SNAPSHOTS_PER_PROJECT: i64 = 20;
const MAX_UI_BYTES: usize = 64 * 1024;

/// These hooks make the two ambiguous import boundaries reproducible without
/// changing production behaviour. They are thread-local because Rust unit tests
/// run concurrently and an injected failure must affect only its own fixture.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LegacyImportFault {
    BeforeCommit,
    AfterCommit,
}

/// Process-local crash boundaries for the destructive reliability harness.
/// These are compiled out of production builds.  The child harness calls
/// `abort`, rather than returning an error, so SQLite/WAL recovery is exercised
/// exactly as it is after a killed application.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SnapshotCrashPhase {
    BeforeTransaction,
    AfterPayloadBeforeCommit,
    DuringRetention,
    AfterCommitAckLoss,
}

/// Crash immediately after each durable SaveOutcome acknowledgement boundary.
/// These hooks are deliberately confined to tests: production never has a
/// process-local switch which can alter its save protocol.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SaveOperationCrashPhase {
    AfterJournal,
    AfterCheckpoint,
    AfterSelector,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SnapshotInjectedFailure {
    FullBeforeTransaction,
    IoAfterPayload,
    BusyDuringRetention,
}

/// Save As has an additional durability boundary after its file rename. These
/// test-only faults prove that its single SQLite authority transfer either
/// rolls back completely or can be retried exactly after acknowledgement loss.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SaveAsInjectedFault {
    BeforeCommit,
    AfterCommitAckLoss,
}

#[cfg(test)]
thread_local! {
    static LEGACY_IMPORT_FAULT: Cell<Option<LegacyImportFault>> = const { Cell::new(None) };
    static SNAPSHOT_CRASH_PHASE: Cell<Option<SnapshotCrashPhase>> = const { Cell::new(None) };
    static SAVE_OPERATION_CRASH_PHASE: Cell<Option<SaveOperationCrashPhase>> = const { Cell::new(None) };
    static SNAPSHOT_INJECTED_FAILURE: Cell<Option<SnapshotInjectedFailure>> = const { Cell::new(None) };
    static SAVE_AS_INJECTED_FAULT: Cell<Option<SaveAsInjectedFault>> = const { Cell::new(None) };
}

/// Two-phase deterministic test barrier immediately after the in-transaction
/// token check. It proves that a discard committed before the writer resumes
/// leaves the writer with no durable row (SQLite must reject/restart it).
#[cfg(test)]
pub(crate) struct SessionFenceBarrier {
    // This test hook is process-global because the test runner creates its own
    // RecoveryDb instances. Scope it to the exact authority under test so a
    // concurrent, unrelated test cannot join its two-party barrier and wait
    // forever for a resume that belongs to a different session.
    pub root: String,
    pub token: String,
    pub reached: Barrier,
    pub resume: Barrier,
}
#[cfg(test)]
static SESSION_FENCE_BARRIER: OnceLock<Mutex<Option<Arc<SessionFenceBarrier>>>> = OnceLock::new();
#[cfg(test)]
pub(crate) fn set_session_fence_barrier(barrier: Option<Arc<SessionFenceBarrier>>) {
    *SESSION_FENCE_BARRIER
        .get_or_init(|| Mutex::new(None))
        .lock()
        .expect("session fence barrier lock") = barrier;
}
#[cfg(test)]
fn session_fence_barrier(root: &str, token: &str) {
    let barrier = SESSION_FENCE_BARRIER
        .get_or_init(|| Mutex::new(None))
        .lock()
        .expect("session fence barrier lock")
        .as_ref()
        .filter(|barrier| barrier.root == root && barrier.token == token)
        .cloned();
    if let Some(barrier) = barrier {
        barrier.reached.wait();
        barrier.resume.wait();
    }
}

/// Deterministically pauses a discard before it observes the active row. This
/// is distinct from the writer fence above: a deferred SQLite transaction can
/// then observe a replacement session after it resumes, proving the discard
/// does not erase that replacement's recovery data.
#[cfg(test)]
pub(crate) struct DiscardSessionBarrier {
    pub root: String,
    pub token: String,
    pub reached: Barrier,
    pub resume: Barrier,
}
#[cfg(test)]
static DISCARD_SESSION_BARRIER: OnceLock<Mutex<Option<Arc<DiscardSessionBarrier>>>> =
    OnceLock::new();
#[cfg(test)]
pub(crate) fn set_discard_session_barrier(barrier: Option<Arc<DiscardSessionBarrier>>) {
    *DISCARD_SESSION_BARRIER
        .get_or_init(|| Mutex::new(None))
        .lock()
        .expect("discard session barrier lock") = barrier;
}
#[cfg(test)]
fn discard_session_barrier(root: &str, token: &str) {
    let barrier = DISCARD_SESSION_BARRIER
        .get_or_init(|| Mutex::new(None))
        .lock()
        .expect("discard session barrier lock")
        .as_ref()
        .filter(|barrier| barrier.root == root && barrier.token == token)
        .cloned();
    if let Some(barrier) = barrier {
        barrier.reached.wait();
        barrier.resume.wait();
    }
}

#[cfg(test)]
pub(crate) fn set_legacy_import_fault(fault: Option<LegacyImportFault>) {
    LEGACY_IMPORT_FAULT.with(|current| current.set(fault));
}

#[cfg(test)]
pub(crate) fn set_snapshot_crash_phase(phase: Option<SnapshotCrashPhase>) {
    SNAPSHOT_CRASH_PHASE.with(|current| current.set(phase));
}

#[cfg(test)]
pub(crate) fn set_save_operation_crash_phase(phase: Option<SaveOperationCrashPhase>) {
    SAVE_OPERATION_CRASH_PHASE.with(|current| current.set(phase));
}

#[cfg(test)]
pub(crate) fn set_snapshot_injected_failure(failure: Option<SnapshotInjectedFailure>) {
    SNAPSHOT_INJECTED_FAILURE.with(|current| current.set(failure));
}

#[cfg(test)]
pub(crate) fn set_save_as_injected_fault(fault: Option<SaveAsInjectedFault>) {
    SAVE_AS_INJECTED_FAULT.with(|current| current.set(fault));
}

#[cfg(test)]
fn take_save_as_injected_fault(fault: SaveAsInjectedFault) -> bool {
    SAVE_AS_INJECTED_FAULT.with(|current| {
        if current.get() == Some(fault) {
            current.set(None);
            true
        } else {
            false
        }
    })
}

#[cfg(test)]
fn fail_at(failure: SnapshotInjectedFailure) -> Result<(), String> {
    SNAPSHOT_INJECTED_FAILURE.with(|current| {
        if current.get() == Some(failure) {
            current.set(None);
            return Err(match failure {
                SnapshotInjectedFailure::FullBeforeTransaction => {
                    "Injected SQLITE_FULL at recovery transaction boundary"
                }
                SnapshotInjectedFailure::IoAfterPayload => {
                    "Injected SQLITE_IOERR after recovery payload insert"
                }
                SnapshotInjectedFailure::BusyDuringRetention => {
                    "Injected SQLITE_BUSY during recovery retention"
                }
            }
            .into());
        }
        Ok(())
    })
}

#[cfg(test)]
fn crash_at(phase: SnapshotCrashPhase) {
    SNAPSHOT_CRASH_PHASE.with(|current| {
        if current.get() == Some(phase) {
            // Do not unwind: this must model a real process crash, including
            // losing the caller's acknowledgement after a committed write.
            std::process::abort();
        }
    });
}

#[cfg(test)]
fn crash_save_operation_at(phase: SaveOperationCrashPhase) {
    SAVE_OPERATION_CRASH_PHASE.with(|current| {
        if current.get() == Some(phase) {
            // This models a killed process after SQLite has acknowledged the
            // boundary and before the caller/webview has received a response.
            std::process::abort();
        }
    });
}

#[cfg(test)]
fn take_legacy_import_fault(fault: LegacyImportFault) -> bool {
    LEGACY_IMPORT_FAULT.with(|current| {
        if current.get() == Some(fault) {
            current.set(None);
            true
        } else {
            false
        }
    })
}

#[derive(Debug, Clone)]
pub(crate) struct RecoveryDb {
    path: PathBuf,
    directory: PathBuf,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SnapshotAck {
    pub snapshot_id: i64,
    pub recovery_sequence: i64,
    pub client_edit_version: u64,
    pub durable: bool,
}

/// The durable result of the Save As authority swap.  Unlike a normal snapshot
/// acknowledgement it carries the backend document identity selected by the
/// transaction, so an acknowledgement-loss retry can finish the in-memory
/// application step without minting a second identity.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SaveAsTransitionAck {
    pub snapshot_id: i64,
    pub recovery_sequence: i64,
    pub client_edit_version: u64,
    pub destination_document_id: u64,
    pub durable: bool,
}

/// Durable evidence recorded immediately after an atomic manuscript rename.
/// It intentionally contains no manuscript payload: disk remains the authority
/// for bytes and this record merely makes the following recovery/selector work
/// safely retryable.
#[derive(Debug, Clone)]
pub(crate) struct SaveOperation {
    pub operation_key: String,
    pub source_key: String,
    pub relative_path: String,
    pub document_id: u64,
    pub write_revision: u64,
    pub client_edit_version: u64,
    pub content_blake3: String,
    pub file_identity: String,
    pub recovery_checkpoint_durable: bool,
    pub selector_durable: bool,
}

/// Disk facts gathered through the trusted project descriptor while a restored
/// project is being activated. They are deliberately supplied by the caller,
/// rather than accepted from the webview, and make a pending post-rename
/// operation eligible for a fresh session only when its exact bytes still
/// exist.
#[derive(Debug, Clone)]
pub(crate) struct SaveOperationRebind {
    pub operation_key: String,
    pub content_blake3: String,
    pub file_identity: String,
}

#[derive(Debug, Clone)]
pub(crate) struct RestoredRecovery {
    pub active_path: Option<String>,
    pub buffers: Vec<RecoveryBuffer>,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

fn err(error: rusqlite::Error) -> String {
    format!("Recovery database error: {error}")
}

const RECOVERY_DIRECTORY: &str = "recovery-v2";
const DATABASE_FILE: &str = "recovery.sqlite3";
const SQLITE_SIDECARS: &[&str] = &[
    "recovery.sqlite3-wal",
    "recovery.sqlite3-shm",
    "recovery.sqlite3-journal",
];

#[cfg(unix)]
fn os_name(name: &str) -> Result<CString, String> {
    CString::new(name).map_err(|_| "Recovery database storage name is invalid".to_string())
}

#[cfg(unix)]
fn openat_file(
    directory: &fs::File,
    name: &OsStr,
    flags: libc::c_int,
    mode: libc::mode_t,
) -> io::Result<fs::File> {
    let name =
        CString::new(name.as_bytes()).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
    let descriptor = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
            mode,
        )
    };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { fs::File::from_raw_fd(descriptor) })
}

#[cfg(unix)]
fn verify_owned_directory(directory: &fs::File, label: &str) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    let metadata = directory
        .metadata()
        .map_err(|error| format!("Could not inspect {label}: {error}"))?;
    if !metadata.is_dir() {
        return Err(format!("{label} must be a directory"));
    }
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(format!("{label} is not owned by the effective user"));
    }
    let result = unsafe { libc::fchmod(directory.as_raw_fd(), 0o700) };
    if result != 0 {
        return Err(format!(
            "Could not harden {label} to 0700: {}",
            io::Error::last_os_error()
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn verify_owned_file(file: &fs::File, label: &str) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    let metadata = file
        .metadata()
        .map_err(|error| format!("Could not inspect {label}: {error}"))?;
    if !metadata.is_file() {
        return Err(format!("{label} must be a regular file"));
    }
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(format!("{label} is not owned by the effective user"));
    }
    if metadata.nlink() != 1 {
        return Err(format!("{label} has unexpected links"));
    }
    let result = unsafe { libc::fchmod(file.as_raw_fd(), 0o600) };
    if result != 0 {
        return Err(format!(
            "Could not harden {label} to 0600: {}",
            io::Error::last_os_error()
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn open_parent_directory<'a>(path: &'a Path, label: &str) -> Result<(fs::File, &'a OsStr), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("{label} has no parent directory"))?;
    let name = path
        .file_name()
        .ok_or_else(|| format!("{label} has no final component"))?;
    let parent = fs::File::open(parent)
        .map_err(|error| format!("Could not open parent of {label}: {error}"))?;
    Ok((parent, name))
}

#[cfg(unix)]
fn open_or_create_private_directory(
    parent: &fs::File,
    name: &OsStr,
    label: &str,
) -> Result<fs::File, String> {
    match openat_file(parent, name, libc::O_RDONLY | libc::O_DIRECTORY, 0) {
        Ok(directory) => {
            verify_owned_directory(&directory, label)?;
            Ok(directory)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let name_c =
                CString::new(name.as_bytes()).map_err(|_| format!("{label} name is invalid"))?;
            let created = unsafe { libc::mkdirat(parent.as_raw_fd(), name_c.as_ptr(), 0o700) };
            if created != 0 && io::Error::last_os_error().kind() != io::ErrorKind::AlreadyExists {
                return Err(format!(
                    "Could not create {label}: {}",
                    io::Error::last_os_error()
                ));
            }
            let directory = openat_file(parent, name, libc::O_RDONLY | libc::O_DIRECTORY, 0)
                .map_err(|open_error| format!("Could not safely open {label}: {open_error}"))?;
            verify_owned_directory(&directory, label)?;
            Ok(directory)
        }
        Err(error) => Err(format!(
            "Could not safely open {label} without following links: {error}"
        )),
    }
}

#[cfg(unix)]
fn open_or_create_private_file(
    parent: &fs::File,
    name: &str,
    label: &str,
) -> Result<fs::File, String> {
    let name = os_name(name)?;
    match openat_file(
        parent,
        OsStr::new(name.to_str().expect("valid static name")),
        libc::O_RDONLY,
        0,
    ) {
        Ok(file) => {
            verify_owned_file(&file, label)?;
            Ok(file)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let descriptor = unsafe {
                libc::openat(
                    parent.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDWR
                        | libc::O_CREAT
                        | libc::O_EXCL
                        | libc::O_CLOEXEC
                        | libc::O_NOFOLLOW,
                    0o600,
                )
            };
            if descriptor < 0 {
                return Err(format!(
                    "Could not create {label}: {}",
                    io::Error::last_os_error()
                ));
            }
            let file = unsafe { fs::File::from_raw_fd(descriptor) };
            verify_owned_file(&file, label)?;
            Ok(file)
        }
        Err(error) => Err(format!(
            "Could not safely open {label} without following links: {error}"
        )),
    }
}

#[cfg(unix)]
fn harden_existing_private_file(parent: &fs::File, name: &str, label: &str) -> Result<(), String> {
    let name = os_name(name)?;
    match openat_file(
        parent,
        OsStr::new(name.to_str().expect("valid static name")),
        libc::O_RDONLY,
        0,
    ) {
        Ok(file) => verify_owned_file(&file, label),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "Could not safely open {label} without following links: {error}"
        )),
    }
}

#[cfg(unix)]
fn prepare_private_storage(config_root: &Path) -> Result<(PathBuf, PathBuf), String> {
    let (parent, config_name) =
        open_parent_directory(config_root, "AMTE application config directory")?;
    let config = open_or_create_private_directory(
        &parent,
        config_name,
        "AMTE application config directory",
    )?;
    let recovery = open_or_create_private_directory(
        &config,
        OsStr::new(RECOVERY_DIRECTORY),
        "AMTE recovery-v2 directory",
    )?;
    let database =
        open_or_create_private_file(&recovery, DATABASE_FILE, "AMTE recovery SQLite database")?;
    drop(database);
    let directory = config_root.join(RECOVERY_DIRECTORY);
    Ok((directory.join(DATABASE_FILE), directory))
}

#[cfg(not(unix))]
fn prepare_private_storage(config_root: &Path) -> Result<(PathBuf, PathBuf), String> {
    fs::create_dir_all(config_root).map_err(|error| error.to_string())?;
    let directory = config_root.join(RECOVERY_DIRECTORY);
    fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
    let path = directory.join(DATABASE_FILE);
    Ok((path, directory))
}

impl RecoveryDb {
    /// `config_root` comes solely from Tauri's app config directory.
    pub(crate) fn for_config(config_root: &Path) -> Result<Self, String> {
        // This may tighten only the AMTE-owned application directory and its
        // recovery-v2 children. Generic XDG parents remain untouched. Every
        // existing component is opened no-follow and type/owner/link-checked
        // before its descriptor is chmodded; no recovery bytes are read here.
        let (path, directory) = prepare_private_storage(config_root)?;
        let db = Self { path, directory };
        // Reject a hostile pre-existing SQLite sidecar before SQLite is allowed
        // to resolve it. A missing sidecar is normal and is checked again after
        // WAL mode can create one.
        db.verify_sidecars()?;
        let connection = db.connect()?;
        db.migrate(&connection)?;
        db.verify_sidecars()?;
        Ok(db)
    }

    fn connect(&self) -> Result<Connection, String> {
        let connection = Connection::open(&self.path).map_err(err)?;
        connection
            .busy_timeout(std::time::Duration::from_secs(2))
            .map_err(err)?;
        connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON; PRAGMA trusted_schema=OFF;").map_err(err)?;
        Ok(connection)
    }

    fn verify_sidecars(&self) -> Result<(), String> {
        #[cfg(unix)]
        {
            let config_root = self
                .directory
                .parent()
                .ok_or_else(|| "AMTE recovery-v2 directory has no config parent".to_string())?;
            let (xdg_parent, config_name) =
                open_parent_directory(config_root, "AMTE application config directory")?;
            let config = open_or_create_private_directory(
                &xdg_parent,
                config_name,
                "AMTE application config directory",
            )?;
            let directory = open_or_create_private_directory(
                &config,
                OsStr::new(RECOVERY_DIRECTORY),
                "AMTE recovery-v2 directory",
            )?;
            harden_existing_private_file(
                &directory,
                DATABASE_FILE,
                "AMTE recovery SQLite database",
            )?;
            // SQLite may create or remove sidecars between transactions. A
            // missing sidecar is normal; every existing one is reopened
            // no-follow and hardened through its descriptor before use.
            for name in SQLITE_SIDECARS {
                harden_existing_private_file(
                    &directory,
                    name,
                    &format!("AMTE SQLite sidecar {name}"),
                )?;
            }
        }
        Ok(())
    }

    fn migrate(&self, connection: &Connection) -> Result<(), String> {
        let version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(err)?;
        if version > SCHEMA_VERSION {
            return Err("Recovery database schema is newer than this AMTE version".into());
        }
        if version == SCHEMA_VERSION {
            self.integrity(connection)?;
            return Ok(());
        }
        let transaction = connection.unchecked_transaction().map_err(err)?;
        if version == 1 {
            transaction.execute_batch("CREATE TABLE events_v2 (id INTEGER PRIMARY KEY AUTOINCREMENT, project_key TEXT NOT NULL REFERENCES projects(project_key) ON DELETE RESTRICT, buffer_id INTEGER REFERENCES buffers(id) ON DELETE RESTRICT, kind TEXT NOT NULL, snapshot_id INTEGER REFERENCES snapshots(id) ON DELETE SET NULL, created_at_ms INTEGER NOT NULL); INSERT INTO events_v2(id, project_key, buffer_id, kind, snapshot_id, created_at_ms) SELECT id, project_key, buffer_id, kind, snapshot_id, created_at_ms FROM events; DROP TABLE events; ALTER TABLE events_v2 RENAME TO events; PRAGMA user_version=2;").map_err(err)?;
            transaction.commit().map_err(err)?;
            return self.integrity(connection);
        }
        if version == 4 {
            // File replacement and SQLite cannot share an OS transaction. This
            // journal is the durable bridge: it is written after rename and
            // before any fallible clean-checkpoint or selector work.
            transaction.execute_batch(
                "CREATE TABLE save_operations (operation_key TEXT PRIMARY KEY, project_key TEXT NOT NULL REFERENCES projects(project_key) ON DELETE RESTRICT, session_token TEXT NOT NULL, source_key TEXT NOT NULL, relative_path TEXT NOT NULL, document_id INTEGER NOT NULL, write_revision INTEGER NOT NULL, client_edit_version INTEGER NOT NULL, content_blake3 TEXT NOT NULL, file_identity TEXT NOT NULL, recovery_checkpoint_durable INTEGER NOT NULL DEFAULT 0 CHECK(recovery_checkpoint_durable IN (0,1)), selector_durable INTEGER NOT NULL DEFAULT 0 CHECK(selector_durable IN (0,1)), created_at_ms INTEGER NOT NULL, updated_at_ms INTEGER NOT NULL); CREATE INDEX save_operations_project ON save_operations(project_key, session_token); PRAGMA user_version=6;"
            ).map_err(err)?;
            transaction.commit().map_err(err)?;
            return self.integrity(connection);
        }
        if version == 5 {
            let has_source_key = transaction
                .prepare("PRAGMA table_info(save_operations)")
                .map_err(err)?
                .query_map([], |row| row.get::<_, String>(1))
                .map_err(err)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(err)?
                .iter()
                .any(|column| column == "source_key");
            if has_source_key {
                transaction
                    .execute_batch("PRAGMA user_version=6;")
                    .map_err(err)?;
            } else {
                transaction.execute_batch("ALTER TABLE save_operations ADD COLUMN source_key TEXT NOT NULL DEFAULT ''; PRAGMA user_version=6;").map_err(err)?;
            }
            transaction.commit().map_err(err)?;
            return self.integrity(connection);
        }
        if version == 3 {
            // V3 rows remain historical evidence, but lacked the backend
            // session capability required to authorize a retry.
            transaction.execute_batch(
                "CREATE TABLE save_as_transitions_v4 (id INTEGER PRIMARY KEY AUTOINCREMENT, project_key TEXT NOT NULL REFERENCES projects(project_key) ON DELETE RESTRICT, source_buffer_id INTEGER NOT NULL REFERENCES buffers(id) ON DELETE RESTRICT, session_token TEXT NOT NULL, destination_key TEXT NOT NULL, content_blake3 TEXT NOT NULL, client_edit_version INTEGER NOT NULL, destination_buffer_id INTEGER NOT NULL REFERENCES buffers(id) ON DELETE RESTRICT, snapshot_id INTEGER NOT NULL REFERENCES snapshots(id) ON DELETE RESTRICT, created_at_ms INTEGER NOT NULL, UNIQUE(project_key, source_buffer_id, session_token, destination_key, content_blake3, client_edit_version));\
                 INSERT INTO save_as_transitions_v4(id, project_key, source_buffer_id, session_token, destination_key, content_blake3, client_edit_version, destination_buffer_id, snapshot_id, created_at_ms) SELECT id, project_key, source_buffer_id, '__v3_audit_only__', destination_key, content_blake3, client_edit_version, destination_buffer_id, snapshot_id, created_at_ms FROM save_as_transitions;\
                 DROP TABLE save_as_transitions;\
                 ALTER TABLE save_as_transitions_v4 RENAME TO save_as_transitions;\
                 PRAGMA user_version=4;"
            ).map_err(err)?;
            transaction.commit().map_err(err)?;
            return self.integrity(connection);
        }
        if version == 2 {
            // Session tokens are backend-issued capabilities, persisted here so
            // a request which was validated before a project switch cannot
            // commit after that switch.  They are deliberately not derived
            // from the frontend-visible project id.
            transaction.execute_batch(
                "CREATE TABLE active_sessions (project_key TEXT PRIMARY KEY REFERENCES projects(project_key) ON DELETE RESTRICT, token TEXT NOT NULL, active INTEGER NOT NULL CHECK(active IN (0, 1)), epoch INTEGER NOT NULL, updated_at_ms INTEGER NOT NULL);\
                 CREATE TABLE save_as_transitions (id INTEGER PRIMARY KEY AUTOINCREMENT, project_key TEXT NOT NULL REFERENCES projects(project_key) ON DELETE RESTRICT, source_buffer_id INTEGER NOT NULL REFERENCES buffers(id) ON DELETE RESTRICT, session_token TEXT NOT NULL, destination_key TEXT NOT NULL, content_blake3 TEXT NOT NULL, client_edit_version INTEGER NOT NULL, destination_buffer_id INTEGER NOT NULL REFERENCES buffers(id) ON DELETE RESTRICT, snapshot_id INTEGER NOT NULL REFERENCES snapshots(id) ON DELETE RESTRICT, created_at_ms INTEGER NOT NULL, UNIQUE(project_key, source_buffer_id, session_token, destination_key, content_blake3, client_edit_version));\
                 PRAGMA user_version=4;"
            ).map_err(err)?;
            transaction.commit().map_err(err)?;
            return self.integrity(connection);
        }
        transaction.execute_batch(
            "CREATE TABLE projects (project_key TEXT PRIMARY KEY, canonical_root TEXT NOT NULL UNIQUE, active_path TEXT, created_at_ms INTEGER NOT NULL, updated_at_ms INTEGER NOT NULL);
             CREATE TABLE buffers (id INTEGER PRIMARY KEY, project_key TEXT NOT NULL REFERENCES projects(project_key) ON DELETE RESTRICT, normalized_buffer_key TEXT NOT NULL, relative_path TEXT, stable_binding TEXT NOT NULL, current_backend_document_id INTEGER, last_client_edit_version INTEGER NOT NULL, latest_sequence INTEGER NOT NULL, disposition TEXT NOT NULL DEFAULT 'active', UNIQUE(project_key, normalized_buffer_key));
             CREATE TABLE snapshots (id INTEGER PRIMARY KEY AUTOINCREMENT, buffer_id INTEGER NOT NULL REFERENCES buffers(id) ON DELETE RESTRICT, sequence INTEGER NOT NULL, client_edit_version INTEGER NOT NULL, write_revision INTEGER NOT NULL, dirty INTEGER NOT NULL, content TEXT, content_blake3 TEXT, baseline_hash TEXT, ui_json TEXT NOT NULL, created_at_ms INTEGER NOT NULL, UNIQUE(buffer_id, sequence), CHECK((dirty = 0 AND content IS NULL AND content_blake3 IS NULL) OR (dirty = 1 AND content IS NOT NULL AND content_blake3 IS NOT NULL)));
             CREATE TABLE events (id INTEGER PRIMARY KEY AUTOINCREMENT, project_key TEXT NOT NULL REFERENCES projects(project_key) ON DELETE RESTRICT, buffer_id INTEGER REFERENCES buffers(id) ON DELETE RESTRICT, kind TEXT NOT NULL, snapshot_id INTEGER REFERENCES snapshots(id) ON DELETE SET NULL, created_at_ms INTEGER NOT NULL);
             CREATE TABLE legacy_imports (project_key TEXT PRIMARY KEY, legacy_manifest_hash TEXT NOT NULL, outcome TEXT NOT NULL, imported_at_ms INTEGER NOT NULL);
             CREATE INDEX snapshots_latest ON snapshots(buffer_id, sequence DESC);
             CREATE INDEX snapshots_dirty ON snapshots(buffer_id, dirty, sequence DESC);
             CREATE TABLE active_sessions (project_key TEXT PRIMARY KEY REFERENCES projects(project_key) ON DELETE RESTRICT, token TEXT NOT NULL, active INTEGER NOT NULL CHECK(active IN (0, 1)), epoch INTEGER NOT NULL, updated_at_ms INTEGER NOT NULL);
               CREATE TABLE save_as_transitions (id INTEGER PRIMARY KEY AUTOINCREMENT, project_key TEXT NOT NULL REFERENCES projects(project_key) ON DELETE RESTRICT, source_buffer_id INTEGER NOT NULL REFERENCES buffers(id) ON DELETE RESTRICT, session_token TEXT NOT NULL, destination_key TEXT NOT NULL, content_blake3 TEXT NOT NULL, client_edit_version INTEGER NOT NULL, destination_buffer_id INTEGER NOT NULL REFERENCES buffers(id) ON DELETE RESTRICT, snapshot_id INTEGER NOT NULL REFERENCES snapshots(id) ON DELETE RESTRICT, created_at_ms INTEGER NOT NULL, UNIQUE(project_key, source_buffer_id, session_token, destination_key, content_blake3, client_edit_version));
               CREATE TABLE save_operations (operation_key TEXT PRIMARY KEY, project_key TEXT NOT NULL REFERENCES projects(project_key) ON DELETE RESTRICT, session_token TEXT NOT NULL, source_key TEXT NOT NULL, relative_path TEXT NOT NULL, document_id INTEGER NOT NULL, write_revision INTEGER NOT NULL, client_edit_version INTEGER NOT NULL, content_blake3 TEXT NOT NULL, file_identity TEXT NOT NULL, recovery_checkpoint_durable INTEGER NOT NULL DEFAULT 0 CHECK(recovery_checkpoint_durable IN (0,1)), selector_durable INTEGER NOT NULL DEFAULT 0 CHECK(selector_durable IN (0,1)), created_at_ms INTEGER NOT NULL, updated_at_ms INTEGER NOT NULL);
               CREATE INDEX save_operations_project ON save_operations(project_key, session_token);
               PRAGMA user_version=5;"
        ).map_err(err)?;
        transaction.commit().map_err(err)?;
        self.integrity(connection)
    }

    fn integrity(&self, connection: &Connection) -> Result<(), String> {
        let check: String = connection
            .query_row("PRAGMA quick_check", [], |row| row.get(0))
            .map_err(err)?;
        if check != "ok" {
            return Err("Recovery database integrity check failed; bytes were retained".into());
        }
        Ok(())
    }

    fn project_key(root: &str) -> String {
        blake3::hash(root.as_bytes()).to_hex().to_string()
    }
    fn buffer_key(buffer: &RecoveryBuffer) -> String {
        buffer
            .relative_path
            .clone()
            .unwrap_or_else(|| "__untitled__".into())
    }

    fn metadata(buffer: &RecoveryBuffer) -> Result<String, String> {
        let mut metadata = buffer.clone();
        metadata.content = None;
        metadata.payload = None;
        metadata.payload_hash = None;
        metadata.payload_bytes = None;
        let json = serde_json::to_string(&metadata).map_err(|e| e.to_string())?;
        if json.len() > MAX_UI_BYTES {
            return Err("Recovery UI metadata exceeds limit".into());
        }
        Ok(json)
    }

    fn upsert_project(
        transaction: &Transaction<'_>,
        root: &str,
        active_path: Option<&str>,
    ) -> Result<String, String> {
        let key = Self::project_key(root);
        let now = now_ms();
        transaction.execute("INSERT INTO projects(project_key, canonical_root, active_path, created_at_ms, updated_at_ms) VALUES(?1, ?2, ?3, ?4, ?4) ON CONFLICT(project_key) DO UPDATE SET active_path=excluded.active_path, updated_at_ms=excluded.updated_at_ms", params![key, root, active_path, now]).map_err(err)?;
        Ok(key)
    }

    pub(crate) fn write_snapshot(
        &self,
        root: &str,
        active_path: Option<&str>,
        buffer: &RecoveryBuffer,
    ) -> Result<SnapshotAck, String> {
        self.write_snapshot_checked(root, active_path, buffer, None)
    }

    /// Production writes carry an opaque token minted during activation.  The
    /// check is made inside the same SQLite transaction as the row insert, not
    /// merely while a frontend request is being decoded.
    pub(crate) fn write_snapshot_for_session(
        &self,
        root: &str,
        active_path: Option<&str>,
        session_token: &str,
        buffer: &RecoveryBuffer,
    ) -> Result<SnapshotAck, String> {
        Self::validate_session_token(session_token)?;
        self.write_snapshot_checked(root, active_path, buffer, Some(session_token))
    }

    fn write_snapshot_checked(
        &self,
        root: &str,
        active_path: Option<&str>,
        buffer: &RecoveryBuffer,
        session_token: Option<&str>,
    ) -> Result<SnapshotAck, String> {
        // Sequence is assigned below, so validation must not use a client value.
        let mut validated = buffer.clone();
        validated.sequence = 1;
        validate_recovery_write_buffer(&validated)?;
        if buffer.client_edit_version > crate::RECOVERY_MAX_SEQUENCE {
            return Err(
                "Recovery client edit version is outside the canonical JavaScript-safe range"
                    .into(),
            );
        }
        if buffer.dirty && buffer.client_edit_version == 0 {
            return Err("Recovery client edit version is missing".into());
        }
        let content = buffer.content.as_deref();
        if content.is_some_and(|text| text.len() > RECOVERY_MAX_PAYLOAD_BYTES) {
            return Err("Recovery snapshot exceeds the per-buffer limit".into());
        }
        let connection = self.connect()?;
        self.integrity(&connection)?;
        #[cfg(test)]
        crash_at(SnapshotCrashPhase::BeforeTransaction);
        #[cfg(test)]
        fail_at(SnapshotInjectedFailure::FullBeforeTransaction)?;
        let transaction = connection.unchecked_transaction().map_err(err)?;
        let project_key = Self::project_key(root);
        if let Some(token) = session_token {
            // Validate before any write acquires SQLite's writer lock. This
            // keeps the discard-vs-queued-writer boundary testable; validation
            // is repeated below at the actual row-commit boundary.
            Self::require_active_session(&transaction, &project_key, token)?;
            #[cfg(test)]
            session_fence_barrier(root, token);
        }
        let project_key = Self::upsert_project(&transaction, root, active_path)?;
        if let Some(token) = session_token {
            Self::require_active_session(&transaction, &project_key, token)?;
        }
        let key = Self::buffer_key(buffer);
        let existing: Option<(i64, i64, i64, Option<i64>, String)> = transaction.query_row("SELECT id, last_client_edit_version, latest_sequence, current_backend_document_id, disposition FROM buffers WHERE project_key=?1 AND normalized_buffer_key=?2", params![project_key, key], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?))).optional().map_err(err)?;
        let (buffer_id, last_version, next_sequence) = match existing {
            Some((id, previous_version, sequence, document_id, disposition)) => {
                if disposition != "active" || document_id.map(|id| id as u64) != buffer.document_id
                {
                    return Err("Stale recovery document binding".into());
                }
                if buffer.client_edit_version < previous_version as u64 {
                    return Err("Stale recovery edit version".into());
                }
                (id, previous_version, sequence + 1)
            }
            None => {
                if session_token.is_some() && buffer.document_id.is_some() {
                    return Err("Stale recovery document binding".into());
                }
                let count: i64 = transaction
                    .query_row(
                        "SELECT COUNT(*) FROM buffers WHERE project_key=?1 AND disposition='active'",
                        params![project_key],
                        |row| row.get(0),
                    )
                    .map_err(err)?;
                if count as usize >= RECOVERY_MAX_BUFFERS {
                    return Err("Recovery buffer limit reached".into());
                }
                transaction.execute("INSERT INTO buffers(project_key, normalized_buffer_key, relative_path, stable_binding, current_backend_document_id, last_client_edit_version, latest_sequence, disposition) VALUES(?1, ?2, ?3, ?2, ?4, 0, 0, 'active')", params![project_key, key, buffer.relative_path, buffer.document_id.map(|id| id as i64)]).map_err(err)?;
                (transaction.last_insert_rowid(), 0, 1)
            }
        };
        let mut persisted = buffer.clone();
        persisted.sequence = next_sequence as u64;
        let metadata = Self::metadata(&persisted)?;
        // Equal client versions are allowed only for UI-only retries. Changed
        // text at the same version would be a delayed/stale writer.
        if last_version == buffer.client_edit_version as i64 {
            let prior: Option<Option<String>> = transaction.query_row("SELECT content_blake3 FROM snapshots WHERE buffer_id=?1 ORDER BY sequence DESC LIMIT 1", params![buffer_id], |row| row.get(0)).optional().map_err(err)?;
            let checksum = content.map(|value| blake3::hash(value.as_bytes()).to_hex().to_string());
            if prior.flatten() != checksum && content.is_some() {
                return Err("Stale recovery content version".into());
            }
        }
        let checksum = content.map(|value| blake3::hash(value.as_bytes()).to_hex().to_string());
        transaction.execute("INSERT INTO snapshots(buffer_id, sequence, client_edit_version, write_revision, dirty, content, content_blake3, baseline_hash, ui_json, created_at_ms) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)", params![buffer_id, next_sequence, buffer.client_edit_version as i64, buffer.write_revision as i64, buffer.dirty as i64, content, checksum, buffer.baseline_hash, metadata, now_ms()]).map_err(err)?;
        #[cfg(test)]
        crash_at(SnapshotCrashPhase::AfterPayloadBeforeCommit);
        #[cfg(test)]
        fail_at(SnapshotInjectedFailure::IoAfterPayload)?;
        let snapshot_id = transaction.last_insert_rowid();
        transaction.execute("UPDATE buffers SET last_client_edit_version=MAX(last_client_edit_version, ?2), latest_sequence=?3 WHERE id=?1", params![buffer_id, buffer.client_edit_version as i64, next_sequence]).map_err(err)?;
        #[cfg(test)]
        crash_at(SnapshotCrashPhase::DuringRetention);
        #[cfg(test)]
        fail_at(SnapshotInjectedFailure::BusyDuringRetention)?;
        self.retain(&transaction, buffer_id)?;
        let total: i64 = transaction
            .query_row(
                "SELECT COALESCE(SUM(length(content)), 0) FROM snapshots",
                [],
                |row| row.get(0),
            )
            .map_err(err)?;
        if total as usize > RECOVERY_MAX_TOTAL_BYTES {
            return Err(
                "Recovery aggregate limit reached; prior durable snapshot was retained".into(),
            );
        }
        transaction.commit().map_err(err)?;
        self.verify_sidecars()?;
        #[cfg(test)]
        crash_at(SnapshotCrashPhase::AfterCommitAckLoss);
        Ok(SnapshotAck {
            snapshot_id,
            recovery_sequence: next_sequence,
            client_edit_version: buffer.client_edit_version,
            durable: true,
        })
    }

    fn validate_session_token(token: &str) -> Result<(), String> {
        // BLAKE3 hex generated by the backend. Reject unbounded opaque values
        // before they can reach SQLite or become an authority substitute.
        if token.len() != 64 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err("Recovery session token is invalid".into());
        }
        Ok(())
    }

    fn require_active_session(
        transaction: &Transaction<'_>,
        project_key: &str,
        token: &str,
    ) -> Result<(), String> {
        let active: Option<i64> = transaction
            .query_row(
                "SELECT active FROM active_sessions WHERE project_key=?1 AND token=?2",
                params![project_key, token],
                |row| row.get(0),
            )
            .optional()
            .map_err(err)?;
        if active != Some(1) {
            return Err("Stale or inactive recovery session".into());
        }
        Ok(())
    }

    /// Serializes the activation epoch in SQLite. The caller must use the
    /// recovery mutex; this method never takes ProjectAuthority.
    pub(crate) fn activate_session(&self, root: &str, token: &str) -> Result<(), String> {
        Self::validate_session_token(token)?;
        let connection = self.connect()?;
        self.integrity(&connection)?;
        let transaction = connection.unchecked_transaction().map_err(err)?;
        let key = Self::upsert_project(&transaction, root, None)?;
        let epoch: i64 = transaction
            .query_row(
                "SELECT COALESCE(MAX(epoch), 0) + 1 FROM active_sessions WHERE project_key=?1",
                params![key],
                |row| row.get(0),
            )
            .map_err(err)?;
        transaction.execute(
            "INSERT INTO active_sessions(project_key, token, active, epoch, updated_at_ms) VALUES(?1, ?2, 1, ?3, ?4) ON CONFLICT(project_key) DO UPDATE SET token=excluded.token, active=1, epoch=excluded.epoch, updated_at_ms=excluded.updated_at_ms",
            params![key, token, epoch, now_ms()],
        ).map_err(err)?;
        transaction.commit().map_err(err)
    }

    pub(crate) fn deactivate_session(&self, root: &str, token: &str) -> Result<(), String> {
        Self::validate_session_token(token)?;
        let connection = self.connect()?;
        self.integrity(&connection)?;
        let transaction = connection.unchecked_transaction().map_err(err)?;
        let key = Self::project_key(root);
        transaction.execute(
            "UPDATE active_sessions SET active=0, updated_at_ms=?3 WHERE project_key=?1 AND token=?2 AND active=1",
            params![key, token, now_ms()],
        ).map_err(err)?;
        transaction.commit().map_err(err)
    }

    /// The project-switch fence. The old-token compare-and-swap and new-token
    /// activation commit together. Discard is finalized separately only after
    /// selector persistence, so a selector failure can restore the exact
    /// still-live old session without reviving discarded buffers.
    pub(crate) fn transition_active_session(
        &self,
        old: Option<(&str, &str)>,
        new_root: &str,
        new_token: &str,
    ) -> Result<(), String> {
        Self::validate_session_token(new_token)?;
        if let Some((_, token)) = old {
            Self::validate_session_token(token)?;
        }
        let connection = self.connect()?;
        self.integrity(&connection)?;
        let transaction = connection.unchecked_transaction().map_err(err)?;
        if let Some((old_root, old_token)) = old {
            let old_key = Self::project_key(old_root);
            let changed = transaction.execute(
                "UPDATE active_sessions SET active=0, updated_at_ms=?3 WHERE project_key=?1 AND token=?2 AND active=1",
                params![old_key, old_token, now_ms()],
            ).map_err(err)?;
            if changed != 1 {
                return Err(
                    "Stale or inactive recovery session; activation was not applied".into(),
                );
            }
            transaction
                .execute(
                    "INSERT INTO events(project_key, kind, created_at_ms) VALUES(?1, ?2, ?3)",
                    params![
                        old_key,
                        format!("activation-cas:{old_token}:{new_token}"),
                        now_ms()
                    ],
                )
                .map_err(err)?;
        }
        let new_key = Self::upsert_project(&transaction, new_root, None)?;
        let epoch: i64 = transaction
            .query_row(
                "SELECT COALESCE(MAX(epoch), 0) + 1 FROM active_sessions WHERE project_key=?1",
                params![new_key],
                |row| row.get(0),
            )
            .map_err(err)?;
        transaction.execute(
            "INSERT INTO active_sessions(project_key, token, active, epoch, updated_at_ms) VALUES(?1, ?2, 1, ?3, ?4) ON CONFLICT(project_key) DO UPDATE SET token=excluded.token, active=1, epoch=excluded.epoch, updated_at_ms=excluded.updated_at_ms",
            params![new_key, new_token, epoch, now_ms()],
        ).map_err(err)?;
        transaction.commit().map_err(err)
    }

    /// Restore a just-transitioned session if selector persistence or the
    /// final authority comparison fails.  The replacement token must still be
    /// the active row; otherwise a newer activation won and must never be
    /// deactivated by this rollback.
    pub(crate) fn rollback_activation_session(
        &self,
        old: Option<(&str, &str)>,
        new_root: &str,
        new_token: &str,
    ) -> Result<(), String> {
        Self::validate_session_token(new_token)?;
        if let Some((_, token)) = old {
            Self::validate_session_token(token)?;
        }
        let connection = self.connect()?;
        self.integrity(&connection)?;
        let transaction = connection.unchecked_transaction().map_err(err)?;
        let new_key = Self::project_key(new_root);
        let changed = transaction.execute(
            "UPDATE active_sessions SET active=0, updated_at_ms=?3 WHERE project_key=?1 AND token=?2 AND active=1",
            params![new_key, new_token, now_ms()],
        ).map_err(err)?;
        if changed != 1 {
            return Err(
                "Activation rollback is stale; replacement session remains authoritative".into(),
            );
        }
        if let Some((old_root, old_token)) = old {
            let old_key = Self::upsert_project(&transaction, old_root, None)?;
            transaction
                .execute(
                    "DELETE FROM events WHERE project_key=?1 AND kind=?2",
                    params![old_key, format!("activation-cas:{old_token}:{new_token}")],
                )
                .map_err(err)?;
            let epoch: i64 = transaction
                .query_row(
                    "SELECT COALESCE(MAX(epoch), 0) + 1 FROM active_sessions WHERE project_key=?1",
                    params![old_key],
                    |row| row.get(0),
                )
                .map_err(err)?;
            transaction.execute(
                "INSERT INTO active_sessions(project_key, token, active, epoch, updated_at_ms) VALUES(?1, ?2, 1, ?3, ?4) ON CONFLICT(project_key) DO UPDATE SET token=excluded.token, active=1, epoch=excluded.epoch, updated_at_ms=excluded.updated_at_ms",
                params![old_key, old_token, epoch, now_ms()],
            ).map_err(err)?;
        }
        transaction.commit().map_err(err)
    }

    /// Complete a requested terminal discard only after the activation CAS has
    /// made `new_token` active.  The expected old token is required so callers
    /// cannot turn an arbitrary active session into a discard capability.
    pub(crate) fn finalize_activation_discard(
        &self,
        old: Option<(&str, &str)>,
        new_root: &str,
        new_token: &str,
    ) -> Result<(), String> {
        let Some((old_root, old_token)) = old else {
            return Ok(());
        };
        Self::validate_session_token(old_token)?;
        Self::validate_session_token(new_token)?;
        let connection = self.connect()?;
        self.integrity(&connection)?;
        let transaction = connection.unchecked_transaction().map_err(err)?;
        let new_key = Self::project_key(new_root);
        Self::require_active_session(&transaction, &new_key, new_token)?;
        let old_key = Self::project_key(old_root);
        // A durable marker is inserted only by the successful expected-old
        // CAS above. It covers same-root replacement too, where the old row is
        // necessarily overwritten by the new active token.
        let cas: i64 = transaction
            .query_row(
                "SELECT COUNT(*) FROM events WHERE project_key=?1 AND kind=?2",
                params![old_key, format!("activation-cas:{old_token}:{new_token}")],
                |row| row.get(0),
            )
            .map_err(err)?;
        if cas != 1 {
            return Err("Stale activation discard was not applied".into());
        }
        transaction
            .execute(
                "UPDATE buffers SET disposition='discarded' WHERE project_key=?1 AND disposition='active'",
                params![old_key],
            )
            .map_err(err)?;
        transaction.execute(
            "INSERT INTO events(project_key, kind, created_at_ms) VALUES(?1, 'terminal-discard', ?2)",
            params![old_key, now_ms()],
        ).map_err(err)?;
        transaction
            .execute(
                "DELETE FROM events WHERE project_key=?1 AND kind=?2",
                params![old_key, format!("activation-cas:{old_token}:{new_token}")],
            )
            .map_err(err)?;
        transaction.commit().map_err(err)
    }

    pub(crate) fn clear_activation_cas(
        &self,
        old: Option<(&str, &str)>,
        new_token: &str,
    ) -> Result<(), String> {
        let Some((old_root, old_token)) = old else {
            return Ok(());
        };
        let connection = self.connect()?;
        let key = Self::project_key(old_root);
        connection
            .execute(
                "DELETE FROM events WHERE project_key=?1 AND kind=?2",
                params![key, format!("activation-cas:{old_token}:{new_token}")],
            )
            .map_err(err)?;
        Ok(())
    }

    pub(crate) fn deactivate_and_discard_session(
        &self,
        root: &str,
        token: &str,
    ) -> Result<(), String> {
        Self::validate_session_token(token)?;
        let connection = self.connect()?;
        self.integrity(&connection)?;
        let transaction = connection.unchecked_transaction().map_err(err)?;
        // Do not read or write before this test seam. A deferred transaction
        // must see a replacement activation that commits while the old discard
        // is paused; the conditional update below is the authoritative check.
        #[cfg(test)]
        discard_session_barrier(root, token);
        let key = Self::project_key(root);
        let deactivated = transaction.execute(
            "UPDATE active_sessions SET active=0, updated_at_ms=?3 WHERE project_key=?1 AND token=?2 AND active=1",
            params![key, token, now_ms()],
        ).map_err(err)?;
        if deactivated != 1 {
            // Returning without commit rolls back the transaction. In
            // particular, never create a project row, discard a buffer, or add
            // a terminal marker for an old or absent capability.
            return Err("Stale or inactive recovery session; discard was not applied".into());
        }
        transaction
            .execute(
                "UPDATE buffers SET disposition='discarded' WHERE project_key=?1",
                params![key],
            )
            .map_err(err)?;
        transaction.execute(
            "INSERT INTO events(project_key, kind, created_at_ms) VALUES(?1, 'terminal-discard', ?2)",
            params![key, now_ms()],
        ).map_err(err)?;
        transaction.commit().map_err(err)
    }

    fn retain(&self, transaction: &Transaction<'_>, buffer_id: i64) -> Result<(), String> {
        // Keep the newest fixed window and the newest dirty row. The latter is
        // intentionally redundant today but protects this invariant if policy
        // changes to retain fewer clean UI revisions.
        transaction.execute("DELETE FROM snapshots WHERE buffer_id=?1 AND id NOT IN (SELECT id FROM snapshots WHERE buffer_id=?1 ORDER BY sequence DESC LIMIT ?2) AND id != COALESCE((SELECT id FROM snapshots WHERE buffer_id=?1 AND dirty=1 AND content_blake3 IS NOT NULL ORDER BY sequence DESC LIMIT 1), -1)", params![buffer_id, RETAIN_PER_BUFFER]).map_err(err)?;
        Ok(())
    }

    fn retain_retired_history(
        transaction: &Transaction<'_>,
        project_key: &str,
    ) -> Result<(), String> {
        // A re-open copies the current authority into a new binding. Without a
        // project-wide retired window, repeated ordinary re-opens would retain
        // another full payload window forever. Bindings remain marked retired
        // for audit; this trims only their non-authoritative payload history.
        transaction
            .execute(
                "DELETE FROM snapshots WHERE buffer_id IN (
                SELECT id FROM buffers WHERE project_key=?1 AND disposition='retired'
            ) AND id NOT IN (
                SELECT s.id FROM snapshots s
                JOIN buffers b ON b.id=s.buffer_id
                WHERE b.project_key=?1 AND b.disposition='retired'
                ORDER BY s.created_at_ms DESC, s.id DESC
                LIMIT ?2
            )",
                params![project_key, RETAIN_RETIRED_SNAPSHOTS_PER_PROJECT],
            )
            .map_err(err)?;
        Ok(())
    }

    pub(crate) fn write_clean_checkpoint(
        &self,
        root: &str,
        relative_path: &str,
        document_id: u64,
        write_revision: u64,
        client_edit_version: u64,
        baseline_hash: String,
    ) -> Result<SnapshotAck, String> {
        self.write_clean_checkpoint_checked(
            root,
            relative_path,
            document_id,
            write_revision,
            client_edit_version,
            baseline_hash,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn write_clean_checkpoint_for_session(
        &self,
        root: &str,
        relative_path: &str,
        document_id: u64,
        write_revision: u64,
        client_edit_version: u64,
        baseline_hash: String,
        session_token: &str,
    ) -> Result<SnapshotAck, String> {
        Self::validate_session_token(session_token)?;
        self.write_clean_checkpoint_checked(
            root,
            relative_path,
            document_id,
            write_revision,
            client_edit_version,
            baseline_hash,
            Some(session_token),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn write_clean_checkpoint_checked(
        &self,
        root: &str,
        relative_path: &str,
        document_id: u64,
        write_revision: u64,
        client_edit_version: u64,
        baseline_hash: String,
        session_token: Option<&str>,
    ) -> Result<SnapshotAck, String> {
        let connection = self.connect()?;
        let key = Self::project_key(root);
        let metadata: Option<String> = connection.query_row(
            "SELECT s.ui_json FROM snapshots s JOIN buffers b ON b.id=s.buffer_id WHERE b.project_key=?1 AND b.normalized_buffer_key=?2 AND b.current_backend_document_id=?3 AND b.disposition='active' ORDER BY s.sequence DESC LIMIT 1",
            params![key, relative_path, document_id as i64], |row| row.get(0)
        ).optional().map_err(err)?;
        let mut buffer = metadata
            .and_then(|json| serde_json::from_str::<RecoveryBuffer>(&json).ok())
            .unwrap_or(RecoveryBuffer {
                relative_path: Some(relative_path.into()),
                document_id: Some(document_id),
                write_revision,
                sequence: 0,
                client_edit_version,
                dirty: false,
                content: None,
                payload: None,
                payload_hash: None,
                payload_bytes: None,
                baseline_hash: None,
                source_scroll: None,
                selection: None,
                preview_scroll: None,
                previewing: None,
                search_term: None,
                chapter_identity: None,
                chapter_scroll: None,
                collapsed_chapter_identities: None,
                activity: None,
                reconciliation_operation_key: None,
                reconciliation_client_edit_version: None,
            });
        buffer.relative_path = Some(relative_path.into());
        buffer.document_id = Some(document_id);
        buffer.write_revision = write_revision;
        buffer.sequence = 0;
        buffer.client_edit_version = client_edit_version;
        buffer.dirty = false;
        buffer.content = None;
        buffer.payload = None;
        buffer.payload_hash = None;
        buffer.payload_bytes = None;
        buffer.baseline_hash = Some(baseline_hash);
        self.write_snapshot_checked(root, None, &buffer, session_token)
    }

    /// Returns a journalled file commit only when it belongs to the currently
    /// active backend session. A retry must compare disk bytes before trusting
    /// this result; the journal is not a replacement for descriptor-bound I/O.
    pub(crate) fn save_operation(
        &self,
        root: &str,
        session_token: &str,
        operation_key: &str,
    ) -> Result<Option<SaveOperation>, String> {
        Self::validate_session_token(session_token)?;
        let connection = self.connect()?;
        self.integrity(&connection)?;
        let project_key = Self::project_key(root);
        let result = connection.query_row(
            "SELECT s.operation_key, s.source_key, s.relative_path, s.document_id, s.write_revision, s.client_edit_version, s.content_blake3, s.file_identity, s.recovery_checkpoint_durable, s.selector_durable FROM save_operations s JOIN active_sessions a ON a.project_key=s.project_key AND a.token=s.session_token AND a.active=1 WHERE s.operation_key=?1 AND s.project_key=?2 AND s.session_token=?3",
            params![operation_key, project_key, session_token],
            |row| Ok(SaveOperation {
                operation_key: row.get(0)?, source_key: row.get(1)?, relative_path: row.get(2)?, document_id: row.get::<_, i64>(3)? as u64,
                write_revision: row.get::<_, i64>(4)? as u64, client_edit_version: row.get::<_, i64>(5)? as u64,
                content_blake3: row.get(6)?, file_identity: row.get(7)?,
                recovery_checkpoint_durable: row.get::<_, i64>(8)? != 0, selector_durable: row.get::<_, i64>(9)? != 0,
            }),
        ).optional().map_err(err)?;
        Ok(result)
    }

    /// Read-only discovery used during trusted restore. These rows remain
    /// unusable until `rebind_session_bindings_and_activate_with_operations`
    /// verifies their matching recovery binding and descriptor-read disk facts
    /// in the activation transaction.
    pub(crate) fn pending_save_operations(&self, root: &str) -> Result<Vec<SaveOperation>, String> {
        let connection = self.connect()?;
        self.integrity(&connection)?;
        let project_key = Self::project_key(root);
        let mut statement = connection
            .prepare("SELECT operation_key, source_key, relative_path, document_id, write_revision, client_edit_version, content_blake3, file_identity, recovery_checkpoint_durable, selector_durable FROM save_operations WHERE project_key=?1")
            .map_err(err)?;
        statement
            .query_map(params![project_key], |row| {
                Ok(SaveOperation {
                    operation_key: row.get(0)?,
                    source_key: row.get(1)?,
                    relative_path: row.get(2)?,
                    document_id: row.get::<_, i64>(3)? as u64,
                    write_revision: row.get::<_, i64>(4)? as u64,
                    client_edit_version: row.get::<_, i64>(5)? as u64,
                    content_blake3: row.get(6)?,
                    file_identity: row.get(7)?,
                    recovery_checkpoint_durable: row.get::<_, i64>(8)? != 0,
                    selector_durable: row.get::<_, i64>(9)? != 0,
                })
            })
            .map_err(err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(err)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn record_file_commit(
        &self,
        root: &str,
        session_token: &str,
        operation_key: &str,
        source_key: &str,
        relative_path: &str,
        document_id: u64,
        write_revision: u64,
        client_edit_version: u64,
        content_blake3: &str,
        file_identity: &str,
    ) -> Result<SaveOperation, String> {
        Self::validate_session_token(session_token)?;
        if !valid_blake3_hex(content_blake3) || document_id == 0 {
            return Err("Invalid committed save operation identity".into());
        }
        let connection = self.connect()?;
        self.integrity(&connection)?;
        let transaction = connection.unchecked_transaction().map_err(err)?;
        let project_key = Self::project_key(root);
        Self::require_active_session(&transaction, &project_key, session_token)?;
        let existing: Option<SaveOperation> = transaction.query_row(
            "SELECT operation_key, source_key, relative_path, document_id, write_revision, client_edit_version, content_blake3, file_identity, recovery_checkpoint_durable, selector_durable FROM save_operations WHERE operation_key=?1",
            params![operation_key],
            |row| Ok(SaveOperation {
                operation_key: row.get(0)?, source_key: row.get(1)?, relative_path: row.get(2)?, document_id: row.get::<_, i64>(3)? as u64,
                write_revision: row.get::<_, i64>(4)? as u64, client_edit_version: row.get::<_, i64>(5)? as u64,
                content_blake3: row.get(6)?, file_identity: row.get(7)?,
                recovery_checkpoint_durable: row.get::<_, i64>(8)? != 0, selector_durable: row.get::<_, i64>(9)? != 0,
            }),
        ).optional().map_err(err)?;
        if let Some(existing) = existing {
            if existing.source_key != source_key
                || existing.relative_path != relative_path
                || existing.document_id != document_id
                || existing.write_revision != write_revision
                || existing.client_edit_version != client_edit_version
                || existing.content_blake3 != content_blake3
                || existing.file_identity != file_identity
            {
                return Err("Save operation key collided with different committed identity".into());
            }
            transaction.commit().map_err(err)?;
            return Ok(existing);
        }
        let now = now_ms();
        transaction.execute(
            "INSERT INTO save_operations(operation_key, project_key, session_token, source_key, relative_path, document_id, write_revision, client_edit_version, content_blake3, file_identity, created_at_ms, updated_at_ms) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11)",
            params![operation_key, project_key, session_token, source_key, relative_path, document_id as i64, write_revision as i64, client_edit_version as i64, content_blake3, file_identity, now],
        ).map_err(err)?;
        transaction.commit().map_err(err)?;
        Ok(SaveOperation {
            operation_key: operation_key.into(),
            source_key: source_key.into(),
            relative_path: relative_path.into(),
            document_id,
            write_revision,
            client_edit_version,
            content_blake3: content_blake3.into(),
            file_identity: file_identity.into(),
            recovery_checkpoint_durable: false,
            selector_durable: false,
        })
    }

    /// Associates a committed journal entry with the latest recovery metadata
    /// for its live binding.  This stores only the opaque key and the saved
    /// edit version; manuscript bytes remain in their existing snapshot.
    /// Save As has no destination binding yet, so its still-live untitled
    /// source is the only legal fallback.
    pub(crate) fn mark_save_operation_pending_metadata(
        &self,
        root: &str,
        session_token: &str,
        operation_key: &str,
    ) -> Result<(), String> {
        let operation = self
            .save_operation(root, session_token, operation_key)?
            .ok_or_else(|| "Committed save operation is absent".to_string())?;
        let connection = self.connect()?;
        let transaction = connection.unchecked_transaction().map_err(err)?;
        let project_key = Self::project_key(root);
        Self::require_active_session(&transaction, &project_key, session_token)?;
        let snapshot: Option<(i64, String)> = transaction
            .query_row(
                "SELECT s.id, s.ui_json FROM snapshots s JOIN buffers b ON b.id=s.buffer_id WHERE b.project_key=?1 AND b.normalized_buffer_key=?2 AND b.current_backend_document_id=?3 AND b.disposition='active' ORDER BY s.sequence DESC LIMIT 1",
                params![project_key, operation.relative_path, operation.document_id as i64],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(err)?
            .or_else(|| {
                transaction.query_row(
                    "SELECT s.id, s.ui_json FROM snapshots s JOIN buffers b ON b.id=s.buffer_id WHERE b.project_key=?1 AND b.normalized_buffer_key='__untitled__' AND b.disposition='active' ORDER BY s.sequence DESC LIMIT 1",
                    params![project_key],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                ).optional().ok().flatten()
            });
        let Some((snapshot_id, metadata)) = snapshot else {
            return Err("Save recovery binding is absent".into());
        };
        let mut buffer: RecoveryBuffer = serde_json::from_str(&metadata)
            .map_err(|_| "Save recovery metadata is unreadable".to_string())?;
        buffer.reconciliation_operation_key = Some(operation.operation_key);
        buffer.reconciliation_client_edit_version = Some(operation.client_edit_version);
        transaction
            .execute(
                "UPDATE snapshots SET ui_json=?2 WHERE id=?1",
                params![snapshot_id, Self::metadata(&buffer)?],
            )
            .map_err(err)?;
        transaction.commit().map_err(err)?;
        // The operation row and restart metadata are now one observable
        // recovery state. Crashing here proves restart can discover the opaque
        // key instead of merely finding an orphaned journal row.
        #[cfg(test)]
        crash_save_operation_at(SaveOperationCrashPhase::AfterJournal);
        Ok(())
    }

    /// Once both acknowledgement legs are durable, remove the restart hint
    /// from the destination's active metadata. The immutable operation journal
    /// remains the audit/idempotency authority.
    pub(crate) fn clear_save_operation_pending_metadata(
        &self,
        root: &str,
        session_token: &str,
        operation_key: &str,
    ) -> Result<(), String> {
        let operation = self
            .save_operation(root, session_token, operation_key)?
            .ok_or_else(|| "Committed save operation is absent".to_string())?;
        let connection = self.connect()?;
        let transaction = connection.unchecked_transaction().map_err(err)?;
        let project_key = Self::project_key(root);
        Self::require_active_session(&transaction, &project_key, session_token)?;
        let snapshot: Option<(i64, String)> = transaction.query_row(
            "SELECT s.id, s.ui_json FROM snapshots s JOIN buffers b ON b.id=s.buffer_id WHERE b.project_key=?1 AND b.normalized_buffer_key=?2 AND b.current_backend_document_id=?3 AND b.disposition='active' ORDER BY s.sequence DESC LIMIT 1",
            params![project_key, operation.relative_path, operation.document_id as i64],
            |row| Ok((row.get(0)?, row.get(1)?)),
        ).optional().map_err(err)?;
        if let Some((snapshot_id, metadata)) = snapshot {
            let mut buffer: RecoveryBuffer = serde_json::from_str(&metadata)
                .map_err(|_| "Save recovery metadata is unreadable".to_string())?;
            buffer.reconciliation_operation_key = None;
            buffer.reconciliation_client_edit_version = None;
            transaction
                .execute(
                    "UPDATE snapshots SET ui_json=?2 WHERE id=?1",
                    params![snapshot_id, Self::metadata(&buffer)?],
                )
                .map_err(err)?;
        }
        transaction.commit().map_err(err)
    }

    /// Complete the clean checkpoint exactly once. If the process died after
    /// SQLite committed the checkpoint but before it marked the operation, the
    /// matching latest row is recognized and no duplicate snapshot is created.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn reconcile_save_checkpoint(
        &self,
        root: &str,
        session_token: &str,
        operation_key: &str,
        relative_path: &str,
        document_id: u64,
        write_revision: u64,
        client_edit_version: u64,
        baseline_hash: String,
    ) -> Result<SaveOperation, String> {
        let mut operation = self
            .save_operation(root, session_token, operation_key)?
            .ok_or_else(|| "Committed save operation is absent".to_string())?;
        if operation.recovery_checkpoint_durable {
            return Ok(operation);
        }
        let connection = self.connect()?;
        let key = Self::project_key(root);
        let clean_exists: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM snapshots s JOIN buffers b ON b.id=s.buffer_id WHERE b.project_key=?1 AND b.normalized_buffer_key=?2 AND b.current_backend_document_id=?3 AND b.disposition='active' AND s.dirty=0 AND s.write_revision=?4 AND s.client_edit_version=?5 AND s.baseline_hash=?6 ORDER BY s.sequence DESC LIMIT 1)",
            params![key, relative_path, document_id as i64, write_revision as i64, client_edit_version as i64, &baseline_hash], |row| row.get::<_, i64>(0)
        ).map_err(err)? != 0;
        if !clean_exists {
            self.write_clean_checkpoint_for_session(
                root,
                relative_path,
                document_id,
                write_revision,
                client_edit_version,
                baseline_hash,
                session_token,
            )?;
        }
        let connection = self.connect()?;
        let transaction = connection.unchecked_transaction().map_err(err)?;
        let key = Self::project_key(root);
        Self::require_active_session(&transaction, &key, session_token)?;
        transaction.execute("UPDATE save_operations SET recovery_checkpoint_durable=1, updated_at_ms=?2 WHERE operation_key=?1", params![operation_key, now_ms()]).map_err(err)?;
        transaction.commit().map_err(err)?;
        operation.recovery_checkpoint_durable = true;
        #[cfg(test)]
        crash_save_operation_at(SaveOperationCrashPhase::AfterCheckpoint);
        Ok(operation)
    }

    pub(crate) fn mark_save_selector_durable(
        &self,
        root: &str,
        session_token: &str,
        operation_key: &str,
    ) -> Result<SaveOperation, String> {
        let mut operation = self
            .save_operation(root, session_token, operation_key)?
            .ok_or_else(|| "Committed save operation is absent".to_string())?;
        if operation.selector_durable {
            return Ok(operation);
        }
        let connection = self.connect()?;
        let transaction = connection.unchecked_transaction().map_err(err)?;
        let key = Self::project_key(root);
        Self::require_active_session(&transaction, &key, session_token)?;
        transaction.execute("UPDATE save_operations SET selector_durable=1, updated_at_ms=?2 WHERE operation_key=?1", params![operation_key, now_ms()]).map_err(err)?;
        transaction.commit().map_err(err)?;
        operation.selector_durable = true;
        #[cfg(test)]
        crash_save_operation_at(SaveOperationCrashPhase::AfterSelector);
        Ok(operation)
    }

    pub(crate) fn record_discard(&self, root: &str) -> Result<(), String> {
        let connection = self.connect()?;
        self.integrity(&connection)?;
        let transaction = connection.unchecked_transaction().map_err(err)?;
        let key = Self::upsert_project(&transaction, root, None)?;
        transaction
            .execute(
                "UPDATE buffers SET disposition='discarded' WHERE project_key=?1",
                params![key],
            )
            .map_err(err)?;
        transaction.execute("INSERT INTO events(project_key, kind, created_at_ms) VALUES(?1, 'terminal-discard', ?2)", params![key, now_ms()]).map_err(err)?;
        transaction.commit().map_err(err)
    }

    /// Finish a Save As authority transfer from its already-journalled file
    /// commit. This deliberately accepts no frontend path, content, source, or
    /// destination: every identity is read from the operation journal and the
    /// latest authoritative untitled snapshot inside one transaction.
    pub(crate) fn reconcile_save_as_operation(
        &self,
        root: &str,
        session_token: &str,
        operation_key: &str,
    ) -> Result<SaveAsTransitionAck, String> {
        let operation = self
            .save_operation(root, session_token, operation_key)?
            .ok_or_else(|| "Committed save operation is absent".to_string())?;
        if operation.source_key != "__untitled__" {
            return Err("Committed operation is not a Save As transition".into());
        }
        let connection = self.connect()?;
        self.integrity(&connection)?;
        let transaction = connection.unchecked_transaction().map_err(err)?;
        let key = Self::upsert_project(&transaction, root, Some(&operation.relative_path))?;
        Self::require_active_session(&transaction, &key, session_token)?;
        if let Some((snapshot_id, sequence, document_id)) = transaction.query_row(
            "SELECT t.snapshot_id, s.sequence, b.current_backend_document_id FROM save_as_transitions t JOIN buffers b ON b.id=t.destination_buffer_id JOIN snapshots s ON s.id=t.snapshot_id WHERE t.project_key=?1 AND t.session_token=?2 AND t.destination_key=?3 AND t.content_blake3=?4 AND t.client_edit_version=?5",
            params![key, session_token, operation.relative_path, operation.content_blake3, operation.client_edit_version as i64],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?)),
        ).optional().map_err(err)? {
            if document_id as u64 != operation.document_id {
                return Err("Save As journal destination identity is stale".into());
            }
            transaction.commit().map_err(err)?;
            return Ok(SaveAsTransitionAck { snapshot_id, recovery_sequence: sequence, client_edit_version: operation.client_edit_version, destination_document_id: operation.document_id, durable: true });
        }
        let source: Option<(i64, i64, i64, String, Option<String>)> = transaction.query_row(
            "SELECT b.id, s.sequence, s.client_edit_version, s.ui_json, s.content_blake3 FROM buffers b JOIN snapshots s ON s.buffer_id=b.id WHERE b.project_key=?1 AND b.normalized_buffer_key='__untitled__' AND b.disposition='active' AND b.current_backend_document_id IS NULL ORDER BY s.sequence DESC LIMIT 1",
            params![key],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
        ).optional().map_err(err)?;
        let Some((source_id, _source_sequence, source_version, metadata, source_hash)) = source
        else {
            return Err("Save As source recovery binding is absent or already transitioned".into());
        };
        if source_version as u64 != operation.client_edit_version
            || source_hash.as_deref() != Some(operation.content_blake3.as_str())
        {
            return Err(
                "Save As source recovery binding no longer matches the committed journal".into(),
            );
        }
        let occupied: Option<i64> = transaction.query_row(
            "SELECT id FROM buffers WHERE project_key=?1 AND normalized_buffer_key=?2 AND disposition='active'",
            params![key, operation.relative_path], |row| row.get(0),
        ).optional().map_err(err)?;
        if occupied.is_some() {
            return Err("Ambiguous Save As destination has an active recovery binding".into());
        }
        let mut clean: RecoveryBuffer = serde_json::from_str(&metadata)
            .map_err(|_| "Save As source recovery metadata is unreadable".to_string())?;
        clean.relative_path = Some(operation.relative_path.clone());
        clean.document_id = Some(operation.document_id);
        clean.write_revision = operation.write_revision;
        clean.sequence = 1;
        clean.client_edit_version = operation.client_edit_version;
        clean.dirty = false;
        clean.content = None;
        clean.payload = None;
        clean.payload_hash = None;
        clean.payload_bytes = None;
        clean.baseline_hash = Some(operation.content_blake3.clone());
        // The destination is the restart authority after the source becomes
        // retired. Carry the journal key from the durable operation itself,
        // never from a caller-provided/source recovery object.
        clean.reconciliation_operation_key = Some(operation.operation_key.clone());
        clean.reconciliation_client_edit_version = Some(operation.client_edit_version);
        transaction.execute(
            "INSERT INTO buffers(project_key, normalized_buffer_key, relative_path, stable_binding, current_backend_document_id, last_client_edit_version, latest_sequence, disposition) VALUES(?1, ?2, ?2, ?2, ?3, ?4, 1, 'active')",
            params![key, operation.relative_path, operation.document_id as i64, operation.client_edit_version as i64],
        ).map_err(err)?;
        let destination_id = transaction.last_insert_rowid();
        transaction.execute(
            "INSERT INTO snapshots(buffer_id, sequence, client_edit_version, write_revision, dirty, content, content_blake3, baseline_hash, ui_json, created_at_ms) VALUES(?1, 1, ?2, ?3, 0, NULL, NULL, ?4, ?5, ?6)",
            params![destination_id, operation.client_edit_version as i64, operation.write_revision as i64, operation.content_blake3, Self::metadata(&clean)?, now_ms()],
        ).map_err(err)?;
        let snapshot_id = transaction.last_insert_rowid();
        transaction
            .execute(
                "UPDATE buffers SET normalized_buffer_key=?2, disposition='retired' WHERE id=?1",
                params![source_id, format!("__retired__{source_id}")],
            )
            .map_err(err)?;
        transaction.execute(
            "INSERT INTO save_as_transitions(project_key, source_buffer_id, session_token, destination_key, content_blake3, client_edit_version, destination_buffer_id, snapshot_id, created_at_ms) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![key, source_id, session_token, operation.relative_path, operation.content_blake3, operation.client_edit_version as i64, destination_id, snapshot_id, now_ms()],
        ).map_err(err)?;
        transaction.execute("INSERT INTO events(project_key, buffer_id, kind, snapshot_id, created_at_ms) VALUES(?1, ?2, 'save-as-transition', ?3, ?4)", params![key, source_id, snapshot_id, now_ms()]).map_err(err)?;
        transaction.commit().map_err(err)?;
        self.verify_sidecars()?;
        Ok(SaveAsTransitionAck {
            snapshot_id,
            recovery_sequence: 1,
            client_edit_version: operation.client_edit_version,
            destination_document_id: operation.document_id,
            durable: true,
        })
    }

    /// Atomically converts the one untitled binding into a saved binding.  The
    /// filesystem rename/write happens before this call; this transaction is
    /// the durable recovery authority transition and is idempotent after an
    /// acknowledgement loss.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn save_as_transition(
        &self,
        root: &str,
        session_token: &str,
        source: &RecoveryBuffer,
        destination_path: &str,
        destination_document_id: u64,
        write_revision: u64,
        client_edit_version: u64,
        saved_hash: String,
    ) -> Result<SaveAsTransitionAck, String> {
        Self::validate_session_token(session_token)?;
        if source.relative_path.is_some()
            || source.document_id.is_some()
            || !source.dirty
            || source.content.is_none()
            || source.client_edit_version != client_edit_version
            || !valid_blake3_hex(&saved_hash)
        {
            return Err("Invalid Save As source recovery binding".into());
        }
        let content_hash = blake3::hash(source.content.as_deref().unwrap().as_bytes())
            .to_hex()
            .to_string();
        if content_hash != saved_hash {
            return Err("Save As content hash does not match committed bytes".into());
        }
        let connection = self.connect()?;
        self.integrity(&connection)?;
        let transaction = connection.unchecked_transaction().map_err(err)?;
        let key = Self::upsert_project(&transaction, root, Some(destination_path))?;
        Self::require_active_session(&transaction, &key, session_token)?;
        let active_source: Option<i64> = transaction.query_row(
            "SELECT id FROM buffers WHERE project_key=?1 AND normalized_buffer_key='__untitled__' AND stable_binding='__untitled__' AND disposition='active' AND current_backend_document_id IS NULL",
            params![key], |row| row.get(0),
        ).optional().map_err(err)?;
        let source_id = if let Some(id) = active_source {
            id
        } else if let Some((_source_id, snapshot_id, sequence, document_id)) = transaction.query_row(
            "SELECT t.source_buffer_id, t.snapshot_id, s.sequence, b.current_backend_document_id FROM save_as_transitions t JOIN buffers source ON source.id=t.source_buffer_id JOIN buffers b ON b.id=t.destination_buffer_id JOIN snapshots s ON s.id=t.snapshot_id WHERE t.project_key=?1 AND source.stable_binding='__untitled__' AND t.session_token=?2 AND t.destination_key=?3 AND t.content_blake3=?4 AND t.client_edit_version=?5",
            params![key, session_token, destination_path, saved_hash, client_edit_version as i64],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?, row.get::<_, i64>(3)?)),
        ).optional().map_err(err)? {
            transaction.commit().map_err(err)?;
            return Ok(SaveAsTransitionAck { snapshot_id, recovery_sequence: sequence, client_edit_version, destination_document_id: document_id as u64, durable: true });
        } else {
            let prior: Option<i64> = transaction.query_row(
                "SELECT t.id FROM save_as_transitions t JOIN buffers source ON source.id=t.source_buffer_id WHERE t.project_key=?1 AND source.stable_binding='__untitled__' AND t.session_token=?2 LIMIT 1",
                params![key, session_token], |row| row.get(0),
            ).optional().map_err(err)?;
            if prior.is_some() {
                return Err("Save As retry does not match the committed destination, content, or edit version".into());
            }
            return Err("Save As source recovery binding is absent or already transitioned".into());
        };
        if let Some((snapshot_id, sequence, document_id)) = transaction.query_row(
            "SELECT t.snapshot_id, s.sequence, b.current_backend_document_id FROM save_as_transitions t JOIN buffers b ON b.id=t.destination_buffer_id JOIN snapshots s ON s.id=t.snapshot_id WHERE t.project_key=?1 AND t.source_buffer_id=?2 AND t.session_token=?3 AND t.destination_key=?4 AND t.content_blake3=?5 AND t.client_edit_version=?6",
            params![key, source_id, session_token, destination_path, saved_hash, client_edit_version as i64],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?)),
        ).optional().map_err(err)? {
            transaction.commit().map_err(err)?;
            return Ok(SaveAsTransitionAck { snapshot_id, recovery_sequence: sequence, client_edit_version, destination_document_id: document_id as u64, durable: true });
        }
        let prior_transition: Option<i64> = transaction.query_row(
            "SELECT id FROM save_as_transitions WHERE project_key=?1 AND source_buffer_id=?2 AND session_token=?3",
            params![key, source_id, session_token], |row| row.get(0),
        ).optional().map_err(err)?;
        if prior_transition.is_some() {
            return Err(
                "Save As retry does not match the committed destination, content, or edit version"
                    .into(),
            );
        }
        let latest_source: Option<(i64, i64, Option<String>)> = transaction.query_row(
            "SELECT dirty, client_edit_version, content_blake3 FROM snapshots WHERE buffer_id=?1 ORDER BY sequence DESC LIMIT 1",
            params![source_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).optional().map_err(err)?;
        if latest_source != Some((1, client_edit_version as i64, Some(content_hash.clone()))) {
            return Err(
                "Save As source recovery binding no longer matches the committed editor snapshot"
                    .into(),
            );
        }
        let existing_destination: Option<i64> = transaction.query_row(
            "SELECT id FROM buffers WHERE project_key=?1 AND normalized_buffer_key=?2 AND disposition='active'",
            params![key, destination_path], |row| row.get(0),
        ).optional().map_err(err)?;
        if existing_destination.is_some() {
            return Err("Ambiguous Save As destination has an active recovery binding".into());
        }
        if destination_document_id == 0 {
            return Err("Save As destination document identity is invalid".into());
        }
        let mut clean = source.clone();
        clean.relative_path = Some(destination_path.to_owned());
        clean.document_id = Some(destination_document_id);
        clean.write_revision = write_revision;
        clean.sequence = 1;
        clean.client_edit_version = client_edit_version;
        clean.dirty = false;
        clean.content = None;
        clean.payload = None;
        clean.payload_hash = None;
        clean.payload_bytes = None;
        clean.baseline_hash = Some(saved_hash.clone());
        // Compatibility callers may use this lower-level transition directly,
        // but production Save As has already journalled the commit. Find that
        // durable key inside this same transaction instead of inheriting any
        // untrusted source metadata.
        let pending_operation: Option<String> = transaction.query_row(
            "SELECT operation_key FROM save_operations WHERE project_key=?1 AND session_token=?2 AND source_key='__untitled__' AND relative_path=?3 AND document_id=?4 AND write_revision=?5 AND client_edit_version=?6 AND content_blake3=?7",
            params![key, session_token, destination_path, destination_document_id as i64, write_revision as i64, client_edit_version as i64, saved_hash],
            |row| row.get(0),
        ).optional().map_err(err)?;
        clean.reconciliation_operation_key = pending_operation;
        clean.reconciliation_client_edit_version = clean
            .reconciliation_operation_key
            .as_ref()
            .map(|_| client_edit_version);
        let metadata = Self::metadata(&clean)?;
        transaction.execute(
            "INSERT INTO buffers(project_key, normalized_buffer_key, relative_path, stable_binding, current_backend_document_id, last_client_edit_version, latest_sequence, disposition) VALUES(?1, ?2, ?2, ?2, ?3, ?4, 1, 'active')",
            params![key, destination_path, destination_document_id as i64, client_edit_version as i64],
        ).map_err(err)?;
        let destination_id = transaction.last_insert_rowid();
        transaction.execute(
            "INSERT INTO snapshots(buffer_id, sequence, client_edit_version, write_revision, dirty, content, content_blake3, baseline_hash, ui_json, created_at_ms) VALUES(?1, 1, ?2, ?3, 0, NULL, NULL, ?4, ?5, ?6)",
            params![destination_id, client_edit_version as i64, write_revision as i64, saved_hash, metadata, now_ms()],
        ).map_err(err)?;
        let snapshot_id = transaction.last_insert_rowid();
        transaction
            .execute(
                "UPDATE buffers SET normalized_buffer_key=?2, disposition='retired' WHERE id=?1",
                params![source_id, format!("__retired__{source_id}")],
            )
            .map_err(err)?;
        transaction.execute(
            "INSERT INTO save_as_transitions(project_key, source_buffer_id, session_token, destination_key, content_blake3, client_edit_version, destination_buffer_id, snapshot_id, created_at_ms) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![key, source_id, session_token, destination_path, content_hash, client_edit_version as i64, destination_id, snapshot_id, now_ms()],
        ).map_err(err)?;
        transaction.execute(
            "INSERT INTO events(project_key, buffer_id, kind, snapshot_id, created_at_ms) VALUES(?1, ?2, 'save-as-transition', ?3, ?4)",
            params![key, source_id, snapshot_id, now_ms()],
        ).map_err(err)?;
        #[cfg(test)]
        if take_save_as_injected_fault(SaveAsInjectedFault::BeforeCommit) {
            return Err("Injected Save As failure before recovery commit".into());
        }
        transaction.commit().map_err(err)?;
        self.verify_sidecars()?;
        #[cfg(test)]
        if take_save_as_injected_fault(SaveAsInjectedFault::AfterCommitAckLoss) {
            return Err("Injected Save As acknowledgement loss after recovery commit".into());
        }
        Ok(SaveAsTransitionAck {
            snapshot_id,
            recovery_sequence: 1,
            client_edit_version,
            destination_document_id,
            durable: true,
        })
    }

    pub(crate) fn latest(&self, root: &str) -> Result<Option<RestoredRecovery>, String> {
        let connection = self.connect()?;
        self.integrity(&connection)?;
        let key = Self::project_key(root);
        let active_path: Option<String> = match connection
            .query_row(
                "SELECT active_path FROM projects WHERE project_key=?1 AND canonical_root=?2",
                params![key, root],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .map_err(err)?
        {
            Some(value) => value,
            None => return Ok(None),
        };
        let mut statement = connection.prepare("SELECT b.id, s.content, s.content_blake3, s.ui_json FROM buffers b JOIN snapshots s ON s.buffer_id=b.id WHERE b.project_key=?1 AND b.disposition='active' ORDER BY b.id, s.sequence DESC").map_err(err)?;
        let mut rows = statement.query(params![key]).map_err(err)?;
        let mut buffers = Vec::new();
        let mut accepted = std::collections::HashSet::new();
        let mut newest_seen = std::collections::HashSet::new();
        while let Some(row) = rows.next().map_err(err)? {
            let buffer_id: i64 = row.get(0).map_err(err)?;
            if accepted.contains(&buffer_id) {
                continue;
            }
            let is_newest = newest_seen.insert(buffer_id);
            let content: Option<String> = row.get(1).map_err(err)?;
            let checksum: Option<String> = row.get(2).map_err(err)?;
            let metadata: String = row.get(3).map_err(err)?;
            let mut buffer: RecoveryBuffer = match serde_json::from_str(&metadata) {
                Ok(value) => value,
                Err(_) if is_newest => return Err("RECOVERY CORRUPTION: newest snapshot metadata is unreadable; draft was not downgraded".into()),
                Err(_) => continue,
            };
            buffer.content = content;
            if buffer.dirty {
                let text = buffer
                    .content
                    .as_ref()
                    .ok_or_else(|| "Dirty recovery snapshot is missing content".to_string())?;
                if text.len() > RECOVERY_MAX_PAYLOAD_BYTES
                    || !checksum.as_deref().is_some_and(valid_blake3_hex)
                    || blake3::hash(text.as_bytes()).to_hex().as_str()
                        != checksum.as_deref().unwrap_or_default()
                {
                    if is_newest {
                        return Err("RECOVERY CORRUPTION: newest dirty snapshot checksum failed; draft was not downgraded".into());
                    }
                    continue;
                }
            }
            accepted.insert(buffer_id);
            buffers.push(buffer);
        }
        Ok(Some(RestoredRecovery {
            active_path,
            buffers,
        }))
    }

    /// Only committed snapshots or explicit terminal outcomes suppress V1.
    /// A project row created for UI metadata alone is deliberately insufficient.
    pub(crate) fn has_committed_recovery_or_terminal_migration(
        &self,
        root: &str,
    ) -> Result<bool, String> {
        let connection = self.connect()?;
        self.integrity(&connection)?;
        let key = Self::project_key(root);
        connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM snapshots s JOIN buffers b ON b.id=s.buffer_id WHERE b.project_key=?1) OR EXISTS(SELECT 1 FROM legacy_imports WHERE project_key=?1) OR EXISTS(SELECT 1 FROM events WHERE project_key=?1 AND kind='terminal-discard')",
            params![key], |row| row.get::<_, i64>(0)
        ).map(|value| value != 0).map_err(err)
    }

    /// Every disk-open is a new backend session. Historical bindings remain
    /// auditable but can never accept writes from fresh document IDs.
    pub(crate) fn rebind_session_binding(
        &self,
        root: &str,
        relative_path: &str,
        document_id: u64,
    ) -> Result<(), String> {
        self.rebind_session_bindings(root, None, &[(relative_path, document_id)], &[])
    }

    /// Re-opening a project never revives the prior process's document
    /// capabilities. In one SQLite transaction this retires every previous
    /// path binding, copies its newest durable recovery row (including bounded
    /// UI metadata) under the freshly allocated document ID, and publishes the
    /// fresh session token. Queued writers holding the old token cannot commit
    /// after this transaction.
    pub(crate) fn rebind_session_bindings_and_activate(
        &self,
        root: &str,
        token: &str,
        bindings: &[(String, u64)],
    ) -> Result<(), String> {
        Self::validate_session_token(token)?;
        self.rebind_session_bindings(
            root,
            Some(token),
            &bindings
                .iter()
                .map(|(path, id)| (path.as_str(), *id))
                .collect::<Vec<_>>(),
            &[],
        )
    }

    /// The restore handoff for pending save journals. The caller provides only
    /// disk facts it read through the active project's descriptor. The old
    /// operation is transferred only when the old live binding, latest recovery
    /// metadata, saved version/hash, on-disk hash/identity and pending state all
    /// agree. This occurs in the same transaction that retires old bindings and
    /// activates the new token.
    pub(crate) fn rebind_session_bindings_and_activate_with_operations(
        &self,
        root: &str,
        token: &str,
        bindings: &[(String, u64)],
        operations: &[SaveOperationRebind],
    ) -> Result<(), String> {
        Self::validate_session_token(token)?;
        self.rebind_session_bindings(
            root,
            Some(token),
            &bindings
                .iter()
                .map(|(path, id)| (path.as_str(), *id))
                .collect::<Vec<_>>(),
            operations,
        )
    }

    fn rebind_session_bindings(
        &self,
        root: &str,
        activate_token: Option<&str>,
        bindings: &[(&str, u64)],
        operations: &[SaveOperationRebind],
    ) -> Result<(), String> {
        let mut paths = std::collections::HashSet::new();
        let mut document_ids = std::collections::HashSet::new();
        for (path, document_id) in bindings {
            if path.is_empty()
                || *document_id == 0
                || !paths.insert(*path)
                || !document_ids.insert(*document_id)
            {
                return Err("Recovery rebind has duplicate or invalid document bindings".into());
            }
        }
        let connection = self.connect()?;
        self.integrity(&connection)?;
        let transaction = connection.unchecked_transaction().map_err(err)?;
        let key = Self::upsert_project(&transaction, root, None)?;
        let prior_token: Option<String> = transaction
            .query_row(
                "SELECT token FROM active_sessions WHERE project_key=?1 AND active=1",
                params![key],
                |row| row.get(0),
            )
            .optional()
            .map_err(err)?;
        let mut rebinds = std::collections::HashMap::new();
        for operation in operations {
            if !valid_blake3_hex(&operation.operation_key)
                || !valid_blake3_hex(&operation.content_blake3)
                || operation.file_identity.is_empty()
                || rebinds
                    .insert(operation.operation_key.as_str(), operation)
                    .is_some()
            {
                return Err("Invalid or duplicate save-operation restore evidence".into());
            }
        }
        // Establish eligibility against the old active binding before it is
        // retired. We intentionally fail closed per operation: a forged,
        // stale, cross-project, disk-changed, or newer-editor journal is never
        // inherited by a fresh token.
        let mut eligible = Vec::new();
        for operation in operations {
            let Some(old_token) = prior_token.as_deref() else {
                continue;
            };
            let journal: Option<(String, String, String, u64, u64, String, String)> = transaction.query_row(
                "SELECT session_token, source_key, relative_path, document_id, client_edit_version, content_blake3, file_identity FROM save_operations WHERE operation_key=?1 AND project_key=?2 AND recovery_checkpoint_durable IN (0,1) AND selector_durable IN (0,1)",
                params![operation.operation_key, key],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get::<_, i64>(3)? as u64, row.get::<_, i64>(4)? as u64, row.get(5)?, row.get(6)?)),
            ).optional().map_err(err)?;
            let Some((session, source, relative_path, old_document, version, hash, identity)) =
                journal
            else {
                continue;
            };
            if session != old_token
                || hash != operation.content_blake3
                || identity != operation.file_identity
            {
                continue;
            }
            // Save As starts from the untitled source, but may already have
            // committed its SQLite authority transfer before the process died.
            // Prefer that destination binding when it still carries the old
            // reserved document id; otherwise verify the live untitled source.
            let targets = if source == "__untitled__" {
                vec![relative_path.as_str(), "__untitled__"]
            } else {
                vec![relative_path.as_str()]
            };
            let mut binding = None;
            for target in targets {
                binding = transaction.query_row(
                    "SELECT b.normalized_buffer_key, b.current_backend_document_id, s.client_edit_version, s.content_blake3, s.baseline_hash, s.ui_json FROM buffers b JOIN snapshots s ON s.buffer_id=b.id WHERE b.project_key=?1 AND b.normalized_buffer_key=?2 AND b.disposition='active' ORDER BY s.sequence DESC LIMIT 1",
                    params![key, target],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<i64>>(1)?, row.get::<_, i64>(2)?, row.get::<_, Option<String>>(3)?, row.get::<_, Option<String>>(4)?, row.get::<_, String>(5)?)),
                ).optional().map_err(err)?;
                if binding.is_some() {
                    break;
                }
            }
            let Some((
                binding_key,
                binding_document,
                snapshot_version,
                snapshot_content,
                snapshot_baseline,
                metadata,
            )) = binding
            else {
                continue;
            };
            let metadata: RecoveryBuffer = match serde_json::from_str(&metadata) {
                Ok(value) => value,
                Err(_) => continue,
            };
            if snapshot_version as u64 != version
                || metadata.reconciliation_operation_key.as_deref()
                    != Some(operation.operation_key.as_str())
                || metadata.reconciliation_client_edit_version != Some(version)
                || (snapshot_content.as_deref() != Some(hash.as_str())
                    && snapshot_baseline.as_deref() != Some(hash.as_str()))
            {
                continue;
            }
            if source == "__untitled__" && binding_key == "__untitled__" {
                // A Save As before its authority transfer has an untitled
                // source. A transfer that already committed is represented by
                // the destination binding and is handled as a normal document.
                if binding_document.is_none() {
                    eligible.push((operation.operation_key.clone(), None));
                }
            } else if binding_document == Some(old_document as i64) {
                if let Some((_, fresh_document)) =
                    bindings.iter().find(|(path, _)| *path == relative_path)
                {
                    eligible.push((operation.operation_key.clone(), Some(*fresh_document)));
                }
            }
        }
        for (relative_path, document_id) in bindings {
            let old: Option<(i64, Option<i64>, i64, i64, String)> = transaction.query_row("SELECT id, current_backend_document_id, last_client_edit_version, latest_sequence, disposition FROM buffers WHERE project_key=?1 AND normalized_buffer_key=?2", params![key, relative_path], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?))).optional().map_err(err)?;
            if let Some((old_id, old_document_id, last_version, latest_sequence, disposition)) = old
            {
                if disposition == "active" && old_document_id == Some(*document_id as i64) {
                    continue;
                }
                #[allow(clippy::type_complexity)]
            let latest: Option<(i64, i64, i64, i64, Option<String>, Option<String>, Option<String>, String)> = transaction.query_row(
                "SELECT sequence, client_edit_version, write_revision, dirty, content, content_blake3, baseline_hash, ui_json FROM snapshots WHERE buffer_id=?1 ORDER BY sequence DESC LIMIT 1",
                params![old_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?, row.get(7)?)),
            ).optional().map_err(err)?;
                transaction
                .execute(
                    "UPDATE buffers SET normalized_buffer_key=?2, disposition='retired' WHERE id=?1",
                    params![old_id, format!("__retired__{old_id}")],
                )
                .map_err(err)?;
                let (replacement_version, replacement_sequence) = if disposition == "active" {
                    (last_version, latest_sequence)
                } else {
                    // A discarded binding is audit-only. A fresh session starts
                    // its own client-version namespace rather than inheriting an
                    // equality fence with no copied live snapshot.
                    (0, 0)
                };
                transaction.execute("INSERT INTO buffers(project_key, normalized_buffer_key, relative_path, stable_binding, current_backend_document_id, last_client_edit_version, latest_sequence, disposition) VALUES(?1, ?2, ?2, ?2, ?3, ?4, ?5, 'active')", params![key, relative_path, *document_id as i64, replacement_version, replacement_sequence]).map_err(err)?;
                let replacement_id = transaction.last_insert_rowid();
                if disposition == "active" {
                    if let Some((
                        sequence,
                        version,
                        revision,
                        dirty,
                        content,
                        checksum,
                        baseline,
                        ui_json,
                    )) = latest
                    {
                        let mut metadata: RecoveryBuffer =
                            serde_json::from_str(&ui_json).map_err(|_| {
                                "Recovery metadata is unreadable during session rebind".to_string()
                            })?;
                        metadata.relative_path = Some((*relative_path).into());
                        metadata.document_id = Some(*document_id);
                        metadata.sequence = sequence as u64;
                        let copied_ui = Self::metadata(&metadata)?;
                        transaction.execute(
                    "INSERT INTO snapshots(buffer_id, sequence, client_edit_version, write_revision, dirty, content, content_blake3, baseline_hash, ui_json, created_at_ms) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                    params![replacement_id, sequence, version, revision, dirty, content, checksum, baseline, copied_ui, now_ms()],
                ).map_err(err)?;
                        transaction.execute("INSERT INTO events(project_key, buffer_id, kind, snapshot_id, created_at_ms) VALUES(?1, ?2, 'session-rebind', ?3, ?4)", params![key, replacement_id, transaction.last_insert_rowid(), now_ms()]).map_err(err)?;
                    }
                }
            }
        }
        if let Some(token) = activate_token {
            for (operation_key, document_id) in eligible {
                let changed = match document_id {
                    Some(document_id) => transaction.execute(
                        "UPDATE save_operations SET session_token=?2, document_id=?3, updated_at_ms=?4 WHERE operation_key=?1 AND project_key=?5",
                        params![operation_key, token, document_id as i64, now_ms(), key],
                    ),
                    None => transaction.execute(
                        "UPDATE save_operations SET session_token=?2, updated_at_ms=?3 WHERE operation_key=?1 AND project_key=?4",
                        params![operation_key, token, now_ms(), key],
                    ),
                }.map_err(err)?;
                if changed != 1 {
                    return Err("Save-operation restore handoff became stale".into());
                }
            }
        }
        Self::retain_retired_history(&transaction, &key)?;
        if let Some(token) = activate_token {
            let epoch: i64 = transaction
                .query_row(
                    "SELECT COALESCE(MAX(epoch), 0) + 1 FROM active_sessions WHERE project_key=?1",
                    params![key],
                    |row| row.get(0),
                )
                .map_err(err)?;
            transaction.execute(
                "INSERT INTO active_sessions(project_key, token, active, epoch, updated_at_ms) VALUES(?1, ?2, 1, ?3, ?4) ON CONFLICT(project_key) DO UPDATE SET token=excluded.token, active=1, epoch=excluded.epoch, updated_at_ms=excluded.updated_at_ms",
                params![key, token, epoch, now_ms()],
            ).map_err(err)?;
        }
        transaction.commit().map_err(err)
    }

    /// Imports a fully validated V1 set in one transaction. A successful or
    /// deliberate `skipped-existing-v2` marker is terminal; a validation/I/O
    /// failure is not marked and remains retryable after the operator fixes the
    /// legacy bytes. Existing V2 buffers win identity conflicts.
    pub(crate) fn import_legacy(
        &self,
        root: &str,
        active_path: Option<&str>,
        manifest_hash: &str,
        buffers: &[RecoveryBuffer],
    ) -> Result<bool, String> {
        if !valid_blake3_hex(manifest_hash) || buffers.len() > RECOVERY_MAX_BUFFERS {
            return Err("Legacy recovery import metadata is invalid".into());
        }
        for buffer in buffers {
            validate_recovery_write_buffer(buffer)?;
            if buffer.client_edit_version != 0
                || buffer.payload.is_some()
                || buffer.payload_hash.is_some()
                || buffer.payload_bytes.is_some()
            {
                return Err("Legacy recovery import has untrusted runtime fields".into());
            }
        }
        let connection = self.connect()?;
        self.integrity(&connection)?;
        let transaction = connection.unchecked_transaction().map_err(err)?;
        let project_key = Self::upsert_project(&transaction, root, active_path)?;
        let marker: Option<String> = transaction
            .query_row(
                "SELECT outcome FROM legacy_imports WHERE project_key=?1",
                params![project_key],
                |row| row.get(0),
            )
            .optional()
            .map_err(err)?;
        if marker.is_some() {
            transaction.commit().map_err(err)?;
            return Ok(false);
        }
        let v2_rows: i64 = transaction.query_row(
            "SELECT COUNT(*) FROM snapshots s JOIN buffers b ON b.id=s.buffer_id WHERE b.project_key=?1",
            params![project_key], |row| row.get(0),
        ).map_err(err)?;
        if v2_rows != 0 {
            transaction.execute("INSERT INTO legacy_imports(project_key, legacy_manifest_hash, outcome, imported_at_ms) VALUES(?1, ?2, 'skipped-existing-v2', ?3)", params![project_key, manifest_hash, now_ms()]).map_err(err)?;
            transaction.commit().map_err(err)?;
            return Ok(false);
        }
        for legacy in buffers {
            let key = Self::buffer_key(legacy);
            transaction.execute("INSERT INTO buffers(project_key, normalized_buffer_key, relative_path, stable_binding, current_backend_document_id, last_client_edit_version, latest_sequence, disposition) VALUES(?1, ?2, ?3, ?2, ?4, ?5, 1, 'active')", params![project_key, key, legacy.relative_path, legacy.document_id.map(|id| id as i64), legacy.sequence as i64]).map_err(err)?;
            let buffer_id = transaction.last_insert_rowid();
            let mut persisted = legacy.clone();
            persisted.sequence = 1;
            persisted.client_edit_version = legacy.sequence.max(1);
            let metadata = Self::metadata(&persisted)?;
            let checksum = legacy
                .content
                .as_ref()
                .map(|content| blake3::hash(content.as_bytes()).to_hex().to_string());
            transaction.execute("INSERT INTO snapshots(buffer_id, sequence, client_edit_version, write_revision, dirty, content, content_blake3, baseline_hash, ui_json, created_at_ms) VALUES(?1, 1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)", params![buffer_id, persisted.client_edit_version as i64, legacy.write_revision as i64, legacy.dirty as i64, legacy.content, checksum, legacy.baseline_hash, metadata, now_ms()]).map_err(err)?;
            transaction.execute("INSERT INTO events(project_key, buffer_id, kind, snapshot_id, created_at_ms) VALUES(?1, ?2, 'legacy-import', ?3, ?4)", params![project_key, buffer_id, transaction.last_insert_rowid(), now_ms()]).map_err(err)?;
        }
        transaction.execute("INSERT INTO legacy_imports(project_key, legacy_manifest_hash, outcome, imported_at_ms) VALUES(?1, ?2, 'imported-v1', ?3)", params![project_key, manifest_hash, now_ms()]).map_err(err)?;
        #[cfg(test)]
        if take_legacy_import_fault(LegacyImportFault::BeforeCommit) {
            return Err("Injected legacy import failure before commit".into());
        }
        transaction.commit().map_err(err)?;
        #[cfg(test)]
        if take_legacy_import_fault(LegacyImportFault::AfterCommit) {
            return Err("Injected legacy import failure after commit".into());
        }
        self.verify_sidecars()?;
        Ok(true)
    }

    #[cfg(test)]
    pub(crate) fn legacy_import_test_state(
        &self,
        root: &str,
    ) -> Result<(Option<String>, i64, i64, Vec<i64>), String> {
        let connection = self.connect()?;
        let key = Self::project_key(root);
        let marker = connection
            .query_row(
                "SELECT outcome FROM legacy_imports WHERE project_key=?1",
                params![key],
                |row| row.get(0),
            )
            .optional()
            .map_err(err)?;
        let buffers = connection
            .query_row(
                "SELECT COUNT(*) FROM buffers WHERE project_key=?1",
                params![key],
                |row| row.get(0),
            )
            .map_err(err)?;
        let snapshots = connection
            .query_row(
                "SELECT COUNT(*) FROM snapshots s JOIN buffers b ON b.id=s.buffer_id WHERE b.project_key=?1",
                params![key],
                |row| row.get(0),
            )
            .map_err(err)?;
        let mut statement = connection
            .prepare("SELECT s.sequence FROM snapshots s JOIN buffers b ON b.id=s.buffer_id WHERE b.project_key=?1 ORDER BY b.id, s.sequence")
            .map_err(err)?;
        let sequences = statement
            .query_map(params![key], |row| row.get(0))
            .map_err(err)?
            .collect::<Result<Vec<i64>, _>>()
            .map_err(err)?;
        Ok((marker, buffers, snapshots, sequences))
    }

    #[cfg(test)]
    pub(crate) fn test_active_session_token(&self, root: &str) -> Result<Option<String>, String> {
        let connection = self.connect()?;
        let key = Self::project_key(root);
        connection
            .query_row(
                "SELECT token FROM active_sessions WHERE project_key=?1 AND active=1",
                params![key],
                |row| row.get(0),
            )
            .optional()
            .map_err(err)
    }

    pub(crate) fn integrity_check(&self) -> Result<(), String> {
        let connection = self.connect()?;
        self.integrity(&connection)?;
        let check: String = connection
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))
            .map_err(err)?;
        if check != "ok" {
            return Err(
                "Recovery database full integrity check failed; bytes were retained".into(),
            );
        }
        Ok(())
    }

    pub(crate) fn set_active_path(
        &self,
        root: &str,
        active_path: Option<&str>,
    ) -> Result<(), String> {
        let connection = self.connect()?;
        self.integrity(&connection)?;
        let transaction = connection.unchecked_transaction().map_err(err)?;
        Self::upsert_project(&transaction, root, active_path)?;
        transaction.commit().map_err(err)
    }

    #[cfg(test)]
    pub(crate) fn test_path(&self) -> PathBuf {
        self.path.clone()
    }

    #[cfg(test)]
    pub(crate) fn harden_storage_components_for_test(config_root: &Path) -> Result<(), String> {
        let (path, directory) = prepare_private_storage(config_root)?;
        Self { path, directory }.verify_sidecars()
    }

    #[cfg(test)]
    pub(crate) fn test_snapshot_stats(&self, root: &str) -> Result<(i64, i64, i64), String> {
        let connection = self.connect()?;
        let key = Self::project_key(root);
        connection
            .query_row(
                "SELECT COUNT(*), COALESCE(MAX(s.sequence), 0), COALESCE(SUM(length(s.content)), 0) FROM snapshots s JOIN buffers b ON b.id=s.buffer_id WHERE b.project_key=?1",
                params![key],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .map_err(err)
    }
}
