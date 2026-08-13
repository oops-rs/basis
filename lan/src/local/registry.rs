//! Per-user discovery for local lifecycle services.
//!
//! A task handle must remain useful after the command that created it exits.
//! The registry therefore contains only endpoint metadata and a bearer
//! capability; task state lives beside it in the daemon's journal. Paths are
//! derived from a stable FNV-1a digest rather than from the workspace text so
//! Unix socket/path limits never become a correctness condition (the actual
//! transport is loopback TCP, but short names are still useful on every OS).

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use fs2::{FileExt, lock_contended_error};
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;
use tokio::{net::TcpStream, time};
use uuid::Uuid;

use super::protocol::{Request, Response, VERSION, read_frame, write_frame};

const STARTUP_ATTEMPTS: usize = 100;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Descriptor {
    pub version: u8,
    pub instance: String,
    pub workspace: String,
    pub endpoint: String,
    pub token: String,
    pub pid: u32,
}

#[derive(Debug, Clone)]
pub(crate) struct Registry {
    root: PathBuf,
}

impl Registry {
    pub(crate) fn discover() -> io::Result<Self> {
        let root = if let Some(registry) = std::env::var_os("LAN_REGISTRY_DIR") {
            PathBuf::from(registry)
        } else if let Some(config) = std::env::var_os("LAN_CONFIG_DIR") {
            PathBuf::from(config).join("agents")
        } else if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
            PathBuf::from(runtime).join("lan")
        } else {
            // On macOS `temp_dir` is per-user; on Windows it is protected by
            // the user's temp ACL. The capability token still protects a
            // mistakenly shared directory.
            std::env::temp_dir().join("lan")
        };

        fs::create_dir_all(&root)?;
        restrict_directory(&root)?;
        Ok(Self { root })
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

    pub(crate) fn workspace_descriptor(&self, workspace: &Path) -> PathBuf {
        self.root
            .join(format!("workspace-{}.json", workspace_key(workspace)))
    }

    pub(crate) fn instance_descriptor(&self, instance: &str) -> PathBuf {
        self.root.join(format!("instance-{instance}.json"))
    }

    pub(crate) fn task_journal(&self, instance: &str) -> PathBuf {
        self.root.join(format!("tasks-{instance}.json"))
    }

    pub(crate) fn history_directory(&self, instance: &str) -> PathBuf {
        self.root.join("history").join(instance)
    }

    pub(crate) fn lock_path(&self, workspace: &Path) -> PathBuf {
        self.root
            .join(format!("workspace-{}.lock", workspace_key(workspace)))
    }

    pub(crate) fn read_workspace(&self, workspace: &Path) -> io::Result<Option<Descriptor>> {
        read_descriptor(&self.workspace_descriptor(workspace))
    }

    pub(crate) fn read_instance(&self, instance: &str) -> io::Result<Option<Descriptor>> {
        read_descriptor(&self.instance_descriptor(instance))
    }

    pub(crate) fn remove_descriptor(&self, descriptor: &Descriptor) -> io::Result<()> {
        // Keep the instance locator: an old task handle needs its workspace in
        // order to restart a crashed or idled service. The next owner replaces
        // it atomically with a fresh endpoint and capability.
        let workspace_path = self.workspace_descriptor(Path::new(&descriptor.workspace));
        if let Some(current) = read_descriptor(&workspace_path)?
            && current.instance == descriptor.instance
            && current.endpoint == descriptor.endpoint
            && current.token == descriptor.token
            && current.pid == descriptor.pid
        {
            let _ = fs::remove_file(workspace_path);
        }
        Ok(())
    }

    pub(crate) fn acquire(&self, workspace: &Path) -> io::Result<Reservation> {
        let path = self.lock_path(workspace);
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        file.try_lock_exclusive().map_err(|error| {
            io::Error::new(
                if is_lock_contended(&error) {
                    io::ErrorKind::AlreadyExists
                } else {
                    error.kind()
                },
                format!("{}: {error}", path.display()),
            )
        })?;
        file.set_len(0)?;
        writeln!(file, "{}", std::process::id())?;
        file.sync_all()?;
        restrict_file(&path)?;
        Ok(Reservation { file: Some(file) })
    }

