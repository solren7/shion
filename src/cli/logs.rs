//! `shion logs` — print (and optionally follow) the gateway log.
//!
//! The gateway writes stderr (where the `tracing` subscriber writes) to
//! `~/.shion/logs/gateway.err.log` and stdout to `gateway.log` when launchd
//! manages it on macOS. Docker deployments usually read process logs through
//! Docker itself.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::time::Duration;

/// How often follow mode polls the file for appended bytes.
const FOLLOW_POLL: Duration = Duration::from_millis(500);

pub fn run(lines: usize, follow: bool, stdout: bool) -> anyhow::Result<()> {
    let name = if stdout {
        "gateway.log"
    } else {
        "gateway.err.log"
    };
    let path = crate::config::shion_home().join("logs").join(name);
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
