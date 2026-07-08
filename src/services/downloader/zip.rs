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
            let mut limited = (&mut file).take(max_decompressed_bytes.saturating_add(1));

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

pub fn zip(tmp_file: &mut SpooledTempFile, filename: &str) -> Option<(SpooledTempFile, usize)> {
    let output_file = tempfile::spooled_tempfile(5 * 1024 * 1024);
    let mut archive = zip::ZipWriter::new(output_file);

    let options: FileOptions<_> = FileOptions::default()
        .compression_level(Some(9))
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o755);

    archive.start_file::<&str, ()>(filename, options).ok()?;

    std::io::copy(tmp_file, &mut archive).ok()?;

    let mut archive_result = archive.finish().ok()?;

    let data_size: usize = archive_result.stream_position().ok()?.try_into().ok()?;

    archive_result.rewind().ok()?;

    Some((archive_result, data_size))
}

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

        let result = unzip(
            tmp_file,
            "fb2",
            GENEROUS_MAX_DECOMPRESSED,
            GENEROUS_MAX_RATIO,
        );

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
