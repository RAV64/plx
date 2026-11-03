//! An attached client: the client core between the daemon's socket and the
//! user's terminal, in one mio loop.
//!
//! Terminal input is parsed into keys and pastes (termina's parser); the
//! daemon's bytes go to the client core, whose drawing and acks are written
//! out as the terminal and socket take them. While terminal output backs up,
//! the daemon is not read: no frame is acknowledged, so the daemon holds
//! frames rather than anything queueing.

use std::io::{self, Read as _, Write as _};
use std::os::fd::{AsFd as _, AsRawFd as _, OwnedFd};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use mio::unix::SourceFd;
use mio::{Events, Interest, Poll, Token};
use plx_bug::OrBug as _;
use plx_client_core::client::{
    ClientCore, ClientEffects, DaemonError, End, PasteTooLarge, Received,
};
use plx_client_core::compose::Capabilities;
use plx_core::command::{Command, SessionCommand};
use plx_core::event::EndReason;
use plx_core::rejection::Rejection;
use plx_protocol::client::ClientWire;
use plx_protocol::framing::{MAX_SERVER_MESSAGE, split};
use plx_protocol::hello::{HELLO, HelloError, check_hello};
use plx_protocol::server::{Decoder, ServerMessage, Unattached};
use rustix::io::Errno;
use signal_hook::consts::signal::{SIGCONT, SIGHUP, SIGINT, SIGSTOP, SIGTERM, SIGTSTP, SIGWINCH};
use signal_hook_mio::v1_0::Signals;
use termina::Event;
use termina::Parser;

use crate::keys::{key_event, mouse_event};
use crate::terminal::{self, Raw, SETUP};

const SOCKET: Token = Token(0);
const TTY: Token = Token(1);
const SIGNALS: Token = Token(2);

/// Terminal output past which the daemon is not read until it drains.
pub const BACKLOG: usize = 1 << 20;

/// How long the daemon has to answer the hello, and a command.
pub const HELLO_TIMEOUT: Duration = Duration::from_secs(5);

/// The most one read of the daemon's socket takes.
const READ_CHUNK: usize = 64 * 1024;

/// How an attached client ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The user detached; the session goes on.
    Detached,
    SessionEnded(EndReason),
    /// The daemon closed the connection without a goodbye: it crashed.
    DaemonLost,
    /// A terminating signal (SIGTERM, SIGINT, SIGHUP) ended the client.
    Signalled,
}

/// What the client noticed but could not show while attached.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Notices {
    /// Pastes too large for one message, which were not sent.
    pub pastes_too_large: usize,
    /// Requests the daemon refused.
    pub rejections: usize,
}

