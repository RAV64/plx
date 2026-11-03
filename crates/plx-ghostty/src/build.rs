//! What the linked libghostty-vt was built with.

use core::ffi::c_void;
use core::fmt;

use plx_ghostty_sys as sys;

use plx_bug::{OrBug as _, breach};

use crate::error::{BuildError, check_contract};

/// The optimisation mode libghostty-vt was compiled with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Optimize {
    Debug,
    ReleaseSafe,
    ReleaseSmall,
    ReleaseFast,
}

/// A libghostty-vt feature that plx's flake compiles out.
///
/// The single list of them: flake.nix's `-Dvt-features` must name exactly
/// these (a test checks), [`verify_build`] refuses a library that reports
/// one compiled in, and every terminal turns off the ones build info cannot
/// report. Both are exhaustive matches, so a new feature needs a decision in
/// each.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Feature {
    KittyGraphics,
    /// Ghostty's APC glyph protocol. Build info has no query for it.
    GlyphProtocol,
}

impl Feature {
    /// Every feature plx compiles out.
    pub const COMPILED_OUT: [Self; 2] = [Self::KittyGraphics, Self::GlyphProtocol];

    /// The feature's `-Dvt-features` token.
    #[must_use]
    pub const fn flag(self) -> &'static str {
        match self {
            Self::KittyGraphics => "kitty-graphics",
            Self::GlyphProtocol => "glyph-protocol",
        }
    }
}

impl fmt::Display for Feature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.flag())
    }
}

/// Whether a feature is part of the linked library.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Presence {
    CompiledIn,
    CompiledOut,
}

/// From the C API's answer to "is it compiled in?".
impl From<bool> for Presence {
    fn from(compiled_in: bool) -> Self {
        if compiled_in {
            Self::CompiledIn
        } else {
            Self::CompiledOut
        }
    }
}

/// Build facts of the linked libghostty-vt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BuildInfo {
    /// e.g. `0.1.0-dev+83fb956-nix`.
    /// Borrowed from the library's static storage, never copied.
    pub version: &'static str,
    pub optimize: Optimize,
    pub kitty_graphics: Presence,
    /// tmux control mode parsing. Not a `-Dvt-features` token (it follows
    /// the regex engine), and plx neither needs nor fears it: reported only.
    pub tmux_control_mode: Presence,
}

/// Reads the build facts of the linked libghostty-vt.
#[must_use]
pub fn build_info() -> BuildInfo {
    let mut kitty_graphics = false;
    query(
        "ghostty_build_info(KITTY_GRAPHICS)",
        sys::GhosttyBuildInfo::GHOSTTY_BUILD_INFO_KITTY_GRAPHICS,
        (&raw mut kitty_graphics).cast(),
    );
    let mut tmux_control_mode = false;
    query(
        "ghostty_build_info(TMUX_CONTROL_MODE)",
        sys::GhosttyBuildInfo::GHOSTTY_BUILD_INFO_TMUX_CONTROL_MODE,
        (&raw mut tmux_control_mode).cast(),
    );

    // Initial value of the out-parameter only; the query overwrites it or fails.
    let mut optimize = sys::GhosttyOptimizeMode::GHOSTTY_OPTIMIZE_DEBUG;
    query(
        "ghostty_build_info(OPTIMIZE)",
        sys::GhosttyBuildInfo::GHOSTTY_BUILD_INFO_OPTIMIZE,
        (&raw mut optimize).cast(),
    );
    let optimize = match optimize {
        sys::GhosttyOptimizeMode::GHOSTTY_OPTIMIZE_DEBUG => Optimize::Debug,
        sys::GhosttyOptimizeMode::GHOSTTY_OPTIMIZE_RELEASE_SAFE => Optimize::ReleaseSafe,
        sys::GhosttyOptimizeMode::GHOSTTY_OPTIMIZE_RELEASE_SMALL => Optimize::ReleaseSmall,
        sys::GhosttyOptimizeMode::GHOSTTY_OPTIMIZE_RELEASE_FAST => Optimize::ReleaseFast,
        other => breach!("the header lists every optimize mode, not {}", other.0),
    };

    let mut version = sys::GhosttyString::default();
    query(
        "ghostty_build_info(VERSION_STRING)",
        sys::GhosttyBuildInfo::GHOSTTY_BUILD_INFO_VERSION_STRING,
        (&raw mut version).cast(),
    );
    let version = if version.len == 0 {
        ""
    } else {
        // SAFETY: libghostty-vt returns a pointer to `len` bytes of static
        // storage that live for the whole process, hence `'static`.
        let bytes: &'static [u8] = unsafe { core::slice::from_raw_parts(version.ptr, version.len) };
        core::str::from_utf8(bytes).or_contract("libghostty's version string is UTF-8")
    };

    BuildInfo {
        version,
        optimize,
        kitty_graphics: kitty_graphics.into(),
        tmux_control_mode: tmux_control_mode.into(),
    }
}

/// Reads the build facts and checks them against what plx requires. Call
/// this once at startup.
///
/// The library must be the pinned build plx was compiled against, and every
/// [`Feature::COMPILED_OUT`] that build info can report must be absent.
///
/// # Errors
///
/// When the build facts cannot be read, the library is not the pinned build,
/// or it was built with a feature plx compiles out.
pub fn verify_build() -> Result<BuildInfo, BuildError> {
    let info = build_info();
    if info.version != sys::LINKED_VERSION {
        return Err(BuildError::NotThePinnedBuild {
            linked: info.version,
            pinned: sys::LINKED_VERSION,
        });
    }
    for feature in Feature::COMPILED_OUT {
        let compiled_in = match feature {
            Feature::KittyGraphics => info.kitty_graphics == Presence::CompiledIn,
            // Not reportable; every terminal turns it off instead.
            Feature::GlyphProtocol => false,
        };
        if compiled_in {
            return Err(BuildError::CompiledIn(feature));
        }
    }
    Ok(info)
}

#[track_caller]
fn query(call: &'static str, data: sys::GhosttyBuildInfo, out: *mut c_void) {
    // SAFETY: every caller passes an `out` pointer to a live value of the
    // type that libghostty-vt documents for `data`.
    check_contract(call, unsafe { sys::ghostty_build_info(data, out) });
}
