//! Per-connection core logs for «Сохранить лог…».
//!
//! Two pieces, written by different processes and joined only when an archive is made:
//!
//! - The **core journal**: every line Mihomo and Xray print, as
//!   `<unix ms>\t<engine>\t<stream>\t<message>` (redacted by `CoreLogBuffer::push` like the
//!   live log), appended to `core-<first unix ms>.log` segments in a `core-sessions` directory
//!   by whichever process runs the cores: the daemon (system-proxy mode) and the on-demand
//!   elevated host write `<data>/logs/core-sessions`; the always-ready service writes its own
//!   per-user `<service>/users/<id>/core-sessions` and serves it to that user only, over the
//!   pipe (`CoreJournal`). Writes go straight to the OS (no fsync, no per-line flush of a
//!   user-space buffer), so the newest lines are readable at once and survive a crash of the
//!   writer. A segment ends at [`JOURNAL_SEGMENT_BYTES`]; segments past
//!   [`JOURNAL_MAX_AGE`] or beyond [`JOURNAL_TOTAL_BYTES`] in all are deleted, oldest first.
//!   Only an unprivileged writer in its own directory deletes (the daemon for the user's
//!   directory, the service for its own); the elevated host only ever creates new files.
//! - The **session index** `sessions.jsonl` next to the user's journal, written by the daemon:
//!   a `start` record when a connection is attempted (unix ms, TUN or system proxy, through the
//!   service or not, the cores, a short hash of the profile id) and an `end` record when it
//!   stops (unix ms and why). No URLs, names or secrets.
//!
//! An archive lists the sessions from the index and gives each the journal lines of its time
//! window ([`session_windows`]).

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use crate::service_protocol::CoreJournalCursor;

/// The directory holding a journal (and, in the user's data directory, the index).
pub const CORE_SESSIONS_DIRECTORY: &str = "core-sessions";
/// The session index inside [`CORE_SESSIONS_DIRECTORY`].
pub const SESSION_INDEX_FILE: &str = "sessions.jsonl";
/// A journal segment is closed and a new one started past this size.
pub const JOURNAL_SEGMENT_BYTES: u64 = 8 * 1024 * 1024;
/// The most one journal directory keeps; the oldest segments go first.
pub const JOURNAL_TOTAL_BYTES: u64 = 64 * 1024 * 1024;
/// Segments not written for this long are deleted, and sessions this old leave the index.
pub const JOURNAL_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);
/// The most of one session's core log an archive holds (its newest lines).
pub const MAX_SESSION_LOG_BYTES: usize = 16 * 1024 * 1024;
/// A session's window starts this long before its connect: the cores' start-up lines (a
/// warm always-ready generation is prepared shortly before).
pub const SESSION_LEAD_MS: u64 = 60_000;
/// …and ends this long after its disconnect: the cores' last words while stopping.
pub const SESSION_TAIL_MS: u64 = 10_000;
/// Sessions the index keeps at most (the newest).
const MAX_INDEXED_SESSIONS: usize = 2_000;
/// The most of the index file that is read (its newest records).
const MAX_INDEX_BYTES: u64 = 4 * 1024 * 1024;

const SEGMENT_PREFIX: &str = "core-";
const SEGMENT_SUFFIX: &str = ".log";

#[must_use]
pub fn unix_ms_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

/// One journal line: `<unix ms>\t<engine>\t<stream>\t<message>\n`, with tabs and line breaks
/// in the message flattened to spaces (the format of `latest-core.log` too).
#[must_use]
pub fn format_core_line(timestamp_ms: u64, engine: &str, stream: &str, message: &str) -> String {
    let message: String = message
        .chars()
        .map(|character| match character {
            '\r' | '\n' | '\t' => ' ',
            other => other,
        })
        .collect();
    format!("{timestamp_ms}\t{engine}\t{stream}\t{message}\n")
}

/// The unix ms a journal line starts with.
#[must_use]
pub fn core_line_timestamp(line: &str) -> Option<u64> {
    let (stamp, _) = line.split_once('\t')?;
    if stamp.is_empty() || !stamp.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    stamp.parse().ok()
}

