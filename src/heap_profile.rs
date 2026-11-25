//! Where the live heap is, by the code that allocated it, for the memory log: what the caches' own sizes don't
//! account for. Off unless jemalloc samples allocations (`_RJEM_MALLOC_CONF=prof:true,lg_prof_sample:20`, a sample
//! every MiB or so on average) and `AMPUP_MEMORY_HEAP_PROFILE_SECS` sets how often to look. Every look dumps
//! jemalloc's heap profile, sums the live bytes by the first frames of memstore's own code that allocated them
//! (the allocator's, std's and the collections' frames skipped), and logs the largest sites and what grew most
//! since the look before.

use std::collections::HashMap;
use std::time::Duration;

/// How many frames of a stack name its site.
const SITE_FRAMES: usize = 3;
/// How many sites a line names.
const TOP: usize = 8;

pub fn start() {
    let Some(every) = std::env::var("AMPUP_MEMORY_HEAP_PROFILE_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|s| *s > 0)
    else {
        return;
    };
    // SAFETY: opt.prof is a bool.
    if !matches!(
        unsafe { tikv_jemalloc_ctl::raw::read::<bool>(b"opt.prof\0") },
        Ok(true)
    ) {
        eprintln!(
            "heap profile: off, jemalloc isn't sampling (set _RJEM_MALLOC_CONF=prof:true in the environment)"
        );
        return;
    }
    std::thread::spawn(move || {
        let mut names: HashMap<usize, String> = HashMap::new();
        let mut last: Option<HashMap<String, f64>> = None;
        loop {
            std::thread::sleep(Duration::from_secs(every));
            match look(&mut names) {
                Ok(now) => {
                    eprintln!("{}", report(&now, last.as_ref()));
                    last = Some(now);
                }
                Err(e) => eprintln!("heap profile: {e:#}"),
            }
        }
    });
}

/// The live bytes by site now.
fn look(names: &mut HashMap<usize, String>) -> anyhow::Result<HashMap<String, f64>> {
    let path = std::env::temp_dir().join(format!("memstore-heap-{}.prof", std::process::id()));
    let c = std::ffi::CString::new(path.to_string_lossy().as_bytes())?;
    // SAFETY: prof.dump takes a C string, read during the call; `c` outlives it.
    unsafe { tikv_jemalloc_ctl::raw::write(b"prof.dump\0", c.as_ptr()) }
        .map_err(|e| anyhow::anyhow!("dump: {e}"))?;
    let text = std::fs::read_to_string(&path)?;
    let _ = std::fs::remove_file(&path);
    Ok(by_site(&text, names))
}

/// Sums a heap_v2 profile's live bytes by site, each stack's sampled bytes scaled up as jeprof does.
fn by_site(text: &str, names: &mut HashMap<usize, String>) -> HashMap<String, f64> {
    let mut lines = text.lines();
    let rate = lines
        .next()
        .and_then(|h| h.strip_prefix("heap_v2/"))
        .and_then(|r| r.trim().parse::<f64>().ok())
        .unwrap_or(524_288.0);
    let mut out: HashMap<String, f64> = HashMap::new();
    let mut stack: Option<Vec<usize>> = None;
    for line in lines {
        if let Some(addrs) = line.strip_prefix("@ ") {
            stack = Some(
                addrs
                    .split_whitespace()
                    .filter_map(|a| usize::from_str_radix(a.trim_start_matches("0x"), 16).ok())
                    .collect(),
            );
            continue;
        }
        if line.starts_with("MAPPED_LIBRARIES") {
            break;
        }
        // The stack's total, "  t*: <count>: <bytes> [...]", follows its "@" line; per-thread lines are skipped.
        let Some(rest) = line.trim_start().strip_prefix("t*:") else {
            continue;
        };
        let Some(addrs) = stack.take() else { continue };
        let mut parts = rest.split(':');
        let count: f64 = parts
            .next()
            .and_then(|c| c.trim().parse().ok())
            .unwrap_or(0.0);
        let bytes: f64 = parts
            .next()
            .and_then(|b| b.split_whitespace().next())
            .and_then(|b| b.parse().ok())
            .unwrap_or(0.0);
        if count <= 0.0 || bytes <= 0.0 {
            continue;
        }
        let scale = 1.0 / (1.0 - (-(bytes / count) / rate).exp());
        *out.entry(site(&addrs, names)).or_default() += bytes * scale;
    }
    out
}

/// The first [`SITE_FRAMES`] frames of a stack that aren't the allocator's, std's or a collection's.
fn site(addrs: &[usize], names: &mut HashMap<usize, String>) -> String {
    let mut frames = vec![];
    for a in addrs {
        let name = names.entry(*a).or_insert_with(|| name_of(*a));
        if skipped(name) {
            continue;
        }
        frames.push(name.clone());
        if frames.len() == SITE_FRAMES {
            break;
        }
    }
    match frames.is_empty() {
        true => "(unnamed)".into(),
        false => frames.join(" <- "),
    }
}

fn name_of(addr: usize) -> String {
    let mut name = None;
    // A return address: the call is the instruction before it.
    backtrace::resolve(addr.saturating_sub(1) as *mut std::ffi::c_void, |s| {
        if name.is_none() {
            name = s.name().map(|n| short(&format!("{n:#}")));
        }
    });
    name.unwrap_or_else(|| format!("{addr:#x}"))
}

/// A symbol without its generic arguments (`::<...>`), at most 120 characters: the site, not its types.
fn short(name: &str) -> String {
    let mut out = String::new();
    let mut depth = 0;
    let mut chars = name.chars().peekable();
    while let Some(c) = chars.next() {
        if depth == 0 && c == ':' && out.ends_with(':') && chars.peek() == Some(&'<') {
            out.pop();
            chars.next();
            depth = 1;
            continue;
        }
        if depth > 0 {
            match c {
                '<' => depth += 1,
                '>' => depth -= 1,
                _ => {}
            }
            continue;
        }
        out.push(c);
    }
    out.chars().take(120).collect()
}

fn skipped(name: &str) -> bool {
    let n = name.trim_start_matches(['<', '_']);
    n.contains("rjem")
        || n.starts_with("do_rallocx")
        || [
            "je_",
            "prof_",
            "imalloc",
            "ialloc",
            "rust_",
            "rdl_",
            "rg_",
            "alloc::",
            "core::",
            "std::",
            "hashbrown::",
            "tikv_jemalloc",
            "0x",
            "arrow_buffer::",
            "arrow_data::",
            "bytes::",
        ]
        .iter()
        .any(|p| n.starts_with(p))
}

fn report(now: &HashMap<String, f64>, last: Option<&HashMap<String, f64>>) -> String {
    let mb = |b: f64| (b / 1048576.0).round() as i64;
    let total: f64 = now.values().sum();
    let mut top: Vec<_> = now.iter().collect();
    top.sort_by(|a, b| b.1.total_cmp(a.1));
    let top: Vec<String> = top
        .iter()
        .take(TOP)
        .map(|(s, b)| format!("{} MB {s}", mb(**b)))
        .collect();
    let mut line = format!(
        "heap profile: {} MB live; largest: {}",
        mb(total),
        top.join("; ")
    );
    if let Some(last) = last {
        let mut grew: Vec<(String, f64)> = now
            .iter()
            .map(|(s, b)| (s.clone(), b - last.get(s).copied().unwrap_or(0.0)))
            .chain(
                last.iter()
                    .filter(|(s, _)| !now.contains_key(*s))
                    .map(|(s, b)| (s.clone(), -b)),
            )
            .collect();
        grew.sort_by(|a, b| b.1.total_cmp(&a.1));
        let grew: Vec<String> = grew
            .iter()
            .take(TOP)
            .filter(|(_, d)| mb(*d) != 0)
            .map(|(s, d)| format!("{:+} MB {s}", mb(*d)))
            .collect();
        line += &format!(" | grew most: {}", grew.join("; "));
    }
    line
}
