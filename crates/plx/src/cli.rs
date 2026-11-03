//! The command line, parsed once: argv and the
//! environment in, an [`Invocation`] out. Nothing past this module looks at
//! an argument or a variable again.

use std::ffi::{OsStr, OsString};
use std::num::NonZeroU16;

use plx_bug::OrBug as _;
use plx_core::command::{SessionCommand, TabSide};
use plx_core::geometry::Direction;
use plx_core::tab::{TabName, TabNameError, TabNumber};
use plx_daemon::paths::{SessionName, SessionNameError};

/// The session when neither `--session` nor `PLX_SESSION` names one.
pub const DEFAULT_SESSION: &str = "main";

pub const USAGE: &str = "\
usage: plx [--session NAME] [COMMAND]

Sessions:
  plx                   attach to the session, starting it if needed
  plx new               start the session and attach to it
  plx attach            attach to the running session
  plx ls                list the running sessions
  plx kill-session      end the session and every program in it
  plx events            print the session's events as JSON lines until it ends

In the session:
  plx split DIRECTION   split the focused pane (left, right, up or down)
  plx close-pane        close the focused pane
  plx focus DIRECTION   focus the neighbouring pane
  plx grow DIRECTION    move the focused pane's border that way
  plx swap DIRECTION    swap the focused pane with its neighbour
  plx zoom              show the focused pane alone, or undo that
  plx rotate            move every pane one place on, the last to the first
  plx move-pane DIRECTION
                        move the focused pane; at the left or right edge,
                        into the tab before or after
  plx break-pane [left|right]
                        move the focused pane into a tab of its own, or
                        into the tab on that side
  plx move-tab left|right
                        move the active tab one place
  plx new-tab           open a tab after the active one
  plx close-tab         close the active tab
  plx next-tab          activate the next tab
  plx previous-tab      activate the previous tab
  plx tab NUMBER        activate tab NUMBER (1 is the first)
  plx rename-tab NAME   rename the active tab

  plx version           show plx's and libghostty's versions
  plx help              show this

The session is --session NAME, else $PLX_SESSION (set in plx's panes),
else \"main\". `plx daemon [--ready]` serves a session; plx starts it.
";

/// What the command line asks for.
#[derive(Debug, PartialEq, Eq)]
pub enum Invocation {
    /// `plx`: attach, starting the session first if it is not running.
    Open(SessionName),
    /// `plx new`: start the session (it must not be running), then attach.
    New(SessionName),
    /// `plx attach`: attach to the running session.
    Attach(SessionName),
    /// `plx ls`
    List,
    /// `plx events`: the session's events, as they happen.
    Events(SessionName),
    /// `plx <command>`: one command, without attaching.
    Command(SessionName, SessionCommand),
    /// `plx daemon`: serve the session.
    Daemon {
        session: SessionName,
        ready: Readiness,
    },
    Version,
    Help,
}

/// Whether the daemon tells its starter when it serves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Readiness {
    /// `--ready`: one byte on stdout (a pipe the starting client waits on).
    Stdout,
    /// Started by hand: nobody waits.
    Nobody,
}

/// The variables the command line reads.
#[derive(Debug, Default)]
pub struct CliEnv<'a> {
    /// `PLX_SESSION`: set in plx's panes.
    pub session: Option<&'a OsStr>,
}

/// Why the command line means nothing.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum CliError {
    #[error("{} is not valid UTF-8", .0.display())]
    NotUnicode(OsString),
    #[error("unknown option {}", .0.display())]
    UnknownOption(OsString),
    #[error("unknown command {}", .0.display())]
    UnknownCommand(OsString),
    #[error("--session needs a name")]
    NoSessionName,
    #[error("--session is given twice")]
    SessionTwice,
    #[error("usage: plx {0}")]
    Usage(&'static str),
    #[error("{} is not a direction: left, right, up or down", .0.display())]
    Direction(OsString),
    #[error("{} is not a tab number (1 is the first)", .0.display())]
    TabNumber(OsString),
    #[error("tab name: {0}")]
    TabName(#[from] TabNameError),
    #[error("session name: {0}")]
    SessionName(#[from] SessionNameError),
    #[error("PLX_SESSION: {0}")]
    SessionVar(SessionNameError),
    #[error(
        "this is inside plx session {}: attaching here would nest plx in itself \
         (unset PLX_SESSION to do it anyway)",
        .0.display()
    )]
    Nested(OsString),
}

