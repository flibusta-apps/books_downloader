use bytes::Buf;
use reqwest::Response;
use tempfile::SpooledTempFile;

use std::io::{Seek, SeekFrom, Write};

pub async fn response_to_tempfile(res: &mut Response) -> Option<(SpooledTempFile, usize)> {
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

            match tmp_file.write_all(data.chunk()) {
                Ok(_) => (),
                Err(_) => return None,
            }
        }

        tmp_file.seek(SeekFrom::Start(0)).unwrap();
    }

    Some((tmp_file, data_size))
}
