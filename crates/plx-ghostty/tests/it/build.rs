use plx_ghostty::{Feature, Optimize, Presence, verify_build};

/// The pinned version comes from the flake build itself (plx-ghostty-sys's
/// build script), so this needs no hand-kept commit hash.
#[test]
fn linked_library_is_the_pinned_build_with_the_flake_feature_set() {
    let info = verify_build().unwrap();
    assert_eq!(info.kitty_graphics, Presence::CompiledOut);
    assert_eq!(info.optimize, Optimize::ReleaseSafe);
}

/// `Feature::COMPILED_OUT` is the one list; the flake must build exactly it.
#[test]
fn the_flake_compiles_out_exactly_the_guarded_features() {
    let flake =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../flake.nix")).unwrap();
    let (_, rest) = flake.split_once("-Dvt-features=").unwrap();
    let (flags, _) = rest.split_once('"').unwrap();
    let expected: Vec<String> = Feature::COMPILED_OUT
        .iter()
        .map(|feature| format!("-{feature}"))
        .collect();
    assert_eq!(flags.split(',').collect::<Vec<_>>(), expected);
}
