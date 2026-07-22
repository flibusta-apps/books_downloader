# File Type Validation and URL Injection Fixes Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the SSRF/injection hole where the client-supplied `file_type` path segment is interpolated unvalidated into the converter and mirror URLs, plus three related hardening fixes from the same spec (constant-time API key check, quoted `Content-Disposition`, and a documented `/metrics` exposure note).

**Architecture:** Add a `FileType` enum (`src/file_type.rs`) with a hand-picked allowlist (`fb2`, `fb2zip`, `epub`, `mobi`, `html`) that implements `serde::Deserialize` directly off axum's path-segment deserializer — axum rejects any segment that doesn't match a variant with `400 Bad Request` before either handler body ever runs, so no unvalidated string can reach `convert_file` or the mirror `download()`. Downstream code keeps taking `&str` (via `FileType::as_str()`), so `book_download`, `get_filename_by_book`, `download_chain`, etc. are untouched. Independently, harden the mirror URL construction with `reqwest::Url::join` instead of `format!`, make the API key comparison constant-time with the `subtle` crate, quote/escape the `Content-Disposition` filename and strip control characters at the source (`get_filename_by_book`), and add an operational note about the unauthenticated `/metrics` endpoint.

**Tech Stack:** Rust, axum 0.8, serde, reqwest, tokio. One new dependency: `subtle` (already resolved transitively at 2.6.1 in `Cargo.lock`, so no version drift).

## Global Constraints

