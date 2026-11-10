# Contributing to AmpUp Memory

Thanks for helping. This file covers building, testing and sending a change. For what AmpUp Memory is and how to
run it, start with the [README](README.md).

AmpUp Memory is licensed under the [Apache License 2.0](LICENSE). Before we can accept a contribution, each of its
authors signs our [Contributor License Agreement](#contributor-license-agreement), once. Please follow the
[Code of Conduct](CODE_OF_CONDUCT.md). Report security issues privately, as [SECURITY.md](SECURITY.md) describes,
never in a public issue.

## Contributor License Agreement

AmpUp Memory is developed by A79, Inc., the company behind AmpUp. Every author of the commits in a pull request
signs the [Individual Contributor License Agreement](CLA.md) (CLA), adapted from the Apache Software Foundation's.
You keep the copyright in your work: the CLA doesn't assign it, it licenses it (and any patent claims your
contribution needs) to A79, Inc. and to everyone who receives the software it distributes, and it states that the
work is yours to license.

- **How to sign.** When you open a pull request, the CLA bot checks its commits' authors. If one hasn't signed, it
  comments with a link to the CLA; each of them signs by posting this comment on the pull request, from their own
  GitHub account:

  > I have read the CLA Document and I hereby sign the CLA

  The pull request's **CLA** check then turns green, with no new push needed (comment `recheck` if it doesn't).
  A signature covers all your future contributions, so you sign once. The bot records it (your GitHub username
  and account id, the pull request and the date) in `signatures/v1/cla.json` on this repository's
  `cla-signatures` branch.
- **Commits must be linked to your account.** The bot matches each commit to a GitHub account by its email, so
  commit with an address [added to your GitHub account](https://docs.github.com/articles/adding-an-email-address-to-your-github-account)
  (or your `users.noreply.github.com` address).
- **Contributing for your employer.** If your employer has rights in what you write, as many employment agreements
  give it, it must agree to your contributing. One way is the
  [Corporate Contributor License Agreement](CLA-CORPORATE.md), signed by someone who can sign for the company and
  listing the employees it covers. To sign it, open an issue asking for the Corporate CLA (don't post the signed
  agreement or anyone's contact details there) and a maintainer will reply with how to send it privately. Its
  employees still sign the individual CLA on their first pull request.
- **If the CLA changes,** it gets a new version and a new signatures file, and contributors sign again.

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

### How an accepted pull request lands

This repository's `main` is generated from AmpUp's internal repository, which it mirrors on every change there: a
commit to `main` here would be overwritten by the next sync, so nothing is merged here directly. When your pull
request is accepted (its CLA and CI checks green, and reviewed):

1. A maintainer brings it into the internal repository as one commit, with you as its author and a
   `Co-authored-by` line for every author of your pull request's commits.
2. The next sync publishes it to `main` here, as a commit by the sync's bot.
3. The maintainer closes your pull request with a link to that commit.

Your pull request shows as closed rather than merged: that is how every accepted change lands here. A pull
request that touches files whose public version is generated (the workflows, the README, `install.sh` among them)
is carried over by hand, so it may land as a slightly different change; the maintainer says so on it.

## Releases

Maintainers release by pushing a tag: `v*` for the server and CLI, `client-v*` for memstore-client alone. The
release workflow (`.github/workflows/release.yml`) builds Linux x86_64, Linux arm64 and Apple Silicon macOS
binaries, publishes them to this repository's GitHub Releases, and installs them with `install.sh` to check them.
