//! «Сохранить лог…»: the per-connection core logs of the archive, `core-sessions/`.
//!
//! The daemon records every connection in `logs/core-sessions/sessions.jsonl`, and the cores'
//! output goes to journals: the user's own in `logs/core-sessions/` (system-proxy mode, the
//! on-demand elevated host) and the always-ready service's, which the daemon fetches
//! (`/v1/core-journal`). See `multicore_core::core_sessions`.
//!
//! The archive gets, for every connection of the chosen [`LogRange`], a folder:
//! `core-sessions/latest/` for the newest connection, `core-sessions/<connect unix>/` for each
//! earlier one, holding `session.txt` (when it began and ended, why, the mode), `core.log`
//! (the Mihomo and Xray lines of its time window, at most
//! [`MAX_SESSION_LOG_BYTES`], newest kept) and the matching slices of `events.log` and the
//! service's `service.log`; plus `core-sessions/index.txt`, one row per folder.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use multicore_core::core_sessions::{
    CORE_SESSIONS_DIRECTORY, CoreSession, MAX_SESSION_LOG_BYTES, core_line_timestamp,
    journal_segments, read_sessions, session_windows,
};
use multicore_i18n::tr;

use crate::daemon::{DaemonCoreJournal, DaemonError};

/// The most core log text the archive holds over all its sessions (newest sessions first).
pub(crate) const MAX_CORE_SESSIONS_BYTES: usize = 64 * 1024 * 1024;
const WEEK_MS: u64 = 7 * 24 * 60 * 60 * 1000;

/// Which connections «Сохранить лог…» puts into the archive.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum LogRange {
    /// The newest connection (the one running now, or the last one).
    #[default]
    Latest,
    /// Every connection that ran during the last seven days.
    Week,
    /// Every connection the index still knows (30 days at most).
    All,
}

impl LogRange {
    /// `latest`, `week` or `all` (the UI's keys); anything else is `Latest`.
    pub(crate) fn from_key(key: &str) -> Self {
        match key {
            "week" => Self::Week,
            "all" => Self::All,
            _ => Self::Latest,
        }
    }

    pub(crate) fn key(self) -> &'static str {
        match self {
            Self::Latest => "latest",
            Self::Week => "week",
            Self::All => "all",
        }
    }

    fn describe(self) -> &'static str {
        match self {
            Self::Latest => tr!("последнее подключение"),
            Self::Week => tr!("за неделю"),
            Self::All => tr!("за всё время"),
        }
    }
}

/// `<data>/logs/core-sessions`.
pub(crate) fn sessions_directory(data_root: &Path) -> PathBuf {
    data_root.join("logs").join(CORE_SESSIONS_DIRECTORY)
}

/// The connections of the index and those a range selects.
#[derive(Clone, Debug)]
pub(crate) struct SessionPlan {
    pub(crate) range: LogRange,
    sessions: Vec<CoreSession>,
    windows: Vec<(u64, u64)>,
    /// Indices into `sessions`, oldest first.
    selected: Vec<usize>,
    now_ms: u64,
}

impl SessionPlan {
    /// The plan for `range` from the index under `data_root`.
    pub(crate) fn read(data_root: Option<&Path>, range: LogRange, now_ms: u64) -> Self {
        let sessions = data_root
            .map(|root| read_sessions(&sessions_directory(root)))
            .unwrap_or_default();
        Self::new(sessions, range, now_ms)
    }

    fn new(sessions: Vec<CoreSession>, range: LogRange, now_ms: u64) -> Self {
        let windows = session_windows(&sessions);
        let selected = match range {
            LogRange::Latest => sessions.len().checked_sub(1).into_iter().collect(),
            LogRange::Week => (0..sessions.len())
                .filter(|&index| {
                    let session = &sessions[index];
                    let ended = session
                        .disconnect_ms
                        .unwrap_or_else(|| windows[index].1.min(now_ms));
                    ended.max(session.connect_ms) >= now_ms.saturating_sub(WEEK_MS)
                })
                .collect(),
            LogRange::All => (0..sessions.len()).collect(),
        };
        Self {
            range,
            sessions,
            windows,
            selected,
            now_ms,
        }
    }

