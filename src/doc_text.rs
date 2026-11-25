//! The text of a document file a person adds (Studio's "Add to memory", `/learn`): a PDF's or a Word file's, read
//! here so what it says is filed as text, as a Markdown or text file's is.
//!
//! - A PDF's text is what its pages draw, in reading order as the file lays it out. A scan (pages that are images)
//!   has none, and is refused, saying so: nothing here reads text off an image.
//! - A Word file (`.docx`) is read from its main part (`word/document.xml`): each paragraph a line, a heading
//!   style as a Markdown heading, a list item as `- `, a table row's cells joined with ` | `. So a procedure's
//!   steps and headings reach the compiler as the author wrote them.
//!
//! - A web page (`.html`, a page fetched for Import from the web) as Markdown: its headings, lists, tables and
//!   links kept, its scripts, styles, navigation, header, footer, forms and media left out ([`html`]).
//!
//! Either is parsed by a library that may panic on a malformed file: [`text_of`] catches that and says the file
//! couldn't be read. Those libraries also inflate every compressed stream in full, and may loop on a malformed file,
//! so the server reads a person's file in a process of its own, limited in memory and time ([`isolated`]). [`parts`] cuts a long text at paragraph breaks into pieces no longer than a limit, so a long
//! document is filed whole, as several items, rather than refused or cut.

use std::io::Read;
use std::sync::LazyLock;
use std::time::Duration;

/// The hidden argument the server's own binary is run with to read one document in a process of its own
/// ([`isolated`], [`child_main`]).
pub const CHILD_ARG: &str = "__doc-text";
/// The address space a reading process may take (Linux): a file whose streams inflate past it (a decompression
/// bomb) ends that process, not the server.
const CHILD_MEMORY: u64 = 1536 << 20;
/// How long one document may take to read, wall clock (the process is killed then) and CPU (its own limit).
const CHILD_TIME: Duration = Duration::from_secs(30);
/// Most text one document gives: more than any document is filed as ([`parts`]' 32 items of 64 KB, with room).
const MAX_OUT: usize = 4 << 20;
/// Documents read at once on this server: each is a process of its own.
static READERS: LazyLock<tokio::sync::Semaphore> = LazyLock::new(|| tokio::sync::Semaphore::new(2));

/// [`text_of`] in a process of its own: the server's binary run with [`CHILD_ARG`], the file on its stdin, its text
/// on its stdout, its memory limited ([`CHILD_MEMORY`]) and killed after [`CHILD_TIME`]. The parsers inflate every
/// stream they meet in full, with no limit of their own, and a malformed file can send them into a loop: here
/// either ends that process, and the person is told the file couldn't be read. A process that isn't the server's
/// binary (a test linking the server in) reads none.
pub async fn isolated(ext: &str, bytes: Vec<u8>) -> Result<String, String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let exe = std::env::current_exe()
        .ok()
        .filter(|p| {
            p.file_stem().is_some_and(|n| {
                n.to_string_lossy() == env!("CARGO_PKG_NAME")
                    || n.to_string_lossy() == "ampup-memory"
            })
        })
        .ok_or_else(|| "documents aren't read by this process".to_string())?;
    let _slot = READERS
        .acquire()
        .await
        .map_err(|_| "the server is stopping".to_string())?;
    let mut child = tokio::process::Command::new(exe)
        .arg(CHILD_ARG)
        .arg(ext)
        .env_clear()
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| {
            eprintln!("doc_text: couldn't start a reader: {e}");
            "it couldn't be read just now".to_string()
        })?;
    let mut stdin = child.stdin.take().expect("piped");
    let mut stdout = child.stdout.take().expect("piped");
    let run = async {
        // Written while the output is read: neither side waits on a full pipe.
        let write = async move {
            let _ = stdin.write_all(&bytes).await;
            drop(stdin);
        };
        let read = async {
            let mut out = Vec::new();
            let _ = (&mut stdout)
                .take(MAX_OUT as u64 + 1)
                .read_to_end(&mut out)
                .await;
            out
        };
        let ((), out) = tokio::join!(write, read);
        let status = child.wait().await;
        (status, out)
    };
    let (status, out) = match tokio::time::timeout(CHILD_TIME, run).await {
        Ok(v) => v,
        // Dropped: the child is killed (`kill_on_drop`).
        Err(_) => return Err("it took too long to read: the file may be damaged".into()),
    };
    let text = String::from_utf8_lossy(&out).into_owned();
    match status.ok().and_then(|s| s.code()) {
        Some(0) if out.len() <= MAX_OUT => Ok(text),
        Some(0) => Err("it has too much text to add: split it".into()),
        // [`child_main`]'s refusal, as it said it.
        Some(2) => Err(text),
        // Killed (its memory or CPU limit) or failed: the file, as far as anyone can tell.
        _ => {
            Err("it couldn't be read: the file looks damaged, or expands far past its size".into())
        }
    }
}