/// Why the client could not run.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("the daemon's hello: {0}")]
    Hello(#[from] HelloError),
    #[error("{what}: {source}")]
    Io {
        what: &'static str,
        source: io::Error,
    },
    #[error("the terminal: {0}")]
    Terminal(#[from] Errno),
    #[error(transparent)]
    Daemon(#[from] DaemonError),
    #[error("the daemon closed the connection without answering")]
    Unanswered,
    #[error("the daemon answered a command with {0}")]
    Answer(&'static str),
}

const fn io_error(what: &'static str) -> impl FnOnce(io::Error) -> ClientError {
    move |source| ClientError::Io { what, source }
}

#[derive(Debug, Default)]
struct Shell {
    daemon: Vec<u8>,
    terminal: Vec<u8>,
    rejections: usize,
}

impl ClientEffects for Shell {
    fn daemon(&mut self) -> &mut Vec<u8> {
        &mut self.daemon
    }
    fn terminal(&mut self) -> &mut Vec<u8> {
        &mut self.terminal
    }
    fn rejected(&mut self, _: Rejection) {
        self.rejections += 1;
    }
}

/// Exchanges hellos on a fresh connection to the daemon.
///
/// # Errors
///
/// When the daemon does not answer in time or speaks another protocol.
pub fn greet(stream: &mut UnixStream) -> Result<(), ClientError> {
    stream
        .write_all(&HELLO)
        .map_err(io_error("sending the hello"))?;
    stream
        .set_read_timeout(Some(HELLO_TIMEOUT))
        .map_err(io_error("setting the hello timeout"))?;
    let mut hello = [0_u8; 8];
    stream
        .read_exact(&mut hello)
        .map_err(io_error("reading the daemon's hello"))?;
    check_hello(hello)?;
    Ok(())
}

/// The daemon's answer to a command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reply {
    Applied,
    Rejected(Rejection),
    /// The session is over: the command ended it, or it was ending.
    SessionEnded(EndReason),
}

/// Runs one session command over `stream` (hellos exchanged) without
/// attaching, as the CLI does, and returns the daemon's answer.
///
/// # Errors
///
/// When the daemon does not answer in time, or breaks the protocol.
pub fn command(stream: &mut UnixStream, command: SessionCommand) -> Result<Reply, ClientError> {
    let mut out = Vec::new();
    plx_protocol::client::encode(&ClientWire::Command(Command::Session(command)), &mut out)
        .or_bug("a session command is far below the message maximum");
    stream
        .write_all(&out)
        .map_err(io_error("sending the command"))?;
    stream
        .set_read_timeout(Some(HELLO_TIMEOUT))
        .map_err(io_error("setting the answer's timeout"))?;
    let mut inbox = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        let framed = split(&inbox, MAX_SERVER_MESSAGE).map_err(DaemonError::from)?;
        if let Some((payload, _)) = framed {
            let message = Decoder::new()
                .decode(payload, &mut Unattached)
                .map_err(DaemonError::from)?;
            return match message {
                ServerMessage::Applied => Ok(Reply::Applied),
                ServerMessage::Rejected(rejection) => Ok(Reply::Rejected(rejection)),
                ServerMessage::SessionEnded(reason) => Ok(Reply::SessionEnded(reason)),
                ServerMessage::Detached => Err(ClientError::Answer("a detach")),
                ServerMessage::Clipboard => Err(ClientError::Answer("clipboard text")),
                // The receiver refuses frames while decoding.
                ServerMessage::Frame(_) => Err(ClientError::Answer("a frame")),
            };
        }
        match stream.read(&mut chunk) {
            Ok(0) => return Err(ClientError::Unanswered),
            Ok(n) => {
                inbox.extend_from_slice(chunk.get(..n).or_bug("read returns at most the buffer"));
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => {
                return Err(ClientError::Io {
                    what: "reading the daemon's answer",
                    source: error,
                });
            }
        }
    }
}

/// How following the events ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Followed {
    /// The daemon closed the stream: the session ended (its last line says
    /// so) or the daemon died.
    Closed,
    /// Whoever reads `out` went away (a closed pipe).
    ReaderGone,
}

/// Subscribes over `stream` (hellos exchanged) and copies the session's
/// event lines (JSON) to `out` as they come, until either side ends.
///
/// # Errors
///
/// When the socket or `out` fails other than by closing.
pub fn follow_events(
    mut stream: UnixStream,
    out: &mut impl io::Write,
) -> Result<Followed, ClientError> {
    let mut message = Vec::new();
    plx_protocol::client::encode(&ClientWire::Subscribe, &mut message)
        .or_bug("a subscription is far below the message maximum");
    stream
        .write_all(&message)
        .map_err(io_error("subscribing"))?;
    // Events come when they happen, however long that takes.
    stream
        .set_read_timeout(None)
        .map_err(io_error("clearing the hello timeout"))?;
    let mut chunk = [0_u8; 16 * 1024];
    loop {
        let n = match stream.read(&mut chunk) {
            Ok(0) => return Ok(Followed::Closed),
            Ok(n) => n,
            // A close with something of ours unread.
            Err(error) if error.kind() == io::ErrorKind::ConnectionReset => {
                return Ok(Followed::Closed);
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => {
                return Err(ClientError::Io {
                    what: "reading the events",
                    source: error,
                });
            }
        };
        let lines = chunk.get(..n).or_bug("read returns at most the buffer");
        match out.write_all(lines).and_then(|()| out.flush()) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::BrokenPipe => {
                return Ok(Followed::ReaderGone);
            }
            Err(error) => {
                return Err(ClientError::Io {
                    what: "writing the events",
                    source: error,
                });
            }
        }
    }
}

/// Whether more is waiting on an edge-triggered descriptor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ready {
    More,
    Drained,
}

