# Developing plx

plx is a terminal multiplexer for Linux on x86-64-v3, built on libghostty-vt
(Ghostty v1.4.0, pinned in `flake.nix`).

## Shape

One daemon per session, one client per attached terminal, talking over a
Unix socket in `$XDG_RUNTIME_DIR/plx/v1`.

- The daemon is a single-threaded mio reactor around a pure, deterministic
  core: `Core::step(now, input, effects)`. Each wake-up handles client input,
  then budgeted PTY reads, then exits, then a due tick.
- The daemon sends frames as cell-row deltas, with one frame in flight per
  client. A slow client gets fewer frames, never a queue.
- The client keeps mirror grids and composes its terminal's output from
  damage.

| Crate | What it is |
|---|---|
| `plx` | The binary: command line, starting and finding daemons, attaching |
| `plx-daemon` | The reactor, PTYs (rustix, pidfds), socket, log, scheduler slice |
| `plx-client` | Raw mode, input parsing, the terminal writer, restore guards |
| `plx-core` | The session core (`no_std`, no IO, no clock) |
| `plx-client-core` | The client state machine and compositor (`no_std`) |
| `plx-protocol` | Hello handshake, u32 length-prefixed postcard frames |
| `plx-emulator` | The `Emulator` trait and its value types: 8-byte cells, grids, tables |
| `plx-ghostty` | The libghostty-vt adapter implementing `Emulator` |
| `plx-ghostty-sys` | Bindings to libghostty-vt (generated, committed) |
| `plx-config` | `config.toml` parsed into validated types (no IO) |
| `plx-collections` | Deterministic interners with id reuse |
| `plx-bug` | The panic macros |
| `plx-sim` | Deterministic whole-system simulation (test-only) |
| `plx-alloc-count` | Counting allocator for allocation tests (dev-only) |
| `xtask` | Gate, source lints, bindings generator (separate workspace) |

Configuration lives in `$XDG_CONFIG_HOME/plx/config.toml`. The keys are
`default_shell`, `scrollback_lines` and `scrollback_editor`, and unknown keys
are errors.

## Rules

These are enforced by the compiler, clippy, `xtask lint` or the gate:

- **No `unwrap`, `expect`, `panic!`, indexing or wildcard match arms.**
  Workspace lints deny them.
- **Panics go through `plx-bug`.** `bug!`, `invariant!`, `contract!`,
  `breach!`, `.or_bug(…)` and `.or_contract(…)` each state their invariant as
  a literal. The set of sites must match `invariants.lock`.
- **No runtime `assert!` in production code.** Compile-time
  `const _: () = assert!(…)` is allowed.
- **Lint exceptions use `#[expect(…, reason = "…")]`.** `#[allow]` is denied.
- **Copying and sharing go through views.** `Clone::clone`, `Rc`, `Arc`,
  `Mutex` and `catch_unwind` are disallowed methods and types
  (`clippy.toml`). Lend a reference or a view instead.
- **No `dyn`.** Static dispatch only.
- **Domain fields have their own types.** In the domain crates, struct fields
  are never bare `bool`, `String` or `str`, apart from single-field
  newtypes; use enums and newtypes.
- **`unsafe` is denied workspace-wide.** The core crates forbid it. Each
  site that needs it carries a SAFETY comment and its own `#[expect]`.
  These sites are:
  - `plx-ghostty`, the FFI adapter;
  - `plx-emulator/src/cell.rs`, for wire views and AVX2 checks;
  - `plx-daemon/src/pty.rs`, the pane's `pre_exec`;
  - `plx-daemon/src/sched.rs`, for `sched_getattr` and `sched_setattr`;
  - `plx/src/cpu.rs`, for `XGETBV`.
- **No allocations in steady state on the measured paths.** These are
  redraw, feed and sync; scrolling without scrollback; key and paste
  encoding; frames; and rendering. Tests with `plx-alloc-count` check them.
- **Release builds use `panic = "abort"` with overflow checks.**

## Environment

Everything runs inside the flake's dev shell (`nix develop`). It provides:

- the pinned nightly toolchain, building for x86-64-v3 (AVX2 is required;
  plx-emulator refuses to compile without it, and `.cargo/config.toml` sets
  it for rustc and rustdoc alike);
- `cargo-nextest`, `cargo-deny`, `cargo-mutants`;
- `perf`, `valgrind`, `taskset`;
- libclang for the bindings generator;
- `PLX_GHOSTTY_VT_DIR`, the libghostty-vt build from the pinned Ghostty
  input (ReleaseSafe, with `plx_ghostty::Feature::COMPILED_OUT` compiled out).

The first `nix develop` after a pin change builds libghostty-vt with Zig. After
that it lives in the Nix store and is never rebuilt until the pin changes.

## Inner loop

```sh
cargo check -p plx-core            # fastest feedback, one crate
cargo nextest run -p plx-core      # unit + property tests, 64 cases each
```

