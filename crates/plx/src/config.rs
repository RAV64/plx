//! The configuration file, found and read (`plx_config` parses it): what a
//! session's panes run, the `default_shell` setting or else `$SHELL`.

use std::ffi::{OsStr, OsString};
use std::fs::{DirBuilder, OpenOptions};
use std::io::{self, Write as _};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};

use plx_bug::OrBug as _;
use plx_config::{Config, ConfigError};
use plx_daemon::pty::Program;
use plx_daemon::reactor::DEFAULT_SCROLLBACK;
use plx_ghostty::Scrollback;
use std::num::NonZeroUsize;

/// Why the configuration cannot be used.
#[derive(Debug, thiserror::Error)]
pub enum ConfigFileError {
    #[error("neither XDG_CONFIG_HOME nor HOME is set; plx looks for its configuration there")]
    NoConfigDir,
    #[error("reading {}: {source}", path.display())]
    Read { path: PathBuf, source: io::Error },
    #[error("{}: {source}", path.display())]
    Parse { path: PathBuf, source: ConfigError },
    #[error("writing the default configuration {}: {source}", path.display())]
    Write { path: PathBuf, source: io::Error },
    #[error(
        "no shell for the panes: set default_shell in {} or SHELL",
        path.display()
    )]
    NoShell { path: PathBuf },
}

/// `$XDG_CONFIG_HOME/plx/config.toml`, by the XDG Base Directory spec
/// `$HOME/.config/plx/config.toml` when `XDG_CONFIG_HOME` is unset or
/// empty.
///
/// # Errors
///
/// When neither variable names a directory.
pub fn path(config_home: Option<&OsStr>, home: Option<&OsStr>) -> Result<PathBuf, ConfigFileError> {
    let dir = match (
        config_home.filter(|dir| !dir.is_empty()),
        home.filter(|dir| !dir.is_empty()),
    ) {
        (Some(dir), _) => PathBuf::from(dir),
        (None, Some(home)) => Path::new(home).join(".config"),
        (None, None) => return Err(ConfigFileError::NoConfigDir),
    };
    Ok(dir.join("plx").join("config.toml"))
}

/// The configuration at `path`, and whether it was just written.
#[derive(Debug)]
pub struct Loaded {
    pub config: Config,
    pub created: Created,
}

/// Whether [`load`] wrote the default file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Created {
    Yes,
    No,
}

/// The configuration at `path`. When there is no file, the default one
/// ([`plx_config::DEFAULT`], nothing set) is written there first, with
/// missing directories created 0700 as the XDG Base Directory spec asks.
///
/// # Errors
///
/// When the file cannot be read, written or parsed.
pub fn load(path: &Path) -> Result<Loaded, ConfigFileError> {
    let read = |path: &Path| match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(ConfigFileError::Read {
            path: PathBuf::from(path.as_os_str()),
            source,
        }),
    };
    let text = match read(path)? {
        Some(text) => text,
        None => match write_default(path)? {
            Created::Yes => {
                return Ok(Loaded {
                    config: parse(path, plx_config::DEFAULT)?,
                    created: Created::Yes,
                });
            }
            // Another plx wrote it first: read that.
            Created::No => read(path)?.ok_or_else(|| ConfigFileError::Read {
                path: PathBuf::from(path.as_os_str()),
                source: io::ErrorKind::NotFound.into(),
            })?,
        },
    };
    Ok(Loaded {
        config: parse(path, &text)?,
        created: Created::No,
    })
}

fn parse(path: &Path, text: &str) -> Result<Config, ConfigFileError> {
    plx_config::parse(text).map_err(|source| ConfigFileError::Parse {
        path: PathBuf::from(path.as_os_str()),
        source,
    })
}

/// Writes the default file at `path`. Another plx starting at the same
/// moment may write it first; then it is that one's file.
fn write_default(path: &Path) -> Result<Created, ConfigFileError> {
    let error = |source| ConfigFileError::Write {
        path: PathBuf::from(path.as_os_str()),
        source,
    };
    if let Some(dir) = path.parent() {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(error)?;
    }
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(path)
    {
        Ok(mut file) => {
            file.write_all(plx_config::DEFAULT.as_bytes())
                .map_err(error)?;
            Ok(Created::Yes)
        }
        Err(source) if source.kind() == io::ErrorKind::AlreadyExists => Ok(Created::No),
        Err(source) => Err(error(source)),
    }
}

/// How much history each pane keeps: `scrollback_lines`, else the
/// daemon's default.
#[must_use]
pub fn scrollback(config: &Config) -> Scrollback {
    config.scrollback_lines.map_or(DEFAULT_SCROLLBACK, |lines| {
        Scrollback::Lines(
            NonZeroUsize::try_from(lines).or_bug("a u32 fits usize on 64-bit targets"),
        )
    })
}

/// The editor a pane's history opens in: `scrollback_editor`, else
/// `$VISUAL`, else `$EDITOR` (an empty variable is no editor). `None`
/// when none is set: the editor's pane then says so.
#[must_use]
pub fn editor_or_none(
    config: &Config,
    visual: Option<&OsStr>,
    editor: Option<&OsStr>,
) -> Option<OsString> {
    let set = |var: Option<&OsStr>| var.filter(|value| !value.is_empty()).map(OsString::from);
    config
        .scrollback_editor
        .as_ref()
        .map(|editor| OsString::from(editor.as_str()))
        .or_else(|| set(visual))
        .or_else(|| set(editor))
}

/// What a new pane runs: `default_shell`, else `$SHELL` (`shell_var`).
/// The configured one wins, because `$SHELL` is whatever started plx
/// (`nix develop` sets it to its own bash).
///
/// # Errors
///
/// When neither names a program; `path` is the file to set it in.
pub fn shell(
    config: &Config,
    shell_var: Option<&OsStr>,
    path: &Path,
) -> Result<Program, ConfigFileError> {
    let program = match (
        &config.default_shell,
        shell_var.filter(|shell| !shell.is_empty()),
    ) {
        (Some(shell), _) => shell.as_str().into(),
        (None, Some(shell)) => shell.into(),
        (None, None) => {
            return Err(ConfigFileError::NoShell {
                path: PathBuf::from(path.as_os_str()),
            });
        }
    };
    Ok(Program {
        path: program,
        args: Vec::new(),
    })
}
