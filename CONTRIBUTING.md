# Contributing to AmpUp Memory

Thanks for helping. This file covers building, testing and sending a change. For what AmpUp Memory is and how to
run it, start with the [README](README.md).

By contributing you agree that your contribution is licensed under the [Apache License 2.0](LICENSE), the
project's license (section 5 of the license). Please follow the [Code of Conduct](CODE_OF_CONDUCT.md). Report
security issues privately, as [SECURITY.md](SECURITY.md) describes, never in a public issue.

## What you need

- **macOS on Apple Silicon, or Linux on x86_64 or arm64.** Intel Macs and Windows aren't supported: the ONNX
  Runtime library the local embedding model runs on (the `ort` crate) has no prebuilt build for them.
- **Rust through [rustup](https://rustup.rs).** `rust-toolchain.toml` pins the toolchain (with rustfmt and
  clippy), and rustup installs it on the first `cargo` command.
- **protoc**, the Protocol Buffers compiler (`brew install protobuf`, `apt install protobuf-compiler`).
- **A C/C++ toolchain** (Xcode Command Line Tools on macOS, `build-essential` on Debian or Ubuntu).
- **Disk space:** a build compiles Lance, DataFusion, Arrow, Iceberg and ONNX Runtime; give `target/` 30 GB or so.
- **Network on the first build and test run:** cargo fetches crates from crates.io, `ort`'s build script downloads
  ONNX Runtime, and the first test that embeds downloads the local embedding model from Hugging Face into
  `~/.cache/memstore/models`. No account, API key or cloud access is needed for any of it.
- **Docker** (optional), only for the REST catalog tests below.

`.cargo/config.toml` sets two environment variables for `cargo test` and `cargo run` and nothing else: a larger
thread stack (`RUST_MIN_STACK`, which debug builds need) and `MEMSTORE_EMBED=local`, so the tests embed with the local
model and never call an embeddings API, whatever keys your shell holds. No compiler wrapper or private registry is
required; every dependency comes from crates.io.

## Build and run

Build and test locally with the `dylib` feature. It links the dependencies from one dynamic library
(`crates/memstore-deps`), so an edit rebuilds in about half the time:

```sh
cargo build --features dylib
cargo run --features dylib -- serve --local /tmp/memstore-dev   # a local store, MCP at http://127.0.0.1:8080/mcp
```

Release builds stay static: never ship a binary built with `--features dylib`. A release build is
`cargo build --release --bin memstore --bin memstore-client`, the command the release workflow runs.

## The code

Two crates and the root:

- `crates/memstore-core` is the store: `Store` (its `lib.rs`), storage, search, query, auth, people, offboarding,
  usage, workspaces, audit, metrics, episode types, platform and retention.
- The root `memstore` crate is what runs on it: the server, agent, advice, sessions, extraction, hooks, telemetry
  and the CLI. Core never depends on memstore, and memstore re-exports core (`pub use memstore_core::*`), so
  `crate::X` and `memstore::X` paths reach core items from either side. An edit in memstore doesn't rebuild core:
  put new code there unless the store itself needs it. A method on `Store` defined in memstore is an extension
  trait (see `qa_log::StoreQaLog`).
- `crates/memstore-client` is what runs on a person's machine: the sign-in and contexts, and the coding agents'
  hooks and MCP bridge.

A new dependency goes in `[workspace.dependencies]` in the root `Cargo.toml`, in the crate that uses it, and in
`crates/memstore-deps` (CI checks the last).

## Tests: what gets one, and how to verify everything else

The test suite is deliberately small. It holds only the tests for what can't be undone once it ships:

- **memory reaching someone who must not read it:** `tests/it/` `isolation`, `access_flow`, `security_review`,
  `local_serve`, and `rest_catalog`'s isolation flow (and the IAM templates' least-privilege runs, where `deploy/`
  has them);
- **memory that was deleted surviving:** `offboarding`, `retention`, `org_retract`, and `rest_catalog`'s forget
  and delete flow.

A change to one of those paths extends the test that already covers it. Nothing else gets a test, unit or
integration (no load, latency, CLI, agent or feature tests), unless a maintainer asks for one. Instead:

- **Verify the change locally before you open a pull request.** Run the flow it touches against a real
  `memstore serve` and store (`serve --local` is fine). For a bug, reproduce it first and watch the fix clear it.
- **Permission and auth changes:** also check that the caller who must be refused is refused.
- **Say in the pull request what you ran and what you saw.**

Those tests drive a user flow end to end (a real `memstore serve` over HTTP and a real store on a local catalog),
not a function in isolation. There are no in-module unit tests besides the two fail-closed principal checks in
memstore-core. `tests/it/` is one test binary: a new flow is a file there plus a `mod` line in `tests/it/main.rs`,
never a file directly in `tests/` (CI checks). A test that sets a process-wide environment variable calls
`crate::own_process` first, so the other tests never see it.

Run the whole suite before you push; it must pass:

```sh
cargo test --features dylib
```

or, faster, with [cargo-nextest](https://nexte.st) (one process per test; `.config/nextest.toml` holds the `ci`
profile CI uses):

```sh
cargo nextest run --features dylib
cargo nextest run --features dylib --profile ci   # what CI runs: every test even after a failure
```

### REST catalog tests

`tests/it/rest_catalog.rs` runs the replica, offboarding and isolation flows against a real Iceberg REST catalog
over object storage. Without the fixture those tests skip. To run them, start Apache Iceberg's REST fixture and
MinIO with Docker:

```sh
docker compose -f tests/fixtures/rest-catalog/docker-compose.yml up -d --wait
source tests/fixtures/rest-catalog/env.sh
cargo test --features dylib --test it rest_catalog::
docker compose -f tests/fixtures/rest-catalog/docker-compose.yml down
```

CI runs them nightly (`.github/workflows/catalog-nightly.yml`).

## Lint

```sh
cargo fmt --all
cargo clippy --all-targets --locked -- -D warnings
```

CI fails on any formatting difference or clippy warning.

## Pull requests

- Keep a pull request to one change, and say why it is needed, not only what it does.
- Fill in the template: what you ran locally and what you saw.
- CI (`.github/workflows/ci.yml`) runs fmt, clippy and the test suite on GitHub-hosted runners, with no secrets.
  It must pass before a maintainer merges.
- Open an issue first for a large change or a new feature, so we can agree on the approach before you build it.

## Releases

Maintainers release by pushing a tag: `v*` for the server and CLI, `client-v*` for memstore-client alone. The
release workflow (`.github/workflows/release.yml`) builds Linux x86_64, Linux arm64 and Apple Silicon macOS
binaries, publishes them to this repository's GitHub Releases, and installs them with `install.sh` to check them.
