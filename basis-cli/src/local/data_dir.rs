//! The global data directory: where every agent's durable state lives.
//!
//! ADR-0019. One root, workspace-keyed, holds task metadata and mentra's
//! store; the repository's `.basis/` stays configuration only. Paths are derived
//! from a stable FNV-1a digest of the canonical workspace path rather than
//! from the workspace text, so path-length limits never become a correctness
//! condition and the task-handle grammar (`<16 hex>/<32 hex>`) stays stable.

use std::{
    fs::{self, File},
    io::{self, Write},
    path::{Path, PathBuf},
};

use tempfile::NamedTempFile;

/// The file that pins which workspace a digest directory describes. Read back
/// and compared on every open: the digest-collision check that used to live on
/// the daemon's descriptor.
const WORKSPACE_MARKER: &str = "workspace";

#[derive(Debug, Clone)]
pub(crate) struct DataDir {
    root: PathBuf,
}

impl DataDir {
    /// Resolves the root: `BASIS_DATA_DIR`, else an absolute `XDG_DATA_HOME`,
    /// else the platform data home. Created private (0700) on first use.
    pub(crate) fn discover() -> io::Result<Self> {
        let root = if let Some(data) = std::env::var_os("BASIS_DATA_DIR") {
            PathBuf::from(data)
        } else if let Some(xdg) = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
        {
            xdg.join("basis")
        } else {
            platform_data_home()?.join("basis")
        };
        Self::from_path(root)
    }

    pub(crate) fn from_path(path: impl Into<PathBuf>) -> io::Result<Self> {
        let root = path.into();
        fs::create_dir_all(&root)?;
        restrict_directory(&root)?;
        Ok(Self { root })
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    fn workspace_dir(&self, key: &str) -> PathBuf {
        self.root.join("workspaces").join(key)
    }

    /// mentra's store for one workspace: the target of
    /// `RuntimeBuilder::with_store_dir` for every task in it.
    pub(crate) fn store_dir(&self, key: &str) -> PathBuf {
        self.workspace_dir(key).join("store")
    }

    pub(crate) fn agents_dir(&self, key: &str) -> PathBuf {
        self.workspace_dir(key).join("agents")
    }

    /// The agent directory a validated handle names, without requiring it to
    /// exist. `None` when the handle does not fit the grammar, so an opaque
    /// string can never become a path outside the root.
    pub(crate) fn agent_dir(&self, task: &str) -> Option<AgentPaths> {
        let (key, id) = valid_task_handle(task)?;
        Some(AgentPaths {
            dir: self.agents_dir(key).join(id),
        })
    }

    /// The workspace a key's directory says it describes, or `None` when no
    /// task has ever run there.
    ///
    /// The read-only half of [`ensure_workspace`](Self::ensure_workspace), for
    /// a verb that observes rather than mints: `basis list` must be able to
    /// report that a workspace has no tasks without creating the directory
    /// that would prove it wrong. A marker that cannot be read is treated as
    /// absent — the collision check it exists for is the caller's to make
    /// against the path it returns.
    pub(crate) fn described_workspace(&self, key: &str) -> Option<PathBuf> {
        let marker = self.workspace_dir(key).join(WORKSPACE_MARKER);
        let described = fs::read_to_string(marker).ok()?;
        Some(PathBuf::from(described.trim_end()))
    }

    /// Ensures the workspace's directory tree exists and that the digest still
    /// describes this workspace, which guards against an FNV collision
    /// silently merging two workspaces' agents.
    pub(crate) fn ensure_workspace(&self, workspace: &Path) -> Result<String, String> {
        let canonical = canonical_workspace(workspace)
            .map_err(|error| format!("resolve workspace {}: {error}", workspace.display()))?;
        let key = workspace_key(&canonical);
        let dir = self.workspace_dir(&key);
        fs::create_dir_all(self.agents_dir(&key))
            .and_then(|()| restrict_directory(&dir))
            .map_err(|error| format!("create workspace data directory: {error}"))?;

        let marker = dir.join(WORKSPACE_MARKER);
        match fs::read_to_string(&marker) {
            Ok(described) => {
                if Path::new(described.trim_end()) == canonical {
                    Ok(key)
                } else {
                    Err(format!(
                        "workspace key collision: {key} describes {}, not {}",
                        described.trim_end(),
                        canonical.display()
                    ))
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let mut text = canonical.to_string_lossy().into_owned();
                text.push('\n');
                write_private_atomic(&marker, text.as_bytes())
                    .map_err(|error| format!("record workspace path: {error}"))?;
                Ok(key)
            }
            Err(error) => Err(format!("read workspace record: {error}")),
        }
    }
}

/// Every per-agent file, from one directory.
#[derive(Debug, Clone)]
pub(crate) struct AgentPaths {
    dir: PathBuf,
}

impl AgentPaths {
    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    pub(crate) fn exists(&self) -> bool {
        self.dir.is_dir()
    }

    pub(crate) fn meta(&self) -> PathBuf {
        self.dir.join("meta.json")
    }

    pub(crate) fn inbox(&self) -> PathBuf {
        self.dir.join("inbox.json")
    }

    pub(crate) fn inbox_lock(&self) -> PathBuf {
        self.dir.join("inbox.lock")
    }

    pub(crate) fn events(&self) -> PathBuf {
        self.dir.join("events.jsonl")
    }

    pub(crate) fn cancel_marker(&self) -> PathBuf {
        self.dir.join("cancel")
    }

    pub(crate) fn terminal(&self) -> PathBuf {
        self.dir.join("terminal.json")
    }

    pub(crate) fn attach_lock(&self) -> PathBuf {
        self.dir.join("attach.lock")
    }
}

pub(crate) fn canonical_workspace(path: &Path) -> io::Result<PathBuf> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    fs::canonicalize(path)
}

pub(crate) fn workspace_key(path: &Path) -> String {
    let canonical = canonical_workspace(path)
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string_lossy().into_owned());
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in canonical.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

pub(crate) fn valid_task_handle(task: &str) -> Option<(&str, &str)> {
    let (key, task_id) = task.split_once('/')?;
    let valid_hex = |value: &str, length| {
        value.len() == length && value.bytes().all(|byte| byte.is_ascii_hexdigit())
    };
    (valid_hex(key, 16) && valid_hex(task_id, 32)).then_some((key, task_id))
}

/// Atomically replaces one private file on Unix and Windows.
///
/// `std::fs::rename` replaces an existing destination on Unix but not on
/// Windows. `NamedTempFile::persist` provides the overwrite operation on both,
/// which matters because meta and inbox rewrites replace the same file. A
/// failed write leaves the previous complete file in place.
pub(crate) fn write_private_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "data path has no parent"))?;
    let mut temporary = NamedTempFile::new_in(parent)?;
    restrict_file(temporary.path())?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    restrict_file(path)?;
    sync_parent(parent)
}