    /// Reserves an unowned workspace lock without rewriting its contents.
    ///
    /// The lock itself, rather than the PID recorded in a descriptor or lock
    /// file, is the authoritative proof that a daemon still owns the
    /// workspace. This probe is intentionally non-destructive so a client
    /// recovering stale discovery metadata cannot impersonate a daemon owner.
    fn reserve_if_unowned(&self, workspace: &Path) -> io::Result<Option<Reservation>> {
        let path = self.lock_path(workspace);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        match file.try_lock_exclusive() {
            Ok(()) => {
                restrict_file(&path)?;
                Ok(Some(Reservation { file: Some(file) }))
            }
            Err(error) if is_lock_contended(&error) => Ok(None),
            Err(error) => Err(io::Error::new(
                error.kind(),
                format!("{}: {error}", path.display()),
            )),
        }
    }

    pub(crate) async fn ensure_daemon(&self, workspace: &Path) -> Result<Descriptor, String> {
        let workspace = canonical_workspace(workspace).map_err(|error| error.to_string())?;

        if let Some(descriptor) = self
            .read_workspace(&workspace)
            .map_err(|error| error.to_string())?
        {
            ensure_descriptor_workspace(&descriptor, &workspace)?;
            if probe(&descriptor).await {
                return Ok(descriptor);
            }

            // A failed handshake is not proof that discovery metadata is
            // stale. The filesystem lock is the authoritative daemon-owner
            // lease on every supported platform, including Windows where the
            // standard library exposes no reliable process-liveness probe.
            match self
                .reserve_if_unowned(&workspace)
                .map_err(|error| format!("inspect lan service owner: {error}"))?
            {
                None => {
                    for _ in 0..20 {
                        time::sleep(Duration::from_millis(50)).await;
                        if let Some(current) = self
                            .read_workspace(&workspace)
                            .map_err(|error| error.to_string())?
                        {
                            ensure_descriptor_workspace(&current, &workspace)?;
                            if probe(&current).await {
                                return Ok(current);
                            }
                        }
                    }

                    // The owner may have exited during the retry window.
                    // Recheck the lease before reporting it unavailable;
                    // capability-match removal below prevents an old
                    // observation from unlinking a replacement descriptor.
                    let Some(reservation) = self
                        .reserve_if_unowned(&workspace)
                        .map_err(|error| format!("inspect lan service owner: {error}"))?
                    else {
                        return Err(format!(
                            "the lan service for {} owns its workspace but did not accept a connection",
                            workspace.display()
                        ));
                    };
                    self.remove_descriptor(&descriptor)
                        .map_err(|error| format!("remove stale lan service: {error}"))?;
                    drop(reservation);
                }
                Some(reservation) => {
                    // Holding the lock proves that no daemon currently owns
                    // this workspace. Remove only the exact descriptor we
                    // probed while the reservation prevents a new daemon from
                    // publishing.
                    self.remove_descriptor(&descriptor)
                        .map_err(|error| format!("remove stale lan service: {error}"))?;
                    drop(reservation);
                }
            }
        }

        let executable =
            std::env::current_exe().map_err(|error| format!("locate lan executable: {error}"))?;
        let mut command = Command::new(executable);
        command
            .arg("__daemon")
            .arg("--workspace")
            .arg(&workspace)
            .arg("--registry")
            .arg(&self.root)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        detach(&mut command);
        let child = command
            .spawn()
            .map_err(|error| format!("start lan service: {error}"))?;
        drop(child);

        for _ in 0..STARTUP_ATTEMPTS {
            time::sleep(Duration::from_millis(50)).await;
            if let Some(descriptor) = self
                .read_workspace(&workspace)
                .map_err(|error| error.to_string())?
                && probe(&descriptor).await
            {
                return Ok(descriptor);
            }
        }

        Err(format!(
            "lan service did not become ready for {}",
            workspace.display()
        ))
    }

    pub(crate) fn descriptor_for_task(&self, task: &str) -> io::Result<Option<Descriptor>> {
        let Some((instance, task_id)) = valid_task_handle(task) else {
            return Ok(None);
        };
        debug_assert!(!task_id.is_empty());
        self.read_instance(instance)
    }
}

