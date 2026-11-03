use std::ffi::OsString;

use plx_daemon::paths::{Env, Paths, PathsError, SessionName, SessionNameError};

#[test]
fn session_names_are_plain_file_names() {
    for good in ["main", "a", "work-2", "x_y.z", &"n".repeat(64)] {
        assert!(SessionName::new(good).is_ok(), "{good}");
    }
    assert_eq!(SessionName::new(""), Err(SessionNameError::Empty));
    assert_eq!(
        SessionName::new(&"n".repeat(65)),
        Err(SessionNameError::TooLong)
    );
    for bad in ["a/b", "a b", "ä", "..", "a\0"] {
        assert!(SessionName::new(bad).is_err(), "{bad:?}");
    }
    assert_eq!(SessionName::new(".hidden"), Err(SessionNameError::Hidden));
}

fn env(runtime: Option<&str>, state: Option<&str>, home: Option<&str>) -> Env {
    Env {
        runtime_dir: runtime.map(OsString::from),
        state_home: state.map(OsString::from),
        home: home.map(OsString::from),
    }
}

#[test]
fn paths_follow_the_xdg_directories() {
    let name = SessionName::new("main").unwrap();
    let paths = Paths::of(&name, &env(Some("/run/user/1000"), Some("/state"), None)).unwrap();
    assert_eq!(
        paths.socket.to_str(),
        Some("/run/user/1000/plx/v1/main.sock")
    );
    assert_eq!(paths.lock.to_str(), Some("/run/user/1000/plx/v1/main.lock"));
    assert_eq!(paths.log.to_str(), Some("/state/plx/main.log"));
    // The spec's default state home, when unset or empty.
    let paths = Paths::of(
        &name,
        &env(Some("/run/user/1000"), Some(""), Some("/home/u")),
    )
    .unwrap();
    assert_eq!(
        paths.log.to_str(),
        Some("/home/u/.local/state/plx/main.log")
    );
}

#[test]
fn missing_directories_are_errors_not_guesses() {
    let name = SessionName::new("main").unwrap();
    assert!(matches!(
        Paths::of(&name, &env(None, Some("/s"), None)),
        Err(PathsError::NoRuntimeDir)
    ));
    assert!(matches!(
        Paths::of(&name, &env(Some(""), Some("/s"), None)),
        Err(PathsError::NoRuntimeDir)
    ));
    assert!(matches!(
        Paths::of(&name, &env(Some("/r"), None, None)),
        Err(PathsError::NoStateDir)
    ));
}

#[test]
fn prepared_directories_are_private_and_checked() {
    let root = std::env::temp_dir().join(format!("plx-paths-{}", std::process::id()));
    let name = SessionName::new("main").unwrap();
    let paths = Paths::of(
        &name,
        &env(root.join("run").to_str(), root.join("state").to_str(), None),
    )
    .unwrap();
    paths.prepare().unwrap();
    // Opened up, it is refused.
    std::fs::set_permissions(
        &paths.runtime,
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    assert!(matches!(
        paths.prepare(),
        Err(PathsError::NotPrivate { .. })
    ));
    std::fs::remove_dir_all(&root).unwrap();
}