    /// How many connections the archive will hold.
    pub(crate) fn count(&self) -> usize {
        self.selected.len()
    }

    /// From when the journals are needed: the start of the oldest selected window.
    pub(crate) fn since_ms(&self) -> Option<u64> {
        self.selected
            .iter()
            .map(|&index| self.windows[index].0)
            .min()
    }
}

/// One hint line per range for the «Сохранить лог» popup.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct RangeHints {
    pub(crate) latest: String,
    pub(crate) week: String,
    pub(crate) all: String,
}

/// The popup's hints: the newest connection's time span, and how many connections the week
/// and the whole index hold; each with the approximate size of their core log when it is
/// known (every connection ran without the service, whose journal size only the service
/// knows). Reads the small index and the journal's file sizes only.
pub(crate) fn range_hints(data_root: Option<&Path>, now_ms: u64) -> RangeHints {
    let all = SessionPlan::read(data_root, LogRange::All, now_ms);
    let week = SessionPlan::new(all.sessions.clone(), LogRange::Week, now_ms);
    let latest = SessionPlan::new(all.sessions.clone(), LogRange::Latest, now_ms);
    let segments = data_root
        .map(|root| journal_segments(&sessions_directory(root)))
        .unwrap_or_default();
    let with_size = |text: String, plan: &SessionPlan| match plan.approximate_bytes(&segments) {
        Some(bytes) => format!("{text} · {}", approximate_size(bytes)),
        None => text,
    };
    let latest_hint = match all.sessions.last() {
        None => tr!("Подключений ещё не было").to_owned(),
        Some(session) => with_size(time_span(session, now_ms), &latest),
    };
    RangeHints {
        latest: latest_hint,
        week: with_size(connection_count(week.count()), &week),
        all: with_size(connection_count(all.count()), &all),
    }
}

impl SessionPlan {
    /// The size of the user's journal segments written during the selected windows; `None`
    /// when nothing is selected or a selected connection ran through the service.
    fn approximate_bytes(
        &self,
        segments: &[multicore_core::core_sessions::JournalSegment],
    ) -> Option<u64> {
        if self.selected.is_empty()
            || self
                .selected
                .iter()
                .any(|&index| self.sessions[index].start.service)
        {
            return None;
        }
        Some(
            segments
                .iter()
                .filter(|segment| {
                    self.selected.iter().any(|&index| {
                        let (from, to) = self.windows[index];
                        segment.start_ms < to && segment.modified_ms >= from
                    })
                })
                .map(|segment| segment.len)
                .sum(),
        )
    }
}

/// `3 подключения`, `5 подключений`, `21 подключение` (English: `1 connection`, `5 connections`).
fn connection_count(count: usize) -> String {
    let russian = multicore_i18n::current() == multicore_i18n::Lang::Ru;
    let (last, last_two) = (count % 10, count % 100);
    if russian && last == 1 && last_two != 11 || !russian && count == 1 {
        tr!("{} подключение", count)
    } else if russian && (2..=4).contains(&last) && !(12..=14).contains(&last_two) {
        tr!("{} подключения", count)
    } else {
        tr!("{} подключений", count)
    }
}

/// `<1 МБ`, `~2 МБ`.
fn approximate_size(bytes: u64) -> String {
    const MIB: u64 = 1024 * 1024;
    if bytes < MIB {
        tr!("<1 МБ").to_owned()
    } else {
        tr!("~{} МБ", (bytes + MIB / 2) / MIB)
    }
}