/// The reading process ([`isolated`]): limits itself, reads the file from stdin, writes its text to stdout (exit
/// 0) or why it is refused (exit 2). Runs first in `main`, before anything else starts.
pub fn child_main(ext: &str) -> ! {
    use std::io::Write;
    limit_self();
    let mut bytes = Vec::new();
    if std::io::stdin()
        .take(64 << 20)
        .read_to_end(&mut bytes)
        .is_err()
    {
        std::process::exit(1);
    }
    let (code, out) = match text_of(ext, &bytes) {
        Ok(t) => (0, t),
        Err(why) => (2, why),
    };
    let mut o = std::io::stdout().lock();
    let _ = o.write_all(out.as_bytes());
    let _ = o.flush();
    std::process::exit(code)
}

/// This process's address space and CPU time, capped ([`CHILD_MEMORY`], [`CHILD_TIME`]). Best effort: where a
/// platform doesn't enforce one (macOS doesn't cap the address space), the wall-clock kill still holds.
fn limit_self() {
    let cap = |resource, n: u64| {
        let r = libc::rlimit {
            rlim_cur: n as libc::rlim_t,
            rlim_max: n as libc::rlim_t,
        };
        // SAFETY: setrlimit reads `r` and changes only this process's limits.
        unsafe { libc::setrlimit(resource, &r) };
    };
    cap(libc::RLIMIT_AS, CHILD_MEMORY);
    cap(libc::RLIMIT_DATA, CHILD_MEMORY);
    cap(libc::RLIMIT_CPU, CHILD_TIME.as_secs());
}

/// The document types read here, by what a file's name ends in.
pub const TYPES: [&str; 2] = ["pdf", "docx"];

/// The text of a document of type `ext` (one of [`TYPES`]), its blank runs folded. Err is why it isn't read, as a
/// person adding it is told.
pub fn text_of(ext: &str, bytes: &[u8]) -> Result<String, String> {
    let read = std::panic::catch_unwind(|| match ext {
        "pdf" => pdf(bytes),
        "docx" => docx(bytes),
        other => Err(format!(".{other} isn't a document read here")),
    })
    .unwrap_or_else(|_| Err("it couldn't be read: the file looks damaged".into()))?;
    let text = tidy(&read);
    if text.trim().is_empty() {
        return Err(match ext {
            "pdf" => "it has no text to read (a scan, or pages that are images): \
                      save it with its text, or paste the text instead"
                .into(),
            _ => "it has no text".into(),
        });
    }
    Ok(text)
}

fn pdf(bytes: &[u8]) -> Result<String, String> {
    pdf_extract::extract_text_from_mem(bytes).map_err(|e| {
        let e = e.to_string();
        if e.to_ascii_lowercase().contains("encrypt") || e.contains("password") {
            "it is password-protected: save a copy without the password".into()
        } else {
            "it couldn't be read as a PDF".to_string()
        }
    })
}