#[cfg(unix)]
fn sync_parent(parent: &Path) -> io::Result<()> {
    File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent(_parent: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
pub(crate) fn restrict_directory(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn restrict_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
pub(crate) fn restrict_file(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn restrict_file(_path: &Path) -> io::Result<()> {
    Ok(())
}

/// The platform's per-user data home, hand-rolled from the environment the
/// same way `workspace_key` hand-rolls FNV: too small to take a dependency on.
fn platform_data_home() -> io::Result<PathBuf> {
    #[cfg(windows)]
    {
        if let Some(appdata) = std::env::var_os("APPDATA") {
            return Ok(PathBuf::from(appdata));
        }
    }
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        #[cfg(target_os = "macos")]
        return Ok(home.join("Library").join("Application Support"));
        #[cfg(not(target_os = "macos"))]
        return Ok(home.join(".local").join("share"));
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "no data directory: set BASIS_DATA_DIR",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_keys_are_stable_and_path_independent_in_length() {
        let key = workspace_key(Path::new("/a/very/long/workspace/path"));
        assert_eq!(key.len(), 16);
        assert_eq!(key, workspace_key(Path::new("/a/very/long/workspace/path")));
    }

    #[test]
    fn an_opaque_handle_never_becomes_a_path_outside_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let data = DataDir::from_path(dir.path()).unwrap();
        assert!(data.agent_dir("../../outside/file").is_none());
        assert!(data.agent_dir("0123456789abcdef").is_none());
        let paths = data
            .agent_dir("0123456789abcdef/0123456789abcdef0123456789abcdef")
            .expect("a well-formed handle resolves");
        assert!(paths.dir().starts_with(dir.path()));
    }

    #[test]
    fn atomic_private_writes_replace_an_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");

        write_private_atomic(&path, b"first").unwrap();
        write_private_atomic(&path, b"second").unwrap();

        assert_eq!(fs::read(path).unwrap(), b"second");
    }

    #[test]
    fn a_failed_atomic_write_leaves_the_previous_complete_file() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("nested");
        fs::create_dir(&nested).unwrap();
        let path = nested.join("state.json");
        write_private_atomic(&path, b"kept").unwrap();
        let contents = fs::read(&path).unwrap();

        // Remove the parent's write permission so the temporary file cannot be
        // created; the destination must be untouched by the failure.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&nested, fs::Permissions::from_mode(0o500)).unwrap();
            assert!(write_private_atomic(&path, b"lost").is_err());
            fs::set_permissions(&nested, fs::Permissions::from_mode(0o700)).unwrap();
        }
        assert_eq!(fs::read(&path).unwrap(), contents);
    }

    #[test]
    fn the_workspace_record_detects_a_digest_collision() {
        let dir = tempfile::tempdir().unwrap();
        let data = DataDir::from_path(dir.path()).unwrap();
        let left = tempfile::tempdir().unwrap();
        let right = tempfile::tempdir().unwrap();

        let key = data.ensure_workspace(left.path()).expect("first use");
        assert_eq!(key, data.ensure_workspace(left.path()).expect("reopen"));

        // Impersonate a digest collision by rewriting the marker.
        let canonical = canonical_workspace(right.path()).unwrap();
        fs::write(
            dir.path()
                .join("workspaces")
                .join(&key)
                .join(WORKSPACE_MARKER),
            format!("{}\n", canonical.display()),
        )
        .unwrap();
        let error = data
            .ensure_workspace(left.path())
            .expect_err("a digest alone must not select another workspace");
        assert!(error.contains("collision"), "{error}");
    }
}
