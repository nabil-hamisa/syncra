//! Recent connections (`<data_dir>/recent.txt`): every ID this PC connected
//! to, newest first. Starred entries are kept for good and listed on top;
//! the others are trimmed to the last [`KEEP`].
//!
//! One entry per line: `*` or `-` (starred), the ID, the last connect time
//! (Unix seconds) and the signal server, separated by tabs.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// Unstarred entries kept.
pub const KEEP: usize = 10;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub id: u32,
    pub server: String,
    /// Last successful connect, Unix seconds.
    pub last: u64,
    pub starred: bool,
}

#[derive(Debug, Default)]
pub struct Recent {
    /// Display order: starred first, then newest first.
    entries: Vec<Entry>,
}

fn path() -> Option<PathBuf> {
    net::crypto::data_dir().ok().map(|d| d.join("recent.txt"))
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl Recent {
    pub const fn new() -> Self {
        Recent {
            entries: Vec::new(),
        }
    }

    pub fn load() -> Self {
        path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map_or_else(Recent::new, |t| Recent::parse(&t))
    }

    pub fn save(&self) {
        let Some(p) = path() else { return };
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(e) = std::fs::write(&p, self.to_text()) {
            tracing::warn!("could not save {}: {e}", p.display());
        }
    }

    fn parse(text: &str) -> Self {
        let mut r = Recent::new();
        for line in text.lines() {
            let mut f = line.split('\t');
            let (Some(star), Some(id), Some(last)) = (f.next(), f.next(), f.next()) else {
                continue;
            };
            let (Ok(id), Ok(last)) = (id.parse(), last.parse()) else {
                continue;
            };
            if r.entries.iter().any(|e| e.id == id) {
                continue;
            }
            r.entries.push(Entry {
                id,
                server: f.next().unwrap_or("").trim().to_string(),
                last,
                starred: star == "*",
            });
        }
        r.tidy();
        r
    }

    fn to_text(&self) -> String {
        self.entries
            .iter()
            .map(|e| {
                format!(
                    "{}\t{}\t{}\t{}\n",
                    if e.starred { "*" } else { "-" },
                    e.id,
                    e.last,
                    e.server
                )
            })
            .collect()
    }

    /// Sort into display order and drop unstarred entries past [`KEEP`].
    fn tidy(&mut self) {
        self.entries
            .sort_by(|a, b| b.starred.cmp(&a.starred).then(b.last.cmp(&a.last)));
        let mut unstarred = 0;
        self.entries.retain(|e| {
            if e.starred {
                return true;
            }
            unstarred += 1;
            unstarred <= KEEP
        });
    }

    /// A successful connect to `id` through `server` at `when`.
    pub fn touch(&mut self, id: u32, server: &str, when: u64) {
        match self.entries.iter_mut().find(|e| e.id == id) {
            Some(e) => {
                e.last = when;
                e.server = server.to_string();
            }
            None => self.entries.push(Entry {
                id,
                server: server.to_string(),
                last: when,
                starred: false,
            }),
        }
        self.tidy();
    }

    /// Star or unstar the entry at display index `i`. Returns its new
    /// index.
    pub fn toggle_star(&mut self, i: usize) -> Option<usize> {
        let e = self.entries.get_mut(i)?;
        e.starred = !e.starred;
        let id = e.id;
        self.tidy();
        self.position(id)
    }

    pub fn remove(&mut self, i: usize) {
        if i < self.entries.len() {
            self.entries.remove(i);
        }
    }

    pub fn get(&self, i: usize) -> Option<&Entry> {
        self.entries.get(i)
    }

    pub fn position(&self, id: u32) -> Option<usize> {
        self.entries.iter().position(|e| e.id == id)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

/// "just now", "5 min ago", "3 h ago", "yesterday", "4 days ago", …
pub fn ago(now: u64, then: u64) -> String {
    let s = now.saturating_sub(then);
    match s {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", s / 60),
        3600..86_400 => format!("{} h ago", s / 3600),
        86_400..172_800 => "yesterday".into(),
        _ if s < 14 * 86_400 => format!("{} days ago", s / 86_400),
        _ if s < 60 * 86_400 => format!("{} weeks ago", s / (7 * 86_400)),
        _ => format!("{} months ago", s / (30 * 86_400)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let mut r = Recent::new();
        r.touch(482_913_077, "1.2.3.4:7000", 100);
        r.touch(111_222_333, "5.6.7.8:7000", 200);
        r.toggle_star(1);
        let back = Recent::parse(&r.to_text());
        assert_eq!(back.entries, r.entries);
        assert!(back.get(0).unwrap().starred);
        assert_eq!(back.get(0).unwrap().id, 482_913_077);
    }

    #[test]
    fn starred_first_then_newest() {
        let mut r = Recent::new();
        r.touch(1, "", 10);
        r.touch(2, "", 20);
        r.touch(3, "", 30);
        assert_eq!(r.position(3), Some(0));
        assert_eq!(r.toggle_star(2), Some(0));
        r.touch(1, "", 40);
        let ids: Vec<u32> = (0..r.len()).map(|i| r.get(i).unwrap().id).collect();
        assert_eq!(ids, [1, 3, 2]);
    }

    #[test]
    fn trims_only_unstarred() {
        let mut r = Recent::new();
        r.touch(999, "", 0);
        r.toggle_star(0);
        for id in 0..(KEEP as u32 + 5) {
            r.touch(id, "", 1 + id as u64);
        }
        assert_eq!(r.len(), KEEP + 1);
        assert_eq!(r.get(0).unwrap().id, 999);
        // The oldest unstarred ones went.
        assert_eq!(r.position(0), None);
    }

    #[test]
    fn bad_lines_are_skipped() {
        let r = Recent::parse("garbage\n*\tx\t1\t\n-\t5\t9\ts\n-\t5\t10\ts\n");
        assert_eq!(r.len(), 1);
        assert_eq!(r.get(0).unwrap().server, "s");
    }

    #[test]
    fn ago_text() {
        assert_eq!(ago(100, 90), "just now");
        assert_eq!(ago(1000, 400), "10 min ago");
        assert_eq!(ago(10_000, 0), "2 h ago");
        assert_eq!(ago(100_000, 0), "yesterday");
        assert_eq!(ago(5 * 86_400, 0), "5 days ago");
        assert_eq!(ago(21 * 86_400, 0), "3 weeks ago");
        assert_eq!(ago(90 * 86_400, 0), "3 months ago");
        assert_eq!(ago(0, 100), "just now");
    }
}