fn docx(bytes: &[u8]) -> Result<String, String> {
    use quick_xml::events::Event;
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes))
        .map_err(|_| "it couldn't be read as a Word file".to_string())?;
    let mut xml = String::new();
    zip.by_name("word/document.xml")
        .map_err(|_| "it couldn't be read as a Word file (no document in it)".to_string())?
        // A document part is text; a bomb that inflates past this isn't read at all.
        .take(64 << 20)
        .read_to_string(&mut xml)
        .map_err(|_| "it couldn't be read as a Word file".to_string())?;
    let mut reader = quick_xml::Reader::from_str(&xml);
    let mut out = String::new();
    // The paragraph being read: its text, and how it starts (a heading's `#`s, a list item's `- `).
    let (mut para, mut lead) = (String::new(), String::new());
    let mut in_text = false;
    let mut cells: Vec<String> = vec![];
    let mut depth = 0usize; // Tables inside tables: only the outermost row is joined.
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => match e.local_name().as_ref() {
                "t" => in_text = true,
                "tbl" => depth += 1,
                "tc" if depth == 1 => cells.push(String::new()),
                // A numbered or bulleted paragraph (its level and list inside), unless a heading.
                "numPr" if lead.is_empty() => lead = "- ".into(),
                _ => {}
            },
            Ok(Event::Empty(e)) => match e.local_name().as_ref() {
                "tab" => para.push('\t'),
                "br" | "cr" => para.push('\n'),
                "pStyle" => {
                    if let Some(v) = attr(&e, "val") {
                        lead = heading(&v)
                            .map(|n| format!("{} ", "#".repeat(n)))
                            .unwrap_or_default();
                    }
                }
                "numPr" if lead.is_empty() => lead = "- ".into(),
                _ => {}
            },
            Ok(Event::Text(t)) if in_text => {
                para.push_str(t.as_ref());
            }
            Ok(Event::GeneralRef(r)) if in_text => {
                // An entity in a run's text (`&amp;`): its character.
                if let Ok(Some(c)) = r.resolve_char_ref() {
                    para.push(c);
                } else {
                    para.push_str(match r.as_ref() {
                        "amp" => "&",
                        "lt" => "<",
                        "gt" => ">",
                        "quot" => "\"",
                        "apos" => "'",
                        _ => "",
                    });
                }
            }
            Ok(Event::End(e)) => match e.local_name().as_ref() {
                "t" => in_text = false,
                "p" => {
                    let line = para.trim();
                    let line = if line.is_empty() {
                        String::new()
                    } else {
                        format!("{lead}{line}")
                    };
                    if depth > 0 {
                        // A cell's paragraphs, one cell.
                        if let Some(c) = cells.last_mut().filter(|_| !line.is_empty()) {
                            if !c.is_empty() {
                                c.push(' ');
                            }
                            c.push_str(&line);
                        }
                    } else {
                        // A heading starts a block of its own.
                        if lead.starts_with('#') && !out.is_empty() && !out.ends_with("\n\n") {
                            out.push('\n');
                        }
                        out.push_str(&line);
                        out.push_str(if lead.starts_with('#') || line.is_empty() {
                            "\n\n"
                        } else {
                            "\n"
                        });
                    }
                    para.clear();
                    lead.clear();
                }
                "tr" if depth == 1 => {
                    if cells.iter().any(|c| !c.is_empty()) {
                        out.push_str(&cells.join(" | "));
                        out.push('\n');
                    }
                    cells.clear();
                }
                "tbl" => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        out.push('\n');
                    }
                }
                _ => {}
            },
            Ok(Event::Eof) => break,
            Err(_) => return Err("it couldn't be read as a Word file".into()),
            _ => {}
        }
    }
    Ok(out)
}

