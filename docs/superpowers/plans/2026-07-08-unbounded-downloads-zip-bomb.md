# Unbounded Downloads & ZIP-Bomb Protection Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Cap every place the downloader buffers or streams untrusted mirror data — the temp-file copy loop, ZIP decompression, and the client-facing pass-through response — so a hostile or broken mirror can no longer exhaust disk space or trigger a ZIP-bomb expansion.

**Architecture:** Add a `config::DownloadLimits { max_download_bytes, max_decompressed_bytes, max_compression_ratio }` struct, populated from optional env vars with sane defaults, and thread it as an explicit parameter through `download_chain` → `response_to_tempfile` / `response_to_download_data` / `unzip`. Enforce the download cap inside the existing copy loop (plus an early check against a declared `Content-Length`). Enforce the ZIP cap by checking the declared entry size and compression ratio before copying, then bounding the actual copy with `Read::take` as a defense-in-depth backstop. Cap the client-facing pass-through stream at the declared `Content-Length` using `AsyncReadExt::take`.

**Tech Stack:** Rust, tokio, reqwest, zip, tempfile. No new dependencies — `tokio`'s `AsyncReadExt::take` and `std::io::Read::take` (already available via existing deps) provide the limiting primitives.

## Global Constraints

- Do not add new crates/dependencies.
- `cargo test` and `cargo build --release` must pass after every task.
- Preserve current streaming behavior: when a mirror sends a valid `Content-Length`, the response must still stream via `Data::Response` (no unnecessary buffering) — it now additionally gets truncated at the declared length.
- New limits are configurable via env vars, each with a sane default, documented via doc comments on the `Config` fields (this repo has no separate env-var README; existing fields like `API_KEY` follow the same doc-comment-only convention):
  - `MAX_DOWNLOAD_BYTES` — default `200 * 1024 * 1024` (200 MiB)
  - `MAX_DECOMPRESSED_BYTES` — default `200 * 1024 * 1024` (200 MiB)
  - `MAX_COMPRESSION_RATIO` — default `100`
- Follow the existing test convention in this codebase: hand-roll a raw HTTP server with `tokio::net::TcpListener` for downloader integration tests (no mocking crate); test pure parsing/limit-check functions directly with literal inputs (no env var mutation, no forcing `config::CONFIG`'s `Lazy` init, which would panic in tests since required env vars like `API_KEY` aren't set in the test process).

---

### Task 1: `DownloadLimits` config, sourced from optional env vars

**Files:**
- Modify: `src/config.rs`

**Interfaces:**
- Produces: `pub struct DownloadLimits { pub max_download_bytes: usize, pub max_decompressed_bytes: u64, pub max_compression_ratio: u64 }` (derives `Clone, Copy`), `pub download_limits: DownloadLimits` field on `Config` — consumed by Task 2 (`response_to_tempfile`/`response_to_download_data`/`download_chain`) and Task 3 (`unzip`).

- [ ] **Step 1: Write the failing tests**

Add to the `mod tests` block at the bottom of `src/config.rs` (after the existing four tests):

```rust
    #[test]
    fn download_limit_missing_value_uses_default() {
        assert_eq!(parse_usize_or(None, "MAX_DOWNLOAD_BYTES", 42), 42);
    }

    #[test]
    fn download_limit_valid_value_overrides_default() {
        assert_eq!(parse_usize_or(Some("1234"), "MAX_DOWNLOAD_BYTES", 42), 1234);
    }

    #[test]
    #[should_panic(expected = "MAX_DOWNLOAD_BYTES must be a valid non-negative integer")]
    fn download_limit_invalid_value_panics() {
        parse_usize_or(Some("not-a-number"), "MAX_DOWNLOAD_BYTES", 42);
    }

    #[test]
    fn compression_ratio_missing_value_uses_default() {
        assert_eq!(parse_u64_or(None, "MAX_COMPRESSION_RATIO", 100), 100);
    }

    #[test]
    fn compression_ratio_valid_value_overrides_default() {
        assert_eq!(parse_u64_or(Some("7"), "MAX_COMPRESSION_RATIO", 100), 7);
    }

    #[test]
    #[should_panic(expected = "MAX_COMPRESSION_RATIO must be a valid non-negative integer")]
    fn compression_ratio_invalid_value_panics() {
        parse_u64_or(Some("nope"), "MAX_COMPRESSION_RATIO", 100);
    }
```

