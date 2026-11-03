//! The configuration file's parser.

#![cfg(test)]

use plx_config::{Config, DEFAULT, parse};

#[test]
fn an_empty_file_sets_nothing() {
    assert_eq!(
        parse("").unwrap(),
        Config {
            default_shell: None,
            scrollback_lines: None,
            scrollback_editor: None,
        }
    );
}

#[test]
fn the_default_shell_is_read() {
    let config = parse("default_shell = \"fish\"\n").unwrap();
    assert_eq!(config.default_shell.unwrap().as_str(), "fish");
    let config = parse("default_shell = \"/run/current-system/sw/bin/fish\"").unwrap();
    assert_eq!(
        config.default_shell.unwrap().as_str(),
        "/run/current-system/sw/bin/fish"
    );
}

#[test]
fn mistakes_are_errors_that_say_where() {
    for (text, says) in [
        ("default_shel = \"fish\"", "default_shel"),
        ("default_shell = \"\"", "empty"),
        ("default_shell = 3", "string"),
        ("default_shell = \"fish", "line 1"),
    ] {
        let error = format!("{}", parse(text).unwrap_err());
        assert!(error.contains(says), "{text:?}: {error}");
    }
}

#[test]
fn the_default_file_sets_nothing_but_shows_every_setting() {
    assert_eq!(
        parse(DEFAULT).unwrap(),
        Config {
            default_shell: None,
            scrollback_lines: None,
            scrollback_editor: None,
        }
    );
    assert!(DEFAULT.contains("# default_shell = "));
    // Uncommenting the example gives a valid setting.
    let uncommented = DEFAULT
        .replace("# default_shell", "default_shell")
        .replace("# scrollback_editor", "scrollback_editor");
    let config = parse(&uncommented).unwrap();
    assert_eq!(config.default_shell.unwrap().as_str(), "fish");
    assert_eq!(config.scrollback_editor.unwrap().as_str(), "nvim");
}

#[test]
fn the_scrollback_editor_is_a_command_line() {
    let config = parse("scrollback_editor = \"code --wait\"").unwrap();
    assert_eq!(config.scrollback_editor.unwrap().as_str(), "code --wait");
    let error = format!("{}", parse("scrollback_editor = \"  \"").unwrap_err());
    assert!(error.contains("empty"), "{error}");
}

#[test]
fn scrollback_lines_are_a_positive_count() {
    let config = parse("scrollback_lines = 10000").unwrap();
    assert_eq!(
        config.scrollback_lines.map(core::num::NonZeroU32::get),
        Some(10000)
    );
    for (text, says) in [
        ("scrollback_lines = 0", "nonzero"),
        ("scrollback_lines = -5", "scrollback_lines"),
    ] {
        let error = format!("{}", parse(text).unwrap_err());
        assert!(error.contains(says), "{text:?}: {error}");
    }
}
