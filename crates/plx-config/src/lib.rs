//! The configuration file, `$XDG_CONFIG_HOME/plx/config.toml`: parsed once
//! and strictly (an unknown key is an error), and purely: the shell reads
//! the file, this crate only parses it.
//!
//! ```toml
//! # The program each new pane runs, instead of $SHELL: a path, or a name
//! # looked up in PATH. Read when a session starts.
//! default_shell = "fish"
//! # How many lines of history each pane keeps.
//! scrollback_lines = 10000
//! # The editor a pane's history opens in.
//! scrollback_editor = "nvim"
//! ```

#![forbid(unsafe_code)]

use core::num::NonZeroU32;

use serde::Deserialize;

/// The settings in the file. Every setting may be left out; plx then does
/// what it does without a file.
#[derive(Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// What a new pane runs instead of `$SHELL`. Absent: `$SHELL`.
    pub default_shell: Option<Shell>,
    /// How many lines of history each pane keeps. Absent: 10 MiB of it.
    pub scrollback_lines: Option<NonZeroU32>,
    /// The editor a pane's history opens in. Absent: `$VISUAL`, then
    /// `$EDITOR`.
    pub scrollback_editor: Option<Editor>,
}

/// An editor: a command line for `/bin/sh` (the file's path is appended),
/// not empty, no NUL byte.
#[derive(Debug, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub struct Editor(Box<str>);

impl Editor {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Why a string is not an [`Editor`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum EditorError {
    #[error("scrollback_editor cannot be empty")]
    Empty,
    #[error("scrollback_editor cannot contain a NUL byte")]
    Nul,
}

impl TryFrom<String> for Editor {
    type Error = EditorError;

    fn try_from(editor: String) -> Result<Self, EditorError> {
        if editor.trim().is_empty() {
            Err(EditorError::Empty)
        } else if editor.contains('\0') {
            Err(EditorError::Nul)
        } else {
            Ok(Self(editor.into_boxed_str()))
        }
    }
}

/// A program to run: not empty, no NUL byte (it becomes an argv entry).
#[derive(Debug, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub struct Shell(Box<str>);

impl Shell {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Why a string is not a [`Shell`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ShellError {
    #[error("default_shell cannot be empty")]
    Empty,
    #[error("default_shell cannot contain a NUL byte")]
    Nul,
}

impl TryFrom<String> for Shell {
    type Error = ShellError;

    fn try_from(shell: String) -> Result<Self, ShellError> {
        if shell.is_empty() {
            Err(ShellError::Empty)
        } else if shell.contains('\0') {
            Err(ShellError::Nul)
        } else {
            Ok(Self(shell.into_boxed_str()))
        }
    }
}

/// The file plx writes when there is none: every setting documented, none
/// set, so plx behaves exactly as without a file.
pub const DEFAULT: &str = "\
# plx configuration. Read when a session starts: after a change, end the
# session (plx kill-session) and start a new one.

# The program each new pane runs: a path, or a name looked up in PATH.
# Without it, panes run $SHELL, which is whatever started plx (nix develop
# sets its own bash).
# default_shell = \"fish\"

# How many lines of history each pane keeps (at least 1; libghostty keeps a
# few dozen more, up to its next page). Without it, 10 MiB per pane: about
# 5900 lines 200 columns wide.
# scrollback_lines = 10000

# The editor a pane's history opens in (Alt-e, or e in scroll mode): a
# command line run by /bin/sh with the file appended. Without it, $VISUAL,
# then $EDITOR.
# scrollback_editor = \"nvim\"
";

/// Why the file is not a configuration; the message says where.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ConfigError(#[from] toml::de::Error);

/// Parses the file's text.
///
/// # Errors
///
/// When it is not TOML, has a key plx does not know, or a value that is
/// not valid for its key.
pub fn parse(text: &str) -> Result<Config, ConfigError> {
    Ok(toml::from_str(text)?)
}