/// `Сегодня 16:46–16:47`, `04.10 16:46–17:02`, `03.10 23:50–04.10 00:10`; a running
/// connection ends in `сейчас`.
fn time_span(session: &CoreSession, now_ms: u64) -> String {
    use chrono::{Datelike as _, Local, TimeZone as _};
    let at = |ms: u64| {
        Local
            .timestamp_millis_opt(i64::try_from(ms).unwrap_or(i64::MAX))
            .single()
    };
    let (Some(start), Some(now)) = (at(session.connect_ms), at(now_ms)) else {
        return (session.connect_ms / 1000).to_string();
    };
    let day = |when: &chrono::DateTime<Local>| {
        if when.year() == now.year() {
            when.format("%d.%m").to_string()
        } else {
            when.format("%d.%m.%Y").to_string()
        }
    };
    let end = match session.disconnect_ms {
        None if session.is_open() => tr!("{}–сейчас", start.format("%H:%M")),
        // Ended without a recorded time (the app stopped meanwhile).
        None => start.format("%H:%M").to_string(),
        Some(end_ms) => match at(end_ms) {
            Some(end) if end.date_naive() == start.date_naive() => {
                format!("{}–{}", start.format("%H:%M"), end.format("%H:%M"))
            }
            Some(end) => format!(
                "{}–{} {}",
                start.format("%H:%M"),
                day(&end),
                end.format("%H:%M")
            ),
            None => start.format("%H:%M").to_string(),
        },
    };
    if start.date_naive() == now.date_naive() {
        tr!("Сегодня {}", end)
    } else {
        format!("{} {end}", day(&start))
    }
}

/// Lines with their unix ms, oldest first.
type Stamped = Vec<(u64, String)>;

