use std::io::Seek;

use tempfile::SpooledTempFile;
use zip::write::FileOptions;


pub fn unzip(tmp_file: SpooledTempFile, file_type: &str) -> Option<(SpooledTempFile, usize)> {
    let mut archive = zip::ZipArchive::new(tmp_file).unwrap();

    let file_type_lower = file_type.to_lowercase();

    for i in 0..archive.len() {
        let mut file = archive.by_index(i).unwrap();
        let filename = file.name();

        if filename.contains(&file_type_lower) || file.name().to_lowercase() == "elector" {
            let mut output_file = tempfile::spooled_tempfile(5 * 1024 * 1024);

            let size: usize = match std::io::copy(&mut file, &mut output_file) {
                Ok(v) => v.try_into().unwrap(),
                Err(_) => return None,
            };

            output_file.rewind().unwrap();

            return Some((output_file, size));
        }
    }

    None
}

pub fn zip(tmp_file: &mut SpooledTempFile, filename: &str) -> Option<(SpooledTempFile, usize)> {
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

    let data_size: usize = archive_result.stream_position().unwrap().try_into().unwrap();

    archive_result.rewind().unwrap();

    Some((archive_result, data_size))
}