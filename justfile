# ress development task runner
# see https://github.com/casey/just for more info

# list available commands
default:
    @just --list

# run all lints (fmt + clippy)
lint:
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets --features ress-core/bench-internals -- -D warnings

# format code
fmt:
    cargo fmt --all

# run tests (debug, nextest)
test:
    cargo nextest run --workspace

# run tests in release mode
test-release:
    cargo nextest run --workspace --release

# run doctests
doctest:
    cargo test --doc --workspace

# run all tests (debug + release + doctests)
test-all: test test-release doctest
    @echo "all tests passed"

# quick compilation check
check:
    cargo check --workspace --all-targets

# build all packages
build:
    cargo build --workspace

# build release binaries
build-release:
    cargo build --workspace --release

# build and check documentation
doc:
    cargo doc --no-deps --workspace

# run criterion benches locally (never in CI)
bench:
    cargo bench --workspace --features ress-core/bench-internals

# compile benches without running them (bit-rot guard, used by ci)
bench-build:
    cargo bench --workspace --no-run --features ress-core/bench-internals

# materialize the standard fixture set into ./fixtures/
fixtures:
    bash scripts/perf.sh --fixtures-only

# end-to-end perf comparison, ress vs less (see docs/perf.md); try --quick
perf *ARGS:
    bash scripts/perf.sh {{ARGS}}

# run all CI checks locally
ci: lint doc test-all bench-build
    @echo "all CI checks passed"

# clean build artifacts
clean:
    cargo clean
