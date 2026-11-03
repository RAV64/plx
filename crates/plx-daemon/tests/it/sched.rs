//! The scheduler slice, as the kernel reports it in `/proc`: plx's threads
//! run with the short one, its panes' programs with the default.

use std::time::{Duration, Instant};

use plx_daemon::pty::{Program, Pty, Read};
use plx_daemon::sched::{self, SLICE_NS, Slice};
use plx_emulator::GridSize;

/// `se.slice` and `prio` from a `sched` file of `/proc`.
fn slice_and_prio(sched: &str) -> (u64, u64) {
    let field = |name: &str| {
        sched
            .lines()
            .find_map(|line| {
                let (key, value) = line.split_once(':')?;
                (key.trim() == name).then(|| value.trim().parse::<u64>().unwrap())
            })
            .unwrap_or_else(|| panic!("{name} in {sched}"))
    };
    (field("se.slice"), field("prio"))
}

fn this_thread() -> (u64, u64) {
    slice_and_prio(&std::fs::read_to_string("/proc/thread-self/sched").unwrap())
}

/// On a thread of its own, so the test's thread keeps its attributes.
fn on_a_thread<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::spawn(f).join().unwrap()
}

#[test]
fn a_thread_asks_for_the_short_slice_and_keeps_its_nice_value() {
    let (default, _) = this_thread();
    let (short, prio, back) = on_a_thread(|| {
        rustix::process::setpriority_process(None, 3).unwrap();
        assert_eq!(sched::shorten().unwrap(), Slice::Short);
        assert_eq!(sched::slice_ns().unwrap(), SLICE_NS);
        let (short, prio) = this_thread();
        sched::reset().unwrap();
        let back = this_thread().0;
        assert_eq!(sched::slice_ns().unwrap(), back, "the slice in effect");
        (short, prio, back)
    });
    assert_eq!(short, SLICE_NS);
    assert_eq!(prio, 120 + 3, "nice 3 stays");
    assert_eq!(back, default, "reset gives the default back");
}

#[test]
fn a_panes_program_runs_with_the_default_slice() {
    let (default, _) = this_thread();
    assert_ne!(default, SLICE_NS, "the default is longer than plx's");
    let output = on_a_thread(|| {
        assert_eq!(sched::shorten().unwrap(), Slice::Short);
        let program = Program {
            path: "/bin/sh".into(),
            args: vec!["-c".into(), "cat /proc/$$/sched".into()],
        };
        let size = GridSize::new(200.try_into().unwrap(), 50.try_into().unwrap()).unwrap();
        let pty = Pty::spawn(&program, size, &[], None).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut out = Vec::new();
        let mut buffer = [0_u8; 4096];
        loop {
            match pty.read(&mut buffer).unwrap() {
                Read::Data(n) => out.extend_from_slice(&buffer[..n]),
                Read::Empty => {
                    assert!(Instant::now() < deadline, "the program did not finish");
                    std::thread::sleep(Duration::from_millis(5));
                }
                Read::Closed => break,
            }
        }
        String::from_utf8(out).unwrap()
    });
    assert_eq!(slice_and_prio(&output).0, default, "{output}");
}
