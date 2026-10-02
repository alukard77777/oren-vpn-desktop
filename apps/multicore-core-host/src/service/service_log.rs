//! `logs/service.log` in the service directory: why the service refused or failed a request.
//! The daemon only ever sees a `BrokerErrorCode`; without this file a failed `Prepare` on a
//! user's machine leaves no trace of which check refused it.
//!
//! The directory is SYSTEM/root-only, so «Сохранить лог…» gets the log through the service:
//! [`read_chunk`] serves it in redacted, frame-sized chunks of whole lines (`ServiceLog`).

use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use multicore_core::elevation_protocol::MAX_ELEVATION_FRAME_BYTES;
use multicore_core::service_protocol::{
    MAX_SERVICE_LOG_TAIL_BYTES, ServiceLogFile, ServiceResponse, encode_service_frame,
    redact_service_log,
};

/// Past this size the log moves to `service.log.1` (replacing the one before), so a failure
/// loop cannot fill the disk and the previous lines survive one rotation.
pub(crate) const MAX_SERVICE_LOG_BYTES: u64 = 256 * 1024;
/// The most raw bytes one `ServiceLog` chunk reads; smaller when the redacted chunk would not
/// fit a frame.
pub(crate) const SERVICE_LOG_CHUNK_BYTES: u64 = 16 * 1024;
const MIN_SERVICE_LOG_CHUNK_BYTES: u64 = 512;

fn log_path(service_directory: &Path, file: ServiceLogFile) -> PathBuf {
    service_directory.join("logs").join(file.file_name())
}

/// Appends one line (`<unix seconds> <message>`) to `<service_directory>/logs/service.log`.
/// Best effort: logging never turns a result into a different one.
pub(crate) fn append(service_directory: &Path, message: &str) {
    let directory = service_directory.join("logs");
    if fs::create_dir_all(&directory).is_err() {
        return;
    }
    let path = log_path(service_directory, ServiceLogFile::Current);
    if fs::metadata(&path).is_ok_and(|metadata| metadata.len() > MAX_SERVICE_LOG_BYTES)
        && fs::rename(&path, log_path(service_directory, ServiceLogFile::Previous)).is_err()
    {
        let _ = fs::remove_file(&path);
    }
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    let line = message.replace(['\r', '\n'], " ");
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(file, "{seconds} {line}");
    }
}