- Only fix what's enumerated in `docs/specs/02-file-type-validation-and-url-injection.md` (02.1–02.5). Don't refactor unrelated code.
- Validate `file_type` at the handler boundary only (an enum implementing `Deserialize`, per the spec's own recommendation) — don't thread a new type through `book_download`/`download_chain`/`get_filename_by_book`; they keep taking `&str`.
- No new dependencies except `subtle` (for constant-time comparison, explicitly suggested by the spec). Everything else (enum validation, URL joining, header escaping) uses crates already in `Cargo.toml`.
- `cargo test` and `cargo build --release` must pass after every task.
- Every acceptance criterion in the spec must be covered by an automated test, except 02.5 (`/metrics` auth) which the spec itself only requires to be documented, not code-changed.

---

### Task 1: `FileType` allowlist enum

**Files:**
- Create: `src/file_type.rs`
- Modify: `src/main.rs:1-3` (register the module)

**Interfaces:**
- Produces: `pub enum FileType { Fb2, Fb2Zip, Epub, Mobi, Html }` implementing `Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize`, plus `impl FileType { pub fn as_str(self) -> &'static str }`. Used by Task 2 (wiring into `views.rs`) and Task 3 (no dependency, just documented here for completeness).

- [ ] **Step 1: Write the failing tests**

Create `src/file_type.rs`:

```rust
use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileType {
    Fb2,
    Fb2Zip,
    Epub,
    Mobi,
    Html,
}

impl FileType {
    pub fn as_str(self) -> &'static str {
        match self {
            FileType::Fb2 => "fb2",
            FileType::Fb2Zip => "fb2zip",
            FileType::Epub => "epub",
            FileType::Mobi => "mobi",
            FileType::Html => "html",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(raw: &str) -> Result<FileType, serde_json::Error> {
        let json = serde_json::to_string(raw).unwrap();
        serde_json::from_str(&json)
    }

    #[test]
    fn accepts_all_allowlisted_values() {
        assert_eq!(parse("fb2").unwrap(), FileType::Fb2);
        assert_eq!(parse("fb2zip").unwrap(), FileType::Fb2Zip);
        assert_eq!(parse("epub").unwrap(), FileType::Epub);
        assert_eq!(parse("mobi").unwrap(), FileType::Mobi);
        assert_eq!(parse("html").unwrap(), FileType::Html);
    }

    #[test]
    fn rejects_path_traversal_payload() {
        // What "%2F" decodes to before it reaches our validation.
        assert!(parse("../admin").is_err());
    }

    #[test]
    fn rejects_query_injection_payload() {
        // What "%3F" decodes to before it reaches our validation.
        assert!(parse("epub?x=y").is_err());
    }

    #[test]
    fn rejects_case_variants() {
        assert!(parse("EPUB").is_err());
        assert!(parse("Fb2").is_err());
    }

    #[test]
    fn rejects_unknown_extension() {
        assert!(parse("pdf").is_err());
    }

    #[test]
    fn rejects_empty_string() {
        assert!(parse("").is_err());
    }

    #[test]
    fn as_str_round_trips_through_deserialize() {
        for ft in [
            FileType::Fb2,
            FileType::Fb2Zip,
            FileType::Epub,
            FileType::Mobi,
            FileType::Html,
        ] {
            assert_eq!(parse(ft.as_str()).unwrap(), ft);
        }
    }
}
```

- [ ] **Step 2: Register the module**

In `src/main.rs`, change:

```rust
pub mod config;
pub mod services;
pub mod views;
```

to:

```rust
pub mod config;
pub mod file_type;
pub mod services;
pub mod views;
```

- [ ] **Step 3: Run the tests**

Run: `cargo test file_type`
Expected: `test result: ok. 7 passed`

(There is no separate "fails then passes" cycle here — the type and its tests are added together since the enum's shape *is* the allowlist; there's no intermediate broken state worth compiling.)

- [ ] **Step 4: Commit**

```bash
git add src/file_type.rs src/main.rs
git commit -m "feat: add FileType allowlist enum for file_type path segments"
```

---

### Task 2: Wire `FileType` into the HTTP boundary, prove rejection happens before any handler logic runs

**Files:**
- Modify: `src/views.rs:1-101` (the two route handlers)
- Modify: `src/file_type.rs` (add the axum-integration test module)

**Interfaces:**
- Consumes: `FileType` and `FileType::as_str` (Task 1).
- Produces: nothing new — `download` and `get_filename` keep their existing signatures as axum handlers; only their `Path<...>` extractor type changes.

- [ ] **Step 1: Write the failing tests**

Add to the bottom of `src/file_type.rs` (a second `#[cfg(test)]` block is fine — keep it separate from Task 1's pure unit tests since this one spins up real HTTP servers):

```rust
#[cfg(test)]
mod http_tests {
    use super::*;
    use axum::{extract::Path, http::StatusCode, routing::get, Router};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use tokio::net::TcpListener;

    async fn spawn_download_route(hits: Arc<AtomicUsize>) -> std::net::SocketAddr {
        let app = Router::new().route(
            "/download/{source_id}/{remote_id}/{file_type}",
            get(move |Path((_, _, _)): Path<(u32, u32, FileType)>| {
                let hits = hits.clone();
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    StatusCode::OK
                }
            }),
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        addr
    }

    async fn spawn_filename_route(hits: Arc<AtomicUsize>) -> std::net::SocketAddr {
        let app = Router::new().route(
            "/filename/{book_id}/{file_type}",
            get(move |Path((_, _)): Path<(u32, FileType)>| {
                let hits = hits.clone();
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    StatusCode::OK
                }
            }),
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        addr
    }

    #[tokio::test]
    async fn download_route_rejects_path_traversal_before_reaching_handler() {
        let hits = Arc::new(AtomicUsize::new(0));
        let addr = spawn_download_route(hits.clone()).await;

        let resp = reqwest::Client::new()
            .get(format!("http://{addr}/download/1/1/..%2Fadmin"))
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
        assert_eq!(
            hits.load(Ordering::SeqCst),
            0,
            "handler must never run for an invalid file_type"
        );
    }

    #[tokio::test]
    async fn download_route_rejects_query_injection_before_reaching_handler() {
        let hits = Arc::new(AtomicUsize::new(0));
        let addr = spawn_download_route(hits.clone()).await;

        let resp = reqwest::Client::new()
            .get(format!("http://{addr}/download/1/1/epub%3Fx=y"))
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn download_route_accepts_allowlisted_file_type() {
        let hits = Arc::new(AtomicUsize::new(0));
        let addr = spawn_download_route(hits.clone()).await;

        let resp = reqwest::Client::new()
            .get(format!("http://{addr}/download/1/1/fb2"))
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn filename_route_rejects_path_traversal_before_reaching_handler() {
        let hits = Arc::new(AtomicUsize::new(0));
        let addr = spawn_filename_route(hits.clone()).await;

        let resp = reqwest::Client::new()
            .get(format!("http://{addr}/filename/1/..%2Fadmin"))
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    }
}
```

- [ ] **Step 2: Run the tests to verify they pass already at the routing layer**

Run: `cargo test file_type::http_tests`
Expected: `test result: ok. 4 passed` — this proves axum's own `Path` deserialization rejects bad segments before any handler code runs. This works today because `FileType`'s `Deserialize` impl is what does the rejecting; the production routes don't use it yet, which is what the rest of this task fixes.

- [ ] **Step 3: Wire `FileType` into the real routes in `src/views.rs`**

Change the imports at the top of `src/views.rs`:

```rust
use crate::{
    config::CONFIG,
    file_type::FileType,
    services::{
        book_library::get_book, downloader::book_download, filename_getter::get_filename_by_book,
    },
};
```

Change the `download` handler's signature and its use of `file_type`:

```rust
pub async fn download(
    Path((source_id, remote_id, file_type)): Path<(u32, u32, FileType)>,
    Query(params): Query<FilenameParams>,
) -> impl IntoResponse {
    let normalized = params.normalized.unwrap_or(true);

    let download_result =
        match book_download(source_id, remote_id, file_type.as_str(), normalized).await {
            Ok(v) => v,
            Err(_) => return Err((StatusCode::NO_CONTENT, "Can't download!".to_string())),
        };
```

Change the `get_filename` handler's signature and its use of `file_type`:

```rust
pub async fn get_filename(
    Path((book_id, file_type)): Path<(u32, FileType)>,
    Query(params): Query<FilenameParams>,
) -> impl IntoResponse {
    let normalized = params.normalized.unwrap_or(true);

    let (filename, filename_ascii) = match get_book(book_id).await {
        Ok(book) => (
            get_filename_by_book(&book, file_type.as_str(), false, false, normalized),
            get_filename_by_book(&book, file_type.as_str(), false, true, normalized),
        ),
        Err(_) => return (StatusCode::BAD_REQUEST, "Book not found!".to_string()),
    };
```

The rest of `views.rs` (router setup, `auth`, `health`) is unchanged in this task.

- [ ] **Step 4: Verify the whole crate still builds and tests still pass**

Run: `cargo test`
Expected: `test result: ok.` for every module, including the new `file_type::tests` and `file_type::http_tests`.

- [ ] **Step 5: Manual smoke test against the real router (optional but recommended)**

```bash
API_KEY=test FL_SOURCES='[{"url":"http://localhost:1"}]' BOOK_LIBRARY_API_KEY=test BOOK_LIBRARY_URL=http://localhost:1 CONVERTER_URL=http://localhost:1 CONVERTER_API_KEY=test cargo run &
sleep 1
curl -s -o /dev/null -w "%{http_code}\n" -H "Authorization: test" "http://localhost:8080/download/1/1/..%2Fadmin"
curl -s -o /dev/null -w "%{http_code}\n" -H "Authorization: test" "http://localhost:8080/filename/1/..%2Fadmin"
kill %1
```

Expected: both `curl` calls print `400`.

- [ ] **Step 6: Commit**

```bash
git add src/views.rs src/file_type.rs
git commit -m "fix: reject unknown file_type path segments with 400 before touching converter/mirror URLs"
```

---

### Task 3: Build mirror URLs with `Url::join` instead of `format!`

**Files:**
- Modify: `src/services/downloader/mod.rs:1-124` (`download()`)

**Interfaces:**
- Consumes: nothing new — `book_file_type: &str` is already validated by Task 2 whenever it originates from the client-supplied `file_type`; when it's `book.file_type` (from `book_library`, out of this spec's scope) it's unchanged.
- Produces: `download()`'s signature and return type (`Option<(Response, bool)>`) are unchanged; only its URL-building internals change.

- [ ] **Step 1: Write the failing test**

Add to the `mod tests` block at the bottom of `src/services/downloader/mod.rs`:

```rust
    #[tokio::test]
    async fn mirror_request_path_matches_expected_url() {
        let received: std::sync::Arc<std::sync::Mutex<String>> =
            std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let received_clone = received.clone();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                if let Ok(n) = socket.read(&mut buf).await {
                    *received_clone.lock().unwrap() =
                        String::from_utf8_lossy(&buf[..n]).to_string();
                }
                let _ = socket
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                    .await;
                let _ = socket.shutdown().await;
            }
        });

        let source_config = make_source_config(format!("http://{addr}"));
        let _ = download(&42, "fb2", &source_config).await;

        let request = received.lock().unwrap().clone();
        assert!(
            request.starts_with("GET /b/42/fb2 "),
            "expected request path /b/42/fb2, got: {request:?}"
        );
    }

    #[tokio::test]
    async fn mirror_url_rejects_invalid_base_url_instead_of_panicking() {
        let source_config = make_source_config("not a valid url".to_string());

        let result = download(&42, "fb2", &source_config).await;

        assert!(result.is_none());
    }
```

- [ ] **Step 2: Run the tests to confirm today's baseline behavior**

Run: `cargo test downloader::tests::mirror_request_path_matches_expected_url downloader::tests::mirror_url_rejects_invalid_base_url_instead_of_panicking`
Expected: both pass today, but for the wrong reason on the second one — `format!("{basic_url}/b/{book_id}/{book_file_type}")` with a non-URL `basic_url` produces a garbage string; `reqwest`'s own lazy URL parsing (inside `RequestBuilder::send`) fails and the existing `request_error` branch catches it, returning `None`. After Step 3, the same test still passes, but now because our own eager `Url::parse` check catches it first (`invalid_base_url` branch) — this step is a safety net proving the refactor doesn't change either test's outcome, not a red/green TDD cycle.

- [ ] **Step 3: Replace `format!`-based URL building with `Url::join`**

In `src/services/downloader/mod.rs`, change the import:

```rust
use reqwest::{Response, Url};
```

Replace the body of `download()` up to the `send()` call:

```rust
pub async fn download<'a>(
    book_id: &'a u32,
    book_file_type: &'a str,
    source_config: &'a config::SourceConfig,
) -> Option<(Response, bool)> {
    let basic_url = &source_config.url;

    let mut base = match Url::parse(basic_url) {
        Ok(v) => v,
        Err(err) => {
            metrics::counter!(
                "downloader_source_requests_total",
                "source" => source_config.url.clone(),
                "outcome" => "invalid_base_url"
            )
            .increment(1);
            warn!(
                source = %source_config.url,
                book_id,
                file_type = book_file_type,
                stage = "mirror_fetch",
                error = %err,
                "configured mirror base URL failed to parse"
            );
            return None;
        }
    };

    if !base.path().ends_with('/') {
        let path_with_slash = format!("{}/", base.path());
        base.set_path(&path_with_slash);
    }

    let relative = if book_file_type == "fb2" || book_file_type == "epub" || book_file_type == "mobi"
    {
        format!("b/{book_id}/{book_file_type}")
    } else {
        format!("b/{book_id}/download")
    };

    let url = match base.join(&relative) {
        Ok(v) => v,
        Err(err) => {
            metrics::counter!(
                "downloader_source_requests_total",
                "source" => source_config.url.clone(),
                "outcome" => "invalid_base_url"
            )
            .increment(1);
            warn!(
                source = %source_config.url,
                book_id,
                file_type = book_file_type,
                stage = "mirror_fetch",
                error = %err,
                "failed to join mirror URL"
            );
            return None;
        }
    };

    let fetch_start = Instant::now();
    let response = source_config.client.get(url).send().await;
```

Everything from `let response = source_config.client.get(url).send().await;` onward stays exactly as it is today (error handling, content-type checks, metrics).

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test downloader`
Expected: `test result: ok.` for all `downloader::tests::*`, including the two new tests and all pre-existing ones (they use base URLs like `http://{addr}` with no path, which `Url::join` handles identically to the old `format!`).

- [ ] **Step 5: Commit**

```bash
git add src/services/downloader/mod.rs
git commit -m "fix: build mirror URLs with Url::join instead of format! string interpolation"
```

---

### Task 4: Constant-time API key comparison

**Files:**
- Modify: `Cargo.toml` (add `subtle` dependency)
- Modify: `src/views.rs` (`auth` function)

**Interfaces:**
- Produces: `fn keys_match(provided: &str, expected: &str) -> bool` (private to `views.rs`), used by `auth`.

- [ ] **Step 1: Add the `subtle` dependency**

In `Cargo.toml`, add to `[dependencies]` (it's already resolved in `Cargo.lock` at `2.6.1` as a transitive dependency, so this pins it as direct without changing the lockfile):

```toml
subtle = "2.6"
```

- [ ] **Step 2: Write the failing tests**

Add to the bottom of `src/views.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_match_identical_strings() {
        assert!(keys_match("secret-key", "secret-key"));
    }

    #[test]
    fn keys_match_rejects_different_strings_of_equal_length() {
        assert!(!keys_match("secret-key", "other-key1"));
    }

    #[test]
    fn keys_match_rejects_different_length() {
        assert!(!keys_match("short", "much-longer-key"));
    }

    #[test]
    fn keys_match_rejects_empty_against_nonempty() {
        assert!(!keys_match("", "secret-key"));
    }

    #[test]
    fn keys_match_empty_against_empty() {
        assert!(keys_match("", ""));
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail to compile**

Run: `cargo test views::tests`
Expected: FAIL with `cannot find function 'keys_match' in this scope`.

- [ ] **Step 4: Implement `keys_match` and use it in `auth`**

In `src/views.rs`, add the import:

```rust
use subtle::ConstantTimeEq;
```

Replace the `auth` function:

```rust
fn keys_match(provided: &str, expected: &str) -> bool {
    let provided = provided.as_bytes();
    let expected = expected.as_bytes();

    if provided.len() != expected.len() {
        return false;
    }

    provided.ct_eq(expected).into()
}

async fn auth(req: Request<axum::body::Body>, next: Next) -> Result<Response, StatusCode> {
    let auth_header = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|header| header.to_str().ok());

    let auth_header = if let Some(auth_header) = auth_header {
        auth_header
    } else {
        return Err(StatusCode::UNAUTHORIZED);
    };

    if !keys_match(auth_header, &CONFIG.api_key) {
        return Err(StatusCode::UNAUTHORIZED);
    }

    Ok(next.run(req).await)
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test views::tests`
Expected: `test result: ok. 5 passed`

- [ ] **Step 6: Run the whole suite as a regression check**

Run: `cargo test`
Expected: `test result: ok.` everywhere.

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml Cargo.lock src/views.rs
git commit -m "fix: compare the API key in constant time to avoid a timing side channel"
```

---

### Task 5: Quote/escape `Content-Disposition` and strip control characters from generated filenames

**Files:**
- Modify: `src/services/filename_getter.rs` (control-character stripping)
- Modify: `src/views.rs` (header quoting)

**Interfaces:**
- Produces: `fn content_disposition_value(filename: &str) -> String` (private to `views.rs`), used by the `download` handler.
- `get_filename_by_book`'s signature is unchanged; only its internal character filter changes.

- [ ] **Step 1: Write the failing test for control-character stripping**

Add to the `mod tests` block at the bottom of `src/services/filename_getter.rs`:

```rust
    #[test]
    fn control_characters_stripped_when_normalized() {
        let book = make_book("Title\r\nwith\tcontrol\u{0007}chars", vec![]);
        let filename = get_filename_by_book(&book, "fb2", false, false, true);
        assert!(!filename.chars().any(|c| c.is_control()));
    }

    #[test]
    fn control_characters_stripped_when_not_normalized() {
        let book = make_book("Заголовок\r\nс\u{0001}контролем", vec![]);
        let filename = get_filename_by_book(&book, "fb2", false, false, false);
        assert!(!filename.chars().any(|c| c.is_control()));
    }

    #[test]
    fn control_characters_stripped_from_ascii_variant() {
        let book = make_book("Evil\"; \r\nX-Injected: yes\r\ntitle", vec![]);
        let filename = get_filename_by_book(&book, "fb2", false, true, true);
        assert!(!filename.chars().any(|c| c.is_control()));
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test filename_getter::tests::control_characters`
Expected: FAIL — `\r`, `\n`, `\t`, and the other control characters survive into the filename today.

- [ ] **Step 3: Strip control characters in the existing filter pass**

In `src/services/filename_getter.rs`, change the `stripped` filter (currently around line 74):

```rust
    let stripped: String = filename_without_type
        .chars()
        .filter(|c| {
            !c.is_control()
                && !matches!(
                    *c,
                    '(' | ')'
                | ',' | '.'
                | '\u{2026}' // …
                | '\u{2019}' // ’
                | '!'
                | '"'
                | '?'
                | '\''
                | ':'
                )
        })
        .collect();
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test filename_getter`
Expected: `test result: ok.` for every test in the module (all pre-existing tests plus the three new ones).

- [ ] **Step 5: Write the failing test for `Content-Disposition` quoting**

Task 4 already added a `#[cfg(test)] mod tests { ... }` block at the bottom of `src/views.rs`. Add these three functions *inside that existing block*, alongside `keys_match_*` (don't create a second `mod tests` — Rust will reject the duplicate module name):

```rust
    #[test]
    fn content_disposition_value_quotes_plain_filename() {
        assert_eq!(
            content_disposition_value("book.fb2"),
            "attachment; filename=\"book.fb2\""
        );
    }

    #[test]
    fn content_disposition_value_escapes_quotes_and_backslashes() {
        let value = content_disposition_value("weird\"na\\me.fb2");
        assert_eq!(value, "attachment; filename=\"weird\\\"na\\\\me.fb2\"");
    }

    #[test]
    fn content_disposition_survives_malicious_title_end_to_end() {
        use crate::services::book_library::types::BookWithRemote;
        use crate::services::filename_getter::get_filename_by_book;

        let book = BookWithRemote {
            id: 1,
            remote_id: 42,
            title: "Evil\"; \r\nX-Injected: yes\r\ntitle".to_string(),
            lang: "en".to_string(),
            file_type: "fb2".to_string(),
            uploaded: "2024-01-01".to_string(),
            authors: vec![],
        };

        let filename_ascii = get_filename_by_book(&book, "fb2", false, true, true);
        let value = content_disposition_value(&filename_ascii);

        assert!(
            header::HeaderValue::from_str(&value).is_ok(),
            "expected a valid header value, got {value:?}"
        );
        assert!(!value.chars().any(|c| c.is_control()));
        assert!(value.starts_with("attachment; filename=\""));
        assert!(value.ends_with('"'));
    }
```

- [ ] **Step 6: Run the tests to verify they fail to compile**

Run: `cargo test views::tests::content_disposition`
Expected: FAIL with `cannot find function 'content_disposition_value' in this scope`.

- [ ] **Step 7: Implement `content_disposition_value` and use it in the `download` handler**

In `src/views.rs`, add the helper function above `pub async fn download`:

```rust
fn content_disposition_value(filename: &str) -> String {
    let escaped = filename.replace('\\', "\\\\").replace('"', "\\\"");
    format!("attachment; filename=\"{escaped}\"")
}
```

Change the header construction inside `download`:

```rust
    let headers = AppendHeaders([
        (
            header::CONTENT_DISPOSITION,
            content_disposition_value(&filename_ascii),
        ),
        (header::CONTENT_LENGTH, format!("{file_size}")),
        (
            header::HeaderName::from_static("x-filename-b64-ascii"),
            encoder.encode(filename_ascii),
        ),
        (
            header::HeaderName::from_static("x-filename-b64"),
            encoder.encode(filename),
        ),
    ]);
```

- [ ] **Step 8: Run the tests to verify they pass**

Run: `cargo test views`
Expected: `test result: ok.` for all `views::tests::*`.

- [ ] **Step 9: Run the whole suite as a regression check**

Run: `cargo test`
Expected: `test result: ok.` everywhere — `config`, `downloader`, `downloader::utils`, `downloader::zip`, `filename_getter`, `file_type`, `file_type::http_tests`, `views`.

- [ ] **Step 10: Commit**

```bash
git add src/services/filename_getter.rs src/views.rs
git commit -m "fix: quote/escape Content-Disposition filename and strip control characters at the source"
```

---

### Task 6: Document the unauthenticated `/metrics` endpoint (02.5)

**Files:**
- Modify: `src/views.rs:get_router` (doc comment only)

**Interfaces:**
- None — comment-only change, no behavior or test impact. The spec's acceptance criteria list doesn't require a code change here (it only says "if exposed, protect it; otherwise document"), and this repo has no deployment manifest to add network-exposure controls to.

- [ ] **Step 1: Add the operational note**

In `src/views.rs`, in `get_router`, add a doc comment directly above the `metric_router` definition:

```rust
    // `/metrics` is intentionally unauthenticated (Prometheus scrapers don't send
    // the API key). It must only be reachable from the internal network/scrape
    // target — do not expose this port publicly. See docs/specs/02-file-type-validation-and-url-injection.md (02.5).
    let metric_router =
        Router::new().route("/metrics", get(|| async move { metric_handle.render() }));
```

- [ ] **Step 2: Verify the crate still builds**

Run: `cargo build`
Expected: build succeeds with no warnings introduced.

- [ ] **Step 3: Commit**

```bash
git add src/views.rs
git commit -m "docs: note that /metrics must stay off the public network"
```

---

## Final acceptance-criteria checklist

- [ ] `GET /download/1/1/..%2Fx` and `GET /filename/1/..%2Fx` return `400` — covered by `file_type::http_tests` (Task 2) and the manual smoke test (Task 2, Step 5).
- [ ] A unit test asserts an unknown `file_type` never produces an outbound HTTP request — covered by the `hits` counter assertions in `file_type::http_tests` (Task 2): the counting handler (which stands in for the code path that would make an HTTP call) never increments for rejected input.
- [ ] API key comparison is constant-time — `keys_match` in `views.rs` (Task 4), using `subtle::ConstantTimeEq`.
- [ ] `Content-Disposition` value is quoted and contains no control characters for any title input (`;`, `"`, `\r\n`) — covered by `control_characters_stripped_*` (Task 5, `filename_getter.rs`) and `content_disposition_survives_malicious_title_end_to_end` (Task 5, `views.rs`).
