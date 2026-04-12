//! Clipboard ring — port of controller/clipboard_ring.py
//!
//! Maintains a bounded history of clipboard entries, shared across
//! machines and Desks.  Pinned entries survive ring rotation.

use std::collections::VecDeque;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

const RING_SIZE: usize = 50;
const CONTENT_LIMIT: usize = 65536; // 64 KiB

// ── Entry ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClipboardEntry {
    pub id: u64,
    pub content: String,
    pub content_type: ContentType,
    pub source_node: String,
    pub source_desk: String,
    pub timestamp: f64,
    pub pinned: bool,
    pub preview: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ContentType {
    Text,
    Image,
    Url,
}

impl ClipboardEntry {
    pub fn to_summary(&self) -> EntrySummary {
        EntrySummary {
            id: self.id,
            preview: if self.preview.is_empty() {
                self.content.chars().take(100).collect()
            } else {
                self.preview.clone()
            },
            content_type: self.content_type.clone(),
            source: self.source_node.clone(),
            timestamp: self.timestamp,
            pinned: self.pinned,
            length: self.content.len(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntrySummary {
    pub id: u64,
    pub preview: String,
    pub content_type: ContentType,
    pub source: String,
    pub timestamp: f64,
    pub pinned: bool,
    pub length: usize,
}

// ── Ring ─────────────────────────────────────────────────────────────────────

pub struct ClipboardRing {
    entries: VecDeque<ClipboardEntry>,
    pinned: Vec<ClipboardEntry>,
    counter: u64,
}

impl ClipboardRing {
    pub fn new() -> Self {
        Self {
            entries: VecDeque::with_capacity(RING_SIZE),
            pinned: Vec::new(),
            counter: 0,
        }
    }

    /// Add a new item.  Returns the entry (possibly the existing one if
    /// it duplicates the most-recent entry).
    pub fn push(
        &mut self,
        content: impl Into<String>,
        source_node: impl Into<String>,
        source_desk: impl Into<String>,
        content_type: ContentType,
    ) -> ClipboardEntry {
        let content = {
            let s: String = content.into();
            if s.len() > CONTENT_LIMIT {
                s[..CONTENT_LIMIT].to_owned()
            } else {
                s
            }
        };

        // Deduplicate against most-recent entry
        if let Some(last) = self.entries.back() {
            if last.content == content {
                return last.clone();
            }
        }

        self.counter += 1;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);

        let preview: String = content
            .chars()
            .take(100)
            .collect::<String>()
            .replace('\n', " ");

        let entry = ClipboardEntry {
            id: self.counter,
            content,
            content_type,
            source_node: source_node.into(),
            source_desk: source_desk.into(),
            timestamp: now,
            pinned: false,
            preview,
        };

        if self.entries.len() == RING_SIZE {
            // Before dropping the oldest, check it isn't pinned
            if let Some(oldest) = self.entries.front() {
                if oldest.pinned {
                    // Move to pinned list before eviction
                    let oldest = oldest.clone();
                    if !self.pinned.iter().any(|p| p.id == oldest.id) {
                        self.pinned.push(oldest);
                    }
                }
            }
            self.entries.pop_front();
        }

        self.entries.push_back(entry.clone());
        entry
    }

    pub fn get(&self, id: u64) -> Option<&ClipboardEntry> {
        self.entries
            .iter()
            .find(|e| e.id == id)
            .or_else(|| self.pinned.iter().find(|e| e.id == id))
    }

    pub fn latest(&self) -> Option<&ClipboardEntry> {
        self.entries.back()
    }

    /// Return up to `limit` most-recent entries (newest first), preceded
    /// by all pinned entries.
    pub fn list(&self, limit: usize) -> Vec<EntrySummary> {
        let pinned: Vec<EntrySummary> = self.pinned.iter().map(|e| e.to_summary()).collect();

        let recent: Vec<EntrySummary> = self
            .entries
            .iter()
            .rev()
            .take(limit)
            .map(|e| e.to_summary())
            .collect();

        [pinned, recent].concat()
    }

    pub fn search(&self, query: &str) -> Vec<EntrySummary> {
        let q = query.to_lowercase();
        self.entries
            .iter()
            .filter(|e| e.content.to_lowercase().contains(&q))
            .rev()
            .take(20)
            .map(|e| e.to_summary())
            .collect()
    }

    pub fn pin(&mut self, id: u64) -> bool {
        if let Some(entry) = self.entries.iter_mut().find(|e| e.id == id) {
            entry.pinned = true;
            let cloned = entry.clone();
            if !self.pinned.iter().any(|p| p.id == id) {
                self.pinned.push(cloned);
            }
            return true;
        }
        false
    }

    pub fn unpin(&mut self, id: u64) -> bool {
        self.pinned.retain(|e| e.id != id);
        if let Some(entry) = self.entries.iter_mut().find(|e| e.id == id) {
            entry.pinned = false;
        }
        true
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

impl Default for ClipboardRing {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_and_get() {
        let mut ring = ClipboardRing::new();
        let e = ring.push("hello", "node1", "", ContentType::Text);
        assert_eq!(e.id, 1);
        assert_eq!(ring.get(1).unwrap().content, "hello");
    }

    #[test]
    fn deduplication() {
        let mut ring = ClipboardRing::new();
        ring.push("same", "n", "", ContentType::Text);
        ring.push("same", "n", "", ContentType::Text);
        assert_eq!(ring.list(50).len(), 1);
    }

    #[test]
    fn ring_rotation() {
        let mut ring = ClipboardRing::new();
        for i in 0..55u64 {
            ring.push(format!("item-{i}"), "", "", ContentType::Text);
        }
        // Only RING_SIZE entries kept
        assert_eq!(ring.entries.len(), RING_SIZE);
        // Oldest evicted
        assert!(ring.get(1).is_none());
    }

    #[test]
    fn pin_survives_rotation() {
        let mut ring = ClipboardRing::new();
        let e = ring.push("pinned", "", "", ContentType::Text);
        ring.pin(e.id);
        for i in 0..55u64 {
            ring.push(format!("filler-{i}"), "", "", ContentType::Text);
        }
        // Pinned entry still accessible
        assert!(ring.get(e.id).is_some());
    }

    #[test]
    fn search() {
        let mut ring = ClipboardRing::new();
        ring.push("hello world", "", "", ContentType::Text);
        ring.push("foo bar", "", "", ContentType::Text);
        let results = ring.search("hello");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].preview, "hello world");
    }

    #[test]
    fn content_truncated_at_64k() {
        let mut ring = ClipboardRing::new();
        let big = "x".repeat(100_000);
        let e = ring.push(big, "", "", ContentType::Text);
        assert_eq!(e.content.len(), CONTENT_LIMIT);
    }
}
