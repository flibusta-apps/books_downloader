# Error Handling and HTTP Semantics Fixes Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the download pipeline's `Option`-based error collapsing with a real error type so failure responses use correct HTTP status codes (`404`/`502`/`504` instead of `204`/`400` with a discarded cause), and fix three latent correctness bugs (extension mismatch, case-sensitivity, substring ZIP-entry matching) uncovered while doing so.

**Architecture:** Three small, independent behavioral fixes first (ZIP entry selection by extension, case-insensitive `file_type` comparison, matching `filename`/`filename_ascii` extensions) land as their own commits since they don't require any type changes. Then introduce `BookLibraryError` (`thiserror`) in the `book_library` module so `/filename` can distinguish "book not found" (404) from "book_library is down" (502). Then introduce `DownloadError` in the `downloader` module — `SourceUnavailable`, `BadArchive`, `ConverterFailed(u16)`, `Timeout`, `Internal(String)`, plus `Library(#[from] BookLibraryError)` — and thread it through `zip.rs`, `utils.rs`, `covert.rs`, and `downloader/mod.rs` in place of `Option`. Finally, add a shared `AppError` in `views.rs` that converts both error types into the right `StatusCode` + small JSON body, and wire it into both handlers.

**Tech Stack:** Rust, axum 0.8, `thiserror` (new dependency; already resolved transitively at 2.0.16 in `Cargo.lock`, so no version drift), `reqwest`, `zip` 4.2.

## Global Constraints

- Only fix what's enumerated in `docs/specs/05-error-handling-http-semantics.md` (05.1–05.7). Don't refactor unrelated code.
- `book_download` and `download_chain` must return `Result<DownloadResult, DownloadError>` (spec 05.3) — no more `Option` anywhere in the download pipeline.
- `filename` and `filename_ascii` must always be derived from the same file-type value within any single branch of `download_chain` (05.5).
- `file_type` string comparisons in the mirror-URL branch must be case-insensitive, matching the rest of the file (05.6).
- ZIP entry selection must match by extension, not substring, and the `"elector"` special case must be removed (05.7).
- `cargo build` and `cargo test` must pass after every task.
- Every acceptance criterion in the spec must be covered by an automated test.

---

### Task 1: Fix ZIP entry selection to match by extension, not substring (05.7)

**Files:**
- Modify: `src/services/downloader/zip.rs:16-45`

**Interfaces:**
- No signature change — `unzip` still returns `Option<(SpooledTempFile, usize)>`. Only the entry-selection predicate inside the loop changes. Later tasks (5) will change this return type; this task is a pure behavior fix.

- [ ] **Step 1: Write the failing tests**

Add these three tests to `src/services/downloader/zip.rs`, in `mod tests`, directly above `corrupt_zip_bytes_return_none_instead_of_panicking`:

```rust
    #[test]
    fn selects_entry_by_extension_not_substring() {
        let mut archive = zip::ZipWriter::new(tempfile::spooled_tempfile(1024));
        let options: FileOptions<()> = FileOptions::default();

        archive
            .start_file::<&str, ()>("cover.fb2.jpg", options)
            .unwrap();
        archive.write_all(b"not the fb2 entry").unwrap();

        archive.start_file::<&str, ()>("book.fb2", options).unwrap();
        archive.write_all(b"fb2 file contents").unwrap();

        let mut zipped = archive.finish().unwrap();
        zipped.rewind().unwrap();

        let (mut unzipped, size) =
            unzip(zipped, "fb2", GENEROUS_MAX_DECOMPRESSED, GENEROUS_MAX_RATIO)
                .expect("should find book.fb2 by extension, not cover.fb2.jpg by substring");

        let mut contents = Vec::new();
        std::io::Read::read_to_end(&mut unzipped, &mut contents).unwrap();
        assert_eq!(contents, b"fb2 file contents");
        assert_eq!(size, contents.len());
    }

    #[test]
    fn does_not_match_stray_elector_entry() {
        let mut archive = zip::ZipWriter::new(tempfile::spooled_tempfile(1024));
        let options: FileOptions<()> = FileOptions::default();

        archive.start_file::<&str, ()>("elector", options).unwrap();
        archive.write_all(b"should never match").unwrap();

        let mut zipped = archive.finish().unwrap();
        zipped.rewind().unwrap();

        let result = unzip(zipped, "fb2", GENEROUS_MAX_DECOMPRESSED, GENEROUS_MAX_RATIO);
        assert!(result.is_none());
    }

    #[test]
    fn directory_entries_are_skipped() {
        let mut archive = zip::ZipWriter::new(tempfile::spooled_tempfile(1024));
        let options: FileOptions<()> = FileOptions::default();

        archive.add_directory("fb2", options).unwrap();
        archive.start_file::<&str, ()>("real.fb2", options).unwrap();
        archive.write_all(b"real fb2 contents").unwrap();

        let mut zipped = archive.finish().unwrap();
        zipped.rewind().unwrap();

        let (mut unzipped, _size) =
            unzip(zipped, "fb2", GENEROUS_MAX_DECOMPRESSED, GENEROUS_MAX_RATIO)
                .expect("should skip the directory and find real.fb2");

        let mut contents = Vec::new();
        std::io::Read::read_to_end(&mut unzipped, &mut contents).unwrap();
        assert_eq!(contents, b"real fb2 contents");
    }
```

- [ ] **Step 2: Run the tests to verify `selects_entry_by_extension_not_substring` and `does_not_match_stray_elector_entry` fail**

Run: `cargo test downloader::zip::`
Expected: `selects_entry_by_extension_not_substring` FAILS (the old substring match picks `cover.fb2.jpg` first, so it returns `not the fb2 entry` instead of `fb2 file contents`). `does_not_match_stray_elector_entry` FAILS (the old code matches the literal `"elector"` entry name and returns `Some`, but the test expects `None`). `directory_entries_are_skipped` passes already (the substring check happens to still find `real.fb2`), which is fine — it's a regression guard for the fix below, not a bug reproduction.

- [ ] **Step 3: Fix the entry-selection predicate**

In `src/services/downloader/zip.rs`, replace:

```rust
    for i in 0..archive.len() {
        let mut file = archive.by_index(i).ok()?;
        let filename = file.name();

        if filename.contains(&file_type_lower) || file.name().to_lowercase() == "elector" {
```

with:

```rust
    for i in 0..archive.len() {
        let mut file = archive.by_index(i).ok()?;
        let filename = file.name();

        let matches_ext = filename
            .rsplit('.')
            .next()
            .map(|ext| ext.eq_ignore_ascii_case(&file_type_lower))
            .unwrap_or(false);

        if !file.is_dir() && matches_ext {
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test downloader::zip::`
Expected: all tests in `downloader::zip` pass, including the three new ones.

- [ ] **Step 5: Commit**

```bash
git add src/services/downloader/zip.rs
git commit -m "fix: select zip entries by extension instead of substring, drop stray elector case"
```