/// `core-<start_ms>.log`.
#[must_use]
pub fn segment_name(start_ms: u64) -> String {
    format!("{SEGMENT_PREFIX}{start_ms}{SEGMENT_SUFFIX}")
}

fn segment_start(name: &str) -> Option<u64> {
    let digits = name
        .strip_prefix(SEGMENT_PREFIX)?
        .strip_suffix(SEGMENT_SUFFIX)?;
    if digits.is_empty() || digits.len() > 20 || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// The core journal writer of one process (see the module documentation). Never fails a
/// caller: a write error disables it for the rest of the process's run.
#[derive(Debug)]
pub struct CoreJournal {
    directory: PathBuf,
    prune: bool,
    segment_limit: u64,
    segment: Option<OpenSegment>,
    disabled: bool,
}

#[derive(Debug)]
struct OpenSegment {
    file: File,
    written: u64,
}

impl CoreJournal {
    /// A journal in `directory`, created with its first line. `prune`: this writer may delete
    /// old segments there (only an unprivileged writer in its own directory).
    #[must_use]
    pub fn new(directory: impl Into<PathBuf>, prune: bool) -> Self {
        Self {
            directory: directory.into(),
            prune,
            segment_limit: JOURNAL_SEGMENT_BYTES,
            segment: None,
            disabled: false,
        }
    }

    #[cfg(test)]
    fn with_segment_limit(mut self, limit: u64) -> Self {
        self.segment_limit = limit;
        self
    }

    /// Appends one whole line (as [`format_core_line`] makes it).
    pub fn append(&mut self, line: &str) {
        if self.disabled {
            return;
        }
        let bytes = line.as_bytes();
        let result = (|| -> io::Result<()> {
            let full = self.segment.as_ref().is_none_or(|segment| {
                segment.written > 0
                    && segment.written.saturating_add(bytes.len() as u64) > self.segment_limit
            });
            if full {
                self.open_segment()?;
            }
            let segment = self
                .segment
                .as_mut()
                .ok_or_else(|| io::Error::other("no journal segment"))?;
            segment.file.write_all(bytes)?;
            segment.written = segment.written.saturating_add(bytes.len() as u64);
            Ok(())
        })();
        if result.is_err() {
            self.segment = None;
            self.disabled = true;
        }
    }

    fn open_segment(&mut self) -> io::Result<()> {
        self.segment = None;
        fs::create_dir_all(&self.directory)?;
        let now = unix_ms_now();
        // A new name every time: never reopen (or follow) something already there.
        for start in now..now.saturating_add(64) {
            let path = self.directory.join(segment_name(start));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => {
                    self.segment = Some(OpenSegment { file, written: 0 });
                    if self.prune {
                        prune_journal(&self.directory, now, Some(&path));
                    }
                    return Ok(());
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::other("no free journal segment name"))
    }
}

/// One `core-<start_ms>.log` of a journal.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JournalSegment {
    pub start_ms: u64,
    pub path: PathBuf,
    pub len: u64,
    pub modified_ms: u64,
}

/// The journal's segments, oldest first. Only regular files named like a segment.
#[must_use]
pub fn journal_segments(directory: &Path) -> Vec<JournalSegment> {
    let Ok(entries) = fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut segments = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let start_ms = segment_start(entry.file_name().to_str()?)?;
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path).ok()?;
            if !metadata.is_file() {
                return None;
            }
            let modified_ms = metadata
                .modified()
                .ok()
                .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
                .map_or(start_ms, |elapsed| {
                    elapsed.as_millis().try_into().unwrap_or(u64::MAX)
                });
            Some(JournalSegment {
                start_ms,
                path,
                len: metadata.len(),
                modified_ms,
            })
        })
        .collect::<Vec<_>>();
    segments.sort_by_key(|segment| segment.start_ms);
    segments
}

