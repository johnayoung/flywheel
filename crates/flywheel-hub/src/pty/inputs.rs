//! The last input id a session took from each viewer key, so input a
//! viewer sends again after a drop is not typed twice (see
//! [`super::PtySession::write_input`]). Carried across a live handoff.

use std::collections::HashMap;

/// Viewer keys remembered per session; beyond, the least recently used
/// key is forgotten (a key is one browser pane, and every page load makes
/// new ones).
pub(crate) const INPUT_KEYS: usize = 64;

#[derive(Debug, Default)]
pub(crate) struct InputsSeen {
    /// key -> (last id taken, when it was last used).
    keys: HashMap<String, (u64, u64)>,
    clock: u64,
}

impl InputsSeen {
    /// Whether input `id` from `key` is new (ids only grow per key).
    pub(crate) fn take(&mut self, key: &str, id: u64) -> bool {
        self.clock += 1;
        let now = self.clock;
        if let Some((last, used)) = self.keys.get_mut(key) {
            *used = now;
            if id <= *last {
                return false;
            }
            *last = id;
            return true;
        }
        if self.keys.len() >= INPUT_KEYS
            && let Some(quietest) = self
                .keys
                .iter()
                .min_by_key(|(_, (_, used))| *used)
                .map(|(k, _)| k.clone())
        {
            self.keys.remove(&quietest);
        }
        self.keys.insert(key.to_owned(), (id, now));
        true
    }

    /// Whether input `id` from `key` got here already (nothing changes).
    pub(crate) fn seen(&self, key: &str, id: u64) -> bool {
        self.keys.get(key).is_some_and(|(last, _)| id <= *last)
    }

    /// Every key's last id, least recently used first (for a handoff).
    pub(crate) fn export(&self) -> Vec<(String, u64)> {
        let mut keys: Vec<_> = self.keys.iter().collect();
        keys.sort_by_key(|(_, (_, used))| *used);
        keys.into_iter()
            .map(|(key, (last, _))| (key.clone(), *last))
            .collect()
    }

    /// The table [`InputsSeen::export`] gave, in the same recency order
    /// (at most [`INPUT_KEYS`] kept: the most recent).
    pub(crate) fn restore(keys: Vec<(String, u64)>) -> Self {
        let mut seen = Self::default();
        let skip = keys.len().saturating_sub(INPUT_KEYS);
        for (key, last) in keys.into_iter().skip(skip) {
            seen.clock += 1;
            seen.keys.insert(key, (last, seen.clock));
        }
        seen
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_repeats_and_keeps_order_per_key() {
        let mut seen = InputsSeen::default();
        assert!(seen.take("a", 1));
        assert!(seen.take("a", 2));
        assert!(!seen.take("a", 2), "sent again after a drop");
        assert!(!seen.take("a", 1));
        assert!(seen.take("b", 1), "keys are independent");
        assert!(seen.take("a", 3));
    }

    #[test]
    fn export_restores_ids_and_recency() {
        let mut seen = InputsSeen::default();
        assert!(seen.take("old", 3));
        assert!(seen.take("new", 9));
        assert!(seen.seen("old", 3) && !seen.seen("old", 4) && !seen.seen("x", 1));
        let mut back = InputsSeen::restore(seen.export());
        assert!(!back.take("old", 3), "a resend after the handoff");
        assert!(!back.take("new", 9));
        assert!(back.take("new", 10));
        // Recency survives: the oldest key goes first once full.
        let mut back = InputsSeen::restore(seen.export());
        for n in 0..INPUT_KEYS - 1 {
            assert!(back.take(&format!("k{n}"), 1));
        }
        assert!(back.seen("new", 9) && !back.seen("old", 3));
        let many: Vec<_> = (0..2 * INPUT_KEYS as u64)
            .map(|n| (format!("k{n}"), n))
            .collect();
        assert_eq!(InputsSeen::restore(many).keys.len(), INPUT_KEYS);
    }

    #[test]
    fn forgets_the_least_recently_used_key_not_a_live_one() {
        let mut seen = InputsSeen::default();
        assert!(seen.take("live", 5));
        for n in 0..INPUT_KEYS - 1 {
            assert!(seen.take(&format!("reload-{n}"), 1));
            // The live pane keeps typing between every new key.
            if n % 8 == 0 {
                assert!(!seen.take("live", 5), "a resend of what it has");
            }
        }
        // Full; every later key evicts one -- never the one in use.
        for n in 0..4 * INPUT_KEYS {
            assert!(seen.take(&format!("late-{n}"), 1));
            assert!(!seen.take("live", 5), "evicted a key in use (round {n})");
        }
        assert_eq!(seen.keys.len(), INPUT_KEYS);
    }
}
