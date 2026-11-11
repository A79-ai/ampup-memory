# AmpUp Memory (ampup-memory): guide for contributors and coding agents

## Build

- The toolchain is pinned in `rust-toolchain.toml`; `rustup` installs it on first use. You also need `protoc`
  (and on Debian/Ubuntu `libprotobuf-dev`, for the well-known `.proto` files Lance imports).
- Build and test locally with `--features dylib` (`cargo test --features dylib`, `cargo run --features dylib -- serve`):
  the dependencies link from one dynamic library (`crates/memstore-deps`), so an edit rebuilds in about half the time.
  Release builds stay static: never ship a binary built with it.
- A new dependency goes in `[workspace.dependencies]`, in the crate that uses it, and in `crates/memstore-deps`
  (CI checks that `memstore-deps` lists exactly the workspace's runtime dependencies).
- The local embedding model is downloaded on first use into `~/.cache/memstore/models`.

## Layout

- `crates/memstore-core` is the store: `Store` (its `lib.rs`), storage, search, query, auth, people, offboard,
  usage, workspace, audit, metrics, episode types, platform, retention.
- The root `memstore` crate (its binary `ampup-memory`) is what runs on it: the server, agent, advice, sessions, extraction, hooks, telemetry
  and the CLI. Core never depends on `memstore`, and `memstore` re-exports core (`pub use memstore_core::*`), so
  `crate::X` and `memstore::X` paths reach core items from either side. An edit in `memstore` doesn't rebuild core:
  put new code there unless the store itself needs it. A method on `Store` defined in `memstore` is an extension
  trait (see `qa_log::StoreQaLog`).
- `crates/memstore-client` is the small client binary the editor plugins run (`plugins/ampup-memory`).
- `schemas/` holds the built-in scope schemas; they are compiled into the binary (`include_str!`).
- `harness/` is the local stack (Apache Polaris over SeaweedFS in Docker), verification scripts and benchmarks. The
  release check CI runs on each build is `.github/scripts/release-check.sh`.
- `deploy/` holds self-hosting material: container images (`deploy/docker`, `deploy/spcs`), the GCP modules and
  scripts (`deploy/gcp`), least-privilege IAM templates (`deploy/iam`), the Snowflake Native App (`deploy/native`),
  Databricks notes and the deployment agent's manifests.
- `ui/` is a Next.js browser for an AmpUp Memory server (see `ui/README.md`).

## Run it locally

- No cloud and no Docker: `cargo run --features dylib -- serve --local` keeps tables, index and catalog under
  `~/.ampup-memory`, serves one caller, and exposes MCP at `http://127.0.0.1:8080/mcp`.
- Against a real Iceberg REST catalog: `docker compose -f harness/docker-compose.yml up -d`, then
  `source harness/env.sh` before running `ampup-memory` commands. `tilt up` runs that stack plus a build, a load and
  `ampup-memory serve` in one loop (see the `Tiltfile`).

## Tests

- `cargo test` (or `cargo nextest run --profile ci`, as CI does, with `AMPUP_MEMORY_CHECK_PLAN=1`) must pass before
  you open a PR, together with `cargo fmt --all --check` and `cargo clippy --all-targets --locked -- -D warnings`.
- Tests drive a user flow end to end (a real `ampup-memory serve` over HTTP and a real store on a local catalog), not a
  function in isolation. There are no in-module unit tests besides the two fail-closed principal checks in
  memstore-core.
- `tests/it/` is one test binary (a binary per file would relink the whole crate per file): a new flow is a file
  there plus a `mod` line in `tests/it/main.rs`, never a file directly in `tests/`. A test that sets a process-wide
  environment variable calls `crate::own_process` first, so the other tests never see it.
- The REST catalog flows need a catalog: `docker compose -f tests/fixtures/rest-catalog/docker-compose.yml up -d --wait`,
  `source tests/fixtures/rest-catalog/env.sh`, then `cargo test --test it rest_catalog::`. Without it they skip.
- Tests are reserved for what can't be undone once it ships:
  - memory reaching someone who must not read it (`isolation`, `access_flow`, `security_review`, `local_serve`,
    `rest_catalog`'s isolation flow, and the IAM templates' least-privilege tests in `deploy/iam`);
  - memory that was deleted surviving (`offboarding`, `retention`, `org_retract`, `rest_catalog`'s forget and
    delete flow).

  A change to one of those paths extends the test that already covers it. Other changes are verified by running
  them, not by adding load, latency, CLI or feature tests.
- Permission and auth changes: also verify that the caller who must be refused is refused. Read
  `docs/directory.md` (and answer its threat-model questions) before changing the directory or authentication.
- Other checks CI runs: `python3 -m unittest discover -s harness -p test_search_cpu_bench.py`; for `deploy/iam` and
  `deploy/gcp/terraform`, `terraform fmt -check -recursive`, `terraform validate`, `terraform test` and
  `python3 deploy/iam/check_permissions.py`. CI doesn't build `ui/`: for a change there, run `npm test` and
  `npm run lint` in `ui/` yourself.

## Pull requests

- Verify a change against a real `ampup-memory serve` and store before opening a PR (a local catalog is fine), and for
  a bug, reproduce it first and watch the fix clear it. Say in the PR what you ran and what you saw.
- Keep each PR to one change, with a description a reviewer can follow without the conversation that produced it.