Before every commit, run the gate. It is the one definition of green; its
steps and their order live in `xtask/src/main.rs`:

```sh
cargo run --manifest-path xtask/Cargo.toml -- gate
```

It runs, in order: fmt (first, because it changes what clippy sees), `xtask
lint`, clippy on the workspace and on xtask, nextest, doctests, xtask's own
tests, and cargo-deny.

After adding or removing a panic site, check whether a type could express
the invariant instead, then accept the change with:

```sh
cargo run --manifest-path xtask/Cargo.toml -- lint --bless
```

- `PROPTEST_CASES` defaults to 64 via `.cargo/config.toml`, and it overrides
  `ProptestConfig::with_cases` in the tests. Export a larger value for deep
  runs, e.g. `PROPTEST_CASES=10000`; the environment wins. Where you cannot
  set the environment, pass it to cargo (nextest ignores this; use
  `cargo test`):
  `cargo --config 'env.PROPTEST_CASES.value="10000"' --config 'env.PROPTEST_CASES.force=true' test -p <crate> --test it`.
- Each crate has a single integration-test binary (`tests/it/main.rs`) to keep
  link steps down.
- Zero-allocation tests install `plx-alloc-count`'s counting allocator for
  their binary. Its counters are per thread: a measurement counts exactly
  what its closure allocated on the calling thread, so the harness's other
  threads cannot disturb it, under nextest or `cargo test`.
- Editing a crate rebuilds only that crate and its dependents. libghostty and
  its bindings never rebuild during normal work. If you see
  `Running .../build-script-build` for `plx-ghostty-sys` without having
  changed the Ghostty pin, something in the environment changed. Fix that
  rather than living with the rebuilds.

## Installing

`nix profile add .` installs the `plx` binary (the flake's default
package): the same nightly and libghostty-vt as the dev shell, built for
x86-64-v3. The package build skips the test suite, which needs PTYs,
sockets and a runtime directory the Nix sandbox lacks; run the gate in
the dev shell before installing.

A running daemon keeps the build it started with, and the protocol version
does not change with every frame change. After installing a new build, end
the older build's sessions with `plx kill-session`.

### Tabs named after the directory

Programs in a pane find their session in `PLX_SESSION`, so a shell can
name its tab after its working directory with `plx rename-tab`. For fish:

```fish
if set -q PLX_SESSION; and command -q plx
    function __plx_tab --on-variable PWD
        set -l title (test (pwd) = $HOME; and echo "~"; or basename (pwd))
        # Tab names hold at most 64 characters.
        plx rename-tab -- (string sub --length 64 -- $title)
    end
    __plx_tab
end
```

`plx rename-tab` renames the active tab: a shell that changes directory in
a tab in the background renames the active one.

## Measuring

The divan benches are `compose` (`plx-client-core`), `core` (`plx-core`),
`codec` (`plx-protocol`) and `ingest` (`plx-ghostty`).
`cargo bench -p <crate> --bench <name> --no-run` prints a bench's binary.
A single benchmark runs a fixed number of iterations, so its instruction
counts are deterministic, as
`<bench binary> --bench --exact <path> --sample-count N --sample-size M`.
Benches build with line tables (`[profile.bench]`), so profiles map to
source lines.

The echo bench drives a real daemon with `/bin/sh` panes in a temporary
directory, so it touches no user session:

```sh
cargo bench -p plx-daemon --bench echo --no-run
taskset -c 8-11 <echo binary> 4 od 0-7    # 4 panes flood with od on cpus 0-7
```

Its arguments are `[N|ingest|resize|raw [feed]] [od|cat] [CPUS] [zoom]`. It
reports keystroke-to-echo percentiles, or ingest throughput. Compare
variants by alternating them in one session; timings drift between
sessions.

Facts that hold for the hot paths:

- Scalar Rust over `[u8; 8]` cell arrays does not vectorise, so the bulk
  cell loops are explicit AVX2.
- A nested `#[target_feature]` helper must be `#[inline]`, or it stays a
  call.
- Hot output paths write runs with one `extend`, not a `push` per byte.
- libghostty's static archive brings Zig's `memcpy`, which overrides
  glibc's in every binary that links it.
- Single-pane ingest is bounded by the kernel's tty path, not by plx.

## Bumping libghostty

1. Change `inputs.ghostty.url` in `flake.nix` and run `nix flake update ghostty`.
2. Re-enter the dev shell; this builds the new libghostty-vt once.
3. Regenerate the committed bindings:
   `cargo run --manifest-path xtask/Cargo.toml -- ghostty-bindings`
4. Run `cargo nextest run -p plx-ghostty-sys -p plx-ghostty`. The layout test
   compares the bindings with the linked library's `ghostty_type_json()`
   manifest, another checks that the bindings were generated from the linked
   version, and the adapter's property tests cover behaviour. The linked
   version itself comes from the pin, so no constant needs editing.