/// The archive's `core-sessions/…` entries and the `system.txt` lines about them. `service`
/// is the daemon's answer for the service's journal (`None`: not asked, nothing selected);
/// `service_logs` are the service's own log files already fetched for `logs/`.
pub(crate) fn session_entries(
    plan: &SessionPlan,
    data_root: Option<&Path>,
    service: Option<Result<DaemonCoreJournal, DaemonError>>,
    service_logs: &[(String, String)],
) -> (String, Vec<(String, String)>) {
    let mut note = String::new();
    let line = |note: &mut String, key: &str, value: &str| {
        let _ = writeln!(note, "{key:<22}{value}");
    };
    line(&mut note, tr!("Диапазон лога"), plan.range.describe());
    if plan.selected.is_empty() {
        line(
            &mut note,
            tr!("Подключения"),
            tr!("нет записанных подключений"),
        );
        return (note, Vec::new());
    }
    let since = plan.since_ms().unwrap_or(0);
    let mut core = user_journal_lines(data_root, since);
    let service_status = match service {
        None => None,
        Some(Err(_)) => Some(tr!("журнал ядер службы недоступен: демон не ответил")),
        Some(Ok(journal)) => match journal.status.as_str() {
            "included" => {
                core.extend(stamped_lines(&journal.text, core_line_timestamp));
                journal
                    .truncated
                    .then_some(tr!("журнал ядер службы обрезан по размеру"))
            }
            "no_service" => None,
            "too_old" => Some(tr!("журнал ядер службы недоступен: служба слишком старая")),
            "busy" => Some(tr!(
                "журнал ядер службы недоступен: службой пользуется другой пользователь"
            )),
            _ => Some(tr!("журнал ядер службы недоступен: служба не ответила")),
        },
    };
    // Both journals interleave by time; equal times keep their order.
    core.sort_by_key(|(stamp, _)| *stamp);
    let mut events = data_root
        .and_then(|root| fs::read_to_string(root.join("logs").join("events.log")).ok())
        .map(|text| stamped_lines(&text, event_timestamp))
        .unwrap_or_default();
    events.sort_by_key(|(stamp, _)| *stamp);
    let mut service_lines = Stamped::new();
    for name in ["service.log.1", "service.log"] {
        if let Some((_, text)) = service_logs.iter().find(|(file, _)| file == name) {
            service_lines.extend(stamped_lines(text, service_log_timestamp));
        }
    }
    service_lines.sort_by_key(|(stamp, _)| *stamp);

    let mut entries = Vec::new();
    let mut rows = Vec::new();
    let mut budget = MAX_CORE_SESSIONS_BYTES;
    let mut used_names = std::collections::BTreeSet::new();
    let newest = plan.sessions.len() - 1;
    // Newest first: the size cap leaves out the oldest.
    for &index in plan.selected.iter().rev() {
        let session = &plan.sessions[index];
        let (from, to) = plan.windows[index];
        let mut folder = if index == newest {
            "latest".to_owned()
        } else {
            (session.connect_ms / 1000).to_string()
        };
        let base = folder.clone();
        let mut suffix = 2;
        while !used_names.insert(folder.clone()) {
            folder = format!("{base}-{suffix}");
            suffix += 1;
        }
        let (core_text, core_lines, cut) =
            newest_within(window(&core, from, to), MAX_SESSION_LOG_BYTES.min(budget));
        budget = budget.saturating_sub(core_text.len());
        let prefix = format!("core-sessions/{folder}");
        entries.push((
            format!("{prefix}/session.txt"),
            session_text(session, (from, to), plan.now_ms, core_lines, cut),
        ));
        entries.push((format!("{prefix}/core.log"), core_text));
        let (events_text, ..) = newest_within(window(&events, from, to), MAX_SESSION_LOG_BYTES);
        if !events_text.is_empty() {
            entries.push((format!("{prefix}/events.log"), events_text));
        }
        let (service_text, ..) =
            newest_within(window(&service_lines, from, to), MAX_SESSION_LOG_BYTES);
        if !service_text.is_empty() {
            entries.push((format!("{prefix}/service.log"), service_text));
        }
        rows.push(index_row(&folder, session, core_lines));
    }
    rows.reverse();
    let mut index = format!(
        "# MULTICORE core sessions · range: {} · generated_unix: {}\n\
         folder\tconnect_unix\tdisconnect_unix\tend_reason\tmode\tservice\tengines\tprofile\tcore_lines\n",
        plan.range.key(),
        plan.now_ms / 1000
    );
    for row in rows {
        index.push_str(&row);
    }
    entries.insert(0, ("core-sessions/index.txt".to_owned(), index));
    let mut summary = format!("{} (core-sessions/)", plan.selected.len());
    if let Some(status) = service_status {
        summary.push_str("; ");
        summary.push_str(status);
    }
    line(&mut note, tr!("Подключения"), &summary);
    (note, entries)
}

/// The lines of the user's own journal in segments written at or after `since_ms`.
fn user_journal_lines(data_root: Option<&Path>, since_ms: u64) -> Stamped {
    let Some(root) = data_root else {
        return Stamped::new();
    };
    let mut lines = Stamped::new();
    for segment in journal_segments(&sessions_directory(root)) {
        if segment.modified_ms < since_ms {
            continue;
        }
        if let Ok(bytes) = fs::read(&segment.path) {
            lines.extend(stamped_lines(
                &String::from_utf8_lossy(&bytes),
                core_line_timestamp,
            ));
        }
    }
    lines
}

fn stamped_lines(text: &str, stamp: fn(&str) -> Option<u64>) -> Stamped {
    text.lines()
        .filter_map(|line| stamp(line).map(|ms| (ms, line.to_owned())))
        .collect()
}

/// `{"timestamp_ms":1791064846123,…}` (the daemon's `events.log`).
fn event_timestamp(line: &str) -> Option<u64> {
    let digits = line.strip_prefix("{\"timestamp_ms\":")?;
    let end = digits
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(digits.len());
    digits[..end].parse().ok()
}

/// `1791064846 message` (the service's `service.log`, unix seconds).
fn service_log_timestamp(line: &str) -> Option<u64> {
    let (seconds, _) = line.split_once(' ')?;
    if seconds.is_empty() || !seconds.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    seconds.parse::<u64>().ok()?.checked_mul(1000)
}

