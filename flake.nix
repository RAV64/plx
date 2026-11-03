{
  description = "plx";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    # libghostty-vt source pin. Do not make it follow our
    # nixpkgs: Ghostty pins its own Zig 0.17 toolchain. Bumping this pin means
    # regenerating crates/plx-ghostty-sys/src/bindings.rs (see xtask/).
    ghostty.url = "github:ghostty-org/ghostty/v1.4.0";
  };

  outputs =
    {
      nixpkgs,
      rust-overlay,
      ghostty,
      ...
    }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs {
        inherit system;
        overlays = [ rust-overlay.overlays.default ];
      };

      # ReleaseSafe keeps Zig's runtime safety checks. The -Dvt-features list
      # must match plx_ghostty::Feature::COMPILED_OUT (a test checks): kitty
      # graphics is verified through ghostty_build_info() at startup; the
      # glyph protocol, which build info cannot report, is also turned off on
      # every terminal.
      libghostty-vt = ghostty.packages.${system}.libghostty-vt-releasesafe.overrideAttrs (old: {
        zigBuildFlags = old.zigBuildFlags ++ [ "-Dvt-features=-kitty-graphics,-glyph-protocol" ];
      });

      # The dev shell's nightly; the package builds with the same compiler.
      nightly = pkgs.rust-bin.nightly."2026-10-27";
      rustPlatform = pkgs.makeRustPlatform {
        cargo = nightly.minimal;
        rustc = nightly.minimal;
      };

      # The `plx` binary, for `nix profile add`. It is built for x86-64-v3
      # (.cargo/config.toml) and checks the CPU at startup. The test suite
      # needs PTYs, sockets and a runtime directory the build sandbox lacks;
      # it runs in the dev shell (`cargo run --manifest-path xtask/Cargo.toml
      # -- gate`).
      plx = rustPlatform.buildRustPackage {
        pname = "plx";
        version = "0.1.0";
        src = ./.;
        cargoLock.lockFile = ./Cargo.lock;
        cargoBuildFlags = [
          "-p"
          "plx"
        ];
        env = {
          PLX_GHOSTTY_VT_DIR = "${libghostty-vt.dev}";
          # nixpkgs' cargo hooks set RUSTFLAGS, which takes precedence over
          # .cargo/config.toml's build.rustflags: the same flag, repeated
          # (plx-emulator refuses to compile without it).
          RUSTFLAGS = "-C target-cpu=x86-64-v3";
        };
        doCheck = false;
        meta.mainProgram = "plx";
      };
    in
    {
      packages.${system} = {
        inherit libghostty-vt plx;
        default = plx;
      };

      devShells.${system}.default = pkgs.mkShell {
        packages = [
          (nightly.default.override {
            extensions = [
              "rust-src"
              "rust-analyzer"
            ];
          })
          pkgs.cargo-nextest
          pkgs.cargo-deny
          pkgs.cargo-mutants
          pkgs.git
          # Profiling (docs/DEVELOPMENT.md "Profiling"): measure, never guess.
          # cachegrind gives instruction counts that ignore machine load;
          # perf gives cycles, IPC and per-instruction samples; cargo-asm
          # shows the generated assembly and LLVM IR; taskset pins a run to
          # one core (this host mixes performance and efficiency cores).
          pkgs.valgrind
          pkgs.perf
          pkgs.cargo-show-asm
          pkgs.util-linux
          # libclang for the bindings generator in xtask/ (never part of the
          # workspace build).
          pkgs.rustPlatform.bindgenHook
        ];

        # Immutable store path; it only changes when the ghostty pin changes,
        # which is the only time plx-ghostty-sys's build script reruns.
        PLX_GHOSTTY_VT_DIR = "${libghostty-vt.dev}";
      };
    };
}
