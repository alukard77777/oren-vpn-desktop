//! Receiving a release asset of a known size, with progress for the update UI.
//!
//! The size comes from the verified release (GitHub's asset metadata), never from the server
//! that sends the bytes: a download is exactly that long or it fails.

use std::io::{ErrorKind, Read, Write};

use thiserror::Error;

/// How far a download of `total` bytes got.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DownloadProgress {
    pub received: u64,
    pub total: u64,
}

impl DownloadProgress {
    /// Whole percent received, rounded down (0 for an unknown or empty total, at most 100).
    #[must_use]
    pub fn percent(self) -> u8 {
        if self.total == 0 {
            return 0;
        }
        let percent = u128::from(self.received) * 100 / u128::from(self.total);
        u8::try_from(percent.min(100)).unwrap_or(100)
    }
}

/// Why [`receive_exact`] stopped.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum DownloadError {
    #[error("the download was interrupted")]
    Read,
    #[error("the download could not be written")]
    Write,
    #[error("the download ended before the declared size")]
    Short,
    #[error("the download is larger than the declared size")]
    Long,
}

/// Copies exactly `total` bytes from `reader` to `writer`, then checks that nothing follows.
///
/// `progress` hears the start (0 %), every change of the whole percent and the end (100 %):
/// at most 101 calls however the bytes arrive, so a UI can redraw on every call without
/// throttling. Nothing is reported after a failure.
pub fn receive_exact(
    reader: &mut impl Read,
    writer: &mut impl Write,
    total: u64,
    mut progress: impl FnMut(DownloadProgress),
) -> Result<(), DownloadError> {
    let mut current = DownloadProgress { received: 0, total };
    progress(current);
    let mut reported = current.percent();
    let mut buffer = vec![0_u8; 64 * 1024];
    while current.received < total {
        let left = total - current.received;
        let limit = buffer
            .len()
            .min(usize::try_from(left).unwrap_or(usize::MAX));
        let read = match reader.read(&mut buffer[..limit]) {
            Ok(0) => return Err(DownloadError::Short),
            Ok(read) => read,
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(_) => return Err(DownloadError::Read),
        };
        writer
            .write_all(&buffer[..read])
            .map_err(|_| DownloadError::Write)?;
        current.received += read as u64;
        let percent = current.percent();
        if percent != reported && current.received < total {
            reported = percent;
            progress(current);
        }
    }
    let mut extra = [0_u8; 1];
    loop {
        match reader.read(&mut extra) {
            Ok(0) => break,
            Ok(_) => return Err(DownloadError::Long),
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(_) => return Err(DownloadError::Read),
        }
    }
    writer.flush().map_err(|_| DownloadError::Write)?;
    if total > 0 {
        progress(current);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{DownloadError, DownloadProgress, receive_exact};
    use std::io::{self, Read};

    /// Hands out at most `chunk` bytes per read, like a network stream.
    struct Trickle {
        data: Vec<u8>,
        position: usize,
        chunk: usize,
        interrupt_first: bool,
    }

    impl Read for Trickle {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if self.interrupt_first {
                self.interrupt_first = false;
                return Err(io::Error::from(io::ErrorKind::Interrupted));
            }
            let end = (self.position + self.chunk.min(buffer.len())).min(self.data.len());
            let read = end - self.position;
            buffer[..read].copy_from_slice(&self.data[self.position..end]);
            self.position = end;
            Ok(read)
        }
    }

    fn trickle(len: usize, chunk: usize) -> Trickle {
        Trickle {
            data: (0..len).map(|index| (index % 251) as u8).collect(),
            position: 0,
            chunk,
            interrupt_first: true,
        }
    }

    #[test]
    fn percent_rounds_down_and_never_passes_one_hundred() {
        let at = |received, total| DownloadProgress { received, total }.percent();
        assert_eq!(at(0, 0), 0);
        assert_eq!(at(5, 0), 0);
        assert_eq!(at(0, 52), 0);
        assert_eq!(at(18, 52), 34);
        assert_eq!(at(51, 52), 98);
        assert_eq!(at(52, 52), 100);
        assert_eq!(at(60, 52), 100);
        assert_eq!(at(u64::MAX / 2, u64::MAX), 49);
    }

    #[test]
    fn progress_is_reported_once_per_whole_percent_from_start_to_end() {
        let mut source = trickle(1_000_003, 7_919);
        let mut output = Vec::new();
        let mut seen = Vec::new();
        receive_exact(&mut source, &mut output, 1_000_003, |progress| {
            seen.push(progress)
        })
        .expect("download");
        assert_eq!(output, source.data);
        assert_eq!(seen.first().map(|first| first.received), Some(0));
        assert_eq!(
            seen.last().copied(),
            Some(DownloadProgress {
                received: 1_000_003,
                total: 1_000_003
            })
        );
        assert!(seen.len() <= 101, "{} reports", seen.len());
        assert!(seen.windows(2).all(|pair| {
            pair[0].received < pair[1].received && pair[0].percent() < pair[1].percent()
        }));
        assert!(seen.iter().all(|progress| progress.total == 1_000_003));
    }

    #[test]
    fn one_large_read_reports_only_the_start_and_the_end() {
        let mut source = trickle(4_096, usize::MAX);
        let mut seen = Vec::new();
        receive_exact(&mut source, &mut Vec::new(), 4_096, |progress| {
            seen.push(progress.percent())
        })
        .expect("download");
        assert_eq!(seen, [0, 100]);
    }

    #[test]
    fn a_short_or_long_download_fails_without_reporting_the_end() {
        let mut seen = Vec::new();
        let short = receive_exact(&mut trickle(900, 100), &mut Vec::new(), 1_000, |progress| {
            seen.push(progress.percent())
        });
        assert_eq!(short, Err(DownloadError::Short));
        assert!(!seen.contains(&100));

        let mut seen = Vec::new();
        let long = receive_exact(
            &mut trickle(1_001, 100),
            &mut Vec::new(),
            1_000,
            |progress| seen.push(progress.percent()),
        );
        assert_eq!(long, Err(DownloadError::Long));
        assert!(!seen.contains(&100));
    }

    #[test]
    fn read_and_write_failures_are_told_apart() {
        struct Broken;
        impl Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::from(io::ErrorKind::ConnectionReset))
            }
        }
        assert_eq!(
            receive_exact(&mut Broken, &mut Vec::new(), 10, |_| {}),
            Err(DownloadError::Read)
        );
        let mut full = [0_u8; 4];
        assert_eq!(
            receive_exact(
                &mut trickle(10, 10),
                &mut io::Cursor::new(&mut full[..]),
                10,
                |_| {}
            ),
            Err(DownloadError::Write)
        );
    }
}
