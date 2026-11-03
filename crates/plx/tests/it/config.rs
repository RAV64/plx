use std::ffi::{OsStr, OsString};
use std::path::Path;

use plx::config::{self, ConfigFileError, Created, load, path, shell};
use plx_config::parse;

#[test]
fn the_file_follows_the_xdg_spec() {
    let home = Some(OsStr::new("/home/u"));
    assert_eq!(
        path(Some(OsStr::new("/cfg")), home).unwrap(),
        Path::new("/cfg/plx/config.toml")
    );
    assert_eq!(
        path(Some(OsStr::new("")), home).unwrap(),
        Path::new("/home/u/.config/plx/config.toml")
    );
    assert!(matches!(
        path(None, None),
        Err(ConfigFileError::NoConfigDir)
    ));
}

#[test]
fn the_configured_shell_wins_over_the_one_that_started_plx() {
    let file = Path::new("/home/u/.config/plx/config.toml");
    let fish = parse("default_shell = \"fish\"").unwrap();
    let nothing = parse("").unwrap();
    // `nix develop` sets SHELL to its bash.
    let nix_bash = Some(OsStr::new("/nix/store/x-bash/bin/bash"));
    assert_eq!(shell(&fish, nix_bash, file).unwrap().path, "fish");
    assert_eq!(
        shell(&nothing, nix_bash, file).unwrap().path,
        "/nix/store/x-bash/bin/bash"
    );
    assert!(shell(&nothing, None, file).is_err());
    assert!(shell(&nothing, Some(OsStr::new("")), file).is_err());
}

#[test]
fn the_first_load_writes_the_default_file() {
    use std::os::unix::fs::PermissionsExt as _;
    let root = std::env::temp_dir().join(format!("plx-config-{}", std::process::id()));
    if root.exists() {
        std::fs::remove_dir_all(&root).unwrap();
    }
    let file = path(Some(root.join("config").as_os_str()), None).unwrap();
    let first = load(&file).unwrap();
    assert_eq!(first.created, Created::Yes);
    assert_eq!(first.config.default_shell, None);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), plx_config::DEFAULT);
    // Directories the spec has plx create are private.
    let mode = std::fs::metadata(file.parent().unwrap())
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o700);
    // Then the file is the user's: read, never rewritten.
    std::fs::write(&file, "default_shell = \"fish\"\n").unwrap();
    let second = load(&file).unwrap();
    assert_eq!(second.created, Created::No);
    assert_eq!(second.config.default_shell.unwrap().as_str(), "fish");
    std::fs::remove_dir_all(root).unwrap();
}

/// The history opens in `scrollback_editor`, else `$VISUAL`, else
/// `$EDITOR`; an empty variable names no editor, and with none at all
/// there is none (the editor's pane says so).
#[test]
fn the_scrollback_editor_comes_from_the_file_then_visual_then_editor() {
    let none = parse("").unwrap();
    let nvim = parse("scrollback_editor = \"nvim\"").unwrap();
    let vis = Some(OsStr::new("vis"));
    let ed = Some(OsStr::new("ed"));
    let empty = Some(OsStr::new(""));
    assert_eq!(
        config::editor_or_none(&nvim, vis, ed),
        Some(OsString::from("nvim"))
    );
    assert_eq!(
        config::editor_or_none(&none, vis, ed),
        Some(OsString::from("vis"))
    );
    assert_eq!(
        config::editor_or_none(&none, empty, ed),
        Some(OsString::from("ed"))
    );
    assert_eq!(config::editor_or_none(&none, None, empty), None);
}
