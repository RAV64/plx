//! Checks the committed bindings against the ABI manifest that the *linked*
//! libghostty-vt exports through `ghostty_type_json()`.
//!
//! bindgen's own layout assertions compare against the headers; this test
//! compares against the library actually linked, so a header/library mismatch
//! after a pin bump (or a stale bindings.rs) fails here instead of corrupting
//! memory at runtime. It covers every type and enum plx-ghostty uses: add new
//! ones here when the wrapper starts using them.

// Marks everything here as test code, so clippy's allow-*-in-tests settings
// (clippy.toml) also cover the helpers outside #[test] functions.
#![cfg(test)]

use core::ffi::CStr;
use std::collections::BTreeSet;
use std::mem::{align_of, offset_of, size_of};

use plx_ghostty_sys as sys;
use serde_json::Value;

fn manifest() -> Value {
    // SAFETY: returns a pointer to a static, NUL-terminated JSON document.
    let json = unsafe { CStr::from_ptr(sys::ghostty_type_json()) };
    serde_json::from_str(json.to_str().unwrap()).unwrap()
}

fn descriptor<'a>(manifest: &'a Value, name: &str) -> &'a Value {
    manifest["types"]
        .get(name)
        .unwrap_or_else(|| panic!("{name} is missing from ghostty_type_json()"))
}

fn as_usize(value: &Value, what: &str) -> usize {
    usize::try_from(
        value
            .as_u64()
            .unwrap_or_else(|| panic!("{what} is not an integer: {value}")),
    )
    .unwrap()
}

/// Asserts size, alignment, the exact field set and every field offset.
macro_rules! check_struct {
    ($manifest:expr, $ty:ident { $($field:ident),+ $(,)? }) => {{
        let desc = descriptor(&$manifest, stringify!($ty));
        assert_eq!(desc["kind"], "struct", "{} kind", stringify!($ty));
        assert_eq!(as_usize(&desc["size"], "size"), size_of::<sys::$ty>(), "{} size", stringify!($ty));
        assert_eq!(as_usize(&desc["align"], "align"), align_of::<sys::$ty>(), "{} align", stringify!($ty));

        let fields = desc["fields"].as_object().unwrap();
        let listed: BTreeSet<&str> = [$(stringify!($field)),+].into_iter().collect();
        let actual: BTreeSet<&str> = fields.keys().map(String::as_str).collect();
        assert_eq!(listed, actual, "{} field set", stringify!($ty));
        $(
            assert_eq!(
                as_usize(&fields[stringify!($field)]["offset"], "offset"),
                offset_of!(sys::$ty, $field),
                "{}.{} offset", stringify!($ty), stringify!($field),
            );
        )+
    }};
}

/// Asserts that every listed constant (including the `*_MAX_VALUE` sentinel)
/// has the library's value, and that the library has no value we did not list.
macro_rules! check_enum {
    ($manifest:expr, $ty:ident { $($variant:ident),+ $(,)? }) => {{
        let desc = descriptor(&$manifest, stringify!($ty));
        assert_eq!(desc["kind"], "enum", "{} kind", stringify!($ty));
        let prefix = desc["prefix"].as_str().unwrap();
        let values = desc["values"].as_object().unwrap();
        let actual: BTreeSet<String> = values.keys().map(|key| format!("{prefix}{key}")).collect();
        let actual: BTreeSet<&str> = actual.iter().map(String::as_str).collect();
        let listed: BTreeSet<&str> = [$(stringify!($variant)),+].into_iter().collect();
        assert_eq!(listed, actual, "{} variant set", stringify!($ty));
        $(
            let key = stringify!($variant).strip_prefix(prefix).unwrap();
            assert_eq!(
                values[key].as_i64().unwrap(),
                i64::from(sys::$ty::$variant.0),
                "{}::{}", stringify!($ty), stringify!($variant),
            );
        )+
    }};
}

