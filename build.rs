// The `dylib` feature (dev builds) links the dependencies, and with them Rust's std, dynamically. Cargo puts std's
// directory on the library path for `cargo run` and `cargo test`; this rpath lets the binaries run outside cargo too.
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var_os("CARGO_FEATURE_DYLIB").is_none() {
        return;
    }
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    let out = std::process::Command::new(rustc)
        .args(["--print", "target-libdir"])
        .output()
        .expect("rustc --print target-libdir");
    let dir = String::from_utf8(out.stdout).expect("target-libdir is UTF-8");
    println!("cargo:rustc-link-arg=-Wl,-rpath,{}", dir.trim());
}
