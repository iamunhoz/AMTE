use std::{
    ffi::CString,
    fs::{self, File},
    io::{Read, Write},
    os::fd::FromRawFd,
    path::{Component, Path, PathBuf},
    sync::Mutex,
};

use pulldown_cmark::{Options, Parser, html};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, State};
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
struct EntryDto {
    name: String,
    relative_path: String,
    kind: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct DocumentDto {
    project_id: u64,
    document_id: u64,
    relative_path: String,
    content: String,
    write_revision: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct RestoreDto {
    project: ProjectDto,
    document: Option<DocumentDto>,
}

#[derive(Debug)]
struct ActiveDocument {
    relative_path: PathBuf,
    identity: u64,
    file_identity: FileIdentity,
    write_revision: u64,
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
    active: Option<ActiveDocument>,
}

#[derive(Default)]
struct ProjectState {
    project: Option<ActiveProject>,
    next_identity: u64,
}
#[derive(Default)]
struct ProjectAuthority(Mutex<ProjectState>);

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
            active: None,
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

fn document_dto(project: &ActiveProject, active: &ActiveDocument, content: String) -> DocumentDto {
    DocumentDto {
        project_id: project.identity,
        document_id: active.identity,
        relative_path: relative_string(&active.relative_path),
        content,
        write_revision: active.write_revision,
    }
}
fn activate_document(
    state: &mut ProjectState,
    relative_path: PathBuf,
    file_identity: FileIdentity,
    write_revision: u64,
) -> Result<u64, String> {
    let identity = ProjectAuthority::next_id(state);
    let project = state
        .project
        .as_mut()
        .ok_or_else(|| "No project is open".to_string())?;
    project.active = Some(ActiveDocument {
        relative_path,
        identity,
        file_identity,
        write_revision,
    });
    Ok(identity)
}

fn write_active_revision(
    project: &mut ActiveProject,
    document_id: u64,
    revision: u64,
    content: &str,
) -> Result<(), String> {
    let active = project
        .active
        .as_ref()
        .ok_or_else(|| "No active project document".to_string())?;
    if active.identity != document_id {
        return Err("The active document changed before save".into());
    }
    if revision < active.write_revision {
        return Err("A newer document revision was already saved".into());
    }
    let relative_path = active.relative_path.clone();
    let expected_identity = active.file_identity;
    let new_identity = write_file_at(
        &project.root_fd,
        &relative_path,
        content,
        Some(expected_identity),
        false,
    )?;
    let active = project
        .active
        .as_mut()
        .expect("active document remains active");
    active.file_identity = new_identity;
    active.write_revision = revision;
    Ok(())
}

#[tauri::command]
fn render_markdown(markdown: String) -> String {
    let mut rendered = String::new();
    html::push_html(&mut rendered, Parser::new_ext(&markdown, Options::all()));
    ammonia::clean(&rendered)
}

#[tauri::command]
async fn open_project(
    app: AppHandle,
    authority: State<'_, ProjectAuthority>,
) -> Result<Option<ProjectDto>, String> {
    let Some(selected) = app.dialog().file().blocking_pick_folder() else {
        return Ok(None);
    };
    let path = selected
        .into_path()
        .map_err(|e| format!("Could not use selected project: {e}"))?;
    let root = canonical_project_root(&path)?;
    let project = authority.activate_project(root.clone())?;
    ProjectStore::for_app(&app)?.write(&root, None)?;
    Ok(Some(project))
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
    activate_document(&mut state, relative.clone(), file_identity, 0)?;
    let project = state.project.as_ref().expect("project remains active");
    ProjectStore::for_app(&app)?.write(&project.root, Some(&relative))?;
    Ok(document_dto(
        project,
        project.active.as_ref().expect("document activated"),
        content,
    ))
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
    let project = state.project.as_mut().expect("project remains active");
    write_active_revision(project, document_id, revision, content)?;
    let relative_path = project
        .active
        .as_ref()
        .expect("active document remains active")
        .relative_path
        .clone();
    ProjectStore::for_app(app)?.write(&project.root, Some(&relative_path))?;
    let active = project
        .active
        .as_ref()
        .expect("active document remains active");
    Ok(document_dto(project, active, content.into()))
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
        if project.identity != project_id || project.active.is_some() {
            return Err("Save As requires the current project with no active file".into());
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
    if project.identity != project_id || project.root != root || project.active.is_some() {
        return Err("The project changed before Save As".into());
    }
    let file_identity = write_file_at(&project.root_fd, &relative, &content, None, true)?;
    activate_document(&mut state, relative.clone(), file_identity, revision)?;
    let project = state.project.as_ref().expect("project remains active");
    let active = project.active.as_ref().expect("document activated");
    ProjectStore::for_app(&app)?.write(&project.root, Some(&relative))?;
    Ok(Some(document_dto(project, active, content)))
}

#[tauri::command]
fn autosave_document(
    authority: State<'_, ProjectAuthority>,
    project_id: u64,
    document_id: u64,
    revision: u64,
    content: String,
) -> Result<(), String> {
    let mut state = authority
        .0
        .lock()
        .map_err(|_| "Project state is unavailable".to_string())?;
    let project = state
        .project
        .as_ref()
        .ok_or_else(|| "No active project document is authorized for autosave".to_string())?;
    if project.identity != project_id {
        return Err("The project changed before autosave".into());
    }
    let project = state.project.as_mut().expect("project remains active");
    write_active_revision(project, document_id, revision, &content)?;
    Ok(())
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
    let document = if let Some(raw_relative) = stored.relative_path {
        let relative = match validate_relative(&raw_relative, true) {
            Ok(value) => value,
            Err(_) => {
                ProjectStore::for_app(app)?.write(&root, None)?;
                return Ok(Some(RestoreDto {
                    project,
                    document: None,
                }));
            }
        };
        let mut state = authority
            .0
            .lock()
            .map_err(|_| "Project state is unavailable".to_string())?;
        let active_project = state
            .project
            .as_ref()
            .ok_or_else(|| "No project is open".to_string())?;
        match read_file_at(&active_project.root_fd, &relative) {
            Ok((content, file_identity)) => {
                activate_document(&mut state, relative, file_identity, 0)?;
                let active_project = state.project.as_ref().expect("project remains active");
                Some(document_dto(
                    active_project,
                    active_project.active.as_ref().expect("document active"),
                    content,
                ))
            }
            Err(_) => {
                ProjectStore::for_app(app)?.write(&root, None)?;
                None
            }
        }
    } else {
        None
    };
    Ok(Some(RestoreDto { project, document }))
}

#[tauri::command]
fn restore_last_project(
    app: AppHandle,
    authority: State<'_, ProjectAuthority>,
) -> Result<Option<RestoreDto>, String> {
    let store = ProjectStore::for_app(&app)?;
    if let Some(stored) = store.read() {
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

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(ProjectAuthority::default())
        .invoke_handler(tauri::generate_handler![
            render_markdown,
            open_project,
            list_project_directory,
            open_project_document,
            save_document,
            save_document_as,
            autosave_document,
            restore_last_project,
            set_application_zoom
        ])
        .run(tauri::generate_context!())
        .expect("error while running amte");
}

#[cfg(test)]
mod tests {
    use super::*;
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
            activate_document(&mut state, PathBuf::from("document.md"), identity, 0).unwrap()
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
            write_active_revision(
                state.project.as_mut().unwrap(),
                document_id,
                9,
                "manual newest",
            )
            .unwrap();
            assert!(
                write_active_revision(
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
            activate_document(&mut locked, PathBuf::from("a.md"), file_identity, 0).unwrap();
        }
        let old_doc = state
            .0
            .lock()
            .unwrap()
            .project
            .as_ref()
            .unwrap()
            .active
            .as_ref()
            .unwrap()
            .identity;
        state
            .activate_project(canonical_project_root(&two).unwrap())
            .unwrap();
        let locked = state.0.lock().unwrap();
        let project = locked.project.as_ref().unwrap();
        assert_ne!(project.identity, one_id);
        assert!(project.active.is_none());
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
            .active
            .as_ref()
            .ok_or_else(|| "no document".to_string())?;
        if active.identity != document_id {
            return Err("stale document".into());
        }
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
}
