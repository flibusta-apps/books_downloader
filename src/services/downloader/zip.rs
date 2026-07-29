use std::io::{Read, Seek};

use tempfile::SpooledTempFile;
use zip::write::FileOptions;

use super::error::DownloadError;

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

/// Returns true when `file_type` names a format that is already compressed
/// (zip/epub), so the ZIP entry should be stored rather than re-deflated.
pub fn is_precompressed_file_type(file_type: &str) -> bool {
    let lower = file_type.to_lowercase();
    lower == "zip" || lower == "epub"
}

pub fn zip<R: std::io::Read>(
    mut source: R,
    filename: &str,
    compression_level: i64,
    stored: bool,
) -> Result<(SpooledTempFile, usize), DownloadError> {
    let output_file = tempfile::spooled_tempfile(5 * 1024 * 1024);
    let mut archive = zip::ZipWriter::new(output_file);

    let options: FileOptions<_> = if stored {
        FileOptions::default()
            .compression_method(zip::CompressionMethod::Stored)
            .unix_permissions(0o755)
    } else {
        FileOptions::default()
            .compression_level(Some(compression_level))
            .compression_method(zip::CompressionMethod::Deflated)
            .unix_permissions(0o755)
    };

    archive
        .start_file::<&str, ()>(filename, options)
        .map_err(|_| DownloadError::Internal("failed to start zip entry".to_string()))?;

    std::io::copy(&mut source, &mut archive)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const GENEROUS_MAX_DECOMPRESSED: u64 = 5 * 1024 * 1024;
    const GENEROUS_MAX_RATIO: u64 = 1000;

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
        assert!(result.is_err());
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

    #[test]
    fn corrupt_zip_bytes_return_none_instead_of_panicking() {
        let mut tmp_file = tempfile::spooled_tempfile(1024);
        tmp_file.write_all(b"this is not a zip file").unwrap();
        tmp_file.rewind().unwrap();

        let result = unzip(
            tmp_file,
            "fb2",
            GENEROUS_MAX_DECOMPRESSED,
            GENEROUS_MAX_RATIO,
        );

        assert!(result.is_err());
    }

    #[test]
    fn zip_then_unzip_round_trips_content() {
        let original = b"fb2 file contents";
        let mut input = tempfile::spooled_tempfile(1024);
        input.write_all(original).unwrap();
        input.rewind().unwrap();

        let (zipped, zipped_size) = zip(input, "book.fb2", 6, false).expect("zip should succeed");
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

        let (zipped, _) = zip(input, "book.fb2", 6, false).expect("zip should succeed");

        let result = unzip(zipped, "fb2", 1024 * 1024, u64::MAX);

        assert!(result.is_err());
    }

    #[test]
    fn high_compression_ratio_entry_is_rejected() {
        let original = vec![0u8; 2 * 1024 * 1024];
        let mut input = tempfile::spooled_tempfile(1024);
        input.write_all(&original).unwrap();
        input.rewind().unwrap();

        let (zipped, zipped_size) = zip(input, "book.fb2", 6, false).expect("zip should succeed");
        assert!(
            zipped_size < original.len() / 20,
            "test fixture must compress well beyond the ratio cap to be meaningful"
        );

        let result = unzip(zipped, "fb2", u64::MAX, 10);

        assert!(result.is_err());
    }
}
