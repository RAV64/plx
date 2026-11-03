//! Links the prebuilt static libghostty-vt from the pinned Ghostty flake input.
//!
//! This never compiles C or Zig and never runs bindgen. It reruns only when
//! `PLX_GHOSTTY_VT_DIR` changes, which is an immutable Nix store path that
//! changes only when the Ghostty pin in flake.lock changes.
//!
//! It also exports the build's pkg-config `Version:` as
//! `PLX_GHOSTTY_VT_VERSION`, so the pin in flake.lock is the one source of
//! the libghostty-vt version.

use std::env;
use std::fmt;
use std::fs;
use std::path::PathBuf;

const DIR_VAR: &str = "PLX_GHOSTTY_VT_DIR";

fn main() -> Result<(), BuildError> {
    // An explicit rerun directive is mandatory: without one, cargo reruns
    // build scripts whenever any file in the package changes.
    println!("cargo::rerun-if-env-changed={DIR_VAR}");
    println!("cargo::rerun-if-changed=build.rs");

    let dir = PathBuf::from(env::var_os(DIR_VAR).ok_or_else(|| {
        BuildError(format!(
            "{DIR_VAR} is not set. Enter the dev shell (`nix develop`); it points \
             {DIR_VAR} at the libghostty-vt build of the pinned Ghostty flake input."
        ))
    })?);

    let lib_dir = dir.join("lib");
    let archive = lib_dir.join("libghostty-vt.a");
    if !archive.is_file() {
        return Err(BuildError(format!(
            "{} does not exist ({DIR_VAR}={})",
            archive.display(),
            dir.display()
        )));
    }

    let pc_path = dir.join("share/pkgconfig/libghostty-vt-static.pc");
    let pc = fs::read_to_string(&pc_path)
        .map_err(|e| BuildError(format!("cannot read {}: {e}", pc_path.display())))?;
    let private_libs =
        libs_private(&pc).map_err(|e| BuildError(format!("{}: {e}", pc_path.display())))?;
    let version = pc
        .lines()
        .find_map(|line| line.strip_prefix("Version:"))
        .map(str::trim)
        .filter(|version| !version.is_empty())
        .ok_or_else(|| BuildError(format!("{}: no `Version:` line", pc_path.display())))?;

    println!("cargo::rustc-link-search=native={}", lib_dir.display());
    println!("cargo::rustc-link-lib=static=ghostty-vt");
    println!("cargo::rustc-env=PLX_GHOSTTY_VT_VERSION={version}");
    for lib in private_libs {
        println!("cargo::rustc-link-lib={lib}");
    }
    Ok(())
}

/// The `-l` libraries in the `Libs.private:` line, which a static link needs
/// on top of the archive itself (e.g. `c++` when simdutf comes from the
/// system). Anything other than `-l<name>` is rejected rather than ignored.
fn libs_private(pc: &str) -> Result<Vec<&str>, String> {
    let line = pc
        .lines()
        .find_map(|line| line.strip_prefix("Libs.private:"))
        .ok_or("no `Libs.private:` line")?;
    line.split_whitespace()
        .map(|token| {
            token
                .strip_prefix("-l")
                .filter(|name| !name.is_empty())
                .ok_or_else(|| format!("unsupported `Libs.private` token `{token}`"))
        })
        .collect()
}

/// Why the build script failed. `main` returning `Err` prints its `Debug`
/// form, so that shows the plain message.
struct BuildError(String);

impl fmt::Debug for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