/// Deletes segments not written for [`JOURNAL_MAX_AGE`], then the oldest until the journal
/// holds at most [`JOURNAL_TOTAL_BYTES`]. `keep` (the segment being written) always stays.
pub fn prune_journal(directory: &Path, now_ms: u64, keep: Option<&Path>) {
    prune_journal_within(
        directory,
        now_ms,
        keep,
        JOURNAL_TOTAL_BYTES,
        JOURNAL_MAX_AGE,
    );
}

fn prune_journal_within(
    directory: &Path,
    now_ms: u64,
    keep: Option<&Path>,
    total_limit: u64,
    max_age: Duration,
) {
    let segments = journal_segments(directory);
    let mut total = segments.iter().map(|segment| segment.len).sum::<u64>();
    let oldest_allowed = now_ms.saturating_sub(max_age.as_millis().try_into().unwrap_or(u64::MAX));
    for segment in segments {
        if keep.is_some_and(|keep| keep == segment.path) {
            continue;
        }
        if (segment.modified_ms < oldest_allowed || total > total_limit)
            && fs::remove_file(&segment.path).is_ok()
        {
            total = total.saturating_sub(segment.len);
        }
    }
}

/// Reads the next piece of a journal for the service's `CoreJournal` answer: whole lines of
/// the segments written at or after `since_ms`, from `cursor` on (the oldest such segment's
/// start when `None`), at most `max_bytes` of them. Returns the raw lines and where to
/// continue (`None`: everything was read). Every call moves forward, so paging ends.
#[must_use]
pub fn read_journal_chunk(
    directory: &Path,
    since_ms: u64,
    cursor: Option<CoreJournalCursor>,
    max_bytes: usize,
) -> (String, Option<CoreJournalCursor>) {
    let segments = journal_segments(directory)
        .into_iter()
        .filter(|segment| segment.modified_ms >= since_ms)
        .collect::<Vec<_>>();
    let position = match cursor {
        None => segments.first().map(|_| (0, 0)),
        Some(cursor) => segments
            .iter()
            .position(|segment| segment.start_ms >= cursor.segment)
            .map(|index| {
                if segments[index].start_ms == cursor.segment {
                    (index, cursor.offset)
                } else {
                    // The segment was deleted meanwhile: go on with the next one.
                    (index, 0)
                }
            }),
    };
    let Some((index, offset)) = position else {
        return (String::new(), None);
    };
    let segment = &segments[index];
    let following = || {
        segments.get(index + 1).map(|next| CoreJournalCursor {
            segment: next.start_ms,
            offset: 0,
        })
    };
    let mut bytes = Vec::new();
    let read = File::open(&segment.path).and_then(|mut file| {
        file.seek(SeekFrom::Start(offset))?;
        file.take(max_bytes.max(1) as u64).read_to_end(&mut bytes)
    });
    if read.is_err() || bytes.is_empty() {
        return (String::new(), following());
    }
    let cut = match bytes.iter().rposition(|byte| *byte == b'\n') {
        Some(newline) => newline + 1,
        // One line longer than the chunk: take it as it is.
        None if bytes.len() >= max_bytes.max(1) => bytes.len(),
        // The unfinished last line of a segment being written: later.
        None => 0,
    };
    let consumed = offset.saturating_add(cut as u64);
    let next = if cut > 0 && consumed < segment.len {
        Some(CoreJournalCursor {
            segment: segment.start_ms,
            offset: consumed,
        })
    } else {
        following()
    };
    (String::from_utf8_lossy(&bytes[..cut]).into_owned(), next)
}

/// What the daemon records when a connection is attempted.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SessionStart {
    /// `tun` or `proxy`.
    pub mode: String,
    /// The cores run in the always-ready service.
    pub service: bool,
    pub engines: Vec<String>,
    /// [`profile_tag`] of the active subscription's id.
    pub profile: Option<String>,
}

/// One connection as the index knows it.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CoreSession {
    pub connect_ms: u64,
    /// `None`: still running, or the daemon stopped without recording the end.
    pub disconnect_ms: Option<u64>,
    /// `user`, `switch`, `quit`, `cores_lost`, `failed`, `error`, `interrupted`; `None`: open.
    pub reason: Option<String>,
    pub start: SessionStart,
}

