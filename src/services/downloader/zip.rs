use std::io::Seek;

use tempfile::SpooledTempFile;
use zip::write::FileOptions;

pub fn unzip(tmp_file: SpooledTempFile, file_type: &str) -> Option<(SpooledTempFile, usize)> {
    let mut archive = zip::ZipArchive::new(tmp_file).ok()?;

    let file_type_lower = file_type.to_lowercase();

    for i in 0..archive.len() {
        let mut file = archive.by_index(i).ok()?;
        let filename = file.name();

        if filename.contains(&file_type_lower) || file.name().to_lowercase() == "elector" {
            let mut output_file = tempfile::spooled_tempfile(5 * 1024 * 1024);

            let size: usize = match std::io::copy(&mut file, &mut output_file) {
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

    #[test]
    fn corrupt_zip_bytes_return_none_instead_of_panicking() {
        let mut tmp_file = tempfile::spooled_tempfile(1024);
        tmp_file.write_all(b"this is not a zip file").unwrap();
        tmp_file.rewind().unwrap();

        let result = unzip(tmp_file, "fb2");

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
            unzip(zipped, "fb2").expect("unzip should find the fb2 entry");
        assert_eq!(unzipped_size, original.len());

        let mut contents = Vec::new();
        std::io::Read::read_to_end(&mut unzipped, &mut contents).unwrap();
        assert_eq!(contents, original);
    }
}