/// A web page's title (its `<title>`, else its first `<h1>`), and its text as Markdown, blank runs folded.
pub fn html(page: &str) -> (Option<String>, String) {
    let conv = htmd::HtmlToMarkdown::builder()
        .skip_tags(vec![
            "head", "script", "style", "noscript", "template", "nav", "header", "footer", "aside",
            "form", "button", "select", "svg", "canvas", "iframe", "img", "picture", "video",
            "audio",
        ])
        .build();
    // The page's own content where it marks it (`<main>`, else `<article>`): the menus and sidebars around it
    // aren't what the page says.
    let body = ["main", "article"]
        .iter()
        .find_map(|t| element(page, t))
        .unwrap_or(page);
    let text = conv.convert(body).map(|t| tidy(&t)).unwrap_or_default();
    let title = tag_text(page, "title")
        .or_else(|| tag_text(page, "h1"))
        .filter(|t| !t.is_empty());
    (title, text)
}

/// The first `<tag ...>` to the last `</tag>` in `page`, where it holds a good part of the page's text (a `<main>`
/// that wraps only a banner isn't the content).
fn element<'a>(page: &'a str, tag: &str) -> Option<&'a str> {
    let lower = page.to_ascii_lowercase();
    let start = lower.find(&format!("<{tag}"))?;
    // `<mainframe>` or `<articles>` isn't one.
    if !lower[start + 1 + tag.len()..].starts_with(['>', ' ', '\n', '\t', '\r']) {
        return None;
    }
    let end = lower.rfind(&format!("</{tag}>"))? + tag.len() + 3;
    (end > start && (end - start) * 5 >= page.len()).then(|| &page[start..end])
}

/// The text of the first `<tag ...>...</tag>` in `page`, entities decoded and whitespace folded, at most 120 chars.
fn tag_text(page: &str, tag: &str) -> Option<String> {
    let lower = page.to_ascii_lowercase();
    let open = lower.find(&format!("<{tag}"))?;
    let start = open + lower[open..].find('>')? + 1;
    let end = start + lower[start..].find(&format!("</{tag}"))?;
    let inner = htmd::convert(&page[start..end]).unwrap_or_default();
    let folded: String = inner
        .replace(['#', '*', '_', '`'], "")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    Some(folded.chars().take(120).collect())
}

/// An attribute's value by its local name (`w:val` is `val`).
fn attr(e: &quick_xml::events::BytesStart, name: &str) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| a.key.local_name().as_ref() == name)
        .map(|a| a.value.into_owned())
}

/// A paragraph style's heading level: `Title` 1, `Heading1`..`Heading6` (and `heading 2`, as some writers name
/// them) theirs; else none.
fn heading(style: &str) -> Option<usize> {
    let s = style.to_ascii_lowercase().replace(' ', "");
    if s == "title" {
        return Some(1);
    }
    let n: usize = s.strip_prefix("heading")?.parse().ok()?;
    (1..=6).contains(&n).then_some(n)
}

/// Trailing spaces off each line, runs of blank lines folded to one, form feeds (a PDF's page breaks) as blank lines.
fn tidy(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blank = 0;
    for line in text.replace('\u{c}', "\n\n").lines() {
        let line = line.trim_end();
        if line.trim().is_empty() {
            blank += 1;
            continue;
        }
        if !out.is_empty() {
            out.push_str(if blank > 0 { "\n\n" } else { "\n" });
        }
        blank = 0;
        out.push_str(line);
    }
    out.push('\n');
    out
}

/// `text` in pieces of at most `max` bytes, cut at a paragraph break where one is near the end, else at a line
/// break, else at a space, else anywhere on a character boundary. One piece where it fits.
pub fn parts(text: &str, max: usize) -> Vec<String> {
    let mut out = vec![];
    let mut rest = text.trim();
    while rest.len() > max {
        let mut end = max;
        while !rest.is_char_boundary(end) {
            end -= 1;
        }
        let head = &rest[..end];
        // A break in the last half of the piece, so no piece is tiny.
        let cut = ["\n\n", "\n", " "]
            .iter()
            .find_map(|b| head.rfind(b).filter(|&i| i > max / 2))
            .unwrap_or(end);
        out.push(rest[..cut].trim_end().to_string());
        rest = rest[cut..].trim_start();
    }
    if !rest.is_empty() {
        out.push(rest.to_string());
    }
    out
}
