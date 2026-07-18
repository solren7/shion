//! `shion logs` — print (and optionally follow) the gateway log.
//!
//! The gateway writes its tracing output to a daily-rotated file
//! (`~/.shion/logs/gateway.YYYY-MM-DD.log`, a month kept — see
//! `main.rs::open_gateway_log`), teed with stderr. This command reads the
//! newest daily file; the pre-rotation launchd capture
//! (`gateway.err.log`) is the fallback for logs from older builds.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How often follow mode polls the file for appended bytes.
const FOLLOW_POLL: Duration = Duration::from_millis(500);

pub fn run(lines: usize, follow: bool, stdout: bool) -> anyhow::Result<()> {
    let dir = crate::config::shion_home().join("logs");
    let path = if stdout {
        dir.join("gateway.log")
    } else {
        // Newest daily file, else the legacy launchd stderr capture.
        latest_daily_log(&dir).unwrap_or_else(|| dir.join("gateway.err.log"))
    };
    if !path.exists() {
        anyhow::bail!(
            "no log file at {} — has the gateway run yet? (check `shion gateway status`)",
            path.display()
        );
    }

    let out = std::io::stdout();
    let mut out = out.lock();

    // Print the last `lines` lines, keeping only that many in memory.
    let mut reader = BufReader::new(File::open(&path)?);
    let mut tail: VecDeque<String> = VecDeque::with_capacity(lines.min(1024) + 1);
    let mut line = String::new();
    while reader.read_line(&mut line)? != 0 {
        if tail.len() == lines {
            tail.pop_front();
        }
        tail.push_back(std::mem::take(&mut line));
    }
    for l in &tail {
        out.write_all(l.as_bytes())?;
    }
    out.flush()?;

    if !follow {
        return Ok(());
    }

    // Follow: stream bytes appended after the point we've already printed.
    let mut pos = reader.stream_position()?;
    loop {
        std::thread::sleep(FOLLOW_POLL);
        let mut file = File::open(&path)?;
        let len = file.metadata()?.len();
        if len < pos {
            // File was truncated or rotated — restart from the top.
            pos = 0;
        }
        if len > pos {
            file.seek(SeekFrom::Start(pos))?;
            let mut buf = Vec::new();
            let read = file.read_to_end(&mut buf)?;
            out.write_all(&buf)?;
            out.flush()?;
            pos += read as u64;
        }
    }
}

/// The newest `gateway.YYYY-MM-DD.log` in `dir`, if any. Date-stamped names
/// sort lexicographically in time order, so the max name is the newest day.
fn latest_daily_log(dir: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(is_daily_log_name)
        })
        .max()
}

/// `gateway.<10-char date>.log` — excludes the launchd captures
/// (`gateway.log` / `gateway.err.log`) and the TUI log.
fn is_daily_log_name(name: &str) -> bool {
    name.strip_prefix("gateway.")
        .and_then(|rest| rest.strip_suffix(".log"))
        .is_some_and(|date| {
            date.len() == 10 && date.chars().all(|c| c.is_ascii_digit() || c == '-')
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daily_log_names_are_recognized() {
        assert!(is_daily_log_name("gateway.2026-07-18.log"));
        assert!(!is_daily_log_name("gateway.log"));
        assert!(!is_daily_log_name("gateway.err.log"));
        assert!(!is_daily_log_name("chat-tui.log"));
    }

    #[test]
    fn newest_daily_file_wins() {
        let dir = std::env::temp_dir().join("shion_logs_test_newest");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for name in [
            "gateway.2026-07-16.log",
            "gateway.2026-07-18.log",
            "gateway.2026-07-17.log",
            "gateway.err.log",
        ] {
            std::fs::write(dir.join(name), "x").unwrap();
        }
        let newest = latest_daily_log(&dir).unwrap();
        assert_eq!(
            newest.file_name().unwrap().to_str().unwrap(),
            "gateway.2026-07-18.log"
        );
    }
}