impl CoreSession {
    /// No `end` record yet.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.reason.is_none()
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
enum IndexRecord {
    Start {
        connect_ms: u64,
        mode: String,
        service: bool,
        #[serde(default)]
        engines: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile: Option<String>,
    },
    End {
        connect_ms: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        disconnect_ms: Option<u64>,
        reason: String,
    },
}

/// A short, stable pseudonym of a profile id (8 hex digits of its SHA-256).
#[must_use]
pub fn profile_tag(profile_id: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(profile_id.as_bytes())[..4]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Every session of the index in `directory`, oldest first.
#[must_use]
pub fn read_sessions(directory: &Path) -> Vec<CoreSession> {
    parse_sessions(&read_index(&directory.join(SESSION_INDEX_FILE)))
}

fn read_index(path: &Path) -> String {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return String::new();
    };
    if !metadata.is_file() {
        return String::new();
    }
    let mut bytes = Vec::new();
    let read = File::open(path).and_then(|mut file| {
        let start = metadata.len().saturating_sub(MAX_INDEX_BYTES);
        file.seek(SeekFrom::Start(start))?;
        file.take(MAX_INDEX_BYTES).read_to_end(&mut bytes)
    });
    if read.is_err() {
        return String::new();
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn parse_sessions(text: &str) -> Vec<CoreSession> {
    let mut sessions: Vec<CoreSession> = Vec::new();
    for line in text.lines() {
        let Ok(record) = serde_json::from_str::<IndexRecord>(line) else {
            continue;
        };
        match record {
            IndexRecord::Start {
                connect_ms,
                mode,
                service,
                engines,
                profile,
            } => {
                sessions.retain(|session| session.connect_ms != connect_ms);
                sessions.push(CoreSession {
                    connect_ms,
                    disconnect_ms: None,
                    reason: None,
                    start: SessionStart {
                        mode,
                        service,
                        engines,
                        profile,
                    },
                });
            }
            IndexRecord::End {
                connect_ms,
                disconnect_ms,
                reason,
            } => {
                if let Some(session) = sessions
                    .iter_mut()
                    .rev()
                    .find(|session| session.connect_ms == connect_ms && session.is_open())
                {
                    session.disconnect_ms = disconnect_ms;
                    session.reason = Some(reason);
                }
            }
        }
    }
    sessions.sort_by_key(|session| session.connect_ms);
    sessions
}

fn render_sessions(sessions: &[CoreSession]) -> String {
    let mut text = String::new();
    for session in sessions {
        let start = IndexRecord::Start {
            connect_ms: session.connect_ms,
            mode: session.start.mode.clone(),
            service: session.start.service,
            engines: session.start.engines.clone(),
            profile: session.start.profile.clone(),
        };
        if let Ok(line) = serde_json::to_string(&start) {
            text.push_str(&line);
            text.push('\n');
        }
        if let Some(reason) = &session.reason {
            let end = IndexRecord::End {
                connect_ms: session.connect_ms,
                disconnect_ms: session.disconnect_ms,
                reason: reason.clone(),
            };
            if let Ok(line) = serde_json::to_string(&end) {
                text.push_str(&line);
                text.push('\n');
            }
        }
    }
    text
}

/// The daemon's writer of the session index (see the module documentation). Best effort:
/// recording never changes what a connection does.
#[derive(Debug)]
pub struct SessionIndex {
    directory: PathBuf,
}

impl SessionIndex {
    /// The index in `directory` (created when missing). A session a previous run left open
    /// (the daemon stopped or crashed meanwhile) is ended as `interrupted`; sessions older
    /// than [`JOURNAL_MAX_AGE`] and beyond the newest [`MAX_INDEXED_SESSIONS`] are dropped,
    /// and the journal there is pruned.
    #[must_use]
    pub fn open(directory: impl Into<PathBuf>, now_ms: u64) -> Self {
        let index = Self {
            directory: directory.into(),
        };
        let _ = fs::create_dir_all(&index.directory);
        let mut sessions = read_sessions(&index.directory);
        let before = sessions.clone();
        for session in sessions.iter_mut().filter(|session| session.is_open()) {
            session.reason = Some("interrupted".to_owned());
        }
        let oldest_allowed =
            now_ms.saturating_sub(JOURNAL_MAX_AGE.as_millis().try_into().unwrap_or(u64::MAX));
        sessions.retain(|session| {
            session.disconnect_ms.unwrap_or(session.connect_ms) >= oldest_allowed
        });
        let excess = sessions.len().saturating_sub(MAX_INDEXED_SESSIONS);
        sessions.drain(..excess);
        let path = index.path();
        if sessions != before
            || fs::metadata(&path).is_ok_and(|metadata| metadata.len() > MAX_INDEX_BYTES / 2)
        {
            index.replace(&render_sessions(&sessions));
        }
        prune_journal(&index.directory, now_ms, None);
        index
    }

    fn path(&self) -> PathBuf {
        self.directory.join(SESSION_INDEX_FILE)
    }

    /// The journal directory next to the index.
    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    fn replace(&self, text: &str) {
        let temporary = self.directory.join(format!("{SESSION_INDEX_FILE}.tmp"));
        let written = fs::write(&temporary, text).is_ok();
        if !written || fs::rename(&temporary, self.path()).is_err() {
            let _ = fs::remove_file(&temporary);
        }
    }

    fn append(&self, record: &IndexRecord) {
        let Ok(mut line) = serde_json::to_string(record) else {
            return;
        };
        line.push('\n');
        let _ = fs::create_dir_all(&self.directory);
        if let Ok(mut file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.path())
        {
            let _ = file.write_all(line.as_bytes());
        }
    }

    /// A connection attempt began at `connect_ms`. Also prunes the journal.
    pub fn start(&self, connect_ms: u64, start: &SessionStart) {
        self.append(&IndexRecord::Start {
            connect_ms,
            mode: start.mode.clone(),
            service: start.service,
            engines: start.engines.clone(),
            profile: start.profile.clone(),
        });
        prune_journal(&self.directory, connect_ms, None);
    }

    /// The connection that began at `connect_ms` ended at `disconnect_ms` because of `reason`.
    pub fn end(&self, connect_ms: u64, disconnect_ms: u64, reason: &str) {
        self.append(&IndexRecord::End {
            connect_ms,
            disconnect_ms: Some(disconnect_ms),
            reason: reason.to_owned(),
        });
    }
}

/// The journal time window `[from, to)` of each session (same order): from
/// [`SESSION_LEAD_MS`] before its connect to [`SESSION_TAIL_MS`] after its disconnect, never
/// overlapping a neighbour, so every line belongs to at most one session. A session without
/// a recorded end lasts until the next one began (the newest: until now and beyond).
#[must_use]
pub fn session_windows(sessions: &[CoreSession]) -> Vec<(u64, u64)> {
    let mut windows = Vec::with_capacity(sessions.len());
    let mut previous_end = 0_u64;
    for (index, session) in sessions.iter().enumerate() {
        let next_connect = sessions.get(index + 1).map(|next| next.connect_ms);
        let from = session
            .connect_ms
            .saturating_sub(SESSION_LEAD_MS)
            .max(previous_end);
        let natural_end = session
            .disconnect_ms
            .map_or(u64::MAX, |end| end.saturating_add(SESSION_TAIL_MS));
        let to = next_connect.map_or(natural_end, |next| natural_end.min(next));
        let to = to.max(from);
        windows.push((from, to));
        previous_end = to;
    }
    windows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(connect_ms: u64, disconnect_ms: Option<u64>, reason: Option<&str>) -> CoreSession {
        CoreSession {
            connect_ms,
            disconnect_ms,
            reason: reason.map(str::to_owned),
            start: SessionStart {
                mode: "tun".into(),
                service: true,
                engines: vec!["xray".into(), "mihomo".into()],
                profile: Some("0a1b2c3d".into()),
            },
        }
    }

    #[test]
    fn lines_keep_the_latest_core_log_format_and_parse_back() {
        let line = format_core_line(1_791_064_846_123, "mihomo", "stdout", "a\tb\nc");
        assert_eq!(line, "1791064846123\tmihomo\tstdout\ta b c\n");
        assert_eq!(core_line_timestamp(&line), Some(1_791_064_846_123));
        assert_eq!(core_line_timestamp("garbage\tline"), None);
        assert_eq!(core_line_timestamp("no tab"), None);
    }

    #[test]
    fn a_core_log_buffer_with_a_journal_writes_every_line_there_at_once() {
        let directory = tempfile::tempdir().unwrap();
        let journal = directory.path().join(CORE_SESSIONS_DIRECTORY);
        let logs = crate::CoreLogBuffer::default().with_journal(&journal, true);
        logs.push(
            crate::Engine::Mihomo,
            crate::DiagnosticStream::Stdout,
            "started https://sub.example/secret",
        );
        logs.push(crate::Engine::Xray, crate::DiagnosticStream::Stderr, "warn");
        // Readable while the writer still holds the segment open.
        let segments = journal_segments(&journal);
        assert_eq!(segments.len(), 1);
        let text = fs::read_to_string(&segments[0].path).unwrap();
        let lines = text.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("\tmihomo\tstdout\tstarted "));
        assert!(
            !text.contains("sub.example"),
            "redacted like the live log: {text}"
        );
        assert!(lines[1].ends_with("\txray\tstderr\twarn"));
        assert!(lines.iter().all(|line| core_line_timestamp(line).is_some()));
    }

    #[test]
    fn the_journal_rolls_over_into_new_segments_and_is_created_lazily() {
        let directory = tempfile::tempdir().unwrap();
        let journal_directory = directory.path().join(CORE_SESSIONS_DIRECTORY);
        let mut journal = CoreJournal::new(&journal_directory, false).with_segment_limit(64);
        assert!(!journal_directory.exists(), "nothing before the first line");
        for index in 0..10 {
            journal.append(&format_core_line(
                1_000 + index,
                "xray",
                "stderr",
                "twenty bytes!",
            ));
        }
        let segments = journal_segments(&journal_directory);
        assert!(segments.len() >= 3, "{segments:?}");
        assert!(segments.iter().all(|segment| segment.len <= 64));
        let text = segments
            .iter()
            .map(|segment| fs::read_to_string(&segment.path).unwrap())
            .collect::<String>();
        assert_eq!(text.lines().count(), 10);
    }

    #[test]
    fn pruning_drops_old_and_excess_segments_oldest_first_but_never_the_kept_one() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        for start in [1_u64, 2, 3, 4] {
            fs::write(root.join(segment_name(start)), vec![b'x'; 100]).unwrap();
        }
        fs::write(root.join("unrelated.log"), "keep").unwrap();
        let now = unix_ms_now();
        prune_journal_within(
            root,
            now,
            Some(&root.join(segment_name(1))),
            250,
            Duration::from_secs(3600),
        );
        let left = journal_segments(root)
            .into_iter()
            .map(|segment| segment.start_ms)
            .collect::<Vec<_>>();
        assert_eq!(
            left,
            vec![1, 4],
            "kept 1, then dropped 2 and 3 to fit 250 bytes"
        );
        assert!(root.join("unrelated.log").exists());
        // Everything older than the age limit goes, except the kept segment.
        prune_journal_within(
            root,
            now + 7_200_000,
            Some(&root.join(segment_name(1))),
            u64::MAX,
            Duration::from_secs(3600),
        );
        let left = journal_segments(root)
            .into_iter()
            .map(|segment| segment.start_ms)
            .collect::<Vec<_>>();
        assert_eq!(left, vec![1]);
    }

    #[test]
    fn chunks_page_through_every_segment_in_whole_lines() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let first = (0..50)
            .map(|index| format_core_line(10_000 + index, "mihomo", "stdout", "line"))
            .collect::<String>();
        let second = (0..30)
            .map(|index| format_core_line(20_000 + index, "xray", "stdout", "more"))
            .collect::<String>();
        fs::write(root.join(segment_name(10_000)), &first).unwrap();
        // The last line is still being written.
        fs::write(root.join(segment_name(20_000)), format!("{second}2003")).unwrap();
        let mut cursor = None;
        let mut text = String::new();
        for _ in 0..1_000 {
            let (chunk, next) = read_journal_chunk(root, 0, cursor, 100);
            assert!(chunk.is_empty() || chunk.ends_with('\n'));
            assert!(chunk.len() <= 100);
            text.push_str(&chunk);
            if next.is_none() {
                break;
            }
            assert_ne!(next, cursor, "paging always moves forward");
            cursor = next;
        }
        assert_eq!(text, format!("{first}{second}"));
        // A cursor into a segment deleted meanwhile continues with the next one.
        let (chunk, _) = read_journal_chunk(
            root,
            0,
            Some(CoreJournalCursor {
                segment: 15_000,
                offset: 77,
            }),
            1 << 20,
        );
        assert!(chunk.starts_with("20000\t"));
        // Segments last written before `since` are skipped.
        let (chunk, next) = read_journal_chunk(root, unix_ms_now() + 60_000, None, 1 << 20);
        assert!(chunk.is_empty() && next.is_none());
    }

