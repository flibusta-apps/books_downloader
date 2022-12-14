use std::io::{Seek, SeekFrom};

use tempfile::SpooledTempFile;
use zip::write::FileOptions;


pub fn unzip(tmp_file: SpooledTempFile, file_type: &str) -> Option<SpooledTempFile> {
    let mut archive = zip::ZipArchive::new(tmp_file).unwrap();

    let file_type_lower = file_type.to_lowercase();

    for i in 0..archive.len() {
        let mut file = archive.by_index(i).unwrap();
        let filename = file.name();

        if filename.contains(&file_type_lower) || file.name().to_lowercase() == "elector" {
            let mut output_file = tempfile::spooled_tempfile(5 * 1024 * 1024);

            match std::io::copy(&mut file, &mut output_file) {
                Ok(_) => (),
                Err(_) => return None,
            };

            output_file.seek(SeekFrom::Start(0)).unwrap();

            return Some(output_file);
        }
    }

    return None;
}

pub fn zip(tmp_file: &mut SpooledTempFile, filename: &str) -> Option<SpooledTempFile> {
    let output_file = tempfile::spooled_tempfile(5 * 1024 * 1024);
    let mut archive = zip::ZipWriter::new(output_file);

    let options = FileOptions::default()
        .compression_level(Some(9))
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o755);

    match archive.start_file(filename, options) {
        Ok(_) => (),
        Err(_) => return None,
    };

    match std::io::copy(tmp_file, &mut archive) {
        Ok(_) => (),
        Err(_) => return None,
    };

    let mut archive_result = match archive.finish() {
        Ok(v) => v,
        Err(_) => return None,
    };

    archive_result.seek(SeekFrom::Start(0)).unwrap();

    Some(archive_result)
}