- [ ] **Step 2: Run tests to verify they fail to compile**

Run: `cargo test --lib config`
Expected: FAIL with `cannot find function 'parse_usize_or' in this scope` (and `parse_u64_or`).

- [ ] **Step 3: Add the parsing helpers and `DownloadLimits` struct**

In `src/config.rs`, add below the existing `get_env` function:

```rust
fn parse_usize_or(raw: Option<&str>, env: &'static str, default: usize) -> usize {
    match raw {
        Some(v) => v
            .parse()
            .unwrap_or_else(|err| panic!("{env} must be a valid non-negative integer: {err}")),
        None => default,
    }
}

fn parse_u64_or(raw: Option<&str>, env: &'static str, default: u64) -> u64 {
    match raw {
        Some(v) => v
            .parse()
            .unwrap_or_else(|err| panic!("{env} must be a valid non-negative integer: {err}")),
        None => default,
    }
}
```

Add below the `SourceConfig`-related code (after `parse_fl_sources`, before `pub struct Config`):

```rust
/// Max bytes buffered from a single mirror response into a temp file.
/// Configurable via `MAX_DOWNLOAD_BYTES` (default 200 MiB).
///
/// Max declared uncompressed bytes / compression ratio allowed for a single
/// ZIP entry before it is treated as a ZIP bomb and rejected.
/// Configurable via `MAX_DECOMPRESSED_BYTES` (default 200 MiB) and
/// `MAX_COMPRESSION_RATIO` (default 100).
#[derive(Clone, Copy)]
pub struct DownloadLimits {
    pub max_download_bytes: usize,
    pub max_decompressed_bytes: u64,
    pub max_compression_ratio: u64,
}
```

Add the field to `Config`:

```rust
pub struct Config {
    pub api_key: String,

    pub fl_sources: Vec<SourceConfig>,

    pub book_library_api_key: String,
    pub book_library_url: String,

    pub converter_url: String,
    pub converter_api_key: String,

    pub sentry_dsn: Option<String>,

    pub download_limits: DownloadLimits,
}
```

Populate it in `Config::load()`:

```rust
impl Config {
    pub fn load() -> Config {
        Config {
            api_key: get_env("API_KEY"),

            fl_sources: parse_fl_sources(&get_env("FL_SOURCES")),

            book_library_api_key: get_env("BOOK_LIBRARY_API_KEY"),
            book_library_url: get_env("BOOK_LIBRARY_URL"),

            converter_url: get_env("CONVERTER_URL"),
            converter_api_key: get_env("CONVERTER_API_KEY"),

            sentry_dsn: std::env::var("SENTRY_DSN").ok(),

            download_limits: DownloadLimits {
                max_download_bytes: parse_usize_or(
                    std::env::var("MAX_DOWNLOAD_BYTES").ok().as_deref(),
                    "MAX_DOWNLOAD_BYTES",
                    200 * 1024 * 1024,
                ),
                max_decompressed_bytes: parse_u64_or(
                    std::env::var("MAX_DECOMPRESSED_BYTES").ok().as_deref(),
                    "MAX_DECOMPRESSED_BYTES",
                    200 * 1024 * 1024,
                ),
                max_compression_ratio: parse_u64_or(
                    std::env::var("MAX_COMPRESSION_RATIO").ok().as_deref(),
                    "MAX_COMPRESSION_RATIO",
                    100,
                ),
            },
        }
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib config`
Expected: `test result: ok. 10 passed` (4 existing + 6 new).

- [ ] **Step 5: Commit**

```bash
git add src/config.rs
git commit -m "feat: add configurable download-size and ZIP-bomb limits to Config"
```

---

