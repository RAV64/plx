//! `ghostty-bindings`: regenerate the committed libghostty-vt bindings.

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use bindgen::EnumVariation;

use crate::Failure;

const DIR_VAR: &str = "PLX_GHOSTTY_VT_DIR";
const OUTPUT: &str = "crates/plx-ghostty-sys/src/bindings.rs";

pub(crate) fn run(root: &Path) -> Result<(), Failure> {
    let dir = PathBuf::from(
        env::var_os(DIR_VAR)
            .ok_or_else(|| Failure(format!("{DIR_VAR} is not set; run inside `nix develop`")))?,
    );
    let include = dir.join("include");
    let header = include.join("ghostty/vt.h");
    if !header.is_file() {
        return Err(Failure(format!("{} does not exist", header.display())));
    }
    let version = pc_version(&dir.join("share/pkgconfig/libghostty-vt-static.pc"))?;

    let bindings = bindgen::Builder::default()
        .header(path_str(&header)?)
        .clang_arg(format!("-I{}", path_str(&include)?))
        .clang_arg("-DGHOSTTY_STATIC")
        .allowlist_function("ghostty_.*")
        .allowlist_type("Ghostty.*")
        .allowlist_var("GHOSTTY_.*")
        // plx builds libghostty-vt without kitty graphics
        // (plx_ghostty::Feature); leaving these out turns any accidental use
        // into a compile error. The glyph protocol has no functions to drop.
        .blocklist_function("ghostty_kitty_graphics_.*")
        // C enums arrive from foreign code and may hold any value, so they are
        // newtypes with associated constants, never Rust enums.
        .default_enum_style(EnumVariation::NewType {
            is_bitfield: false,
            is_global: false,
        })
        .prepend_enum_name(false)
        .use_core()
        .derive_default(true)
        .derive_debug(true)
        .layout_tests(true)
        .merge_extern_blocks(true)
        .sort_semantically(true)
        .generate()?;

    let mut out = String::new();
    writeln!(out, "// Generated from libghostty-vt {version} headers by")?;
    writeln!(
        out,
        "// `cargo run --manifest-path xtask/Cargo.toml -- ghostty-bindings`."
    )?;
    writeln!(out, "// Do not edit by hand.")?;
    writeln!(out)?;
    // plx-ghostty-sys compares this with the version it links, so a pin bump
    // without regenerated bindings fails its tests.
    writeln!(
        out,
        "/// The libghostty-vt version these bindings were generated from."
    )?;
    writeln!(out, "pub const BINDINGS_VERSION: &str = \"{version}\";")?;
    writeln!(out)?;
    write!(out, "{bindings}")?;

    let output = root.join(OUTPUT);
    fs::write(&output, out)?;
    eprintln!("wrote {}", output.display());
    Ok(())
}

fn pc_version(pc: &Path) -> Result<String, Failure> {
    let text = fs::read_to_string(pc)
        .map_err(|e| Failure(format!("cannot read {}: {e}", pc.display())))?;
    let version = text
        .lines()
        .find_map(|line| line.strip_prefix("Version:"))
        .ok_or_else(|| Failure(format!("{} has no `Version:` line", pc.display())))?;
    Ok(String::from(version.trim()))
}

fn path_str(path: &Path) -> Result<&str, Failure> {
    path.to_str()
        .ok_or_else(|| Failure(format!("{} is not UTF-8", path.display())))
}
