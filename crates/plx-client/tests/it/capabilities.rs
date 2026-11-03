use std::ffi::OsStr;

use plx_client::capabilities::{TerminalEnv, detect};
use plx_client_core::compose::{ColorDepth, ProgressBars, UnderlineStyles};

fn env<'a>(
    term: Option<&'a str>,
    colorterm: Option<&'a str>,
    program: Option<&'a str>,
) -> TerminalEnv<'a> {
    TerminalEnv {
        term: term.map(OsStr::new),
        colorterm: colorterm.map(OsStr::new),
        term_program: program.map(OsStr::new),
    }
}

#[test]
fn colour_depth_follows_the_conventions() {
    assert_eq!(
        detect(&env(Some("xterm-256color"), Some("truecolor"), None)).color,
        ColorDepth::TrueColor
    );
    assert_eq!(
        detect(&env(Some("xterm-256color"), Some("24bit"), None)).color,
        ColorDepth::TrueColor
    );
    assert_eq!(
        detect(&env(Some("xterm-direct"), None, None)).color,
        ColorDepth::TrueColor
    );
    assert_eq!(
        detect(&env(Some("screen-256color"), None, None)).color,
        ColorDepth::Palette256
    );
    assert_eq!(
        detect(&env(Some("xterm"), None, None)).color,
        ColorDepth::Ansi16
    );
    assert_eq!(detect(&env(None, None, None)).color, ColorDepth::Ansi16);
}

#[test]
fn styled_underlines_only_on_terminals_known_to_draw_them() {
    assert_eq!(
        detect(&env(Some("xterm-ghostty"), None, None)).underlines,
        UnderlineStyles::Styled
    );
    assert_eq!(
        detect(&env(Some("xterm-kitty"), None, None)).underlines,
        UnderlineStyles::Styled
    );
    assert_eq!(
        detect(&env(Some("xterm-256color"), None, Some("WezTerm"))).underlines,
        UnderlineStyles::Styled
    );
    assert_eq!(
        detect(&env(Some("xterm-256color"), None, Some("Apple_Terminal"))).underlines,
        UnderlineStyles::Plain
    );
    assert_eq!(
        detect(&env(None, None, None)).underlines,
        UnderlineStyles::Plain
    );
}

/// Progress reports go only to terminals known to draw them as a bar:
/// others (foot, xterm, iTerm2 before 3.6) may show any OSC 9 as a
/// notification.
#[test]
fn progress_bars_only_on_terminals_known_to_draw_them() {
    for (term, program, progress) in [
        (Some("xterm-ghostty"), Some("ghostty"), ProgressBars::Drawn),
        (Some("xterm-kitty"), None, ProgressBars::Drawn),
        (Some("xterm-256color"), Some("WezTerm"), ProgressBars::Drawn),
        (Some("foot"), None, ProgressBars::Unsupported),
        (Some("xterm-256color"), None, ProgressBars::Unsupported),
        (None, None, ProgressBars::Unsupported),
    ] {
        assert_eq!(
            detect(&env(term, None, program)).progress,
            progress,
            "{term:?} {program:?}"
        );
    }
}