### Task 2: Enforce max download size when buffering to a temp file (Spec 03.1)

**Files:**
- Modify: `src/services/downloader/utils.rs`
- Modify: `src/services/downloader/mod.rs`

**Interfaces:**
- Consumes: `config::DownloadLimits` (Task 1).
- Produces (signature changes): `pub async fn response_to_tempfile(res: &mut Response, max_bytes: usize) -> Option<(SpooledTempFile, usize)>`, `pub async fn response_to_download_data(response: Response, max_bytes: usize) -> Option<(Data, usize)>`, `pub async fn download_chain(book: BookWithRemote, file_type: String, source_config: config::SourceConfig, converting: bool, normalized: bool, limits: config::DownloadLimits) -> Option<DownloadResult>` (adds the `limits` parameter — consumed by Task 3 for the `unzip` call without further signature changes).

- [ ] **Step 1: Update the test module in `src/services/downloader/mod.rs` — add a `generous_limits()` helper, fix the 4 existing `download_chain` calls, and add the new failing test**

In the `mod tests` block of `src/services/downloader/mod.rs`, add next to `make_source_config`/`make_book`:

```rust
    fn generous_limits() -> config::DownloadLimits {
        config::DownloadLimits {
            max_download_bytes: 5 * 1024 * 1024,
            max_decompressed_bytes: 5 * 1024 * 1024,
            max_compression_ratio: 1000,
        }
    }
```

Update the 4 existing `download_chain(...)` calls (`missing_content_length_falls_back_to_buffering`, `html_zip_missing_content_length_falls_back_to_buffering`, `valid_content_length_streams_without_buffering`, `corrupt_zip_body_returns_none_instead_of_panicking`) to append `, generous_limits()` as the last argument, e.g.:

```rust
        let result =
            download_chain(book, "fb2".to_string(), source_config, false, true, generous_limits())
                .await;
```

Add the new test:

```rust
    #[tokio::test]
    async fn oversized_body_without_content_length_is_rejected() {
        let body = vec![b'a'; 64];
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/fb2\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{}\r\n0\r\n\r\n",
            body.len(),
            std::str::from_utf8(&body).unwrap()
        );
        let base_url = spawn_raw_server(response.into_bytes()).await;
        let source_config = make_source_config(base_url);
        let book = make_book("fb2");
        let limits = config::DownloadLimits {
            max_download_bytes: 10,
            max_decompressed_bytes: 5 * 1024 * 1024,
            max_compression_ratio: 1000,
        };

        let result =
            download_chain(book, "fb2".to_string(), source_config, false, true, limits).await;

        assert!(result.is_none());
    }
```

- [ ] **Step 2: Run tests to verify they fail to compile**