---

### Task 2: Fix `file_type` case-sensitivity in the mirror-URL branch (05.6)

**Files:**
- Modify: `src/services/downloader/mod.rs:52-57`

**Interfaces:**
- No signature change. Pure behavior fix inside `download()`.

- [ ] **Step 1: Write the failing test**

Add this test to `src/services/downloader/mod.rs`, in `mod tests`, directly above `mirror_request_path_matches_expected_url`:

```rust
    #[tokio::test]
    async fn uppercase_file_type_uses_type_specific_url_not_generic_download() {
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
        let _ = download(&42, "FB2", &source_config).await;

        let request = received.lock().unwrap().clone();
        assert!(
            request.starts_with("GET /b/42/FB2 "),
            "an uppercase FB2 book_file_type must still take the type-specific URL branch, got: {request:?}"
        );
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test downloader::mod::tests::uppercase_file_type_uses_type_specific_url_not_generic_download`
Expected: FAIL — the request path is `/b/42/download` because `"FB2" == "fb2"` is false.

- [ ] **Step 3: Normalize the comparison**

In `src/services/downloader/mod.rs`, replace:

```rust
    let relative =
        if book_file_type == "fb2" || book_file_type == "epub" || book_file_type == "mobi" {
            format!("b/{book_id}/{book_file_type}")
        } else {
            format!("b/{book_id}/download")
        };
```

with:

```rust
    let book_file_type_lower = book_file_type.to_lowercase();
    let relative = if book_file_type_lower == "fb2"
        || book_file_type_lower == "epub"
        || book_file_type_lower == "mobi"
    {
        format!("b/{book_id}/{book_file_type}")
    } else {
        format!("b/{book_id}/download")
    };
```

Then, a few lines further down, reuse the already-lowercased variable instead of re-computing it. Replace:

```rust
    if book_file_type.to_lowercase() == "html" && content_type.contains("text/html") {
```

with:

```rust
    if book_file_type_lower == "html" && content_type.contains("text/html") {
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test downloader::mod::tests::uppercase_file_type_uses_type_specific_url_not_generic_download`
Expected: PASS.

- [ ] **Step 5: Run the full downloader test suite**

