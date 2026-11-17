//! The Studio, the web page for looking at your memory, at `/studio/` on `serve --local` only (a hosted server's
//! web UI is its console). `studio/` is built with Node into `studio/dist`, which build.rs embeds in the binary at
//! compile time; a binary built without it answers `/studio/` with a page saying how to build it in
//! (`studio/not-built.html`). The page calls this server's own routes (`/me`, `/browse`, `/search`, `/mcp`, …) from
//! the same origin, as the one local caller.
//!
//! `/studio` redirects to `/studio/`; `/studio/` and `/studio/index.html` are the app, never cached; every other
//! path is a file of the build (the hashed ones under `assets/` cached for good) or 404. The app routes in the URL's
//! hash (`#/f/...`), so no path falls back to the app.

use http_body_util::Full;
use hyper::body::Bytes;
use hyper::{Method, Response, StatusCode};

// `FILES`: every file of studio/dist, by its path there (build.rs).
include!(concat!(env!("OUT_DIR"), "/studio.rs"));

/// What a binary built without `studio/dist` answers at `/studio/`.
const NOT_BUILT: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/studio/not-built.html"
));

/// The page loads only this server's own scripts, styles, fonts and images, calls only this server, and is never
/// framed (`frame-ancestors`).
const CSP: &str = "default-src 'self'; connect-src 'self'; img-src 'self' data:; style-src 'self'; \
                   font-src 'self'; frame-ancestors 'none'; base-uri 'none'";

/// Whether this binary carries a build of the Studio.
pub fn built() -> bool {
    !FILES.is_empty()
}

/// Whether `path` is the Studio's.
pub(super) fn is_studio(path: &str) -> bool {
    path == "/studio" || path.starts_with("/studio/")
}

/// `GET` (or `HEAD`) under `/studio`.
pub(super) fn route(method: &Method, path: &str) -> Response<Full<Bytes>> {
    if method != Method::GET && method != Method::HEAD {
        let mut r = answer(
            StatusCode::METHOD_NOT_ALLOWED,
            "text/plain; charset=utf-8",
            "no-store",
            b"GET",
        );
        r.headers_mut()
            .insert(hyper::header::ALLOW, "GET, HEAD".parse().unwrap());
        return r;
    }
    let file = match path.strip_prefix("/studio") {
        Some("") => {
            let mut r = answer(
                StatusCode::PERMANENT_REDIRECT,
                "text/plain; charset=utf-8",
                "no-store",
                b"",
            );
            r.headers_mut()
                .insert(hyper::header::LOCATION, "/studio/".parse().unwrap());
            return r;
        }
        Some("/") => "index.html",
        Some(p) => p.trim_start_matches('/'),
        None => "",
    };
    let found = FILES.iter().find(|(name, _)| *name == file);
    match (file, found) {
        ("index.html", None) => answer(
            StatusCode::OK,
            "text/html; charset=utf-8",
            "no-cache",
            NOT_BUILT,
        ),
        (_, None) => answer(
            StatusCode::NOT_FOUND,
            "text/plain; charset=utf-8",
            "no-store",
            b"not found",
        ),
        (name, Some((_, body))) => {
            // Vite names every file under assets/ by its content's hash: a new build has new names.
            let cache = if name.starts_with("assets/") {
                "public, max-age=31536000, immutable"
            } else {
                "no-cache"
            };
            answer(StatusCode::OK, content_type(name), cache, body)
        }
    }
}

fn answer(
    status: StatusCode,
    content_type: &str,
    cache: &str,
    body: &'static [u8],
) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("content-type", content_type)
        .header("cache-control", cache)
        .header("content-security-policy", CSP)
        .header("x-content-type-options", "nosniff")
        .header("referrer-policy", "no-referrer")
        .body(Full::new(Bytes::from_static(body)))
        .unwrap()
}

/// A built file's type, by its extension.
fn content_type(name: &str) -> &'static str {
    match name.rsplit_once('.').map_or("", |(_, ext)| ext) {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json",
        "webmanifest" => "application/manifest+json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "wasm" => "application/wasm",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}