Run: `cargo test --lib downloader`
Expected: FAIL — `this function takes 5 arguments but 6 arguments were supplied` (`download_chain` doesn't accept `limits` yet).

- [ ] **Step 3: Update `src/services/downloader/utils.rs` with the capped implementation**

Replace `response_to_tempfile` and `response_to_download_data` (everything between the `parse_content_length` function and the `#[cfg(test)]` block) with:

```rust
pub async fn response_to_tempfile(
    res: &mut Response,
    max_bytes: usize,
) -> Option<(SpooledTempFile, usize)> {
    if let Some(declared) = res.content_length() {
        if declared > max_bytes as u64 {
            return None;
        }
    }

    let mut tmp_file = tempfile::spooled_tempfile(5 * 1024 * 1024);

    let mut data_size: usize = 0;

    {
        loop {
            let chunk = res.chunk().await;

            let result = match chunk {
                Ok(v) => v,
                Err(_) => return None,
            };

            let data = match result {
                Some(v) => v,
                None => break,
            };

            data_size += data.len();

            if data_size > max_bytes {
                return None;
            }

            match tmp_file.write_all(data.chunk()) {
                Ok(_) => (),
                Err(_) => return None,
            }
        }

        match tmp_file.seek(SeekFrom::Start(0)) {
            Ok(_) => (),
            Err(_) => return None,
        }
    }

    Some((tmp_file, data_size))
}

pub async fn response_to_download_data(
    mut response: Response,
    max_bytes: usize,
) -> Option<(Data, usize)> {
    if let Some(size) = parse_content_length(response.headers()) {
        return Some((Data::Response(response), size));
    }

    let (tmp_file, size) = response_to_tempfile(&mut response, max_bytes).await?;
    Some((
        Data::SpooledTempAsyncRead(SpooledTempAsyncRead::new(tmp_file)),
        size,
    ))
}
```

(The `parse_content_length` function and its `#[cfg(test)]` module below are unchanged.)

- [ ] **Step 4: Update `src/services/downloader/mod.rs` — thread `limits` through `download_chain` and its callers**

Change the `download_chain` signature and its four call sites to `response_to_tempfile`/`response_to_download_data`:

```rust
pub async fn download_chain(
    book: BookWithRemote,
    file_type: String,
    source_config: config::SourceConfig,
    converting: bool,
    normalized: bool,
    limits: config::DownloadLimits,
) -> Option<DownloadResult> {
    let final_need_zip = file_type == "fb2zip";

    let file_type_ = if converting {
        book.file_type.clone()
    } else {
        file_type.clone()
    };

    let (mut response, is_zip) = match download(&book.remote_id, &file_type_, &source_config).await
    {
        Some(v) => v,
        None => return None,
    };

    if is_zip && book.file_type.to_lowercase() == "html" {
        let filename = get_filename_by_book(&book, &file_type, true, false, normalized);
        let filename_ascii = get_filename_by_book(&book, &file_type, true, true, normalized);
        let (data, data_size) = match response_to_download_data(response, limits.max_download_bytes).await
        {
            Some(v) => v,
            None => return None,
        };

        return Some(DownloadResult::new(
            data,
            filename,
            filename_ascii,
            data_size,
        ));
    }

    if !is_zip && !final_need_zip && !converting {
        let filename = get_filename_by_book(&book, &book.file_type, false, false, normalized);
        let filename_ascii = get_filename_by_book(&book, &file_type, false, true, normalized);
        let (data, data_size) = match response_to_download_data(response, limits.max_download_bytes).await
        {
            Some(v) => v,
            None => return None,
        };

        return Some(DownloadResult::new(
            data,
            filename,
            filename_ascii,
            data_size,
        ));
    };

    let (unzipped_temp_file, data_size) = {
        let temp_file_to_unzip_result =
            response_to_tempfile(&mut response, limits.max_download_bytes).await;
        let temp_file_to_unzip = match temp_file_to_unzip_result {
            Some(v) => v.0,
            None => return None,
        };

        match unzip(temp_file_to_unzip, "fb2") {
            Some(v) => v,
            None => return None,
        }
    };

    let (mut clean_file, data_size) = if converting {
        match convert_file(unzipped_temp_file, file_type.to_string()).await {
            Some(mut response) => {
                match response_to_tempfile(&mut response, limits.max_download_bytes).await {
                    Some(v) => v,
                    None => return None,
                }
            }
            None => return None,
        }
    } else {
        (unzipped_temp_file, data_size)
    };

    if !final_need_zip {
        let t = SpooledTempAsyncRead::new(clean_file);
        let filename = get_filename_by_book(&book, &file_type, false, false, normalized);
        let filename_ascii = get_filename_by_book(&book, &file_type, false, true, normalized);

        return Some(DownloadResult::new(
            Data::SpooledTempAsyncRead(t),
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
    match zip(&mut clean_file, filename.as_str()) {
        Some((t_file, data_size)) => {
            let t = SpooledTempAsyncRead::new(t_file);
            let filename = get_filename_by_book(&book, &file_type, true, false, normalized);
            let filename_ascii = get_filename_by_book(&book, &file_type, true, true, normalized);

            Some(DownloadResult::new(
                Data::SpooledTempAsyncRead(t),
                filename,
                filename_ascii,
                data_size,
            ))
        }
        None => None,
    }
}
```

- [ ] **Step 5: Update `start_download_futures` to pass the configured limits**

(Note: the `unzip(temp_file_to_unzip, "fb2")` call inside `download_chain` above keeps its current 2-arg form for now — Task 3 changes it.)

```rust
pub async fn start_download_futures(
    book: &BookWithRemote,
    file_type: &str,
    normalized: bool,
) -> Option<DownloadResult> {
    let mut tasks = JoinSet::new();

    for source_config in &config::CONFIG.fl_sources {
        tasks.spawn(download_chain(
            book.clone(),
            file_type.to_string(),
            source_config.clone(),
            false,
            normalized,
            config::CONFIG.download_limits,
        ));

        if file_type == "epub" || file_type == "fb2" {
            tasks.spawn(download_chain(
                book.clone(),
                file_type.to_string(),
                source_config.clone(),
                true,
                normalized,
                config::CONFIG.download_limits,
            ));
        }
    }

    while let Some(task_result) = tasks.join_next().await {
        if let Ok(Some(task_result)) = task_result {
            return Some(task_result);
        }
    }

    None
}
```

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test --lib downloader`
Expected: `test result: ok.` — all tests in `downloader::tests` and `downloader::utils::tests` pass, including the new `oversized_body_without_content_length_is_rejected` test from Step 1.

- [ ] **Step 7: Commit**

```bash
git add src/services/downloader/utils.rs src/services/downloader/mod.rs
git commit -m "fix: cap buffered download size to prevent disk exhaustion from oversized mirror responses"
```

---

### Task 3: ZIP-bomb protection in `unzip` (Spec 03.2)

**Files:**
- Modify: `src/services/downloader/zip.rs`
- Modify: `src/services/downloader/mod.rs` — the single `unzip(...)` call site inside `download_chain`

**Interfaces:**
- Consumes: `limits: config::DownloadLimits` (already a `download_chain` parameter from Task 2).
- Produces (signature change): `pub fn unzip(tmp_file: SpooledTempFile, file_type: &str, max_decompressed_bytes: u64, max_compression_ratio: u64) -> Option<(SpooledTempFile, usize)>`.

- [ ] **Step 1: Update the test module in `src/services/downloader/zip.rs`**

Replace the `#[cfg(test)] mod tests` block with:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const GENEROUS_MAX_DECOMPRESSED: u64 = 5 * 1024 * 1024;
    const GENEROUS_MAX_RATIO: u64 = 1000;

    #[test]
    fn corrupt_zip_bytes_return_none_instead_of_panicking() {
        let mut tmp_file = tempfile::spooled_tempfile(1024);
        tmp_file.write_all(b"this is not a zip file").unwrap();
        tmp_file.rewind().unwrap();

        let result = unzip(tmp_file, "fb2", GENEROUS_MAX_DECOMPRESSED, GENEROUS_MAX_RATIO);

        assert!(result.is_none());
    }

    #[test]
    fn zip_then_unzip_round_trips_content() {
        let original = b"fb2 file contents";
        let mut input = tempfile::spooled_tempfile(1024);
        input.write_all(original).unwrap();
        input.rewind().unwrap();

        let (zipped, zipped_size) = zip(&mut input, "book.fb2").expect("zip should succeed");
        assert!(zipped_size > 0);

        let (mut unzipped, unzipped_size) =
            unzip(zipped, "fb2", GENEROUS_MAX_DECOMPRESSED, GENEROUS_MAX_RATIO)
                .expect("unzip should find the fb2 entry");
        assert_eq!(unzipped_size, original.len());

        let mut contents = Vec::new();
        std::io::Read::read_to_end(&mut unzipped, &mut contents).unwrap();
        assert_eq!(contents, original);
    }

    #[test]
    fn oversized_declared_entry_is_rejected() {
        let original = vec![b'a'; 2 * 1024 * 1024];
        let mut input = tempfile::spooled_tempfile(1024);
        input.write_all(&original).unwrap();
        input.rewind().unwrap();

        let (zipped, _) = zip(&mut input, "book.fb2").expect("zip should succeed");

        let result = unzip(zipped, "fb2", 1024 * 1024, u64::MAX);

        assert!(result.is_none());
    }

    #[test]
    fn high_compression_ratio_entry_is_rejected() {
        let original = vec![0u8; 2 * 1024 * 1024];
        let mut input = tempfile::spooled_tempfile(1024);
        input.write_all(&original).unwrap();
        input.rewind().unwrap();

        let (zipped, zipped_size) = zip(&mut input, "book.fb2").expect("zip should succeed");
        assert!(
            zipped_size < original.len() / 20,
            "test fixture must compress well beyond the ratio cap to be meaningful"
        );

        let result = unzip(zipped, "fb2", u64::MAX, 10);

        assert!(result.is_none());
    }
}
```

- [ ] **Step 2: Run tests to verify they fail to compile**

Run: `cargo test --lib downloader::zip`
Expected: FAIL — `this function takes 2 arguments but 4 arguments were supplied` (existing calls already updated to 4 args, but `unzip` itself still only accepts 2).

- [ ] **Step 3: Implement the cap in `unzip`**

Replace the `unzip` function (above `pub fn zip`) with:

```rust
use std::io::{Read, Seek};

