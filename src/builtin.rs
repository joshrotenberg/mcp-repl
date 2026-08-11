//! What a REPL command is, independent of what this REPL is about.
//!
//! Nothing here knows about MCP. A built-in is a name, a summary, a usage
//! line, and a paragraph, and the four readers of that record want different
//! fields: completion and highlighting want the name, the completion menu and
//! `find` want the summary, `help <name>` and the man page want all four.
//!
//! One record rather than several tables. Those readers used to draw from two
//! parallel arrays keyed by name, which is a shape that drifts: a command
//! could be listed for completion and undocumented, or documented and
//! unreachable. A test existed to catch exactly that, which is the sort of
//! test worth deleting by making the mistake unrepresentable.
//!
//! The definitions themselves belong to the application. This module holds
//! the type and the lookups.

/// One command the REPL provides itself, as opposed to one a server offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Builtin {
    /// The word typed at the prompt.
    pub name: &'static str,
    /// One line, for the completion menu, `help`, and `find`. No trailing
    /// period: it sits in a column beside other summaries.
    pub summary: &'static str,
    /// How to invoke it, in the usual `command <required> [optional]` form.
    pub usage: &'static str,
    /// What it does and what to watch out for, for `help <name>`.
    pub detail: &'static str,
}

/// The set a REPL was built with.
///
/// A slice rather than a map: these are read far more often than they are
/// searched, the sets are small, and a `const` slice keeps the definitions
/// readable in source order, which is the order `help` prints them in.
#[derive(Clone, Copy)]
pub struct Builtins(pub &'static [Builtin]);

impl Builtins {
    /// The command with this exact name.
    pub fn get(&self, name: &str) -> Option<&'static Builtin> {
        self.0.iter().find(|builtin| builtin.name == name)
    }

    /// Whether the word names a built-in.
    pub fn contains(&self, name: &str) -> bool {
        self.get(name).is_some()
    }

    /// Whether any built-in starts with this word, for highlighting a
    /// half-typed line as plausible rather than wrong.
    pub fn any_starts_with(&self, prefix: &str) -> bool {
        self.0
            .iter()
            .any(|builtin| builtin.name.starts_with(prefix))
    }

    /// Every command, in declaration order.
    pub fn iter(&self) -> impl Iterator<Item = &'static Builtin> {
        self.0.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: Builtins = Builtins(&[
        Builtin {
            name: "help",
            summary: "list the commands",
            usage: "help [command]",
            detail: "With no argument, list them. With one, explain it.",
        },
        Builtin {
            name: "quit",
            summary: "leave",
            usage: "quit",
            detail: "Close the session.",
        },
    ]);

    #[test]
    fn a_command_is_found_by_its_exact_name() {
        assert_eq!(SAMPLE.get("help").map(|b| b.usage), Some("help [command]"));
        assert!(SAMPLE.contains("quit"));
        // Not a prefix match: `hel` is not a command, however close it looks.
        assert!(!SAMPLE.contains("hel"));
        assert!(SAMPLE.get("nope").is_none());
    }

    #[test]
    fn a_prefix_is_recognised_separately_from_a_name() {
        // Highlighting asks this while a word is still being typed.
        assert!(SAMPLE.any_starts_with("hel"));
        assert!(SAMPLE.any_starts_with(""));
        assert!(!SAMPLE.any_starts_with("zz"));
    }

    #[test]
    fn iteration_follows_declaration_order() {
        // `help` prints them in this order, so it is part of the contract.
        assert_eq!(
            SAMPLE.iter().map(|b| b.name).collect::<Vec<_>>(),
            ["help", "quit"]
        );
    }
}
