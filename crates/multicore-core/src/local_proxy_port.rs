//! The loopback port of MULTICORE's own Mihomo mixed (HTTP + SOCKS) listener.
//!
//! It is chosen once, saved in the user's data directory and kept across reconnects and
//! restarts, so other apps can be pointed at `127.0.0.1:<port>` for good. Another port is
//! taken (and saved) only when the saved one is busy.

use std::{
    fs, io,
    net::TcpListener,
    path::{Path, PathBuf},
    sync::Mutex,
};

/// The port tried first: not the default of any common proxy client (7890 Clash, 7897 Clash
/// Verge, 2080 NekoRay/sing-box, 10808/10809 v2rayN), so it rarely collides.
pub const DEFAULT_LOCAL_PROXY_PORT: u16 = 10780;

/// The file in the data directory that holds the port, as decimal text.
pub const LOCAL_PROXY_PORT_FILE: &str = "local-proxy-port";

/// The saved port in `data_directory`, if any.
pub fn read_local_proxy_port(data_directory: &Path) -> Option<u16> {
    fs::read_to_string(data_directory.join(LOCAL_PROXY_PORT_FILE))
        .ok()?
        .trim()
        .parse::<u16>()
        .ok()
        .filter(|port| *port != 0)
}

/// Whether a listener can be opened on `127.0.0.1:port` right now.
fn bindable(port: u16) -> bool {
    TcpListener::bind(("127.0.0.1", port)).is_ok()
}

fn any_free_port() -> io::Result<u16> {
    TcpListener::bind(("127.0.0.1", 0))?
        .local_addr()
        .map(|address| address.port())
}

pub struct LocalProxyPort {
    path: PathBuf,
    preferred: u16,
    saved: Mutex<Option<u16>>,
}

impl LocalProxyPort {
    pub fn open(data_directory: &Path) -> Self {
        Self::with_preferred(data_directory, DEFAULT_LOCAL_PROXY_PORT)
    }

    pub fn with_preferred(data_directory: &Path, preferred: u16) -> Self {
        Self {
            path: data_directory.join(LOCAL_PROXY_PORT_FILE),
            preferred,
            saved: Mutex::new(read_local_proxy_port(data_directory)),
        }
    }

    /// The port to stage a generation with. A saved port is kept even while something listens
    /// on it: that is normally our own Mihomo of the previous generation, which is stopped
    /// before the next one starts. Without one, the preferred port if it is free, otherwise
    /// any free port; either is saved.
    pub fn current(&self) -> io::Result<u16> {
        let mut saved = self
            .saved
            .lock()
            .map_err(|_| io::Error::other("poisoned"))?;
        if let Some(port) = *saved {
            return Ok(port);
        }
        let port = if bindable(self.preferred) {
            self.preferred
        } else {
            any_free_port()?
        };
        self.save(&mut saved, port)?;
        Ok(port)
    }

    /// The port to use when none of our cores can be holding it (they are stopped, or the
    /// last start failed): the saved port if it can be bound now, otherwise a free one, saved.
    pub fn verified(&self) -> io::Result<u16> {
        let port = self.current()?;
        if bindable(port) {
            return Ok(port);
        }
        let mut saved = self
            .saved
            .lock()
            .map_err(|_| io::Error::other("poisoned"))?;
        let port = any_free_port()?;
        self.save(&mut saved, port)?;
        Ok(port)
    }

    fn save(&self, saved: &mut Option<u16>, port: u16) -> io::Result<()> {
        *saved = Some(port);
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let staging = self.path.with_extension("tmp");
        fs::write(&staging, port.to_string())?;
        fs::rename(&staging, &self.path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn directory() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    /// A port nothing listens on right now.
    fn free_port() -> u16 {
        any_free_port().unwrap()
    }

    #[test]
    fn the_first_port_is_the_preferred_one_and_is_saved() {
        let data = directory();
        let preferred = free_port();
        let port = LocalProxyPort::with_preferred(data.path(), preferred);
        assert_eq!(port.current().unwrap(), preferred);
        assert_eq!(read_local_proxy_port(data.path()), Some(preferred));
        // A restart reads it back.
        let restarted = LocalProxyPort::with_preferred(data.path(), free_port());
        assert_eq!(restarted.current().unwrap(), preferred);
    }

    #[test]
    fn a_busy_preferred_port_falls_back_to_a_free_one_that_is_saved() {
        let data = directory();
        let taken = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let preferred = taken.local_addr().unwrap().port();
        let port = LocalProxyPort::with_preferred(data.path(), preferred);
        let chosen = port.current().unwrap();
        assert_ne!(chosen, preferred);
        assert_eq!(read_local_proxy_port(data.path()), Some(chosen));
        assert_eq!(port.current().unwrap(), chosen);
    }

    #[test]
    fn the_saved_port_is_kept_while_our_own_core_may_hold_it() {
        let data = directory();
        let port = LocalProxyPort::with_preferred(data.path(), free_port());
        let saved = port.current().unwrap();
        let _our_previous_mihomo = TcpListener::bind(("127.0.0.1", saved)).unwrap();
        assert_eq!(port.current().unwrap(), saved);
    }

    #[test]
    fn a_saved_port_another_app_took_is_replaced_once_ours_are_stopped() {
        let data = directory();
        let port = LocalProxyPort::with_preferred(data.path(), free_port());
        let saved = port.current().unwrap();
        // Free: kept.
        assert_eq!(port.verified().unwrap(), saved);
        let _other_app = TcpListener::bind(("127.0.0.1", saved)).unwrap();
        let replaced = port.verified().unwrap();
        assert_ne!(replaced, saved);
        assert_eq!(read_local_proxy_port(data.path()), Some(replaced));
        assert_eq!(port.current().unwrap(), replaced);
    }

    #[test]
    fn an_unreadable_saved_port_counts_as_none() {
        let data = directory();
        for text in ["", "0", "70000", "port"] {
            fs::write(data.path().join(LOCAL_PROXY_PORT_FILE), text).unwrap();
            assert_eq!(read_local_proxy_port(data.path()), None, "{text:?}");
        }
        fs::write(data.path().join(LOCAL_PROXY_PORT_FILE), " 2081\n").unwrap();
        assert_eq!(read_local_proxy_port(data.path()), Some(2081));
    }
}
