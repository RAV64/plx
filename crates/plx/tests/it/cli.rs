use std::ffi::{OsStr, OsString};
use std::num::NonZeroU16;

use plx::cli::{CliEnv, CliError, Invocation, Readiness, parse};
use plx_core::command::SessionCommand;
use plx_core::geometry::Direction;
use plx_core::tab::{TabName, TabNumber};
use plx_daemon::paths::SessionName;
use proptest::prelude::*;

fn args(words: &[&str]) -> Vec<OsString> {
    words.iter().map(OsString::from).collect()
}

fn name(name: &str) -> SessionName {
    SessionName::new(name).unwrap()
}

const NO_ENV: CliEnv<'static> = CliEnv { session: None };

/// A session command as words, and what it parses to.
#[derive(Clone, Debug)]
enum Spec {
    Bare(&'static str),
    Directed(&'static str, Direction),
    Tab(u16),
    Rename(String),
}

impl Spec {
    fn words(&self) -> Vec<String> {
        let direction = |direction| match direction {
            Direction::Left => "left",
            Direction::Right => "right",
            Direction::Up => "up",
            Direction::Down => "down",
        };
        match self {
            Self::Bare(word) => vec![String::from(*word)],
            Self::Directed(word, dir) => vec![String::from(*word), String::from(direction(*dir))],
            Self::Tab(n) => vec![String::from("tab"), format!("{n}")],
            Self::Rename(name) => vec![String::from("rename-tab"), String::from(name.as_str())],
        }
    }

    fn command(&self) -> SessionCommand {
        match self {
            Self::Bare("close-pane") => SessionCommand::ClosePane,
            Self::Bare("zoom") => SessionCommand::ToggleZoom,
            Self::Bare("new-tab") => SessionCommand::NewTab,
            Self::Bare("close-tab") => SessionCommand::CloseTab,
            Self::Bare("next-tab") => SessionCommand::NextTab,
            Self::Bare("previous-tab") => SessionCommand::PreviousTab,
            Self::Bare("kill-session") => SessionCommand::KillSession,
            Self::Directed("split", direction) => SessionCommand::Split {
                direction: *direction,
            },
            Self::Directed("focus", direction) => SessionCommand::Focus {
                direction: *direction,
            },
            Self::Directed("grow", direction) => SessionCommand::Grow {
                direction: *direction,
            },
            Self::Directed("swap", direction) => SessionCommand::Swap {
                direction: *direction,
            },
            Self::Tab(n) => SessionCommand::FocusTab {
                tab: TabNumber::new(NonZeroU16::new(*n).unwrap()),
            },
            Self::Rename(name) => SessionCommand::RenameTab {
                name: TabName::new(name).unwrap(),
            },
            other @ (Self::Bare(_) | Self::Directed(..)) => panic!("not a spec: {other:?}"),
        }
    }
}

fn spec() -> impl Strategy<Value = Spec> {
    let direction = prop_oneof![
        Just(Direction::Left),
        Just(Direction::Right),
        Just(Direction::Up),
        Just(Direction::Down),
    ];
    prop_oneof![
        prop::sample::select(vec![
            "close-pane",
            "zoom",
            "new-tab",
            "close-tab",
            "next-tab",
            "previous-tab",
            "kill-session",
        ])
        .prop_map(Spec::Bare),
        (
            prop::sample::select(vec!["split", "focus", "grow", "swap"]),
            direction
        )
            .prop_map(|(word, direction)| Spec::Directed(word, direction)),
        (1..=u16::MAX).prop_map(Spec::Tab),
        // Leading dashes too: they need `--`, which every rendering has.
        "[a-zA-Z0-9 _.äö✓-]{1,64}".prop_map(Spec::Rename),
    ]
}

/// Where the session option goes, if anywhere.
#[derive(Clone, Copy, Debug)]
enum Placement {
    None,
    Before,
    BeforeJoined,
}

proptest! {
    /// Every command, with or without a session, parses back to itself.
    #[test]
    fn commands_parse_to_what_they_say(
        spec in spec(),
        session in "[a-zA-Z0-9_-][a-zA-Z0-9_.-]{0,63}",
        placement in prop_oneof![Just(Placement::None), Just(Placement::Before), Just(Placement::BeforeJoined)],
        var in prop::option::of("[a-z]{1,8}"),
    ) {
        let mut argv: Vec<String> = Vec::new();
        match placement {
            Placement::None => {}
            Placement::Before => argv.extend([String::from("-s"), String::from(session.as_str())]),
            Placement::BeforeJoined => argv.push(format!("--session={session}")),
        }
        argv.push(String::from("--"));
        argv.extend(spec.words());
        let env = CliEnv { session: var.as_deref().map(OsStr::new) };
        let expected = match (placement, &var) {
            (Placement::Before | Placement::BeforeJoined, _) => name(&session),
            (Placement::None, Some(var)) => name(var),
            (Placement::None, None) => name("main"),
        };
        let parsed = parse(argv.into_iter().map(OsString::from), &env).unwrap();
        prop_assert_eq!(parsed, Invocation::Command(expected, spec.command()));
    }

    /// Any command line parses or is refused; it never panics.
    #[test]
    fn any_command_line_is_parsed_or_refused(words in prop::collection::vec("\\PC{0,12}", 0..6)) {
        let env = CliEnv { session: None };
        match parse(words.into_iter().map(OsString::from), &env) {
            Ok(_) | Err(_) => {}
        }
    }
}

#[test]
fn bare_plx_opens_the_default_session() {
    assert_eq!(
        parse(args(&[]), &NO_ENV),
        Ok(Invocation::Open(name("main")))
    );
    assert_eq!(
        parse(args(&["-s", "work"]), &NO_ENV),
        Ok(Invocation::Open(name("work")))
    );
    assert_eq!(
        parse(args(&["new", "--session", "w"]), &NO_ENV),
        Ok(Invocation::New(name("w")))
    );
    assert_eq!(
        parse(args(&["attach"]), &NO_ENV),
        Ok(Invocation::Attach(name("main")))
    );
}

#[test]
fn inside_a_pane_commands_reach_its_session_and_attaching_is_refused() {
    let env = CliEnv {
        session: Some(OsStr::new("work")),
    };
    assert_eq!(
        parse(args(&["zoom"]), &env),
        Ok(Invocation::Command(
            name("work"),
            SessionCommand::ToggleZoom
        ))
    );
    for attaching in [&[][..], &["new"], &["attach"], &["attach", "-s", "other"]] {
        assert_eq!(
            parse(args(attaching), &env),
            Err(CliError::Nested(OsString::from("work"))),
            "{attaching:?}"
        );
    }
}

#[test]
fn the_daemon_is_told_when_to_say_it_serves() {
    assert_eq!(
        parse(args(&["daemon", "--ready", "-s", "w"]), &NO_ENV),
        Ok(Invocation::Daemon {
            session: name("w"),
            ready: Readiness::Stdout,
        })
    );
    assert_eq!(
        parse(args(&["ls", "--ready"]), &NO_ENV),
        Err(CliError::Usage("daemon [--ready]"))
    );
}

#[test]
fn wrong_command_lines_say_what_is_wrong() {
    let cases: [(&[&str], CliError); 9] = [
        (
            &["frobnicate"],
            CliError::UnknownCommand("frobnicate".into()),
        ),
        (&["--frob"], CliError::UnknownOption("--frob".into())),
        (&["-s"], CliError::NoSessionName),
        (&["-s", "a", "-s", "b"], CliError::SessionTwice),
        (&["split"], CliError::Usage("split left|right|up|down")),
        (
            &["split", "sideways"],
            CliError::Direction("sideways".into()),
        ),
        (&["tab", "0"], CliError::TabNumber("0".into())),
        (&["zoom", "now"], CliError::Usage("zoom")),
        (&["ls", "extra"], CliError::Usage("ls")),
    ];
    for (words, error) in cases {
        assert_eq!(parse(args(words), &NO_ENV), Err(error), "{words:?}");
    }
    assert!(matches!(
        parse(args(&["-s", "a/b"]), &NO_ENV),
        Err(CliError::SessionName(_))
    ));
    assert!(matches!(
        parse(args(&["rename-tab", "a\u{7}"]), &NO_ENV),
        Err(CliError::TabName(_))
    ));
}

#[test]
fn help_and_version_have_their_flags() {
    assert_eq!(parse(args(&["--help"]), &NO_ENV), Ok(Invocation::Help));
    assert_eq!(parse(args(&["-V"]), &NO_ENV), Ok(Invocation::Version));
    assert_eq!(parse(args(&["ls"]), &NO_ENV), Ok(Invocation::List));
    assert_eq!(
        parse(args(&["events", "-s", "w"]), &NO_ENV),
        Ok(Invocation::Events(name("w")))
    );
    assert_eq!(
        parse(args(&["events", "now"]), &NO_ENV),
        Err(CliError::Usage("events"))
    );
}