pub(crate) struct Reservation {
    file: Option<File>,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if let Some(file) = self.file.take() {
            let _ = FileExt::unlock(&file);
        }
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

pub(crate) fn new_token() -> String {
    Uuid::new_v4().to_string()
}

pub(crate) async fn request(
    descriptor: &Descriptor,
    operation: super::protocol::Operation,
) -> Result<Response, String> {
    let mut stream = TcpStream::connect(&descriptor.endpoint)
        .await
        .map_err(|error| format!("connect to lan service: {error}"))?;
    let request = Request {
        version: VERSION,
        id: 1,
        token: descriptor.token.clone(),
        operation,
    };
    write_frame(&mut stream, &request)
        .await
        .map_err(|error| format!("send request: {error}"))?;
    let response: Response = read_frame(&mut stream)
        .await
        .map_err(|error| format!("read response: {error}"))?;
    if response.version != VERSION || response.id != 1 {
        return Err(format!(
            "lan service returned mismatched response v{} id {}",
            response.version, response.id
        ));
    }
    Ok(response)
}

pub(crate) async fn probe(descriptor: &Descriptor) -> bool {
    let Ok(mut stream) = TcpStream::connect(&descriptor.endpoint).await else {
        return false;
    };
    let request = Request {
        version: VERSION,
        id: 0,
        token: descriptor.token.clone(),
        operation: super::protocol::Operation::Inbox {
            task: "__probe__".to_string(),
        },
    };
    if write_frame(&mut stream, &request).await.is_err() {
        return false;
    }
    time::timeout(
        Duration::from_millis(300),
        read_frame::<_, Response>(&mut stream),
    )
    .await
    .is_ok_and(|result| {
        result.is_ok_and(|response| {
            response.version == VERSION
                && response.id == 0
                && matches!(response.kind, super::protocol::ResponseKind::Ok)
                && response.payload["state"] == "ready"
        })
    })
}

fn read_descriptor(path: &Path) -> io::Result<Option<Descriptor>> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

pub(crate) fn write_descriptor(path: &Path, descriptor: &Descriptor) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(descriptor)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    write_private_atomic(path, &bytes)
}

/// Atomically replaces one private registry file on Unix and Windows.
///
/// `std::fs::rename` replaces an existing destination on Unix but not on
/// Windows. `NamedTempFile::persist` provides the overwrite operation on both,
/// which matters because every task transition rewrites the same journal.
pub(crate) fn write_private_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "registry path has no parent")
    })?;
    let mut temporary = NamedTempFile::new_in(parent)?;
    restrict_file(temporary.path())?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    restrict_file(path)?;
    sync_parent(parent)
}

fn ensure_descriptor_workspace(descriptor: &Descriptor, workspace: &Path) -> Result<(), String> {
    let described = canonical_workspace(Path::new(&descriptor.workspace))
        .map_err(|error| format!("resolve registered workspace: {error}"))?;
    if described == workspace {
        Ok(())
    } else {
        Err(format!(
            "lan service registry collision: {} describes {}, not {}",
            descriptor.instance,
            described.display(),
            workspace.display()
        ))
    }
}

