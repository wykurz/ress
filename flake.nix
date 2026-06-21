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
    flake-utils.lib.eachDefaultSystem (system:
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
          ];
          RUST_SRC_PATH = "${rustToolchain}/lib/rustlib/src/rust/src";
          shellHook = ''
            echo "ress dev environment — run 'just' to list commands, 'just ci' to run all checks"
          '';
        };
      });
}
