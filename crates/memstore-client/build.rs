//! Embeds the plugins `ampup-memory-client hooks install` writes into OpenCode's, Kilo Code's and Amp's plugin
//! directories (`plugins/integrations/<agent>/ampup-memory.ts`), where the checkout has them: a build without one
//! says so when asked to install it.

use std::path::Path;

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins");
    let integrations = root.join("integrations");
    // Only paths that exist: cargo takes a missing one as changed, and would build the client again every time.
    // A directory counts as changed when anything under it does, so `plugins/` stands in until `integrations/` is
    // there.
    let watched = if integrations.is_dir() {
        &integrations
    } else {
        &root
    };
    if watched.exists() {
        println!("cargo:rerun-if-changed={}", watched.display());
    }
    let mut out = String::new();
    for (name, agent) in [("OPENCODE", "opencode"), ("KILO", "kilo"), ("AMP", "amp")] {
        let file = integrations.join(agent).join("ampup-memory.ts");
        out.push_str(&match file.canonicalize() {
            Ok(p) => format!(
                "pub const {name}: Option<&str> = Some(include_str!({:?}));\n",
                p
            ),
            Err(_) => format!("pub const {name}: Option<&str> = None;\n"),
        });
    }
    let dest = Path::new(&std::env::var("OUT_DIR").unwrap()).join("plugins.rs");
    std::fs::write(dest, out).unwrap();
}
