//! Each plugin's log: its `console` output and the host's notes about it, in a bounded ring.

use std::collections::VecDeque;
use std::sync::Mutex;

use apitorrent_plugin_api::{PluginLogEntry, PluginLogLevel};

use crate::store::now_ms;

/// Longer messages are cut, so one log call can't fill the ring's memory.
const MAX_MESSAGE: usize = 8 * 1024;

#[derive(Debug)]
pub(crate) struct LogRing {
    plugin: String,
    capacity: usize,
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    entries: VecDeque<PluginLogEntry>,
    next_seq: u64,
}

impl LogRing {
    pub fn new(plugin: &str, capacity: usize) -> Self {
        Self {
            plugin: plugin.to_owned(),
            capacity: capacity.max(1),
            inner: Mutex::new(Inner {
                entries: VecDeque::new(),
                next_seq: 1,
            }),
        }
    }

    pub fn push(&self, level: PluginLogLevel, host: bool, message: &str) {
        let mut message = message.to_owned();
        if message.len() > MAX_MESSAGE {
            let mut cut = MAX_MESSAGE;
            while !message.is_char_boundary(cut) {
                cut -= 1;
            }
            message.truncate(cut);
            message.push_str(" … (truncated)");
        }
        let plugin = self.plugin.as_str();
        match (level, host) {
            (PluginLogLevel::Error, true) => tracing::warn!(plugin, "{message}"),
            (PluginLogLevel::Warn | PluginLogLevel::Error, _) | (_, true) => {
                tracing::info!(plugin, "{message}");
            }
            _ => tracing::debug!(plugin, "{message}"),
        }
        let mut inner = self.inner.lock().expect("log lock poisoned");
        let seq = inner.next_seq;
        inner.next_seq += 1;
        if inner.entries.len() == self.capacity {
            inner.entries.pop_front();
        }
        inner.entries.push_back(PluginLogEntry {
            seq,
            time: now_ms(),
            level,
            host,
            message,
        });
    }

    pub fn host(&self, level: PluginLogLevel, message: &str) {
        self.push(level, true, message);
    }

    pub fn since(&self, after: Option<u64>) -> Vec<PluginLogEntry> {
        let inner = self.inner.lock().expect("log lock poisoned");
        let after = after.unwrap_or(0);
        inner
            .entries
            .iter()
            .filter(|e| e.seq > after)
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_keeps_the_newest() {
        let ring = LogRing::new("t", 3);
        for i in 0..5 {
            ring.push(PluginLogLevel::Info, false, &format!("m{i}"));
        }
        let all = ring.since(None);
        assert_eq!(
            all.iter().map(|e| e.message.as_str()).collect::<Vec<_>>(),
            ["m2", "m3", "m4"]
        );
        assert_eq!(all[0].seq, 3);
        assert_eq!(ring.since(Some(4)).len(), 1);
        ring.push(PluginLogLevel::Info, false, &"é".repeat(MAX_MESSAGE));
        assert!(ring.since(Some(5))[0].message.ends_with("(truncated)"));
    }
}
