//! Where a session lives on disk: its socket and lock in
//! a private runtime directory, its log in the state directory.

use std::ffi::OsString;
use std::fs::{self, DirBuilder};
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _};
use std::path::{Path, PathBuf};

use plx_protocol::hello::PROTOCOL_VERSION;

/// A session's name: 1 to 64 characters of ASCII letters, digits, `-`, `_`
/// and `.`, not starting with `.`, so it is always a plain file name.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct SessionName(String);

/// Why a string is not a [`SessionName`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SessionNameError {
    #[error("a session name cannot be empty")]
    Empty,
    #[error("a session name has at most {} characters", SessionName::MAX_LEN)]
    TooLong,
    #[error("a session name holds only ASCII letters, digits, '-', '_' and '.'")]
    Character,
    #[error("a session name cannot start with '.'")]
    Hidden,
}

impl SessionName {
    pub const MAX_LEN: usize = 64;

    /// # Errors
    ///
    /// When `name` is not a valid session name.
    pub fn new(name: &str) -> Result<Self, SessionNameError> {
        if name.is_empty() {
            return Err(SessionNameError::Empty);
        }
        if name.len() > Self::MAX_LEN {
            return Err(SessionNameError::TooLong);
        }
        if !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        {
            return Err(SessionNameError::Character);
        }
        if name.starts_with('.') {
            return Err(SessionNameError::Hidden);
        }
        Ok(Self(name.into()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The environment variables paths come from, read once.
#[derive(Debug, PartialEq, Eq)]
pub struct Env {
    pub runtime_dir: Option<OsString>,
    pub state_home: Option<OsString>,
    pub home: Option<OsString>,
}

impl Env {
    /// The process's environment.
    #[must_use]
    pub fn current() -> Self {
        Self {
            runtime_dir: std::env::var_os("XDG_RUNTIME_DIR"),
            state_home: std::env::var_os("XDG_STATE_HOME"),
            home: std::env::var_os("HOME"),
        }
    }
}

/// Why a session's paths cannot be used.
#[derive(Debug, thiserror::Error)]
pub enum PathsError {
    #[error("XDG_RUNTIME_DIR is not set; plx keeps its sockets there")]
    NoRuntimeDir,
    #[error("neither XDG_STATE_HOME nor HOME is set; plx keeps its logs there")]
    NoStateDir,
    #[error("cannot create {path}: {source}")]
    Create {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path} must be owned by you and private (mode 0700); it is uid {uid}, mode {mode:o}")]
    NotPrivate { path: PathBuf, uid: u32, mode: u32 },
}

/// A session's files.
#[derive(Debug, PartialEq, Eq)]
pub struct Paths {
    /// `$XDG_RUNTIME_DIR/plx/v{PROTOCOL_VERSION}`: private.
    pub runtime: PathBuf,
    pub socket: PathBuf,
    pub lock: PathBuf,
    /// `$XDG_STATE_HOME/plx` (by the XDG spec, `$HOME/.local/state/plx`
    /// when `XDG_STATE_HOME` is unset or empty).
    pub state: PathBuf,
    pub log: PathBuf,
}

impl Paths {
    /// The paths of session `name`, without touching the file system.
    ///
    /// # Errors
    ///
    /// When the environment names no runtime or state directory.
    pub fn of(name: &SessionName, env: &Env) -> Result<Self, PathsError> {
        let runtime_dir = env
            .runtime_dir
            .as_ref()
            .filter(|dir| !dir.is_empty())
            .ok_or(PathsError::NoRuntimeDir)?;
        let runtime = Path::new(runtime_dir)
            .join("plx")
            .join(format!("v{PROTOCOL_VERSION}"));
        let state_home = match env.state_home.as_ref().filter(|dir| !dir.is_empty()) {
            Some(dir) => PathBuf::from(dir),
            // The XDG Base Directory spec's own default.
            None => Path::new(
                env.home
                    .as_ref()
                    .filter(|dir| !dir.is_empty())
                    .ok_or(PathsError::NoStateDir)?,
            )
            .join(".local/state"),
        };
        let state = state_home.join("plx");
        Ok(Self {
            socket: runtime.join(format!("{}.sock", name.as_str())),
            lock: runtime.join(format!("{}.lock", name.as_str())),
            log: state.join(format!("{}.log", name.as_str())),
            runtime,
            state,
        })
    }

    /// The directory of every session's socket, for listing them.
    ///
    /// # Errors
    ///
    /// When the environment names no runtime directory.
    pub fn runtime_dir(env: &Env) -> Result<PathBuf, PathsError> {
        let dir = env
            .runtime_dir
            .as_ref()
            .filter(|dir| !dir.is_empty())
            .ok_or(PathsError::NoRuntimeDir)?;
        Ok(Path::new(dir)
            .join("plx")
            .join(format!("v{PROTOCOL_VERSION}")))
    }

    /// Creates the runtime and state directories if needed, and checks the
    /// runtime directory is yours alone.
    ///
    /// # Errors
    ///
    /// When a directory cannot be created, or the runtime directory is not
    /// private.
    pub fn prepare(&self) -> Result<(), PathsError> {
        for dir in [&self.runtime, &self.state] {
            DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)
                .map_err(|source| PathsError::Create {
                    path: PathBuf::from(dir.as_os_str()),
                    source,
                })?;
        }
        check_private(&self.runtime)
    }
}

/// `path` is a directory owned by this user that nobody else can enter.
///
/// # Errors
///
/// When it is not, or cannot be read.
pub fn check_private(path: &Path) -> Result<(), PathsError> {
    let meta = fs::metadata(path).map_err(|source| PathsError::Create {
        path: path.into(),
        source,
    })?;
    let me = rustix::process::getuid().as_raw();
    if meta.uid() != me || meta.mode() & 0o077 != 0 {
        return Err(PathsError::NotPrivate {
            path: path.into(),
            uid: meta.uid(),
            mode: meta.mode() & 0o7777,
        });
    }
    Ok(())
}