    #[test]
    fn the_index_records_starts_and_ends_and_closes_what_a_previous_run_left_open() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join(CORE_SESSIONS_DIRECTORY);
        let now = unix_ms_now();
        let index = SessionIndex::open(&root, now);
        let start = SessionStart {
            mode: "proxy".into(),
            service: false,
            engines: vec!["xray".into(), "mihomo".into()],
            profile: Some(profile_tag("profile-a")),
        };
        index.start(now - 5_000, &start);
        index.end(now - 5_000, now - 4_000, "user");
        index.start(now - 3_000, &start);
        let sessions = read_sessions(&root);
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].reason.as_deref(), Some("user"));
        assert_eq!(sessions[0].disconnect_ms, Some(now - 4_000));
        assert_eq!(sessions[0].start, start);
        assert!(sessions[1].is_open());
        let text = fs::read_to_string(root.join(SESSION_INDEX_FILE)).unwrap();
        assert!(!text.contains("profile-a"), "only the hash: {text}");

        // The daemon restarts: the open session is ended as interrupted, without an end time.
        let _ = SessionIndex::open(&root, now);
        let sessions = read_sessions(&root);
        assert_eq!(sessions[1].reason.as_deref(), Some("interrupted"));
        assert_eq!(sessions[1].disconnect_ms, None);
    }

    #[test]
    fn the_index_forgets_sessions_past_the_age_limit() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let now = unix_ms_now();
        let month: u64 = JOURNAL_MAX_AGE.as_millis().try_into().unwrap();
        let sessions = vec![
            session(
                now - month - 10_000,
                Some(now - month - 5_000),
                Some("user"),
            ),
            session(now - 1_000, Some(now - 500), Some("user")),
        ];
        fs::write(root.join(SESSION_INDEX_FILE), render_sessions(&sessions)).unwrap();
        let _ = SessionIndex::open(root, now);
        assert_eq!(read_sessions(root), vec![sessions[1].clone()]);
    }

    #[test]
    fn garbage_in_the_index_is_skipped() {
        let sessions = parse_sessions(
            "not json\n{\"event\":\"end\",\"connect_ms\":5,\"reason\":\"user\"}\n{\"event\":\"start\",\"connect_ms\":7,\"mode\":\"tun\",\"service\":true}\n",
        );
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].connect_ms, 7);
        assert!(sessions[0].is_open());
    }

    #[test]
    fn windows_cover_lead_and_tail_without_overlapping() {
        let sessions = vec![
            session(100_000, Some(200_000), Some("user")),
            // Reconnected 5 s later: the tail of the first stops at this connect.
            session(205_000, Some(300_000), Some("switch")),
            // Left open by a crashed daemon: lasts until the next connect.
            session(400_000, None, Some("interrupted")),
            session(500_000, None, None),
        ];
        assert_eq!(
            session_windows(&sessions),
            vec![
                (40_000, 205_000),
                (205_000, 310_000),
                (340_000, 500_000),
                (500_000, u64::MAX),
            ]
        );
    }
}
