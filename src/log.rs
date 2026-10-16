// One line per mate event, so the desk can read what happened without a transcript.

use std::{
    fs::OpenOptions,
    io::Write,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use super::Result;

#[derive(Clone, Copy)]
pub enum Event {
    Started,
    Linked,
    Owner,
    Brief,
    Closed,
}

impl Event {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Linked => "linked",
            Self::Owner => "owner",
            Self::Brief => "brief",
            Self::Closed => "closed",
        }
    }
}

// Unix seconds, event, detail. The detail stays on one line.
#[must_use]
pub fn line(at: u64, event: Event, detail: &str) -> String {
    format!("{at} {} {}", event.as_str(), detail.replace('\n', " "))
}

// Appends to `log` inside `dir`, creating the directory on first use.
pub fn append(dir: &Path, event: Event, detail: &str) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let at = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("log"))?;
    writeln!(file, "{}", line(at, event, detail))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_is_time_event_detail() {
        assert_eq!(
            line(1_700_000_000, Event::Started, "pane wAX:p1"),
            "1700000000 started pane wAX:p1"
        );
    }

    #[test]
    fn detail_stays_on_one_line() {
        assert_eq!(line(1, Event::Owner, "a\nb"), "1 owner a b");
    }

    #[test]
    fn append_writes_one_line_per_event() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("drover-log-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        append(&dir, Event::Started, "first")?;
        append(&dir, Event::Owner, "second")?;
        let text = std::fs::read_to_string(dir.join("log"))?;
        std::fs::remove_dir_all(&dir)?;
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines.first().is_some_and(|l| l.ends_with(" started first")));
        assert!(lines.last().is_some_and(|l| l.ends_with(" owner second")));
        Ok(())
    }
}
