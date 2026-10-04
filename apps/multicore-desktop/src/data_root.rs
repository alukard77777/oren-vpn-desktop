//! The user's MULTICORE data directory (`%LOCALAPPDATA%\MultiCore`, `~/.local/share/multicore`)
//! for every path of the desktop app: logs, preferences, updates, markers, the data reset.
//!
//! Unit tests replay failing daemon exchanges, panics and crashes through the very code that
//! records them, so under `cfg(test)` this is a per-process directory in the OS temp directory
//! instead: no test ever reads, writes or removes the real user's data. Nothing else in the
//! app asks `multicore_platform` for the directory (a test below enforces that).

use std::io;
use std::path::PathBuf;

#[cfg(not(test))]
pub(crate) fn user_data_root() -> io::Result<PathBuf> {
    multicore_platform::dirs::user_data_root()
}

#[cfg(test)]
pub(crate) fn user_data_root() -> io::Result<PathBuf> {
    Ok(std::env::temp_dir().join(format!("multicore-desktop-test-{}", std::process::id())))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real data directory, which tests must never touch.
    fn real_data_root() -> Option<PathBuf> {
        multicore_platform::dirs::user_data_root().ok()
    }

    #[test]
    fn tests_resolve_a_temporary_directory_outside_the_real_data_root() {
        let root = user_data_root().expect("test data root");
        assert!(root.starts_with(std::env::temp_dir()), "{root:?}");
        if let Some(real) = real_data_root() {
            assert!(!root.starts_with(&real), "{root:?} is inside {real:?}");
            assert!(!real.starts_with(&root), "{real:?} is inside {root:?}");
        }
    }

    /// Every desktop source asks this module, never `multicore_platform`, for the directory,
    /// so a new path cannot bypass the test redirection.
    #[test]
    fn no_desktop_source_resolves_the_real_data_root_directly() {
        let sources = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let forbidden = concat!("multicore_platform::dirs::", "user_data_root");
        let mut offenders = Vec::new();
        for entry in std::fs::read_dir(&sources).expect("desktop sources") {
            let path = entry.expect("source entry").path();
            if path.extension().is_none_or(|extension| extension != "rs")
                || path.file_name().is_some_and(|name| name == "data_root.rs")
            {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("read source");
            if text.contains(forbidden) {
                offenders.push(path);
            }
        }
        assert!(offenders.is_empty(), "{offenders:?} bypass data_root");
    }

    /// The low-level log, the crash log and a daemon exchange that fails the way the fixtures
    /// do: none of them may reach the real user's `logs/`.
    #[test]
    fn a_test_run_never_writes_the_real_user_logs() {
        let probe = format!(
            "real_log_guard_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        crate::daemon::record_low_level(&probe, "test only");
        crate::crash_log::record_event_loop_exit(&probe, "test only");
        // A daemon answering garbage: the client records the decode failure.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("address");
        let body = format!(r#"{{"events":"{probe}"}}"#);
        let server = std::thread::spawn(move || {
            use std::io::{Read as _, Write as _};
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buffer = [0_u8; 2048];
            let _ = stream.read(&mut buffer);
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        });
        let client = crate::daemon::HttpDaemonClient::new(format!("http://{address}"), "token")
            .expect("client");
        let _ = crate::daemon::DaemonClient::events(&client, 0, None);
        server.join().expect("server");

        let test_logs = user_data_root().expect("test root").join("logs");
        let written = std::fs::read_to_string(test_logs.join("client-errors.log"))
            .expect("the test log received the lines");
        assert!(written.contains(&probe));
        if let Some(real) = real_data_root() {
            for name in ["client-errors.log", "crash.log"] {
                let real_log = real.join("logs").join(name);
                let text = std::fs::read_to_string(&real_log).unwrap_or_default();
                assert!(!text.contains(&probe), "a test wrote into {real_log:?}");
            }
        }
    }
}