/// The lines of `sorted` (oldest first) stamped in `[from, to)`.
fn window(sorted: &Stamped, from: u64, to: u64) -> &[(u64, String)] {
    let start = sorted.partition_point(|(stamp, _)| *stamp < from);
    let end = sorted.partition_point(|(stamp, _)| *stamp < to);
    &sorted[start..end.max(start)]
}

/// The newest whole lines of `lines` that fit in `limit` bytes, how many they are, and
/// whether older ones were left out.
fn newest_within(lines: &[(u64, String)], limit: usize) -> (String, usize, bool) {
    let mut size = 0_usize;
    let mut first = lines.len();
    for (index, (_, line)) in lines.iter().enumerate().rev() {
        if size + line.len() + 1 > limit {
            break;
        }
        size += line.len() + 1;
        first = index;
    }
    let mut text = String::with_capacity(size);
    for (_, line) in &lines[first..] {
        text.push_str(line);
        text.push('\n');
    }
    (text, lines.len() - first, first > 0)
}

fn session_text(
    session: &CoreSession,
    (from, to): (u64, u64),
    now_ms: u64,
    core_lines: usize,
    cut: bool,
) -> String {
    let mut text = String::new();
    let mut line = |key: &str, value: String| {
        let _ = writeln!(text, "{key:<18}{value}");
    };
    line("connect_unix", (session.connect_ms / 1000).to_string());
    line(
        "disconnect_unix",
        session
            .disconnect_ms
            .map_or_else(|| "open".to_owned(), |end| (end / 1000).to_string()),
    );
    line(
        "connect_local",
        local_time_full(session.connect_ms).unwrap_or_default(),
    );
    if let Some(end) = session.disconnect_ms {
        line("disconnect_local", local_time_full(end).unwrap_or_default());
        line(
            "duration_s",
            (end.saturating_sub(session.connect_ms) / 1000).to_string(),
        );
    }
    line(
        "end_reason",
        session.reason.clone().unwrap_or_else(|| "open".to_owned()),
    );
    line("mode", session.start.mode.clone());
    line(
        "service",
        if session.start.service { "yes" } else { "no" }.to_owned(),
    );
    line("engines", session.start.engines.join(","));
    line(
        "profile",
        session
            .start
            .profile
            .clone()
            .unwrap_or_else(|| "-".to_owned()),
    );
    line(
        "log_window_ms",
        format!(
            "{from}..{}",
            if to == u64::MAX {
                now_ms.to_string()
            } else {
                to.to_string()
            }
        ),
    );
    line(
        "core_lines",
        if cut {
            format!("{core_lines} (older lines left out: size cap)")
        } else {
            core_lines.to_string()
        },
    );
    text
}

fn local_time_full(unix_ms: u64) -> Option<String> {
    use chrono::{Local, TimeZone as _};
    Local
        .timestamp_millis_opt(i64::try_from(unix_ms).ok()?)
        .single()
        .map(|when| when.format("%Y-%m-%d %H:%M:%S %z").to_string())
}