use tempfile::SpooledTempFile;
use zip::write::FileOptions;

pub fn unzip(
    tmp_file: SpooledTempFile,
    file_type: &str,
    max_decompressed_bytes: u64,
    max_compression_ratio: u64,
) -> Option<(SpooledTempFile, usize)> {
    let mut archive = zip::ZipArchive::new(tmp_file).ok()?;

    let file_type_lower = file_type.to_lowercase();

    for i in 0..archive.len() {
        let mut file = archive.by_index(i).ok()?;
        let filename = file.name();

        if filename.contains(&file_type_lower) || file.name().to_lowercase() == "elector" {
            let declared_size = file.size();
            let compressed_size = file.compressed_size().max(1);

            if declared_size > max_decompressed_bytes {
                return None;
            }

            if declared_size / compressed_size > max_compression_ratio {
                return None;
            }

            let mut output_file = tempfile::spooled_tempfile(5 * 1024 * 1024);
            let mut limited = (&mut file).take(max_decompressed_bytes + 1);

            let size: usize = match std::io::copy(&mut limited, &mut output_file) {
                Ok(v) if v > max_decompressed_bytes => return None,
                Ok(v) => v.try_into().ok()?,
                Err(_) => return None,
            };

            output_file.rewind().ok()?;

            return Some((output_file, size));
        }
    }

    None
}
```

(Change the top `use std::io::Seek;` to `use std::io::{Read, Seek};` as shown — `Read` brings `.take()` into scope. `pub fn zip` below is unchanged.)

- [ ] **Step 4: Update the call site in `src/services/downloader/mod.rs`**

Change:

```rust
        match unzip(temp_file_to_unzip, "fb2") {
```

to:

```rust
        match unzip(
            temp_file_to_unzip,
            "fb2",
            limits.max_decompressed_bytes,
            limits.max_compression_ratio,
        ) {
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --lib downloader`
Expected: `test result: ok.` for `downloader::zip::tests` (5 tests) and all other `downloader::tests`/`downloader::utils::tests` (unaffected by this task).

- [ ] **Step 6: Commit**

```bash
git add src/services/downloader/zip.rs src/services/downloader/mod.rs
git commit -m "fix: reject oversized or high-ratio ZIP entries to prevent ZIP-bomb expansion"
```

---

### Task 4: Cap the client-facing pass-through stream at the declared `Content-Length` (Spec 03.3)

**Files:**
- Modify: `src/services/downloader/types.rs`
- Modify: `src/services/downloader/mod.rs` (test module only — one new/extended test)

**Interfaces:**
- Produces: private `fn limit_async_read<R: AsyncRead>(read: R, limit: u64) -> impl AsyncRead` in `types.rs`; `DownloadResult::get_async_read` behavior change (signature unchanged).

- [ ] **Step 1: Write the failing test in `src/services/downloader/types.rs`**

Add at the bottom of `src/services/downloader/types.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn limit_async_read_truncates_to_declared_length() {
        let data: &[u8] = b"HELLO WORLD EXTRA BYTES";
        let mut limited = limit_async_read(data, 5);

        let mut buf = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut limited, &mut buf)
            .await
            .unwrap();

        assert_eq!(buf, b"HELLO");
    }
}
```

- [ ] **Step 2: Run test to verify it fails to compile**

Run: `cargo test --lib downloader::types`
Expected: FAIL with `cannot find function 'limit_async_read' in this scope`.

- [ ] **Step 3: Add `limit_async_read` and wire it into `get_async_read`**

In `src/services/downloader/types.rs`, change the `tokio::io::AsyncRead` import to also bring in `AsyncReadExt`:

```rust
use tokio::io::{AsyncRead, AsyncReadExt};
```

Add the helper below `get_response_async_read`:

```rust
fn limit_async_read<R: AsyncRead>(read: R, limit: u64) -> impl AsyncRead {
    read.take(limit)
}
```

Update `DownloadResult::get_async_read`:

```rust
    pub fn get_async_read(self) -> Pin<Box<dyn AsyncRead + Send>> {
        let data_size = self.data_size as u64;

        match self.data {
            Data::Response(v) => {
                Box::pin(limit_async_read(get_response_async_read(v), data_size))
            }
            Data::SpooledTempAsyncRead(v) => Box::pin(v),
        }
    }
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib downloader::types`
Expected: `test result: ok. 1 passed`.

- [ ] **Step 5: Extend the existing end-to-end passthrough test to prove real streaming still works**

In `src/services/downloader/mod.rs`'s `mod tests`, replace `valid_content_length_streams_without_buffering` with:

```rust
    #[tokio::test]
    async fn valid_content_length_streams_without_buffering() {
        let body = b"fake fb2 content";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/fb2\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            std::str::from_utf8(body).unwrap()
        );
        let base_url = spawn_raw_server(response.into_bytes()).await;
        let source_config = make_source_config(base_url);
        let book = make_book("fb2");

        let result =
            download_chain(book, "fb2".to_string(), source_config, false, true, generous_limits())
                .await;

        let data = result.expect("download_chain should succeed with valid Content-Length");
        assert_eq!(data.data_size, body.len());
        assert!(matches!(data.data, Data::Response(_)));

        let mut reader = data.get_async_read();
        let mut buf = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut reader, &mut buf)
            .await
            .unwrap();
        assert_eq!(buf, body);
    }
