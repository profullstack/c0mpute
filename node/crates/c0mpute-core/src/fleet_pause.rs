//! "Pause" from the operator's fleet (c0mpute.com/dashboard/nodes,
//! `c0mpute fleet pause`, the fleet MCP tools): the worker stops bidding on new
//! jobs; jobs it already won finish. Survives restarts through a marker file
//! in the config dir, so a paused node does not resume by rebooting.

use std::sync::atomic::{AtomicBool, Ordering};

static PAUSED: AtomicBool = AtomicBool::new(false);
static LOADED: AtomicBool = AtomicBool::new(false);

const MARKER: &str = "fleet-paused";

fn marker() -> Option<std::path::PathBuf> {
    crate::config::config_dir().map(|d| d.join(MARKER))
}

/// Whether the operator paused this node. Reads the marker once per process.
pub fn paused() -> bool {
    if !LOADED.swap(true, Ordering::SeqCst) {
        if marker().is_some_and(|m| m.exists()) {
            PAUSED.store(true, Ordering::SeqCst);
        }
    }
    PAUSED.load(Ordering::SeqCst)
}

/// Pause (true) or resume (false), now and across restarts.
pub fn set(on: bool) -> std::io::Result<()> {
    LOADED.store(true, Ordering::SeqCst);
    PAUSED.store(on, Ordering::SeqCst);
    if let Some(m) = marker() {
        if on {
            if let Some(d) = m.parent() {
                std::fs::create_dir_all(d)?;
            }
            std::fs::write(&m, b"paused by the operator's fleet; `c0mpute fleet resume` or delete this file\n")?;
        } else if m.exists() {
            std::fs::remove_file(&m)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn set_flips_the_flag() {
        // Only the in-memory flag: the marker lives in the user's config dir.
        super::PAUSED.store(true, std::sync::atomic::Ordering::SeqCst);
        super::LOADED.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(super::paused());
        super::PAUSED.store(false, std::sync::atomic::Ordering::SeqCst);
        assert!(!super::paused());
    }
}
