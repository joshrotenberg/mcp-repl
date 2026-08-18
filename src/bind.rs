//! Sticky default values for tool parameters, kept for the life of a
//! connection (#161).
//!
//! `bind <param>=<value>` sets a default value applied to any tool call whose
//! schema declares that parameter and whose call did not supply it. An
//! explicit argument always wins: a bind fills a gap, it never overrides.
//!
//! Values are stored raw, exactly as typed, and coerced to the type a
//! specific tool's schema declares only when a call actually uses them (see
//! `tool_args::apply_binds`). The same parameter name could in principle be
//! typed differently by two different tools on the same server, so there is
//! no single type to fix at bind time.
//!
//! Binds are per connection: `connect` clears them, the same way captured
//! variables, background tasks, and resource subscriptions already are, so a
//! session id bound for one server cannot leak into a same-named parameter
//! on another.

use std::collections::BTreeMap;

/// The binds in effect for the current connection.
#[derive(Default)]
pub struct Binds(BTreeMap<String, String>);

impl Binds {
    /// Set a bind, replacing any previous value. Returns the previous raw
    /// value, if there was one.
    pub fn set(&mut self, name: &str, value: &str) -> Option<String> {
        self.0.insert(name.to_string(), value.to_string())
    }

    /// Remove a bind. Returns its raw value, if it existed.
    pub fn remove(&mut self, name: &str) -> Option<String> {
        self.0.remove(name)
    }

    /// The raw value bound to `name`, if any.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0.get(name).map(String::as_str)
    }

    /// Every bind, name-sorted.
    pub fn entries(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
    }

    /// Whether no binds are set.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Drop every bind, e.g. when `connect` switches servers. Returns how
    /// many were dropped.
    pub fn clear(&mut self) -> usize {
        let count = self.0.len();
        self.0.clear();
        count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_reports_what_it_replaced() {
        let mut binds = Binds::default();
        assert_eq!(binds.set("session", "abc"), None);
        assert_eq!(binds.get("session"), Some("abc"));
        assert_eq!(binds.set("session", "def"), Some("abc".to_string()));
        assert_eq!(binds.get("session"), Some("def"));
    }

    #[test]
    fn remove_reports_the_value_that_was_there() {
        let mut binds = Binds::default();
        assert_eq!(binds.remove("session"), None);
        binds.set("session", "abc");
        assert_eq!(binds.remove("session"), Some("abc".to_string()));
        assert_eq!(binds.get("session"), None);
    }

    #[test]
    fn entries_are_name_sorted() {
        let mut binds = Binds::default();
        binds.set("z", "1");
        binds.set("a", "2");
        binds.set("m", "3");
        let names: Vec<&str> = binds.entries().map(|(name, _)| name).collect();
        assert_eq!(names, ["a", "m", "z"]);
    }

    #[test]
    fn is_empty_and_clear_track_the_count() {
        let mut binds = Binds::default();
        assert!(binds.is_empty());
        assert_eq!(binds.clear(), 0);
        binds.set("a", "1");
        binds.set("b", "2");
        assert!(!binds.is_empty());
        assert_eq!(binds.clear(), 2);
        assert!(binds.is_empty());
        assert_eq!(binds.get("a"), None);
    }
}