/// Attaches over `stream` (hellos exchanged) to the terminal `tty` (the
/// client's own description of it), until the session or the client ends.
///
/// # Errors
///
/// When the terminal cannot be set up, or the daemon broke the protocol.
pub fn attach(
    stream: UnixStream,
    tty: &OwnedFd,
    capabilities: Capabilities,
) -> Result<(Outcome, Notices), ClientError> {
    stream
        .set_nonblocking(true)
        .map_err(io_error("making the socket non-blocking"))?;
    let mut stream = mio::net::UnixStream::from_std(stream);
    rustix::io::ioctl_fionbio(tty, true)?;
    let raw = Raw::enter(tty.as_fd(), capabilities.progress)?;
    terminal::write_all(tty, SETUP)?;
    let mut shell = Shell::default();
    let size = terminal::size(tty)?;
    let mut core = Some(ClientCore::attach(size, None, capabilities, &mut shell));
    let poll = Poll::new().map_err(io_error("creating the reactor"))?;
    poll.registry()
        .register(&mut stream, SOCKET, Interest::READABLE | Interest::WRITABLE)
        .map_err(io_error("registering the socket"))?;
    poll.registry()
        .register(
            &mut SourceFd(&tty.as_raw_fd()),
            TTY,
            Interest::READABLE | Interest::WRITABLE,
        )
        .map_err(io_error("registering the terminal"))?;
    let mut signals = Signals::new([SIGWINCH, SIGTSTP, SIGCONT, SIGTERM, SIGINT, SIGHUP])
        .map_err(io_error("installing signal handlers"))?;
    poll.registry()
        .register(&mut signals, SIGNALS, Interest::READABLE)
        .map_err(io_error("registering signals"))?;
    let mut run = Run {
        poll,
        stream,
        tty,
        raw: &raw,
        signals,
        parser: Parser::default(),
        to_daemon: std::mem::take(&mut shell.daemon),
        to_tty: Vec::new(),
        inbox: Vec::new(),
        chunk: vec![0; READ_CHUNK].into_boxed_slice(),
        socket: Ready::More,
        input: Ready::More,
        notices: Notices::default(),
    };
    let outcome = run.serve(&mut core, &mut shell);
    let mut notices = run.notices;
    notices.rejections = shell.rejections;
    // Whatever was drawn reaches the terminal before it is restored.
    let flushed = terminal::write_all(tty, &run.to_tty);
    drop(run);
    raw.leave()?;
    flushed?;
    Ok((outcome?, notices))
}

struct Run<'a> {
    poll: Poll,
    stream: mio::net::UnixStream,
    tty: &'a OwnedFd,
    raw: &'a Raw<'a>,
    signals: Signals,
    parser: Parser,
    to_daemon: Vec<u8>,
    to_tty: Vec<u8>,
    inbox: Vec<u8>,
    /// What one read of the socket lands in.
    chunk: Box<[u8]>,
    socket: Ready,
    input: Ready,
    notices: Notices,
}

