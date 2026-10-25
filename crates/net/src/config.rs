//! Persistent settings (`<data_dir>/settings.txt`, `key=value` lines),
//! currently just the signal server the windows connect through.

use std::path::PathBuf;

/// The signal server used when none is configured: `SYNCRA_SIGNAL` at build
/// time (e.g. `SYNCRA_SIGNAL=signal.example.com:7000 cargo build`), else none.
pub const DEFAULT_SIGNAL: &str = match option_env!("SYNCRA_SIGNAL") {
    Some(s) => s,
    None => "",
};

#[derive(Clone, Debug)]
pub struct Settings {
    pub signal: String,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            signal: DEFAULT_SIGNAL.into(),
        }
    }
}

fn path() -> Option<PathBuf> {
    crate::crypto::data_dir()
        .ok()
        .map(|d| d.join("settings.txt"))
}

impl Settings {
    pub fn load() -> Self {
        let mut s = Settings::default();
        let Some(text) = path().and_then(|p| std::fs::read_to_string(p).ok()) else {
            return s;
        };
        for line in text.lines() {
            if let Some((k, v)) = line.split_once('=') {
                if k.trim() == "signal" && !v.trim().is_empty() {
                    s.signal = v.trim().to_string();
                }
            }
        }
        s
    }

    pub fn save(&self) {
        let Some(p) = path() else { return };
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(e) = std::fs::write(&p, format!("signal={}\n", self.signal)) {
            tracing::warn!("could not save {}: {e}", p.display());
        }
    }
}