#[test]
fn structs_match_the_linked_library() {
    let m = manifest();
    check_struct!(m, GhosttyString { ptr, len });
    check_struct!(m, GhosttyAllocator { ctx, vtable });
    check_struct!(
        m,
        GhosttyAllocatorVtable {
            alloc,
            resize,
            remap,
            free
        }
    );
    check_struct!(m, GhosttyBuffer { ptr, cap, len });
    check_struct!(m, GhosttyColorRgb { r, g, b });
    check_struct!(m, GhosttyStyleColor { tag, value });
    check_struct!(
        m,
        GhosttyStyle {
            size,
            fg_color,
            bg_color,
            underline_color,
            bold,
            italic,
            faint,
            blink,
            inverse,
            invisible,
            strikethrough,
            overline,
            underline,
        }
    );
    check_struct!(
        m,
        GhosttyRenderStateCursor {
            size,
            viewport_has_value,
            viewport_x,
            viewport_y,
            wide_tail,
            visible,
            blinking,
            password_input,
            visual_style,
        }
    );
    check_struct!(
        m,
        GhosttyDeviceAttributesPrimary {
            conformance_level,
            features,
            num_features
        }
    );
    check_struct!(
        m,
        GhosttyDeviceAttributesSecondary {
            device_type,
            firmware_version,
            rom_cartridge
        }
    );
    check_struct!(m, GhosttyDeviceAttributesTertiary { unit_id });
    check_struct!(
        m,
        GhosttyDeviceAttributes {
            primary,
            secondary,
            tertiary
        }
    );
}

#[test]
fn enums_match_the_linked_library() {
    let m = manifest();
    check_enum!(
        m,
        GhosttyRenderStateDirty {
            GHOSTTY_RENDER_STATE_DIRTY_FALSE,
            GHOSTTY_RENDER_STATE_DIRTY_PARTIAL,
            GHOSTTY_RENDER_STATE_DIRTY_FULL,
            GHOSTTY_RENDER_STATE_DIRTY_MAX_VALUE,
        }
    );
    check_enum!(
        m,
        GhosttyResult {
            GHOSTTY_SUCCESS,
            GHOSTTY_OUT_OF_MEMORY,
            GHOSTTY_INVALID_VALUE,
            GHOSTTY_OUT_OF_SPACE,
            GHOSTTY_NO_VALUE,
            GHOSTTY_IO_ERROR,
            GHOSTTY_LIMIT_EXCEEDED,
            GHOSTTY_REJECTED,
            GHOSTTY_RESULT_MAX_VALUE,
        }
    );
    check_enum!(
        m,
        GhosttyCellWide {
            GHOSTTY_CELL_WIDE_NARROW,
            GHOSTTY_CELL_WIDE_WIDE,
            GHOSTTY_CELL_WIDE_SPACER_TAIL,
            GHOSTTY_CELL_WIDE_SPACER_HEAD,
            GHOSTTY_CELL_WIDE_MAX_VALUE,
        }
    );
    check_enum!(
        m,
        GhosttyStyleColorTag {
            GHOSTTY_STYLE_COLOR_NONE,
            GHOSTTY_STYLE_COLOR_PALETTE,
            GHOSTTY_STYLE_COLOR_RGB,
            GHOSTTY_STYLE_COLOR_TAG_MAX_VALUE,
        }
    );
    check_enum!(
        m,
        GhosttySgrUnderline {
            GHOSTTY_SGR_UNDERLINE_NONE,
            GHOSTTY_SGR_UNDERLINE_SINGLE,
            GHOSTTY_SGR_UNDERLINE_DOUBLE,
            GHOSTTY_SGR_UNDERLINE_CURLY,
            GHOSTTY_SGR_UNDERLINE_DOTTED,
            GHOSTTY_SGR_UNDERLINE_DASHED,
            GHOSTTY_SGR_UNDERLINE_MAX_VALUE,
        }
    );
    check_enum!(
        m,
        GhosttyRenderStateCursorVisualStyle {
            GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_BAR,
            GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_BLOCK,
            GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_UNDERLINE,
            GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_BLOCK_HOLLOW,
            GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_MAX_VALUE,
        }
    );
    check_enum!(
        m,
        GhosttyOptimizeMode {
            GHOSTTY_OPTIMIZE_DEBUG,
            GHOSTTY_OPTIMIZE_RELEASE_SAFE,
            GHOSTTY_OPTIMIZE_RELEASE_SMALL,
            GHOSTTY_OPTIMIZE_RELEASE_FAST,
            GHOSTTY_OPTIMIZE_MODE_MAX_VALUE,
        }
    );
}

/// A pin bump (a new linked version) must come with regenerated bindings.
#[test]
fn bindings_were_generated_from_the_linked_version() {
    assert_eq!(
        sys::BINDINGS_VERSION,
        sys::LINKED_VERSION,
        "regenerate: cargo run --manifest-path xtask/Cargo.toml -- ghostty-bindings"
    );
}
