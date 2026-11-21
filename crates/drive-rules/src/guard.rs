//! Loop protection. A file a rule just touched (moved, copied, tagged) is
//! left alone by every rule for [`COOL_MS`]; what a rule made (a copy, a
//! moved file) is never taken by that same rule again, so a rule that
//! copies into a folder it watches cannot feed itself.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// How long every rule leaves a just-touched file alone.
pub const COOL_MS: i64 = 10 * 60 * 1000;
/// How long a rule remembers what it made.
pub const MADE_MS: i64 = 30 * 24 * 3600 * 1000;

#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
pub struct Touch {
    pub at: i64,
    pub rule: String,
}

#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
pub struct Guard {
    /// Item key → the last touch.
    pub touched: BTreeMap<String, Touch>,
    /// `<rule>|<item key>` and `<rule>|<content>` → when the rule made it.
    pub made: BTreeMap<String, i64>,
}

impl Guard {
    /// Why a rule must leave a file alone now, if it must.
    pub fn holds_off(&self, rule: &str, key: &str, content: &str, now: i64) -> Option<String> {
        if let Some(t) = self.touched.get(key)
            && now - t.at < COOL_MS
        {
            return Some(format!(
                "a rule touched it {} s ago (rules leave a file alone for {} min)",
                (now - t.at) / 1000,
                COOL_MS / 60_000
            ));
        }
        let made = |k: &str| {
            self.made
                .get(&format!("{rule}|{k}"))
                .is_some_and(|at| now - at < MADE_MS)
        };
        if made(key) || (!content.is_empty() && made(content)) {
            return Some("this rule made it (a rule never feeds itself)".into());
        }
        None
    }

    pub fn touch(&mut self, rule: &str, key: &str, now: i64) {
        self.touched.insert(
            key.to_string(),
            Touch {
                at: now,
                rule: rule.to_string(),
            },
        );
    }

    /// A rule made a file (its key, and its content when known).
    pub fn made(&mut self, rule: &str, key: &str, content: &str, now: i64) {
        self.touch(rule, key, now);
        self.made.insert(format!("{rule}|{key}"), now);
        if !content.is_empty() {
            self.made.insert(format!("{rule}|{content}"), now);
        }
    }

    /// Forget what no longer matters.
    pub fn prune(&mut self, now: i64) {
        self.touched.retain(|_, t| now - t.at < COOL_MS);
        self.made.retain(|_, at| now - *at < MADE_MS);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_touched_file_cools_down() {
        let mut g = Guard::default();
        g.touch("r1", "loc:a:x.wav", 1_000);
        assert!(
            g.holds_off("r2", "loc:a:x.wav", "", 1_000 + 60_000)
                .is_some()
        );
        assert!(
            g.holds_off("r2", "loc:a:x.wav", "", 1_000 + COOL_MS)
                .is_none()
        );
        assert!(g.holds_off("r2", "loc:a:y.wav", "", 2_000).is_none());
    }

    #[test]
    fn a_rule_never_feeds_itself() {
        let mut g = Guard::default();
        g.made("r1", "loc:a:Copies/x.wav", "h1", 0);
        let later = COOL_MS * 3;
        // Long after the cool-down, r1 still leaves its copy alone, by
        // path or by content (a copy of the copy elsewhere)…
        assert!(g.holds_off("r1", "loc:a:Copies/x.wav", "", later).is_some());
        assert!(g.holds_off("r1", "loc:b:other.wav", "h1", later).is_some());
        // …while other rules may take it.
        assert!(
            g.holds_off("r2", "loc:a:Copies/x.wav", "h1", later)
                .is_none()
        );
        g.prune(later);
        assert!(g.touched.is_empty());
        assert_eq!(g.made.len(), 2);
    }
}
