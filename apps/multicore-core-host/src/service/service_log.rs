//! `logs/service.log` in the service directory: why the service refused or failed a request.
//! The daemon only ever sees a `BrokerErrorCode`; without this file a failed `Prepare` on a
//! user's machine leaves no trace of which check refused it.

use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

/// Past this size the log is started over, so a failure loop cannot fill the disk.
pub(crate) const MAX_SERVICE_LOG_BYTES: u64 = 256 * 1024;

/// Appends one line (`<unix seconds> <message>`) to `<service_directory>/logs/service.log`.
/// Best effort: logging never turns a result into a different one.
pub(crate) fn append(service_directory: &Path, message: &str) {
    let directory = service_directory.join("logs");
    if fs::create_dir_all(&directory).is_err() {
        return;
    }
    let path = directory.join("service.log");
    if fs::metadata(&path).is_ok_and(|metadata| metadata.len() > MAX_SERVICE_LOG_BYTES) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_single_lines_and_starts_over_past_the_cap() {
        let root = std::env::temp_dir().join(format!(
            "multicore-service-log-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
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
        fs::remove_dir_all(&root).unwrap();
    }
}
