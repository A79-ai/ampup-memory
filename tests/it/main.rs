//! Every integration test, in one test binary: each file here is a module. A binary per file linked the crate
//! and all of its dependencies (Lance, DataFusion, ONNX Runtime) once per file, so a change to `src/` relinked
//! two dozen binaries of over 300 MB each; one binary links once.
//!
//! A new flow goes in a file here, with a `mod` line below (CI fails a file in `tests/` itself, which would
//! build a binary of its own, and a file here without its `mod` line, which would never run).

// A test's async block holds the server's request handler, whose layout went past the default query depth (128)
// on CI's x86_64 build (access_flow's conflict test), as the crate's own did (src/lib.rs).
#![recursion_limit = "256"]

mod access_flow;
mod advice_files;
mod advice_fit;
mod advice_hold;
mod advice_never_blocks_agents;
mod advice_reads;
mod advice_versions;
mod agent_cells;
mod cache_budget;
mod cells;
mod classic_ranking;
mod compile_settings;
mod error_leads;
mod governance;
mod hooks_never_block;
mod index_known_cap;
mod index_old_versions;
mod isolation;
mod listing;
mod local_serve;
mod mcp_act_as;
mod merge_subjects;
mod offboarding;
mod org_retract;
mod personal_advice;
mod rest_catalog;
mod retention;
mod security_review;
mod session_end_keeps_everything;
mod train_log;

/// Whether the calling test goes on in this process. `cargo test` runs every test of this binary in one
/// process, so a test that sets process-wide settings (environment variables the server reads, such as
/// `AMPUP_MEMORY_INDEX_URI` or `AMPUP_MEMORY_IAP_AUDIENCE`) would hand them to every other test. Such a test calls
/// this first: it runs again in a process of its own (this binary, that test only), and here this returns
/// false once that process passed. In that process, and under nextest (a process per test already), it
/// returns true.
///
/// `test` is the test's path in this binary: `file::name`.
pub fn own_process(test: &str) -> bool {
    const OWN: &str = "AMPUP_MEMORY_TEST_OWN_PROCESS";
    if std::env::var(OWN).as_deref() == Ok(test)
        || std::env::var("NEXTEST_EXECUTION_MODE").as_deref() == Ok("process-per-test")
    {
        return true;
    }
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            test,
            "--exact",
            "--include-ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(OWN, test)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    // Captured like this test's own output: shown if it fails, or with --nocapture.
    print!("{stdout}");
    eprint!("{}", String::from_utf8_lossy(&out.stderr));
    // "1 passed" too: a misspelled path runs no test at all, and that process passes.
    assert!(
        out.status.success() && stdout.contains("test result: ok. 1 passed"),
        "{test}, in a process of its own, did not pass"
    );
    false
}

/// The store `memstore serve --local dir` serves, read in-process: its catalog, and its index (`dir/index`)
/// rather than the default location (`~/.cache/memstore/index`), which every test here without one of its
/// own would share, mid-write, with this one.
pub async fn local_store(dir: &std::path::Path) -> memstore::Store {
    memstore::Store::new(memstore::platform::local_dir(dir).await.unwrap())
        .with_index_base(dir.join("index").display().to_string())
}