impl Run<'_> {
    fn serve(
        &mut self,
        core: &mut Option<ClientCore>,
        shell: &mut Shell,
    ) -> Result<Outcome, ClientError> {
        let mut events = Events::with_capacity(64);
        loop {
            self.flush()?;
            match self.poll.poll(&mut events, None) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    return Err(ClientError::Io {
                        what: "polling",
                        source: error,
                    });
                }
            }
            let mut signalled = false;
            for event in &events {
                match event.token() {
                    SOCKET if event.is_readable() || event.is_read_closed() => {
                        self.socket = Ready::More;
                    }
                    TTY if event.is_readable() => self.input = Ready::More,
                    SIGNALS => signalled = true,
                    _ => {}
                }
            }
            let Some(attached) = core.as_mut() else {
                return Ok(Outcome::DaemonLost);
            };
            if signalled {
                // Which arrived (repeats collapse), then each handled once.
                let (mut resized, mut stop, mut cont, mut end) = (false, false, false, false);
                for signal in self.signals.pending() {
                    match signal {
                        SIGWINCH => resized = true,
                        SIGTSTP => stop = true,
                        SIGCONT => cont = true,
                        _ => end = true,
                    }
                }
                if end {
                    return Ok(Outcome::Signalled);
                }
                if stop {
                    self.suspend(attached, shell)?;
                } else if cont {
                    self.resume(attached, shell)?;
                } else if resized {
                    attached.resized(terminal::size(self.tty)?, None, shell);
                }
            }
            if self.input == Ready::More {
                self.read_terminal(attached, shell)?;
            }
            if self.socket == Ready::More
                && self.to_tty.len() < BACKLOG
                && let Some(outcome) = self.read_daemon(core, shell)?
            {
                return Ok(outcome);
            }
            self.to_daemon.append(&mut shell.daemon);
            self.to_tty.append(&mut shell.terminal);
        }
    }

    /// Writes what the socket and the terminal take now.
    fn flush(&mut self) -> Result<(), ClientError> {
        while !self.to_daemon.is_empty() {
            match self.stream.write(&self.to_daemon) {
                Ok(n) => {
                    self.to_daemon.drain(..n);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                // The daemon went; reading its socket tells how.
                Err(error) if error.kind() == io::ErrorKind::BrokenPipe => {
                    self.to_daemon.clear();
                }
                Err(error) => {
                    return Err(ClientError::Io {
                        what: "writing to the daemon",
                        source: error,
                    });
                }
            }
        }
        while !self.to_tty.is_empty() {
            match rustix::io::write(self.tty, &self.to_tty) {
                Ok(n) => {
                    self.to_tty.drain(..n);
                }
                Err(Errno::AGAIN) => break,
                Err(Errno::INTR) => {}
                Err(errno) => return Err(ClientError::Terminal(errno)),
            }
        }
        Ok(())
    }

    fn read_terminal(
        &mut self,
        core: &mut ClientCore,
        shell: &mut Shell,
    ) -> Result<(), ClientError> {
        let mut buf = [0_u8; 4096];
        loop {
            match rustix::io::read(self.tty, &mut buf) {
                // End of input, or nothing more for now.
                Ok(0) | Err(Errno::AGAIN) => {
                    self.input = Ready::Drained;
                    return Ok(());
                }
                Ok(n) => {
                    let bytes = buf.get(..n).or_bug("read returns at most the buffer");
                    self.parser.parse(bytes, n == buf.len());
                    while let Some(event) = self.parser.pop() {
                        match event {
                            Event::Key(key) => {
                                if let Some(key) = key_event(&key) {
                                    core.key(key, shell);
                                }
                            }
                            Event::Paste(text) => match core.paste(&text, shell) {
                                Ok(()) => {}
                                Err(PasteTooLarge { .. }) => self.notices.pastes_too_large += 1,
                            },
                            Event::Mouse(mouse) => core.mouse(mouse_event(&mouse), shell),
                            // Focus and the terminal's replies to queries
                            // plx does not make.
                            Event::WindowResized(_)
                            | Event::FocusIn
                            | Event::FocusOut
                            | Event::Csi(_)
                            | Event::Osc(_)
                            | Event::Dcs(_) => {}
                        }
                    }
                }
                Err(Errno::INTR) => {}
                Err(errno) => return Err(ClientError::Terminal(errno)),
            }
        }
    }

    /// Reads the daemon until its socket would block. Returns how the
    /// session ended, if it did.
    fn read_daemon(
        &mut self,
        core: &mut Option<ClientCore>,
        shell: &mut Shell,
    ) -> Result<Option<Outcome>, ClientError> {
        let mut closed = false;
        loop {
            match self.stream.read(&mut self.chunk) {
                // A reset is a close: the daemon closed the connection with
                // input of ours unread (it detached us after a key, with an
                // acknowledgement in flight). What it sent before still
                // counts, so it is read like an end of stream.
                Ok(0) => {
                    closed = true;
                    break;
                }
                Err(error) if error.kind() == io::ErrorKind::ConnectionReset => {
                    closed = true;
                    break;
                }
                Ok(n) => self.inbox.extend_from_slice(
                    self.chunk
                        .get(..n)
                        .or_bug("read returns at most the buffer"),
                ),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => {
                    return Err(ClientError::Io {
                        what: "reading from the daemon",
                        source: error,
                    });
                }
            }
        }
        self.socket = Ready::Drained;
        let Some(attached) = core.take() else {
            return Ok(Some(Outcome::DaemonLost));
        };
        match attached.receive(&mut self.inbox, shell)? {
            Received::Attached(attached) => *core = Some(attached),
            Received::Ended(End::Detached) => return Ok(Some(Outcome::Detached)),
            Received::Ended(End::SessionEnded(reason)) => {
                return Ok(Some(Outcome::SessionEnded(reason)));
            }
        }
        Ok(closed.then_some(Outcome::DaemonLost))
    }

    /// Stops the client (SIGTSTP): the terminal restored, then stopped
    /// until SIGCONT, then raw again and redrawn.
    fn suspend(&mut self, core: &mut ClientCore, shell: &mut Shell) -> Result<(), ClientError> {
        terminal::write_all(self.tty, &self.to_tty)?;
        self.to_tty.clear();
        self.raw.modes_off()?;
        self.raw.pause()?;
        signal_hook::low_level::raise(SIGSTOP).map_err(io_error("stopping"))?;
        self.resume(core, shell)
    }

    /// Back after a stop: raw mode and plx's modes again, everything
    /// redrawn, and the size the terminal has now.
    fn resume(&self, core: &mut ClientCore, shell: &mut Shell) -> Result<(), ClientError> {
        self.raw.resume()?;
        terminal::write_all(self.tty, SETUP)?;
        core.resized(terminal::size(self.tty)?, None, shell);
        core.resumed(shell);
        Ok(())
    }
}
