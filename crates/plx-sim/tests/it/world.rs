use plx_sim::daemon::{Activity, READ_BUDGET};
use plx_sim::program::Ending;
use plx_sim::world::{Choice, MAX_CLIENTS, World, command, letter, size};
use proptest::prelude::*;

fn choice() -> impl Strategy<Value = Choice> {
    (any::<u16>(), any::<u32>()).prop_map(|(pick, arg)| Choice { pick, arg })
}

/// Runs `schedule` on a fresh world and settles it; any failed check fails.
fn run(world: &mut World, schedule: &[Choice]) -> Result<(), TestCaseError> {
    for (n, &choice) in schedule.iter().enumerate() {
        world
            .step(choice)
            .map_err(|failure| TestCaseError::fail(format!("step {n} ({choice:?}): {failure}")))?;
    }
    world
        .settle()
        .map_err(|failure| TestCaseError::fail(format!("settling: {failure}")))
}

/// A world with two clients of different sizes and three panes, settled.
fn two_clients_three_panes() -> Result<World, String> {
    let mut world = World::new(Ending::Avoided)?;
    world.attach(size(60, 20)?)?;
    world.press(0, &command(0))?;
    world.press(0, &command(3))?;
    world.settle()?;
    Ok(world)
}

proptest! {
    #![proptest_config(ProptestConfig { max_shrink_iters: 2000, ..ProptestConfig::default() })]

    /// Any schedule keeps every check: what clients draw is the
    /// compositor's vocabulary only and shows their mirror, and once
    /// settled every mirror is the daemon's truth and the daemon holds
    /// exactly the clients that exist.
    #[test]
    fn any_schedule_keeps_every_check(schedule in proptest::collection::vec(choice(), 0..200)) {
        let mut world = World::new(Ending::Allowed).map_err(TestCaseError::fail)?;
        run(&mut world, &schedule)?;
    }

    /// The gate's size: two clients, three panes, two hundred steps.
    #[test]
    fn two_clients_three_panes_two_hundred_steps(schedule in proptest::collection::vec(choice(), 200)) {
        let mut world = two_clients_three_panes().map_err(TestCaseError::fail)?;
        prop_assert_eq!(world.clients(), MAX_CLIENTS);
        prop_assert_eq!(world.panes(), 3);
        run(&mut world, &schedule)?;
    }

    /// The same schedule, run twice, does exactly the same: every byte on
    /// every pipe and PTY, and every event, in order.
    #[test]
    fn the_same_schedule_traces_identically(schedule in proptest::collection::vec(choice(), 0..120)) {
        let mut first = World::new(Ending::Allowed).map_err(TestCaseError::fail)?;
        run(&mut first, &schedule)?;
        let mut second = World::new(Ending::Allowed).map_err(TestCaseError::fail)?;
        run(&mut second, &schedule)?;
        prop_assert_eq!(first.trace(), second.trace());
    }
}

/// A keystroke reaches the focused pane within one reactor iteration, even
/// with every pane flooding: client input comes
/// first, and pane output is read in budgets.
#[test]
fn a_keystroke_reaches_its_pane_while_every_pane_floods() {
    let mut world = two_clients_three_panes().unwrap();
    world.flood(READ_BUDGET * 16);
    let focus = world.daemon().session().structure().active().focus();
    let before = world.daemon().received(focus);
    // The base mode is locked: a plain letter goes to the focused pane.
    world.press(0, &[letter(b'z')]).unwrap();
    world.deliver_all();
    world.iterate().unwrap();
    assert!(
        world.daemon().received(focus) > before,
        "the key waited behind the flood"
    );
    // The flood is still being read, a budget at a time.
    assert_eq!(
        world.iterate().unwrap(),
        Activity::Busy,
        "the flood takes more than one iteration"
    );
    world.settle().unwrap();
}

/// Schedules reach what they check: frames drawn and compared, quiescence
/// compared with the daemon, several panes, both clients, and sessions that
/// survive to the end. A deterministic sample, measured at 1192 draws, 845
/// quiet checks and 5 ended sessions in 64 cases; the bounds
/// leave room for change, not for a harness that stops testing anything.
#[test]
fn schedules_reach_what_they_check() {
    use proptest::strategy::ValueTree;
    use proptest::test_runner::TestRunner;
    let mut runner = TestRunner::deterministic();
    let (mut draws, mut quiet, mut keys, mut commands, mut ended) = (0, 0, 0, 0, 0);
    let (mut most_panes, mut most_clients) = (0, 0);
    for _ in 0..64 {
        let schedule = proptest::collection::vec(choice(), 200)
            .new_tree(&mut runner)
            .unwrap()
            .current();
        let mut world = two_clients_three_panes().unwrap();
        for &c in &schedule {
            world.step(c).unwrap();
        }
        world.settle().unwrap();
        let stats = world.stats();
        draws += stats.draws;
        quiet += stats.quiet_checks;
        keys += stats.keys;
        commands += stats.commands;
        most_panes = most_panes.max(stats.most_panes);
        most_clients = most_clients.max(stats.most_clients);
        ended += usize::from(world.over());
    }
    assert!(draws >= 600, "{draws} draws");
    assert!(quiet >= 400, "{quiet} quiet checks");
    assert!(
        keys >= 600 && commands >= 600,
        "{keys} keys, {commands} commands"
    );
    assert!(most_panes >= 4, "at most {most_panes} panes");
    assert_eq!(most_clients, MAX_CLIENTS);
    assert!(ended <= 16, "{ended} of 64 sessions ended");
}
