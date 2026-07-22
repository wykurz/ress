{
  description = "ress - a fast pager for huge files";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay }:
    # ress is Linux-only by design (ress-perf's pty/screen-capture modules need
    # pipe2/ppoll, Linux-only syscalls with no Darwin equivalent) -- eachDefaultSystem
    # advertised aarch64-darwin/x86_64-darwin outputs too (flake-utils' own
    # defaultSystems: aarch64-darwin, aarch64-linux, x86_64-darwin, x86_64-linux),
    # so `nix build`/`nix flake check` on a Mac would evaluate cleanly and then fail
    # deep in ress-perf's checkPhase compile with a raw missing-symbol/syscall error,
    # not a clear "this doesn't run here." eachSystem with an explicit Linux-only list
    # makes the unsupported platform fail at flake-eval time instead, with every
    # downstream tool (nix build, nix flake check, Determinate's own eval-all-systems
    # check) reporting it plainly rather than a confusing deep build failure.
    flake-utils.lib.eachSystem [ "x86_64-linux" "aarch64-linux" ] (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs {
          inherit system overlays;
        };

        rustToolchain = pkgs.rust-bin.stable."1.95.0".default.override {
          extensions = [ "rustfmt" "clippy" "rust-src" ];
        };

        rustPlatform = pkgs.makeRustPlatform {
          cargo = rustToolchain;
          rustc = rustToolchain;
        };
      in
      {
        # `nix build` produces the ress binary.
        packages.default = rustPlatform.buildRustPackage {
          pname = "ress";
          version = "0.1.0";
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;
          # found in PR #44 pass-2 review: with no package selector, cargo's own default
          # "workspace, no explicit -p" build behavior built (and buildRustPackage's own
          # installPhase then installed) every workspace member's binaries -- ress-perf and its
          # own fake-pager test double included -- into result/bin, though this package only
          # ever claimed to produce ress. Restricted to the ONE package this flake actually
          # ships; deliberately does NOT touch cargoTestFlags (unset, so checkPhase's own
          # default `cargo test` keeps exercising the WHOLE workspace, ress-perf's own real-less
          # screen tests included -- see nativeCheckInputs' own comment just below) -- the two
          # are independent buildRustPackage settings, build scope and check scope, and only the
          # former needed narrowing.
          cargoBuildFlags = [ "-p" "ress" ];
          # ress-perf's screen tests capture a frame from a real `less`; the
          # hermetic check sandbox must provide it (the dev shell and the
          # plain-cargo CI jobs already do).
          nativeCheckInputs = [ pkgs.less ];
          meta = with pkgs.lib; {
            description = "A fast pager for huge files";
            homepage = "https://github.com/wykurz/ress";
            license = licenses.mit;
            maintainers = [ ];
          };
        };

        # `nix develop` / direnv dev shell with the pinned toolchain + tools.
        devShells.default = pkgs.mkShell {
          buildInputs = [
            rustToolchain
            pkgs.rust-analyzer
            pkgs.just
            pkgs.cargo-nextest
            pkgs.cargo-flamegraph
            pkgs.inferno
            pkgs.tokio-console
            pkgs.gh
            pkgs.less
          ];
          RUST_SRC_PATH = "${rustToolchain}/lib/rustlib/src/rust/src";
          shellHook = ''
            echo "ress dev environment — run 'just' to list commands, 'just ci' to run all checks"
          '';
        };
      });
}