const SPLIT: &str = "split left|right|up|down";
const FOCUS: &str = "focus left|right|up|down";
const GROW: &str = "grow left|right|up|down";
const SWAP: &str = "swap left|right|up|down";
const TAB: &str = "tab NUMBER";
const RENAME_TAB: &str = "rename-tab NAME";
const MOVE_PANE: &str = "move-pane left|right|up|down";
const BREAK_PANE: &str = "break-pane [left|right]";
const MOVE_TAB: &str = "move-tab left|right";

/// Parses `args` (without the program name) and `env`.
///
/// # Errors
///
/// When they do not form an invocation.
pub fn parse<I: IntoIterator<Item = OsString>>(
    args: I,
    env: &CliEnv<'_>,
) -> Result<Invocation, CliError> {
    let Options {
        session,
        ready,
        words,
    } = options(args)?;
    let words: Vec<&str> = words.iter().map(String::as_str).collect();
    if ready == Readiness::Stdout && words.first() != Some(&"daemon") {
        return Err(CliError::Usage("daemon [--ready]"));
    }
    let chosen = session;
    let session = || -> Result<SessionName, CliError> {
        match (chosen, env.session) {
            (Some(name), _) => Ok(name),
            (None, Some(var)) => {
                let var = var
                    .to_str()
                    .ok_or_else(|| CliError::NotUnicode(var.into()))?;
                SessionName::new(var).map_err(CliError::SessionVar)
            }
            (None, None) => {
                Ok(SessionName::new(DEFAULT_SESSION).or_bug("the default session name is valid"))
            }
        }
    };
    // Attaching from inside a session's pane would draw the session in
    // itself.
    let attaching = |name| {
        env.session
            .map_or(Ok(name), |var| Err(CliError::Nested(var.into())))
    };
    let invocation = match words.as_slice() {
        [] => Invocation::Open(attaching(session()?)?),
        ["new"] => Invocation::New(attaching(session()?)?),
        ["attach"] => Invocation::Attach(attaching(session()?)?),
        ["ls"] => Invocation::List,
        ["events"] => Invocation::Events(session()?),
        ["daemon"] => Invocation::Daemon {
            session: session()?,
            ready,
        },
        ["version"] => Invocation::Version,
        ["help"] => Invocation::Help,
        ["new", ..] => return Err(CliError::Usage("new")),
        ["attach", ..] => return Err(CliError::Usage("attach")),
        ["ls", ..] => return Err(CliError::Usage("ls")),
        ["events", ..] => return Err(CliError::Usage("events")),
        ["daemon", ..] => return Err(CliError::Usage("daemon [--ready]")),
        ["version", ..] => return Err(CliError::Usage("version")),
        ["help", ..] => return Err(CliError::Usage("help")),
        [word, rest @ ..] => Invocation::Command(session()?, command(word, rest)?),
    };
    Ok(invocation)
}

/// The options anywhere on the command line, and the words in order.
struct Options {
    session: Option<SessionName>,
    ready: Readiness,
    words: Vec<String>,
}

fn options<I: IntoIterator<Item = OsString>>(args: I) -> Result<Options, CliError> {
    let mut session = None;
    let mut ready = Readiness::Nobody;
    let mut words = Vec::new();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let arg = arg.into_string().map_err(CliError::NotUnicode)?;
        let name = match arg.as_str() {
            "--" => {
                for arg in args.by_ref() {
                    words.push(arg.into_string().map_err(CliError::NotUnicode)?);
                }
                break;
            }
            "-s" | "--session" => match args.next() {
                Some(name) => Some(name.into_string().map_err(CliError::NotUnicode)?),
                None => return Err(CliError::NoSessionName),
            },
            "-h" | "--help" => {
                words.push(String::from("help"));
                None
            }
            "-V" | "--version" => {
                words.push(String::from("version"));
                None
            }
            "--ready" => {
                ready = Readiness::Stdout;
                None
            }
            option if option.starts_with("--session=") => {
                Some(String::from(option.trim_start_matches("--session=")))
            }
            option if option.starts_with('-') && option.len() > 1 => {
                return Err(CliError::UnknownOption(arg.into()));
            }
            _ => {
                words.push(arg);
                None
            }
        };
        if let Some(name) = name {
            if session.is_some() {
                return Err(CliError::SessionTwice);
            }
            session = Some(SessionName::new(&name)?);
        }
    }
    Ok(Options {
        session,
        ready,
        words,
    })
}

