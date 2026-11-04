//! The last input id a session took from each viewer key, so input a
//! viewer sends again after a drop is not typed twice (see
//! [`super::PtySession::take_input`]).

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