```

- [ ] **Step 6: Run the full test suite**

Run: `cargo test --lib`
Expected: `test result: ok.` across every module — `config`, `downloader`, `downloader::types`, `downloader::utils`, `downloader::zip`, `filename_getter`.

- [ ] **Step 7: Verify the release build still compiles**

Run: `cargo build --release`
Expected: build succeeds with no errors.

- [ ] **Step 8: Commit**

```bash
git add src/services/downloader/types.rs src/services/downloader/mod.rs
git commit -m "fix: truncate client-facing pass-through stream at the declared Content-Length"
```

---

## Acceptance Criteria Checklist

- [x] Task 2: a mock mirror serving an oversized body (chunked, no `Content-Length`) causes `download_chain` to return `None` once the configured cap is exceeded; the copy loop aborts mid-stream instead of buffering the whole body.
- [x] Task 3: a ZIP fixture with a declared entry size over the cap, and separately one with a compression ratio over the cap, are both rejected by `unzip`, with dedicated tests (`oversized_declared_entry_is_rejected`, `high_compression_ratio_entry_is_rejected`).
- [x] Tasks 1–3: limits are configurable via `MAX_DOWNLOAD_BYTES`, `MAX_DECOMPRESSED_BYTES`, `MAX_COMPRESSION_RATIO` env vars with sane defaults, documented via doc comments on `config::DownloadLimits`.
- [x] Task 4: the client-facing pass-through response can never emit more than the declared `Content-Length`, verified by a unit test on the limiting primitive and an end-to-end streaming test.