/// `ServiceLog{file, before}`: the redacted whole lines of `file` that end at `before` (its
/// end when `None`), at most [`SERVICE_LOG_CHUNK_BYTES`] of them and never further back than
/// [`MAX_SERVICE_LOG_TAIL_BYTES`] from the file's end. Only the two fixed files are read.
pub(crate) fn read_chunk(
    service_directory: &Path,
    file: ServiceLogFile,
    before: Option<u64>,
) -> ServiceResponse {
    let empty = |end| ServiceResponse::ServiceLog {
        file,
        start: end,
        end,
        text: String::new(),
    };
    let path = log_path(service_directory, file);
    // The service writes these files itself; anything else in their place is not read.
    if !fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.is_file()) {
        return empty(0);
    }
    let Ok(mut handle) = File::open(&path) else {
        return empty(0);
    };
    let Ok(length) = handle.metadata().map(|metadata| metadata.len()) else {
        return empty(0);
    };
    let end = before.map_or(length, |before| before.min(length));
    let floor = length.saturating_sub(MAX_SERVICE_LOG_TAIL_BYTES);
    if end <= floor {
        return empty(end);
    }
    let mut limit = SERVICE_LOG_CHUNK_BYTES;
    loop {
        let raw_start = end.saturating_sub(limit).max(floor);
        let mut bytes = Vec::new();
        let read = handle
            .seek(SeekFrom::Start(raw_start))
            .and_then(|_| (&mut handle).take(end - raw_start).read_to_end(&mut bytes));
        if read.is_err() {
            return empty(end);
        }
        // Start at a whole line, unless the line is longer than the chunk.
        let skip = if raw_start > 0 {
            bytes
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(0, |newline| newline + 1)
        } else {
            0
        };
        let skip = if skip >= bytes.len() { 0 } else { skip };
        let response = ServiceResponse::ServiceLog {
            file,
            start: raw_start + skip as u64,
            end: raw_start + bytes.len() as u64,
            text: redact_service_log(&String::from_utf8_lossy(&bytes[skip..])),
        };
        let fits = encode_service_frame(&response)
            .is_ok_and(|frame| frame.len() <= MAX_ELEVATION_FRAME_BYTES);
        if fits {
            return response;
        }
        if limit <= MIN_SERVICE_LOG_CHUNK_BYTES {
            return empty(end);
        }
        limit /= 2;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temporary_directory(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "multicore-service-log-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn appends_single_lines_and_rotates_past_the_cap() {
        let root = temporary_directory("append");
        append(&root, "prepare: first\nsecond");
        append(&root, "prepare: third");
        let path = root.join("logs").join("service.log");
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 2);
        assert!(
            text.lines()
                .next()
                .unwrap()
                .ends_with("prepare: first second")
        );
        fs::write(&path, vec![b'x'; MAX_SERVICE_LOG_BYTES as usize + 1]).unwrap();
        append(&root, "after cap");
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.ends_with("after cap\n"));
        assert!(text.len() < 100);
        let previous = root.join("logs").join("service.log.1");
        assert_eq!(
            fs::metadata(&previous).unwrap().len(),
            MAX_SERVICE_LOG_BYTES + 1,
            "the full log became the predecessor"
        );
        fs::remove_dir_all(&root).unwrap();
    }

    /// Pages a file back to front the way the daemon does.
    fn read_all(root: &Path, file: ServiceLogFile) -> (Vec<String>, u64) {
        let mut pieces = Vec::new();
        let mut before = None;
        let mut first_start = u64::MAX;
        for _ in 0..1000 {
            let ServiceResponse::ServiceLog {
                file: answered,
                start,
                end,
                text,
            } = read_chunk(root, file, before)
            else {
                panic!("not a service log chunk");
            };
            assert_eq!(answered, file);
            assert!(start <= end);
            if let Some(before) = before {
                assert_eq!(end, before);
            }
            let response = ServiceResponse::ServiceLog {
                file,
                start,
                end,
                text: text.clone(),
            };
            assert!(encode_service_frame(&response).unwrap().len() <= MAX_ELEVATION_FRAME_BYTES);
            pieces.push(text);
            first_start = start;
            if start == end {
                break;
            }
            before = Some(start);
        }
        pieces.reverse();
        (pieces, first_start)
    }

    #[test]
    fn chunks_are_whole_redacted_lines_within_a_frame_and_the_tail_cap() {
        let root = temporary_directory("chunks");
        fs::create_dir_all(root.join("logs")).unwrap();
        let mut log = String::new();
        let mut index = 0_u32;
        while log.len() < 700 * 1024 {
            log.push_str(&format!(
                "17000{index:05} prepare refused at C:\\Users\\alice\\x token=Zx9QpLm2Kd7Vb4Ns https://panel.example.net/sub \"quoted\\path\"\n"
            ));
            index += 1;
        }
        fs::write(root.join("logs").join("service.log"), &log).unwrap();
        let (pieces, first_start) = read_all(&root, ServiceLogFile::Current);
        let text = pieces.concat();
        assert!(
            pieces
                .iter()
                .all(|piece| piece.is_empty() || piece.ends_with('\n')),
            "every chunk is whole lines"
        );
        for leak in ["alice", "Zx9QpLm2Kd7Vb4Ns", "panel.example.net"] {
            assert!(!text.contains(leak), "leaks {leak}");
        }
        assert!(text.contains(&format!("17000{:05} prepare", index - 1)));
        assert!(!text.contains("1700000000 prepare"), "older than the cap");
        let length = log.len() as u64;
        assert!(first_start >= length - MAX_SERVICE_LOG_TAIL_BYTES);
        assert!(first_start <= length - MAX_SERVICE_LOG_TAIL_BYTES + 200);
        // Further back than the cap: nothing.
        assert_eq!(
            read_chunk(
                &root,
                ServiceLogFile::Current,
                Some(length - MAX_SERVICE_LOG_TAIL_BYTES)
            ),
            ServiceResponse::ServiceLog {
                file: ServiceLogFile::Current,
                start: length - MAX_SERVICE_LOG_TAIL_BYTES,
                end: length - MAX_SERVICE_LOG_TAIL_BYTES,
                text: String::new(),
            }
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_missing_file_is_empty_and_a_shrunk_file_says_so() {
        let root = temporary_directory("missing");
        assert_eq!(
            read_chunk(&root, ServiceLogFile::Previous, None),
            ServiceResponse::ServiceLog {
                file: ServiceLogFile::Previous,
                start: 0,
                end: 0,
                text: String::new(),
            }
        );
        append(&root, "only line");
        let ServiceResponse::ServiceLog {
            start, end, text, ..
        } = read_chunk(&root, ServiceLogFile::Current, Some(1 << 20))
        else {
            panic!("not a service log chunk");
        };
        assert_eq!(start, 0);
        assert!(end < 1 << 20, "the end shows the file is shorter");
        assert!(text.ends_with("only line\n"));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_line_longer_than_a_chunk_still_makes_progress() {
        let root = temporary_directory("long");
        fs::create_dir_all(root.join("logs")).unwrap();
        let long = "y".repeat(SERVICE_LOG_CHUNK_BYTES as usize * 2);
        fs::write(
            root.join("logs").join("service.log"),
            format!("1 first\n2 {long}\n3 last\n"),
        )
        .unwrap();
        let (pieces, first_start) = read_all(&root, ServiceLogFile::Current);
        assert_eq!(first_start, 0);
        assert!(pieces.concat().starts_with("1 first\n"));
        assert!(pieces.concat().ends_with("3 last\n"));
        fs::remove_dir_all(&root).unwrap();
    }
}
