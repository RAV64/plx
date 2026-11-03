//! What the user's terminal can show, from its environment.
//!
//! Terminals do not report colour depth or underline styles reliably, so
//! these follow the conventions terminals set: `COLORTERM` for true colour,
//! `TERM` for the 256-colour palette, and the terminal's own name for styled
//! underlines and progress bars. A terminal that says nothing gets what
//! every terminal shows: the 16 colours, plain underlines, and no progress
//! bars.

use std::ffi::OsStr;

use plx_client_core::compose::{Capabilities, ColorDepth, ProgressBars, UnderlineStyles};

/// The variables capabilities come from.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct TerminalEnv<'a> {
    pub term: Option<&'a OsStr>,
    pub colorterm: Option<&'a OsStr>,
    pub term_program: Option<&'a OsStr>,
}

/// Terminals known to draw styled and coloured underlines (`4:3`, `58`).
const STYLED_UNDERLINES: [&str; 6] = ["ghostty", "kitty", "WezTerm", "foot", "contour", "rio"];

/// Terminals on Linux known to draw progress reports (`OSC 9;4`) as a
/// progress bar. Others are sent none: some show any `OSC 9` as a desktop
/// notification.
const PROGRESS_BARS: [&str; 3] = ["ghostty", "kitty", "WezTerm"];

/// The terminal's capabilities.
#[must_use]
pub fn detect(env: &TerminalEnv<'_>) -> Capabilities {
    let term = env.term.and_then(OsStr::to_str);
    let colorterm = env.colorterm.and_then(OsStr::to_str);
    let program = env.term_program.and_then(OsStr::to_str);
    let color = match (colorterm, term) {
        (Some("truecolor" | "24bit"), _) => ColorDepth::TrueColor,
        (_, Some(term)) if term.ends_with("-direct") => ColorDepth::TrueColor,
        (_, Some(term)) if term.contains("256color") => ColorDepth::Palette256,
        // Says nothing: what every terminal shows.
        _ => ColorDepth::Ansi16,
    };
    let named = |name: &str| {
        program.is_some_and(|program| program.eq_ignore_ascii_case(name))
            || term.is_some_and(|term| {
                term.to_ascii_lowercase()
                    .contains(&name.to_ascii_lowercase())
            })
    };
    let underlines = if STYLED_UNDERLINES.into_iter().any(named) {
        UnderlineStyles::Styled
    } else {
        UnderlineStyles::Plain
    };
    let progress = if PROGRESS_BARS.into_iter().any(named) {
        ProgressBars::Drawn
    } else {
        ProgressBars::Unsupported
    };
    Capabilities {
        color,
        underlines,
        progress,
    }
}