fn index_row(folder: &str, session: &CoreSession, core_lines: usize) -> String {
    format!(
        "{folder}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{core_lines}\n",
        session.connect_ms / 1000,
        session
            .disconnect_ms
            .map_or_else(|| "open".to_owned(), |end| (end / 1000).to_string()),
        session.reason.as_deref().unwrap_or("open"),
        session.start.mode,
        if session.start.service { "yes" } else { "no" },
        session.start.engines.join(","),
        session.start.profile.as_deref().unwrap_or("-"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use multicore_core::core_sessions::{
        SessionIndex, SessionStart, format_core_line, segment_name,
    };
    use std::collections::BTreeMap;

    const DAY: u64 = 24 * 60 * 60 * 1000;

    fn start(service: bool) -> SessionStart {
        SessionStart {
            mode: "tun".into(),
            service,
            engines: vec!["xray".into(), "mihomo".into()],
            profile: Some("0a1b2c3d".into()),
        }
    }

    /// A data root with three connections: ten days ago (per-user), two days ago and a
    /// recent one (both through the service), each with core lines in the user journal.
    fn data_root(now: u64) -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        let sessions = sessions_directory(directory.path());
        let index = SessionIndex::open(&sessions, now);
        let old = now - 10 * DAY;
        let recent = now - 2 * DAY;
        let latest = now - 60_000;
        index.start(old, &start(false));
        index.end(old, old + 30_000, "user");
        index.start(recent, &start(true));
        index.end(recent, recent + 30_000, "cores_lost");
        index.start(latest, &start(true));
        index.end(latest, latest + 20_000, "user");
        let mut journal = String::new();
        for (at, text) in [
            (old + 1_000, "old line"),
            (recent - 30_000, "warm-up before recent"),
            (recent + 5_000, "recent line"),
            (recent + 30_000 + 5_000, "recent stopping"),
            (recent + 2 * 60 * 60 * 1000, "idle warm line"),
            (latest + 1_000, "latest line"),
        ] {
            journal.push_str(&format_core_line(at, "mihomo", "stdout", text));
        }
        fs::write(sessions.join(segment_name(old)), journal).unwrap();
        fs::write(
            directory.path().join("logs").join("events.log"),
            format!(
                "{{\"timestamp_ms\":{},\"safe_message\":\"connect latest\"}}\n{{\"timestamp_ms\":{},\"safe_message\":\"connect old\"}}\n",
                latest + 10,
                old + 10
            ),
        )
        .unwrap();
        directory
    }

    fn entries_map(entries: Vec<(String, String)>) -> BTreeMap<String, String> {
        entries.into_iter().collect()
    }

    #[test]
    fn ranges_select_the_newest_the_last_week_or_every_connection() {
        let now = multicore_core::core_sessions::unix_ms_now();
        let root = data_root(now);
        let count = |range| SessionPlan::read(Some(root.path()), range, now).count();
        assert_eq!(count(LogRange::Latest), 1);
        assert_eq!(count(LogRange::Week), 2);
        assert_eq!(count(LogRange::All), 3);
        assert_eq!(SessionPlan::read(None, LogRange::All, now).count(), 0);
        assert_eq!(LogRange::from_key("week"), LogRange::Week);
        assert_eq!(LogRange::from_key("all"), LogRange::All);
        assert_eq!(LogRange::from_key("anything"), LogRange::Latest);
    }

    #[test]
    fn every_connection_gets_its_folder_with_its_own_lines_latest_first() {
        let now = multicore_core::core_sessions::unix_ms_now();
        let root = data_root(now);
        let plan = SessionPlan::read(Some(root.path()), LogRange::All, now);
        let service = DaemonCoreJournal {
            status: "included".into(),
            text: format_core_line(now - 2 * DAY + 7_000, "xray", "stderr", "service recent"),
            truncated: false,
        };
        let service_logs = vec![(
            "service.log".to_owned(),
            format!("{} set_tun on ok\n", (now - 60_000) / 1000 + 2),
        )];
        let (note, entries) =
            session_entries(&plan, Some(root.path()), Some(Ok(service)), &service_logs);
        let entries = entries_map(entries);
        let old = (now - 10 * DAY) / 1000;
        let recent = (now - 2 * DAY) / 1000;
        let names = entries.keys().cloned().collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                format!("core-sessions/{old}/core.log"),
                format!("core-sessions/{old}/events.log"),
                format!("core-sessions/{old}/session.txt"),
                format!("core-sessions/{recent}/core.log"),
                format!("core-sessions/{recent}/session.txt"),
                "core-sessions/index.txt".to_owned(),
                "core-sessions/latest/core.log".to_owned(),
                "core-sessions/latest/events.log".to_owned(),
                "core-sessions/latest/service.log".to_owned(),
                "core-sessions/latest/session.txt".to_owned(),
            ]
        );
        let recent_log = &entries[&format!("core-sessions/{recent}/core.log")];
        assert!(recent_log.contains("warm-up before recent"));
        assert!(recent_log.contains("recent line"));
        assert!(
            recent_log.contains("service recent"),
            "the service's journal too"
        );
        assert!(recent_log.contains("recent stopping"));
        assert!(
            !recent_log.contains("idle warm line"),
            "outside every window"
        );
        let order = ["warm-up before recent", "recent line", "service recent"];
        let positions = order.map(|text| recent_log.find(text).unwrap());
        assert!(
            positions.windows(2).all(|pair| pair[0] < pair[1]),
            "by time"
        );
        assert!(entries["core-sessions/latest/core.log"].contains("latest line"));
        assert!(!entries["core-sessions/latest/core.log"].contains("recent"));
        assert!(entries["core-sessions/latest/events.log"].contains("connect latest"));
        assert!(entries["core-sessions/latest/service.log"].contains("set_tun on ok"));
        let session = &entries["core-sessions/latest/session.txt"];
        assert!(session.contains(&format!("connect_unix      {}", (now - 60_000) / 1000)));
        assert!(session.contains("end_reason        user"));
        assert!(session.contains("service           yes"));
        let old_session = &entries[&format!("core-sessions/{old}/session.txt")];
        assert!(old_session.contains("service           no"));
        let index = &entries["core-sessions/index.txt"];
        assert!(index.contains("range: all"));
        let rows = index.lines().skip(2).collect::<Vec<_>>();
        assert_eq!(rows.len(), 3);
        assert!(rows[0].starts_with(&format!("{old}\t")));
        assert!(rows[1].contains("\tcores_lost\t"));
        assert!(rows[2].starts_with("latest\t"));
        assert!(note.contains("3 (core-sessions/)"), "{note}");
    }

    #[test]
    fn the_latest_range_holds_one_folder_and_an_open_connection_runs_until_now() {
        let now = multicore_core::core_sessions::unix_ms_now();
        let root = data_root(now);
        let sessions = sessions_directory(root.path());
        // Connected again, still running.
        let index = SessionIndex::open(&sessions, now);
        index.start(now - 5_000, &start(true));
        fs::write(
            sessions.join(segment_name(now - 4_000)),
            format_core_line(now - 1_000, "xray", "stdout", "running now"),
        )
        .unwrap();
        let plan = SessionPlan::read(Some(root.path()), LogRange::Latest, now);
        let (_, entries) = session_entries(&plan, Some(root.path()), None, &[]);
        let entries = entries_map(entries);
        assert!(
            entries
                .keys()
                .all(|name| name.starts_with("core-sessions/latest/")
                    || name == "core-sessions/index.txt")
        );
        assert!(entries["core-sessions/latest/core.log"].contains("running now"));
        assert!(entries["core-sessions/latest/session.txt"].contains("disconnect_unix   open"));
    }

    #[test]
    fn a_service_too_old_or_silent_is_named_and_nothing_recorded_is_said() {
        let now = multicore_core::core_sessions::unix_ms_now();
        let root = data_root(now);
        let plan = SessionPlan::read(Some(root.path()), LogRange::Latest, now);
        let too_old = DaemonCoreJournal {
            status: "too_old".into(),
            ..DaemonCoreJournal::default()
        };
        let (note, _) = session_entries(&plan, Some(root.path()), Some(Ok(too_old)), &[]);
        assert!(note.contains("служба слишком старая"), "{note}");
        let (note, _) = session_entries(
            &plan,
            Some(root.path()),
            Some(Err(DaemonError::new("down"))),
            &[],
        );
        assert!(note.contains("демон не ответил"), "{note}");
        let empty = tempfile::tempdir().unwrap();
        let plan = SessionPlan::read(Some(empty.path()), LogRange::All, now);
        let (note, entries) = session_entries(&plan, Some(empty.path()), None, &[]);
        assert!(entries.is_empty());
        assert!(note.contains("нет записанных подключений"), "{note}");
    }

    #[test]
    fn a_session_keeps_its_newest_lines_within_the_cap() {
        let lines = (0..10)
            .map(|index| (index, format!("line {index}")))
            .collect::<Vec<_>>();
        let (text, count, cut) = newest_within(&lines, 21);
        assert_eq!(text, "line 7\nline 8\nline 9\n");
        assert_eq!(count, 3);
        assert!(cut);
        let (text, count, cut) = newest_within(&lines, 1 << 20);
        assert_eq!(count, 10);
        assert!(!cut && text.starts_with("line 0\n"));
    }

    #[test]
    fn hints_name_the_newest_connection_and_count_the_others() {
        let now = multicore_core::core_sessions::unix_ms_now();
        let root = data_root(now);
        let hints = range_hints(Some(root.path()), now);
        // The newest two ran through the service: their size is unknown.
        assert!(
            hints.latest.contains('–') && !hints.latest.contains("МБ"),
            "{hints:?}"
        );
        assert_eq!(hints.week, "2 подключения");
        assert_eq!(hints.all, "3 подключения");
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(
            range_hints(Some(empty.path()), now).latest,
            "Подключений ещё не было"
        );
        assert_eq!(range_hints(Some(empty.path()), now).all, "0 подключений");
    }

    #[test]
    fn hints_add_the_journal_size_when_no_connection_used_the_service() {
        let now = multicore_core::core_sessions::unix_ms_now();
        let directory = tempfile::tempdir().unwrap();
        let sessions = sessions_directory(directory.path());
        let index = SessionIndex::open(&sessions, now);
        index.start(now - 120_000, &start(false));
        index.end(now - 120_000, now - 60_000, "user");
        fs::write(
            sessions.join(segment_name(now - 119_000)),
            vec![b'x'; 3 * 1024 * 1024],
        )
        .unwrap();
        let hints = range_hints(Some(directory.path()), now);
        assert!(hints.latest.ends_with(" · ~3 МБ"), "{hints:?}");
        assert_eq!(hints.week, "1 подключение · ~3 МБ");
        index.start(now - 30_000, &start(false));
        let hints = range_hints(Some(directory.path()), now);
        assert!(hints.latest.contains("–сейчас"), "{hints:?}");
        assert_eq!(hints.all, "2 подключения · ~3 МБ");
    }

    #[test]
    fn connection_counts_follow_russian_and_english_grammar() {
        let forms = [1, 2, 5, 11, 12, 21, 22, 25, 111, 104]
            .map(connection_count)
            .join(", ");
        assert_eq!(
            forms,
            "1 подключение, 2 подключения, 5 подключений, 11 подключений, 12 подключений, \
             21 подключение, 22 подключения, 25 подключений, 111 подключений, 104 подключения"
        );
        let _english = multicore_i18n::ThreadLanguage::set(multicore_i18n::Lang::En);
        assert_eq!(connection_count(1), "1 connection");
        assert_eq!(connection_count(21), "21 connections");
        assert_eq!(approximate_size(10), "<1 MB");
        assert_eq!(approximate_size(5 * 1024 * 1024 + 100), "~5 MB");
    }

    #[test]
    fn timestamps_of_every_log_format_are_read() {
        assert_eq!(
            event_timestamp("{\"timestamp_ms\":1791064846123,\"x\":1}"),
            Some(1_791_064_846_123)
        );
        assert_eq!(event_timestamp("{\"other\":1}"), None);
        assert_eq!(
            service_log_timestamp("1791064846 prepare ok"),
            Some(1_791_064_846_000)
        );
        assert_eq!(service_log_timestamp("[redacted] x"), None);
    }
}