fn valid_task_handle(task: &str) -> Option<(&str, &str)> {
    let (instance, task_id) = task.split_once('/')?;
    let valid_hex = |value: &str, length| {
        value.len() == length && value.bytes().all(|byte| byte.is_ascii_hexdigit())
    };
    (valid_hex(instance, 16) && valid_hex(task_id, 32)).then_some((instance, task_id))
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
fn restrict_directory(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn restrict_file(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict_file(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn detach(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

#[cfg(windows)]
fn detach(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
}

#[cfg(not(any(unix, windows)))]
fn detach(_command: &mut Command) {}

fn is_lock_contended(error: &io::Error) -> bool {
    let expected = lock_contended_error();
    match (error.raw_os_error(), expected.raw_os_error()) {
        // The OS code is the discriminant on Windows (ERROR_LOCK_VIOLATION)
        // and Unix (EWOULDBLOCK/EAGAIN). Prefer it whenever both are present
        // so an unrelated `Uncategorized` error cannot look like contention.
        (Some(actual), Some(expected)) => actual == expected,
        // Keep the kind-only fallback for targets where fs2 cannot expose an
        // OS code. `lock_contended_error` is the crate's contract in that case.
        _ => error.kind() == expected.kind(),
    }
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
    fn task_ids_split_at_the_instance_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Registry::from_path(dir.path()).unwrap();
        let descriptor = Descriptor {
            version: VERSION,
            instance: "0123456789abcdef".to_string(),
            workspace: "/repo".to_string(),
            endpoint: "127.0.0.1:1".to_string(),
            token: "token".to_string(),
            pid: 1,
        };
        write_descriptor(
            &registry.instance_descriptor("0123456789abcdef"),
            &descriptor,
        )
        .unwrap();
        assert_eq!(
            registry
                .descriptor_for_task("0123456789abcdef/0123456789abcdef0123456789abcdef")
                .unwrap()
                .unwrap()
                .instance,
            "0123456789abcdef"
        );
        assert!(
            registry
                .descriptor_for_task("../../outside/file")
                .unwrap()
                .is_none(),
            "an opaque handle must not become a registry path"
        );
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
    fn full_workspace_path_detects_a_digest_collision() {
        let left = tempfile::tempdir().unwrap();
        let right = tempfile::tempdir().unwrap();
        let descriptor = Descriptor {
            version: VERSION,
            instance: "0123456789abcdef".to_string(),
            workspace: left.path().to_string_lossy().into_owned(),
            endpoint: "127.0.0.1:1".to_string(),
            token: "token".to_string(),
            pid: 1,
        };

        let error = ensure_descriptor_workspace(&descriptor, right.path())
            .expect_err("a digest alone must not select another workspace");
        assert!(error.contains("collision"), "{error}");
    }

    #[test]
    fn lock_contention_error_is_recognized_across_platforms() {
        assert!(is_lock_contended(&lock_contended_error()));
    }

    #[test]
    fn acquire_maps_real_lock_contention_to_already_exists() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Registry::from_path(dir.path()).unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let _first = registry.acquire(workspace.path()).unwrap();

        let error = match registry.acquire(workspace.path()) {
            Ok(_) => panic!("a second owner must not acquire the workspace lease"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    }

    #[test]
    fn stale_descriptor_can_be_removed_while_the_owner_lock_is_free() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Registry::from_path(dir.path()).unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let descriptor = Descriptor {
            version: VERSION,
            instance: "0123456789abcdef".to_string(),
            workspace: workspace.path().to_string_lossy().into_owned(),
            endpoint: "127.0.0.1:1".to_string(),
            token: "stale-token".to_string(),
            pid: u32::MAX,
        };
        let descriptor_path = registry.workspace_descriptor(workspace.path());
        let lock_path = registry.lock_path(workspace.path());
        write_descriptor(&descriptor_path, &descriptor).unwrap();
        fs::write(&lock_path, b"previous owner\n").unwrap();

        let reservation = registry
            .reserve_if_unowned(workspace.path())
            .unwrap()
            .expect("a free lock proves that the descriptor has no owner");
        assert_eq!(
            fs::read(&lock_path).unwrap(),
            b"previous owner\n",
            "the recovery reservation must not rewrite daemon-owner metadata"
        );
        registry.remove_descriptor(&descriptor).unwrap();
        drop(reservation);

        assert!(!descriptor_path.exists());
        assert!(registry.acquire(workspace.path()).is_ok());
    }

    #[test]
    fn contended_owner_lock_preserves_an_unresponsive_descriptor() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Registry::from_path(dir.path()).unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let descriptor = Descriptor {
            version: VERSION,
            instance: "0123456789abcdef".to_string(),
            workspace: workspace.path().to_string_lossy().into_owned(),
            endpoint: "127.0.0.1:1".to_string(),
            token: "owned-token".to_string(),
            pid: std::process::id(),
        };
        let descriptor_path = registry.workspace_descriptor(workspace.path());
        write_descriptor(&descriptor_path, &descriptor).unwrap();
        let _owner = registry.acquire(workspace.path()).unwrap();

        assert!(
            registry
                .reserve_if_unowned(workspace.path())
                .unwrap()
                .is_none(),
            "contention is authoritative evidence of a daemon owner"
        );
        assert_eq!(
            read_descriptor(&descriptor_path).unwrap().unwrap().token,
            descriptor.token,
            "a failed endpoint probe must not unlink an owned descriptor"
        );
    }

    #[test]
    fn descriptor_removal_requires_the_current_capability_and_owner() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Registry::from_path(dir.path()).unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let descriptor = Descriptor {
            version: VERSION,
            instance: "0123456789abcdef".to_string(),
            workspace: workspace.path().to_string_lossy().into_owned(),
            endpoint: "127.0.0.1:1234".to_string(),
            token: "current-token".to_string(),
            pid: 42,
        };
        let path = registry.workspace_descriptor(workspace.path());
        write_descriptor(&path, &descriptor).unwrap();

        let mut replacement = descriptor.clone();
        replacement.endpoint = "127.0.0.1:5678".to_string();
        registry.remove_descriptor(&replacement).unwrap();
        assert!(
            path.exists(),
            "an endpoint replacement must survive cleanup"
        );

        replacement.endpoint = descriptor.endpoint.clone();
        replacement.pid = 43;
        registry.remove_descriptor(&replacement).unwrap();
        assert!(path.exists(), "a PID replacement must survive cleanup");

        replacement.pid = descriptor.pid;
        replacement.token = "replacement-token".to_string();
        registry.remove_descriptor(&replacement).unwrap();
        assert!(
            path.exists(),
            "a replacement descriptor must survive cleanup"
        );

        registry.remove_descriptor(&descriptor).unwrap();
        assert!(!path.exists());
    }
}
