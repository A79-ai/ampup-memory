//! Adaptive memory store engine: facts live in Iceberg tables in the customer's catalog,
//! and one ledger table is the commit point (spec v1.1, "Storage and commit model").
//! A row is visible only once the ledger row for its commit has landed, and a fact is
//! visible to a principal only if they can see every episode it was derived from.

// The request handler's future holds DataFusion plans, and proving it Send/Sync went past the default limit
// (128) on CI once two changes each grew it a little (E0275 on the hyper service in server/mod.rs).
#![recursion_limit = "256"]

// The store, its storage and its read paths are memstore-core (crates/memstore-core); this crate is what runs on
// them: the server, the agent loop, extraction and the CLI's commands. Every core item is here at the same path.
pub use memstore_core::*;

// The `dylib` feature links every dependency from crates/memstore-deps, a dynamic library (dev builds only).
#[cfg(feature = "dylib")]
extern crate memstore_deps;
#[cfg(all(feature = "dylib", not(debug_assertions)))]
compile_error!(
    "the dylib feature is for dev builds: a release binary built with it needs target/'s dylib to run"
);

/// A log line, naming the org it is about when one is known (`log_org`): `org=<org> ` first. Defined before
/// the modules, so it is every module's `eprintln!`.
macro_rules! eprintln {
    () => { ::std::eprintln!() };
    ($($t:tt)+) => {
        match $crate::log_org::current() {
            Some(org) => ::std::eprintln!("org={org} {}", ::std::format_args!($($t)+)),
            None => ::std::eprintln!($($t)+),
        }
    };
}

pub mod activity;
pub mod advice;
pub mod advice_curate;
pub mod advice_files;
pub mod advice_lookups;
pub mod advice_loop;
pub mod advice_regressions;
pub mod advice_requests;
pub mod advice_route;
pub mod advice_traces;
pub mod agent;
pub mod aggregate;
pub mod alerts;
pub mod audit_sink;
pub mod auto_review;
pub mod brief;
pub mod browse;
#[cfg(feature = "control-plane")]
#[rustfmt::skip] // not in this repository; rustfmt would look for it
pub mod cloud;
pub mod compile;
pub mod cosign;
pub mod decide;
pub mod deployment;
pub mod dream;
pub mod error_leads;
pub mod extract;
pub mod feedback;
pub mod harness;
pub mod listing;
pub use memstore_client::hook;
pub mod hook_install;
pub mod keep;
pub mod mail;
pub mod notes_dream;
#[cfg(feature = "control-plane")]
#[rustfmt::skip] // not in this repository; rustfmt would look for it
pub mod onboard;
pub mod people;
pub mod phases;
pub mod pii;
pub mod preflight;
pub mod profile;
pub mod provision;
pub mod public_capabilities;
pub mod qa_log;
pub mod report;
pub mod runs;
pub mod server;
pub mod session_prs;
pub mod sessions;
pub mod support_bundle;
pub mod telemetry;
pub mod train_data;
pub mod transcripts;