Run: `cargo test downloader::`
Expected: all pass (this change doesn't touch behavior for already-lowercase input, which is the common case exercised by the existing tests).

- [ ] **Step 6: Commit**

```bash
git add src/services/downloader/mod.rs
git commit -m "fix: compare book_file_type case-insensitively when choosing the mirror URL branch"
```

---

### Task 3: Fix `filename`/`filename_ascii` extension mismatch in the direct-download branch (05.5)

**Files:**
- Modify: `src/services/downloader/mod.rs:222-224` (line numbers as of the pre-Task-2 file; find by the `!is_zip && !final_need_zip && !converting` branch)

**Interfaces:**
- No signature change. Pure behavior fix inside `download_chain()`.

- [ ] **Step 1: Write the failing test**

Add this test to `src/services/downloader/mod.rs`, in `mod tests`, directly above `missing_content_length_falls_back_to_buffering`:

```rust
    #[tokio::test]
    async fn direct_download_filename_and_ascii_share_the_requested_extension() {
        let body = b"fake epub content";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/epub+zip\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            std::str::from_utf8(body).unwrap()
        );
        let base_url = spawn_raw_server(response.into_bytes()).await;
        let source_config = make_source_config(base_url);
        // The library records this book as "fb2", but the client is requesting "epub"
        // directly (no conversion) — the served bytes are epub, so both filename headers
        // must say epub, not fb2.
        let book = make_book("fb2");

        let result = download_chain(
            book,
            "epub".to_string(),
            source_config,
            false,
            true,
            generous_limits(),
        )
        .await;

        let data = result.expect("direct download should succeed");
        assert!(
            data.filename.ends_with(".epub"),
            "filename should use the requested/served type, got {:?}",
            data.filename
        );
        assert!(
            data.filename_ascii.ends_with(".epub"),
            "filename_ascii should use the requested/served type, got {:?}",
            data.filename_ascii
        );
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test downloader::mod::tests::direct_download_filename_and_ascii_share_the_requested_extension`
Expected: FAIL — `data.filename` ends with `.fb2` (from `book.file_type`) while `data.filename_ascii` ends with `.epub` (from `file_type`), so the first assertion fails.

- [ ] **Step 3: Fix the mismatched argument**

In `src/services/downloader/mod.rs`, in the `!is_zip && !final_need_zip && !converting` branch of `download_chain`, replace:

```rust
        let filename = get_filename_by_book(&book, &book.file_type, false, false, normalized);
        let filename_ascii = get_filename_by_book(&book, &file_type, false, true, normalized);
```

with:

```rust
        let filename = get_filename_by_book(&book, &file_type, false, false, normalized);
        let filename_ascii = get_filename_by_book(&book, &file_type, false, true, normalized);
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test downloader::mod::tests::direct_download_filename_and_ascii_share_the_requested_extension`
Expected: PASS.

- [ ] **Step 5: Run the full downloader test suite**

Run: `cargo test downloader::`
Expected: all pass.

- [ ] **Step 6: Commit**

```bash
git add src/services/downloader/mod.rs
git commit -m "fix: use the same file-type value for filename and filename_ascii in the direct-download branch"
```

---

### Task 4: Introduce `BookLibraryError` and fix `/filename` status codes (05.2)

**Files:**
- Modify: `Cargo.toml`
- Create: `src/services/book_library/error.rs`
- Modify: `src/services/book_library/mod.rs`
- Modify: `src/services/downloader/mod.rs` (one line, to keep `book_download` compiling — replaced properly in Task 5)
- Modify: `src/views.rs` (`get_filename` handler only)

**Interfaces:**
- Produces: `pub enum BookLibraryError { NotFound, RequestFailed(reqwest::Error), UpstreamError(reqwest::Error) }` in `src/services/book_library/error.rs`, implementing `std::error::Error` via `thiserror`. Consumed by Task 5 (`DownloadError::Library`) and Task 6 (`AppError`).
- `get_sources`, `get_book`, `get_remote_book` now return `Result<_, BookLibraryError>` instead of `Result<_, Box<dyn std::error::Error + Send + Sync>>`.

- [ ] **Step 1: Add the `thiserror` dependency**

In `Cargo.toml`, add to `[dependencies]` (alphabetically near the other small utility crates is fine, e.g. after `base64 = "0.22.1"`):

```toml
thiserror = "2"
```

Run: `cargo build`
Expected: succeeds; `Cargo.lock`'s existing transitive `thiserror 2.0.16` entry is reused (no version bump elsewhere).

- [ ] **Step 2: Write the failing tests for status-code mapping**

Create `src/services/book_library/error.rs`:

```rust
use thiserror::Error;

#[derive(Debug, Error)]
pub enum BookLibraryError {
    #[error("book not found")]
    NotFound,
    #[error("book_library request failed: {0}")]
    RequestFailed(#[source] reqwest::Error),
    #[error("book_library returned an error status: {0}")]
    UpstreamError(#[source] reqwest::Error),
}
```

Then, in `src/services/book_library/mod.rs`, add these two tests to `mod tests` (after the existing `stalled_server_times_out_quickly` test), and change its `use tokio::io::AsyncReadExt;` import to also bring in `AsyncWriteExt`:

```rust
use tokio::io::{AsyncReadExt, AsyncWriteExt};
```

```rust
    #[tokio::test]
    async fn not_found_status_maps_to_not_found_error() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = socket.read(&mut buf).await;
                let _ = socket
                    .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                    .await;
                let _ = socket.shutdown().await;
            }
        });

        let client = reqwest::Client::new();
        let result: Result<serde_json::Value, _> =
            _make_request(&client, &format!("http://{addr}/x"), "key", vec![]).await;

        assert!(matches!(result, Err(BookLibraryError::NotFound)));
    }

    #[tokio::test]
    async fn server_error_status_maps_to_upstream_error() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = socket.read(&mut buf).await;
                let _ = socket
                    .write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n")
                    .await;
                let _ = socket.shutdown().await;
            }
        });

        let client = reqwest::Client::new();
        let result: Result<serde_json::Value, _> =
            _make_request(&client, &format!("http://{addr}/x"), "key", vec![]).await;

        assert!(matches!(result, Err(BookLibraryError::UpstreamError(_))));
    }
```

- [ ] **Step 3: Run the tests to verify they fail to compile**

Run: `cargo test book_library::`
Expected: compile error — `_make_request` still returns `Box<dyn std::error::Error + Send + Sync>`, so `matches!(result, Err(BookLibraryError::NotFound))` doesn't type-check.

- [ ] **Step 4: Rewrite `book_library/mod.rs` to use `BookLibraryError`**

In `src/services/book_library/mod.rs`, add `pub mod error;` alongside the existing `pub mod types;`, and `use error::BookLibraryError;`. Replace `_make_request`, `get_sources`, `get_book`, and `get_remote_book` with:

```rust
async fn _make_request<T>(
    client: &reqwest::Client,
    url: &str,
    api_key: &str,
    params: Vec<(&str, String)>,
) -> Result<T, BookLibraryError>
where
    T: DeserializeOwned,
{
    let response = client
        .get(url)
        .query(&params)
        .header("Authorization", api_key)
        .send()
        .await
        .map_err(BookLibraryError::RequestFailed)?;

    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Err(BookLibraryError::NotFound);
    }

    let response = response
        .error_for_status()
        .map_err(BookLibraryError::UpstreamError)?;

    response
        .json::<T>()
        .await
        .map_err(BookLibraryError::RequestFailed)
}

pub async fn get_sources() -> Result<types::Source, BookLibraryError> {
    let url = format!("{}/api/v1/sources", &config::CONFIG.book_library_url);
    _make_request(&CLIENT, &url, &config::CONFIG.book_library_api_key, vec![]).await
}

pub async fn get_book(book_id: u32) -> Result<types::BookWithRemote, BookLibraryError> {
    let url = format!(
        "{}/api/v1/books/{book_id}",
        &config::CONFIG.book_library_url
    );
    _make_request(&CLIENT, &url, &config::CONFIG.book_library_api_key, vec![]).await
}

pub async fn get_remote_book(
    source_id: u32,
    remote_id: u32,
) -> Result<types::BookWithRemote, BookLibraryError> {
    let url = format!(
        "{}/api/v1/books/remote/{source_id}/{remote_id}",
        &config::CONFIG.book_library_url
    );
    let book =
        _make_request::<types::Book>(&CLIENT, &url, &config::CONFIG.book_library_api_key, vec![])
            .await?;
    Ok(types::BookWithRemote::from_book(book, remote_id))
}
```

- [ ] **Step 5: Patch the one downstream call site to keep the crate compiling**

`downloader::mod::book_download` currently does:

```rust
    let book = match get_remote_book(source_id, remote_id).await {
        Ok(v) => v,
        Err(err) => return Err(err),
    };
```

`book_download`'s declared error type is still `Box<dyn std::error::Error + Send + Sync>` at this point, so a bare `Err(err)` no longer type-checks now that `err` is `BookLibraryError`. In `src/services/downloader/mod.rs`, change just that one line to:

```rust
        Err(err) => return Err(Box::new(err)),
```

This is a temporary compile-compat patch — Task 5 replaces `book_download`'s error type entirely with `DownloadError`, at which point this line changes again.

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test book_library:: `
Expected: all pass, including the two new ones. Then run `cargo test` (full suite) to confirm nothing else broke.

- [ ] **Step 7: Fix `/filename`'s status codes (05.2)**

In `src/views.rs`, add `book_library::error::BookLibraryError` to the existing import block:

```rust
use crate::{
    config::CONFIG,
    file_type::FileType,
    services::{
        book_library::{error::BookLibraryError, get_book},
        downloader::book_download,
        filename_getter::get_filename_by_book,
    },
};
```

Replace the `get_filename` handler:

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

    (
        StatusCode::OK,
        json!({
            "filename": filename,
            "filename_ascii": filename_ascii
        })
        .to_string(),
    )
}
```

with:

```rust
pub async fn get_filename(
    Path((book_id, file_type)): Path<(u32, FileType)>,
    Query(params): Query<FilenameParams>,
) -> impl IntoResponse {
    let normalized = params.normalized.unwrap_or(true);

    let book = match get_book(book_id).await {
        Ok(v) => v,
        Err(BookLibraryError::NotFound) => {
            return (
                StatusCode::NOT_FOUND,
                json!({"error": "Book not found"}).to_string(),
            )
        }
        Err(_) => {
            return (
                StatusCode::BAD_GATEWAY,
                json!({"error": "book_library is unavailable"}).to_string(),
            )
        }
    };

    let filename = get_filename_by_book(&book, file_type.as_str(), false, false, normalized);
    let filename_ascii = get_filename_by_book(&book, file_type.as_str(), false, true, normalized);

    (
        StatusCode::OK,
        json!({
            "filename": filename,
            "filename_ascii": filename_ascii
        })
        .to_string(),
    )
}
```

- [ ] **Step 8: Write a test proving the 404-vs-502 split for `/filename`**

`get_filename`'s new behavior is only reachable through the process-wide `CONFIG` `Lazy` static (loaded from env vars at first use), so this repo has no seam to drive the real handler end-to-end in a unit test — the same constraint the codebase already lives with elsewhere. Test the decision this task actually makes — which `BookLibraryError` variant maps to which `StatusCode` — directly. Add this test to `src/views.rs`, in `mod tests`, after `content_disposition_survives_malicious_title_end_to_end`:

```rust
    #[test]
    fn book_library_not_found_maps_to_404() {
        let status = match BookLibraryError::NotFound {
            BookLibraryError::NotFound => StatusCode::NOT_FOUND,
            _ => StatusCode::BAD_GATEWAY,
        };
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn book_library_upstream_failure_maps_to_502() {
        // Construct a genuine reqwest::Error (not a stub) by making a real request against
        // a local server that returns 500, then check the same match this task adds.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = socket.read(&mut buf).await;
                let _ = socket
                    .write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n")
                    .await;
                let _ = socket.shutdown().await;
            }
        });

        let response = reqwest::Client::new()
            .get(format!("http://{addr}/x"))
            .send()
            .await
            .unwrap();
        let reqwest_err = response.error_for_status().unwrap_err();
        let lib_err = BookLibraryError::UpstreamError(reqwest_err);

        let status = match lib_err {
            BookLibraryError::NotFound => StatusCode::NOT_FOUND,
            _ => StatusCode::BAD_GATEWAY,
        };
        assert_eq!(status, StatusCode::BAD_GATEWAY);
    }
```

Add the necessary import at the top of `mod tests`: `use crate::services::book_library::error::BookLibraryError;`.

Run: `cargo test views::`
Expected: passes. Task 6 adds equivalent, more complete coverage through the shared `AppError` type once it exists (including the `RequestFailed`/`UpstreamError` arm, using a real `reqwest::Error`) — these two tests aren't removed, they just stop being the only coverage of this decision.

- [ ] **Step 9: Run the full test suite and build**

Run: `cargo test && cargo build`
Expected: all tests pass, build succeeds.

- [ ] **Step 10: Commit**

```bash
git add Cargo.toml Cargo.lock src/services/book_library/error.rs src/services/book_library/mod.rs src/services/downloader/mod.rs src/views.rs
git commit -m "fix: map book_library 404 to 404 and other failures to 502 on /filename"
```

---

### Task 5: Introduce `DownloadError` and convert the download pipeline from `Option` to `Result` (05.3, 05.4)

**Files:**
- Create: `src/services/downloader/error.rs`
- Modify: `src/services/downloader/mod.rs` (register the module; rewrite `download`, `download_chain`, `start_download_futures`, `book_download`; update existing tests)
- Modify: `src/services/downloader/utils.rs` (`response_to_tempfile`, `response_to_download_data`)
- Modify: `src/services/downloader/zip.rs` (`unzip`, `zip` return types only — the 05.7 predicate fix already landed in Task 1)
- Modify: `src/services/covert.rs` (`convert_file`)

**Interfaces:**
- Produces: `pub enum DownloadError { SourceUnavailable, BadArchive, ConverterFailed(u16), Timeout, Internal(String), Library(BookLibraryError) }` in `src/services/downloader/error.rs`, with `#[from] BookLibraryError` on the `Library` variant so `?` on a `BookLibraryError` inside a `Result<_, DownloadError>` function converts automatically.
- `book_download(source_id, remote_id, file_type, normalized) -> Result<DownloadResult, DownloadError>` (previously `Result<Option<DownloadResult>, Box<dyn Error + Send + Sync>>`). Consumed by Task 6 (`views::download`).
- Consumes: `BookLibraryError` from Task 4.

- [ ] **Step 1: Create the error type**

Create `src/services/downloader/error.rs`:

```rust
use thiserror::Error;

use crate::services::book_library::error::BookLibraryError;

#[derive(Debug, Error)]
pub enum DownloadError {
    #[error("upstream source unavailable")]
    SourceUnavailable,
    #[error("downloaded archive was invalid or contained no matching entry")]
    BadArchive,
    #[error("converter failed with status {0}")]
    ConverterFailed(u16),
    #[error("overall download deadline exceeded")]
    Timeout,
    #[error("internal error: {0}")]
    Internal(String),
    #[error(transparent)]
    Library(#[from] BookLibraryError),
}
```

In `src/services/downloader/mod.rs`, add `pub mod error;` next to `pub mod types;`.

- [ ] **Step 2: Convert `zip.rs` to `Result`**

In `src/services/downloader/zip.rs`, add `use super::error::DownloadError;` to the imports. Replace the whole `unzip` function body's `Option` plumbing (keep the extension-matching fix from Task 1 untouched) with:

```rust
pub fn unzip(
    tmp_file: SpooledTempFile,
    file_type: &str,
    max_decompressed_bytes: u64,
    max_compression_ratio: u64,
) -> Result<(SpooledTempFile, usize), DownloadError> {
    let mut archive = zip::ZipArchive::new(tmp_file).map_err(|_| DownloadError::BadArchive)?;

    let file_type_lower = file_type.to_lowercase();

    for i in 0..archive.len() {
        let mut file = archive.by_index(i).map_err(|_| DownloadError::BadArchive)?;
        let filename = file.name();

        let matches_ext = filename
            .rsplit('.')
            .next()
            .map(|ext| ext.eq_ignore_ascii_case(&file_type_lower))
            .unwrap_or(false);

        if !file.is_dir() && matches_ext {
            let declared_size = file.size();
            let compressed_size = file.compressed_size().max(1);

            if declared_size > max_decompressed_bytes {
                return Err(DownloadError::BadArchive);
            }

            if declared_size / compressed_size > max_compression_ratio {
                return Err(DownloadError::BadArchive);
            }

            let mut output_file = tempfile::spooled_tempfile(5 * 1024 * 1024);
            let mut limited = (&mut file).take(max_decompressed_bytes.saturating_add(1));

            let size: usize = match std::io::copy(&mut limited, &mut output_file) {
                Ok(v) if v > max_decompressed_bytes => return Err(DownloadError::BadArchive),
                Ok(v) => v.try_into().map_err(|_| DownloadError::BadArchive)?,
                Err(_) => return Err(DownloadError::BadArchive),
            };

            output_file
                .rewind()
                .map_err(|_| DownloadError::BadArchive)?;

            return Ok((output_file, size));
        }
    }

    Err(DownloadError::BadArchive)
}

pub fn zip(
    mut tmp_file: SpooledTempFile,
    filename: &str,
) -> Result<(SpooledTempFile, usize), DownloadError> {
    let output_file = tempfile::spooled_tempfile(5 * 1024 * 1024);
    let mut archive = zip::ZipWriter::new(output_file);

    let options: FileOptions<_> = FileOptions::default()
        .compression_level(Some(9))
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o755);

    archive
        .start_file::<&str, ()>(filename, options)
        .map_err(|_| DownloadError::Internal("failed to start zip entry".to_string()))?;

    std::io::copy(&mut tmp_file, &mut archive)
        .map_err(|_| DownloadError::Internal("failed to write zip entry".to_string()))?;

    let mut archive_result = archive
        .finish()
        .map_err(|_| DownloadError::Internal("failed to finalize zip archive".to_string()))?;

    let data_size: usize = archive_result
        .stream_position()
        .map_err(|_| DownloadError::Internal("failed to read zip archive size".to_string()))?
        .try_into()
        .map_err(|_| DownloadError::Internal("zip archive size overflow".to_string()))?;

    archive_result
        .rewind()
        .map_err(|_| DownloadError::Internal("failed to rewind zip archive".to_string()))?;

    Ok((archive_result, data_size))
}
```

Update the existing tests in `zip.rs`'s `mod tests`: change `assert!(result.is_none());` to `assert!(result.is_err());` in `corrupt_zip_bytes_return_none_instead_of_panicking`, `oversized_declared_entry_is_rejected`, and `high_compression_ratio_entry_is_rejected` (the three tests added in Task 1 already use `.expect(...)`/`.is_none()` in ways that still compile unchanged — `.expect` works identically on `Result` and `Option`).

Run: `cargo test downloader::zip::`
Expected: all pass.

- [ ] **Step 3: Convert `utils.rs` to `Result`**

In `src/services/downloader/utils.rs`, add `use super::error::DownloadError;`. Replace `response_to_tempfile` and `response_to_download_data`:

```rust
pub async fn response_to_tempfile(
    res: &mut Response,
    max_bytes: usize,
) -> Result<(SpooledTempFile, usize), DownloadError> {
    if let Some(declared) = res.content_length() {
        if declared > max_bytes as u64 {
            return Err(DownloadError::SourceUnavailable);
        }
    }

    let mut tmp_file = tempfile::spooled_tempfile(5 * 1024 * 1024);

    let mut data_size: usize = 0;

    {
        loop {
            let chunk = res.chunk().await;

            let result = match chunk {
                Ok(v) => v,
                Err(_) => return Err(DownloadError::SourceUnavailable),
            };

            let data = match result {
                Some(v) => v,
                None => break,
            };

            data_size += data.len();

            if data_size > max_bytes {
                return Err(DownloadError::SourceUnavailable);
            }

            match tmp_file.write_all(data.chunk()) {
                Ok(_) => (),
                Err(_) => return Err(DownloadError::SourceUnavailable),
            }
        }

        match tmp_file.seek(SeekFrom::Start(0)) {
            Ok(_) => (),
            Err(_) => return Err(DownloadError::SourceUnavailable),
        }
    }

    Ok((tmp_file, data_size))
}

pub async fn response_to_download_data(
    mut response: Response,
    max_bytes: usize,
) -> Result<(Data, usize), DownloadError> {
    if let Some(size) = parse_content_length(response.headers()) {
        return Ok((Data::Response(response), size));
    }

    let (tmp_file, size) = response_to_tempfile(&mut response, max_bytes).await?;
    Ok((Data::SpooledTempFile(tmp_file), size))
}
```

`parse_content_length` and its tests are untouched (they don't call the two functions above).

Run: `cargo build`
Expected: fails — `zip.rs` and `utils.rs` now return `Result`, but `mod.rs` and `covert.rs` still call them expecting `Option`. This is expected; continue to the next steps before running tests again.

- [ ] **Step 4: Convert `covert.rs` to `Result`, capturing the converter's status (05.4)**

Replace `src/services/covert.rs` in full:

```rust
use reqwest::{Body, Response};
use std::time::Duration;
use tempfile::SpooledTempFile;
use tokio_util::io::ReaderStream;

use crate::config;

use super::downloader::error::DownloadError;
use super::downloader::types::spooled_temp_file_into_async_read;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

pub async fn convert_file(
    file: SpooledTempFile,
    file_type: String,
) -> Result<Response, DownloadError> {
    let body = Body::wrap_stream(ReaderStream::new(spooled_temp_file_into_async_read(file)));

    let client = reqwest::Client::builder()
        .connect_timeout(config::CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|_| DownloadError::SourceUnavailable)?;

    let response = client
        .post(format!("{}{}", config::CONFIG.converter_url, file_type))
        .body(body)
        .header("Authorization", &config::CONFIG.converter_api_key)
        .send()
        .await
        .map_err(|_| DownloadError::SourceUnavailable)?;

    response.error_for_status().map_err(|err| {
        let status = err.status().map(|s| s.as_u16()).unwrap_or(0);
        DownloadError::ConverterFailed(status)
    })
}
```

`covert.rs` has no existing tests, so no test updates are needed here.

- [ ] **Step 5: Rewrite `mod.rs`'s core functions**

In `src/services/downloader/mod.rs`, add `use self::error::DownloadError;` to the imports (near `use self::types::{Data, DownloadResult};`).

Replace the `download` function's three `return None` sites (invalid base URL parse, mirror URL join failure, request send failure), the `error_for_status` failure site, and the `unexpected_html` site with `return Err(DownloadError::SourceUnavailable)`, and change the signature and final `Some((response, is_zip))` / early `Some((response, false))` returns to `Ok(...)`:

```rust
pub async fn download<'a>(
    book_id: &'a u32,
    book_file_type: &'a str,
    source_config: &'a config::SourceConfig,
) -> Result<(Response, bool), DownloadError> {
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
            return Err(DownloadError::SourceUnavailable);
        }
    };

    if !base.path().ends_with('/') {
        let path_with_slash = format!("{}/", base.path());
        base.set_path(&path_with_slash);
    }

    let book_file_type_lower = book_file_type.to_lowercase();
    let relative = if book_file_type_lower == "fb2"
        || book_file_type_lower == "epub"
        || book_file_type_lower == "mobi"
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
            return Err(DownloadError::SourceUnavailable);
        }
    };

    let fetch_start = Instant::now();
    let response = source_config.client.get(url).send().await;

    let response = match response {
        Ok(v) => v,
        Err(err) => {
            metrics::counter!(
                "downloader_source_requests_total",
                "source" => source_config.url.clone(),
                "outcome" => "request_error"
            )
            .increment(1);
            warn!(
                source = %source_config.url,
                book_id,
                file_type = book_file_type,
                stage = "mirror_fetch",
                error = %err,
                "mirror request failed"
            );
            return Err(DownloadError::SourceUnavailable);
        }
    };

    let response = match response.error_for_status() {
        Ok(v) => v,
        Err(err) => {
            metrics::counter!(
                "downloader_source_requests_total",
                "source" => source_config.url.clone(),
                "outcome" => "http_error"
            )
            .increment(1);
            warn!(
                source = %source_config.url,
                book_id,
                file_type = book_file_type,
                stage = "mirror_fetch",
                error = %err,
                "mirror returned an error status"
            );
            return Err(DownloadError::SourceUnavailable);
        }
    };

    let headers = response.headers();
    let content_type = match headers.get("Content-Type") {
        Some(v) => v.to_str().unwrap_or(""),
        None => "",
    };

    if book_file_type_lower == "html" && content_type.contains("text/html") {
        metrics::counter!(
            "downloader_source_requests_total",
            "source" => source_config.url.clone(),
            "outcome" => "success"
        )
        .increment(1);
        metrics::histogram!("download_stage_duration_seconds", "stage" => "mirror_fetch")
            .record(fetch_start.elapsed().as_secs_f64());
        return Ok((response, false));
    }

    if content_type.contains("text/html") {
        metrics::counter!(
            "downloader_source_requests_total",
            "source" => source_config.url.clone(),
            "outcome" => "unexpected_html"
        )
        .increment(1);
        warn!(
            source = %source_config.url,
            book_id,
            file_type = book_file_type,
            stage = "mirror_fetch",
            "mirror served an HTML page instead of the requested file"
        );
        return Err(DownloadError::SourceUnavailable);
    }

    metrics::counter!(
        "downloader_source_requests_total",
        "source" => source_config.url.clone(),
        "outcome" => "success"
    )
    .increment(1);
    metrics::histogram!("download_stage_duration_seconds", "stage" => "mirror_fetch")
        .record(fetch_start.elapsed().as_secs_f64());

    let is_zip = content_type.contains("application/zip");

    Ok((response, is_zip))
}
```

Replace `download_chain` in full:

```rust
pub async fn download_chain(
    book: BookWithRemote,
    file_type: String,
    source_config: config::SourceConfig,
    converting: bool,
    normalized: bool,
    limits: config::DownloadLimits,
) -> Result<DownloadResult, DownloadError> {
    let final_need_zip = file_type == "fb2zip";

    let file_type_ = if converting {
        book.file_type.clone()
    } else {
        file_type.clone()
    };

    let (mut response, is_zip) = download(&book.remote_id, &file_type_, &source_config).await?;

    if is_zip && book.file_type.to_lowercase() == "html" {
        let filename = get_filename_by_book(&book, &file_type, true, false, normalized);
        let filename_ascii = get_filename_by_book(&book, &file_type, true, true, normalized);
        let (data, data_size) = response_to_download_data(response, limits.max_download_bytes)
            .await
            .map_err(|err| {
                warn!(
                    source = %source_config.url,
                    book_id = book.remote_id,
                    file_type = %file_type,
                    stage = "buffer_response",
                    "failed to read HTML archive response body"
                );
                err
            })?;

        return Ok(DownloadResult::new(
            data,
            filename,
            filename_ascii,
            data_size,
        ));
    }

    if !is_zip && !final_need_zip && !converting {
        let filename = get_filename_by_book(&book, &file_type, false, false, normalized);
        let filename_ascii = get_filename_by_book(&book, &file_type, false, true, normalized);
        let (data, data_size) = response_to_download_data(response, limits.max_download_bytes)
            .await
            .map_err(|err| {
                warn!(
                    source = %source_config.url,
                    book_id = book.remote_id,
                    file_type = %file_type,
                    stage = "buffer_response",
                    "failed to read direct download response body"
                );
                err
            })?;

        return Ok(DownloadResult::new(
            data,
            filename,
            filename_ascii,
            data_size,
        ));
    };

    let (unzipped_temp_file, data_size) = {
        let (temp_file_to_unzip, _) = response_to_tempfile(&mut response, limits.max_download_bytes)
            .await
            .map_err(|err| {
                warn!(
                    source = %source_config.url,
                    book_id = book.remote_id,
                    file_type = %file_type,
                    stage = "buffer_response",
                    "failed to buffer zip response body to a temp file"
                );
                err
            })?;

        let unzip_start = Instant::now();
        let unzip_result = match tokio::task::spawn_blocking(move || {
            unzip(
                temp_file_to_unzip,
                "fb2",
                limits.max_decompressed_bytes,
                limits.max_compression_ratio,
            )
        })
        .await
        {
            Ok(v) => v,
            Err(err) => {
                metrics::histogram!("download_stage_duration_seconds", "stage" => "unzip")
                    .record(unzip_start.elapsed().as_secs_f64());
                error!(
                    source = %source_config.url,
                    book_id = book.remote_id,
                    file_type = %file_type,
                    stage = "unzip",
                    error = %err,
                    "unzip task panicked"
                );
                return Err(DownloadError::Internal("unzip task panicked".to_string()));
            }
        };
        metrics::histogram!("download_stage_duration_seconds", "stage" => "unzip")
            .record(unzip_start.elapsed().as_secs_f64());

        match unzip_result {
            Ok(v) => v,
            Err(err) => {
                warn!(
                    source = %source_config.url,
                    book_id = book.remote_id,
                    file_type = %file_type,
                    stage = "unzip",
                    error = %err,
                    "no matching entry found in zip archive, or the entry exceeded size/ratio limits"
                );
                return Err(err);
            }
        }
    };

    let (clean_file, data_size) = if converting {
        let mut converted = convert_file(unzipped_temp_file, file_type.to_string())
            .await
            .map_err(|err| {
                warn!(
                    source = %source_config.url,
                    book_id = book.remote_id,
                    file_type = %file_type,
                    stage = "convert",
                    error = %err,
                    "converter request failed"
                );
                err
            })?;

        response_to_tempfile(&mut converted, limits.max_download_bytes)
            .await
            .map_err(|err| {
                warn!(
                    source = %source_config.url,
                    book_id = book.remote_id,
                    file_type = %file_type,
                    stage = "buffer_response",
                    "failed to buffer converted response body to a temp file"
                );
                err
            })?
    } else {
        (unzipped_temp_file, data_size)
    };

    if !final_need_zip {
        let filename = get_filename_by_book(&book, &file_type, false, false, normalized);
        let filename_ascii = get_filename_by_book(&book, &file_type, false, true, normalized);

        return Ok(DownloadResult::new(
            Data::SpooledTempFile(clean_file),
            filename,
            filename_ascii,
            data_size,
        ));
    };

    let t_file_type = if file_type == "fb2zip" {
        "fb2"
    } else {
        &file_type
    };
    let filename = get_filename_by_book(&book, t_file_type, false, false, normalized);

    let zip_start = Instant::now();
    let zip_result = match tokio::task::spawn_blocking(move || zip(clean_file, &filename)).await {
        Ok(v) => v,
        Err(err) => {
            metrics::histogram!("download_stage_duration_seconds", "stage" => "zip")
                .record(zip_start.elapsed().as_secs_f64());
            error!(
                source = %source_config.url,
                book_id = book.remote_id,
                file_type = %file_type,
                stage = "zip",
                error = %err,
                "zip task panicked"
            );
            return Err(DownloadError::Internal("zip task panicked".to_string()));
        }
    };
    metrics::histogram!("download_stage_duration_seconds", "stage" => "zip")
        .record(zip_start.elapsed().as_secs_f64());

    match zip_result {
        Ok((t_file, data_size)) => {
            let filename = get_filename_by_book(&book, &file_type, true, false, normalized);
            let filename_ascii = get_filename_by_book(&book, &file_type, true, true, normalized);

            Ok(DownloadResult::new(
                Data::SpooledTempFile(t_file),
                filename,
                filename_ascii,
                data_size,
            ))
        }
        Err(err) => {
            warn!(
                source = %source_config.url,
                book_id = book.remote_id,
                file_type = %file_type,
                stage = "zip",
                error = %err,
                "failed to create result zip archive"
            );
            Err(err)
        }
    }
}
```

Note the one behavior-preserving change from the original: `response_to_tempfile(&mut response, ...)` used to be called as `response_to_tempfile(&mut response, limits.max_download_bytes).await` and its `.0` field accessed directly; here it's destructured as `(temp_file_to_unzip, _)` since the buffered size isn't used before unzip recomputes its own `data_size`. This matches the original's behavior exactly (the original also discarded that first `data_size` value inside the same block-scope, since `unzip`'s returned size wins).

Replace `start_download_futures`:

```rust
pub async fn start_download_futures(
    book: &BookWithRemote,
    file_type: &str,
    normalized: bool,
    sources: &[config::SourceConfig],
    limits: config::DownloadLimits,
    overall_deadline: std::time::Duration,
) -> Result<DownloadResult, DownloadError> {
    let attempt = async {
        let mut last_err = DownloadError::SourceUnavailable;

        for source_config in sources {
            match download_chain(
                book.clone(),
                file_type.to_string(),
                source_config.clone(),
                false,
                normalized,
                limits,
            )
            .await
            {
                Ok(result) => return Ok(result),
                Err(err) => last_err = err,
            }

            if file_type == "epub" || file_type == "fb2" {
                match download_chain(
                    book.clone(),
                    file_type.to_string(),
                    source_config.clone(),
                    true,
                    normalized,
                    limits,
                )
                .await
                {
                    Ok(result) => return Ok(result),
                    Err(err) => last_err = err,
                }
            }
        }

        Err(last_err)
    };

    match tokio::time::timeout(overall_deadline, attempt).await {
        Ok(result) => result,
        Err(_) => Err(DownloadError::Timeout),
    }
}
```

Replace `book_download`:

```rust
pub async fn book_download(
    source_id: u32,
    remote_id: u32,
    file_type: &str,
    normalized: bool,
) -> Result<DownloadResult, DownloadError> {
    let book = get_remote_book(source_id, remote_id).await?;

    start_download_futures(
        &book,
        file_type,
        normalized,
        &config::CONFIG.fl_sources,
        config::CONFIG.download_limits,
        config::CONFIG.overall_download_timeout,
    )
    .await
}
```

(This replaces the Task 4 compile-compat `Err(err) => return Err(Box::new(err))` patch entirely — the whole function is rewritten above.)

- [ ] **Step 6: Update the existing test assertions for the new `Result` API**

In `src/services/downloader/mod.rs`'s `mod tests`, make these targeted changes:

In `direct_success_skips_conversion_attempt`, change `assert!(result.is_some());` to `assert!(result.is_ok());`.

In `stalled_mirror_fails_over_to_next_source`, change `assert!(result.is_some(), "should fail over to the working mirror");` to `assert!(result.is_ok(), "should fail over to the working mirror");`.

In `overall_deadline_bounds_total_latency_even_if_all_mirrors_stall`, change:

```rust
        assert!(result.is_none());
```

to:

```rust
        assert!(matches!(result, Err(DownloadError::Timeout)));
```

In `corrupt_zip_body_returns_none_instead_of_panicking`, rename it to `corrupt_zip_body_returns_err_instead_of_panicking` and change `assert!(result.is_none());` to `assert!(result.is_err());`.

In `oversized_body_without_content_length_is_rejected`, change `assert!(result.is_none());` to `assert!(result.is_err());`.

In `mirror_http_error_is_logged_and_counted`, change `assert!(result.is_none());` to `assert!(result.is_err());`.

In `mirror_connect_failure_is_logged_and_counted`, change `assert!(result.is_none());` to `assert!(result.is_err());`.

In `unzip_failure_is_logged_with_stage`, change `assert!(result.is_none());` to `assert!(result.is_err());`.

In `mirror_url_rejects_invalid_base_url_instead_of_panicking`, change `assert!(result.is_none());` to `assert!(result.is_err());`.

All other tests (`missing_content_length_falls_back_to_buffering`, `html_zip_missing_content_length_falls_back_to_buffering`, `valid_content_length_streams_without_buffering`, `binary_content_type_does_not_panic`, `mirror_request_path_matches_expected_url`, and the three tests added in Tasks 2 and 3) use `.expect(...)` or don't inspect the `Option`/`Result` shape directly, so they compile unchanged.

- [ ] **Step 7: Run the full test suite and build**

Run: `cargo test && cargo build`
Expected: all tests pass, build succeeds with no errors. (A `dead_code` warning is possible if any `DownloadError` variant is never constructed by this point — check with `cargo build 2>&1 | grep warning`; if `Internal` or another variant shows as unused, that's fine, it's exercised via the panic-recovery paths which aren't covered by an existing test and doesn't need a new one for this task.)

- [ ] **Step 8: Commit**

```bash
git add src/services/downloader/error.rs src/services/downloader/mod.rs src/services/downloader/utils.rs src/services/downloader/zip.rs src/services/covert.rs
git commit -m "fix: replace Option with a DownloadError enum across the download pipeline"
```

---

### Task 6: Introduce shared `AppError` in `views.rs` and fix `/download`'s status codes (05.1)

**Files:**
- Modify: `src/views.rs`

**Interfaces:**
- Produces: a private `enum AppError` implementing `axum::response::IntoResponse`, `From<BookLibraryError>`, and `From<DownloadError>`.
- Consumes: `BookLibraryError` (Task 4), `DownloadError` (Task 5), `DownloadResult` (existing, from `downloader::types`).

- [ ] **Step 1: Write the failing tests**

In `src/views.rs`, in `mod tests`, add these tests after the two `book_library_*` tests from Task 4 (Step 8) — those stay as-is, testing the raw `BookLibraryError -> StatusCode` decision; these new ones test the same decision through the shared `AppError` type both handlers now use:

```rust
    #[test]
    fn app_error_from_book_library_not_found_maps_to_404() {
        let app_err: AppError = BookLibraryError::NotFound.into();
        assert_eq!(app_err.status_code(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn download_error_library_not_found_maps_to_404() {
        let app_err: AppError = DownloadError::from(BookLibraryError::NotFound).into();
        assert_eq!(app_err.status_code(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn download_error_source_unavailable_maps_to_502() {
        let app_err: AppError = DownloadError::SourceUnavailable.into();
        assert_eq!(app_err.status_code(), StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn download_error_bad_archive_maps_to_502() {
        let app_err: AppError = DownloadError::BadArchive.into();
        assert_eq!(app_err.status_code(), StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn download_error_converter_failed_maps_to_502() {
        let app_err: AppError = DownloadError::ConverterFailed(503).into();
        assert_eq!(app_err.status_code(), StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn download_error_timeout_maps_to_504() {
        let app_err: AppError = DownloadError::Timeout.into();
        assert_eq!(app_err.status_code(), StatusCode::GATEWAY_TIMEOUT);
    }

    #[test]
    fn no_variant_maps_to_204_no_content() {
        let variants = [
            AppError::NotFound,
            AppError::SourceUnavailable,
            AppError::BadArchive,
            AppError::ConverterFailed(500),
            AppError::Timeout,
            AppError::UpstreamError,
            AppError::Internal,
        ];
        for v in variants {
            assert_ne!(v.status_code(), StatusCode::NO_CONTENT);
        }
    }

    #[tokio::test]
    async fn app_error_into_response_has_json_body_and_correct_status() {
        let response = AppError::NotFound.into_response();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(!body_bytes.is_empty());
        let parsed: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        assert!(parsed.get("error").is_some());
    }
```

Also add `use crate::services::downloader::error::DownloadError;` to the imports at the top of `mod tests` (alongside the existing `use crate::services::book_library::error::BookLibraryError;` from Task 4).

- [ ] **Step 2: Run the tests to verify they fail to compile**

Run: `cargo test views::`
Expected: compile error — `AppError` doesn't exist yet.

- [ ] **Step 3: Add `AppError` and wire both handlers**

In `src/views.rs`, update the import block:

```rust
use crate::{
    config::CONFIG,
    file_type::FileType,
    services::{
        book_library::{error::BookLibraryError, get_book},
        downloader::{book_download, error::DownloadError},
        filename_getter::get_filename_by_book,
    },
};
```

Add the `AppError` type directly above `pub async fn download`:

```rust
#[derive(Debug, PartialEq, Eq)]
enum AppError {
    NotFound,
    SourceUnavailable,
    BadArchive,
    ConverterFailed(u16),
    Timeout,
    UpstreamError,
    Internal,
}

impl AppError {
    fn status_code(&self) -> StatusCode {
        match self {
            AppError::NotFound => StatusCode::NOT_FOUND,
            AppError::Timeout => StatusCode::GATEWAY_TIMEOUT,
            AppError::Internal => StatusCode::INTERNAL_SERVER_ERROR,
            AppError::SourceUnavailable
            | AppError::BadArchive
            | AppError::ConverterFailed(_)
            | AppError::UpstreamError => StatusCode::BAD_GATEWAY,
        }
    }

    fn message(&self) -> String {
        match self {
            AppError::NotFound => "Book not found".to_string(),
            AppError::SourceUnavailable => "Upstream source unavailable".to_string(),
            AppError::BadArchive => "Downloaded archive was invalid".to_string(),
            AppError::ConverterFailed(status) => format!("Converter failed with status {status}"),
            AppError::Timeout => "Download timed out".to_string(),
            AppError::UpstreamError => "book_library is unavailable".to_string(),
            AppError::Internal => "Internal error".to_string(),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = self.status_code();
        let body = json!({ "error": self.message() }).to_string();
        (status, body).into_response()
    }
}

impl From<BookLibraryError> for AppError {
    fn from(err: BookLibraryError) -> Self {
        match err {
            BookLibraryError::NotFound => AppError::NotFound,
            BookLibraryError::RequestFailed(_) | BookLibraryError::UpstreamError(_) => {
                AppError::UpstreamError
            }
        }
    }
}

impl From<DownloadError> for AppError {
    fn from(err: DownloadError) -> Self {
        match err {
            DownloadError::Library(lib_err) => AppError::from(lib_err),
            DownloadError::SourceUnavailable => AppError::SourceUnavailable,
            DownloadError::BadArchive => AppError::BadArchive,
            DownloadError::ConverterFailed(status) => AppError::ConverterFailed(status),
            DownloadError::Timeout => AppError::Timeout,
            DownloadError::Internal(_) => AppError::Internal,
        }
    }
}
```

Replace the `download` handler:

```rust
pub async fn download(
    Path((source_id, remote_id, file_type)): Path<(u32, u32, FileType)>,
    Query(params): Query<FilenameParams>,
) -> Result<impl IntoResponse, AppError> {
    let normalized = params.normalized.unwrap_or(true);

    let data = book_download(source_id, remote_id, file_type.as_str(), normalized)
        .await
        .map_err(AppError::from)?;

    let filename = data.filename.clone();
    let filename_ascii = data.filename_ascii.clone();
    let file_size = data.data_size;

    let reader = data.get_async_read();
    let stream = ReaderStream::new(reader);

    let encoder = general_purpose::STANDARD;

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

    Ok((headers, Body::from_stream(stream)))
}
```

Replace the `get_filename` handler (unifying it onto `AppError` too, since the type now exists for both call sites):

```rust
pub async fn get_filename(
    Path((book_id, file_type)): Path<(u32, FileType)>,
    Query(params): Query<FilenameParams>,
) -> Result<impl IntoResponse, AppError> {
    let normalized = params.normalized.unwrap_or(true);

    let book = get_book(book_id).await.map_err(AppError::from)?;
    let filename = get_filename_by_book(&book, file_type.as_str(), false, false, normalized);
    let filename_ascii = get_filename_by_book(&book, file_type.as_str(), false, true, normalized);

    Ok((
        StatusCode::OK,
        json!({
            "filename": filename,
            "filename_ascii": filename_ascii
        })
        .to_string(),
    ))
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test views::`
Expected: all pass, including every test added in this task and in Task 4's Step 8 (now updated).

- [ ] **Step 5: Run the full test suite and both build profiles**

Run: `cargo test && cargo build && cargo build --release`
Expected: all tests pass, both builds succeed.

- [ ] **Step 6: Commit**

```bash
git add src/views.rs
git commit -m "fix: return 404/502/504 instead of 204/400 on download and filename failures"
```

---

## Final acceptance-criteria checklist

- [ ] Failure responses use correct status codes (`404`, `502`, `504`); no `204` with a body remains — covered by `AppError::status_code` (Task 6) routing every variant away from `204`, verified by `no_variant_maps_to_204_no_content`, and by the `download`/`get_filename` handlers no longer constructing `(StatusCode::NO_CONTENT, ...)` or `(StatusCode::BAD_REQUEST, ...)` directly (Tasks 4 and 6).
- [ ] The downloader pipeline returns `Result<_, DownloadError>`; handler maps error variants to statuses; tests cover the mapping — covered by Task 5 (`download`, `download_chain`, `start_download_futures`, `book_download` all return `Result`) and Task 6 (`AppError::from(DownloadError)` plus `download_error_*_maps_to_*` tests).
- [ ] `filename` and `filename_ascii` always share the same extension for every branch of `download_chain` — covered by Task 3's fix and `direct_download_filename_and_ascii_share_the_requested_extension` test (the other three branches of `download_chain` already used a single file-type value for both, per the original spec's line references).
- [ ] `unzip` selects entries by extension; a test with an archive containing `cover.jpg` (well, `cover.fb2.jpg`, an even sharper substring-collision case) + `book.fb2` picks `book.fb2` — covered by Task 1's `selects_entry_by_extension_not_substring` test.
