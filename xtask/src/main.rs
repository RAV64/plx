//! Maintenance tasks and source lints for plx.
//!
//! - `gate`: the pre-commit gate, in order: fmt, lint, clippy (workspace and
//!   xtask), tests (nextest, doctests, xtask's own), cargo-deny. The one
//!   definition of "green" (docs/DEVELOPMENT.md references it).
//! - `lint [--bless]`: source rules that rustc and clippy cannot express (see
//!   [`lint`]); `--bless` rewrites `invariants.lock` after reviewing new or
//!   removed panic sites.
//! - `ghostty-bindings`: regenerate `crates/plx-ghostty-sys/src/bindings.rs`
//!   from the libghostty-vt headers that `PLX_GHOSTTY_VT_DIR` (set by the dev
//!   shell) points at. Run it after bumping the Ghostty flake input.

mod bindings;
mod lint;

use std::env;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

const TASKS: &str = "tasks: gate, lint [--bless], ghostty-bindings";

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = match args.as_slice() {
        ["gate"] => workspace_root().and_then(|root| gate(&root)),
        ["lint"] => workspace_root().and_then(|root| lint::run(&root, lint::Lock::Check)),
        ["lint", "--bless"] => workspace_root().and_then(|root| lint::run(&root, lint::Lock::Bless)),
        ["ghostty-bindings"] => workspace_root().and_then(|root| bindings::run(&root)),
        [] => Err(Failure(format!("usage: xtask <task>; {TASKS}"))),
        other => Err(Failure(format!("unknown task `{}`; {TASKS}", other.join(" ")))),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(Failure(message)) => {
            eprintln!("xtask: {message}");
            ExitCode::FAILURE
        }
    }
}

/// Every cargo step of the gate after `fmt` and `lint`, in order.
const CARGO_STEPS: [&[&str]; 6] = [
    &["clippy", "--workspace", "--all-targets", "--", "-D", "warnings"],
    &[
        "clippy",
        "--manifest-path",
        "xtask/Cargo.toml",
        "--all-targets",
        "--",
        "-D",
        "warnings",
    ],
    &["nextest", "run", "--workspace"],
    &["test", "--doc", "--workspace"],
    &["test", "--manifest-path", "xtask/Cargo.toml"],
    &["deny", "check"],
];

/// The pre-commit gate. `fmt` runs first because it rewrites code the later
/// steps check (a committed clippy failure once came from running it last).
fn gate(root: &Path) -> Result<(), Failure> {
    cargo(root, &["fmt", "--all"])?;
    lint::run(root, lint::Lock::Check)?;
    for step in CARGO_STEPS {
        cargo(root, step)?;
    }
    eprintln!("gate: green");
    Ok(())
}

fn cargo(root: &Path, args: &[&str]) -> Result<(), Failure> {
    eprintln!("gate: cargo {}", args.join(" "));
    let status = Command::new(env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .args(args)
        .current_dir(root)
        .status()
        .map_err(|e| Failure(format!("cannot run cargo {}: {e}", args.join(" "))))?;
    if status.success() {
        Ok(())
    } else {
        Err(Failure(format!("cargo {} failed ({status})", args.join(" "))))
    }
}

/// The plx repository root (xtask lives one level below it).
fn workspace_root() -> Result<PathBuf, Failure> {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| Failure(format!("{} has no parent directory", manifest.display())))
}

/// Why a task failed, as a message for the terminal.
#[derive(Debug)]
pub(crate) struct Failure(pub(crate) String);

impl<E: fmt::Display> From<E> for Failure {
    fn from(error: E) -> Self {
        Self(format!("{error}"))
    }
}