/// `plx <word> <rest>`: a session command.
fn command(word: &str, rest: &[&str]) -> Result<SessionCommand, CliError> {
    let bare = |command, usage| match rest {
        [] => Ok(command),
        _ => Err(CliError::Usage(usage)),
    };
    match word {
        "split" => Ok(SessionCommand::Split {
            direction: direction(rest, SPLIT)?,
        }),
        "focus" => Ok(SessionCommand::Focus {
            direction: direction(rest, FOCUS)?,
        }),
        "grow" => Ok(SessionCommand::Grow {
            direction: direction(rest, GROW)?,
        }),
        "swap" => Ok(SessionCommand::Swap {
            direction: direction(rest, SWAP)?,
        }),
        "close-pane" => bare(SessionCommand::ClosePane, "close-pane"),
        "zoom" => bare(SessionCommand::ToggleZoom, "zoom"),
        "rotate" => bare(SessionCommand::RotatePanes, "rotate"),
        "move-pane" => Ok(SessionCommand::MovePane {
            direction: direction(rest, MOVE_PANE)?,
        }),
        "break-pane" => match rest {
            [] => Ok(SessionCommand::BreakPane),
            [side] => Ok(SessionCommand::MovePaneToTab {
                side: tab_side(side, BREAK_PANE)?,
            }),
            _ => Err(CliError::Usage(BREAK_PANE)),
        },
        "move-tab" => Ok(SessionCommand::MoveTab {
            side: tab_side(one(rest, MOVE_TAB)?, MOVE_TAB)?,
        }),
        "new-tab" => bare(SessionCommand::NewTab, "new-tab"),
        "close-tab" => bare(SessionCommand::CloseTab, "close-tab"),
        "next-tab" => bare(SessionCommand::NextTab, "next-tab"),
        "previous-tab" => bare(SessionCommand::PreviousTab, "previous-tab"),
        "kill-session" => bare(SessionCommand::KillSession, "kill-session"),
        "tab" => {
            let number = one(rest, TAB)?;
            let tab = number
                .parse::<NonZeroU16>()
                .map_err(|_not_a_number| CliError::TabNumber(number.into()))?;
            Ok(SessionCommand::FocusTab {
                tab: TabNumber::new(tab),
            })
        }
        "rename-tab" => Ok(SessionCommand::RenameTab {
            name: TabName::new(one(rest, RENAME_TAB)?)?,
        }),
        _ => Err(CliError::UnknownCommand(word.into())),
    }
}

/// The one argument a command takes.
const fn one<'a>(rest: &[&'a str], usage: &'static str) -> Result<&'a str, CliError> {
    match rest {
        [word] => Ok(word),
        _ => Err(CliError::Usage(usage)),
    }
}

/// `left` or `right` along the row of tabs.
fn tab_side(word: &str, usage: &'static str) -> Result<TabSide, CliError> {
    match word {
        "left" => Ok(TabSide::Previous),
        "right" => Ok(TabSide::Next),
        _ => Err(CliError::Usage(usage)),
    }
}

fn direction(rest: &[&str], usage: &'static str) -> Result<Direction, CliError> {
    match one(rest, usage)? {
        "left" => Ok(Direction::Left),
        "right" => Ok(Direction::Right),
        "up" => Ok(Direction::Up),
        "down" => Ok(Direction::Down),
        other => Err(CliError::Direction(other.into())),
    }
}
