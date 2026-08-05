//! Discovery over the server surface: the `find` keyword search and the
//! did-you-mean suggestion printed for an unknown command word.
//!
//! A server with dozens of tools is not navigable by listing it. Both
//! functions here work off the cached [`Surface`], so neither issues a
//! request.

use crate::{BUILTINS, Surface};

/// Which list a hit came from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Tool,
    Prompt,
    Resource,
    Template,
    /// A REPL command rather than something the server offers. `find alias`
    /// should reach the alias command, not report that nothing matched.
    Builtin,
}

impl Kind {
    /// The heading this kind prints under, matching the list command that
    /// shows the same entries.
    pub fn heading(self) -> &'static str {
        match self {
            Kind::Tool => "tools",
            Kind::Prompt => "prompts",
            Kind::Resource => "resources",
            Kind::Template => "templates",
            Kind::Builtin => "built-ins",
        }
    }

    /// Group order for rendering: tools first, since they are what a user
    /// is usually hunting for.
    fn order(self) -> u8 {
        match self {
            Kind::Tool => 0,
            Kind::Prompt => 1,
            Kind::Resource => 2,
            Kind::Template => 3,
            // Last: a search is usually about the server, and the built-ins
            // are already one `help` away.
            Kind::Builtin => 4,
        }
    }
}

/// One matched surface entry.
#[derive(Clone, Debug)]
pub struct Hit {
    pub kind: Kind,
    /// The word you would type to use it: a tool or prompt name, a resource
    /// URI, a URI template.
    pub name: String,
    pub description: String,
    /// Higher is a better match. See [`score`].
    pub score: u32,
}

/// How well `query` matches an entry, given its typed name and description.
/// `None` means no match at all.
///
/// The ladder is deliberately coarse: an exact or prefix name match outranks
/// anything found only in prose, and a subsequence match (`gvd` for
/// `get_version_downloads`) is last so it never buries a literal one.
/// The ranking itself, over strings already folded (or deliberately not) by
/// the caller: exact name, then prefix, then substring, then description,
/// then subsequence, so a loose match never outranks a literal one.
fn score_prepared(query: &str, name: &str, description: &str) -> Option<u32> {
    if name == query {
        return Some(100);
    }
    if name.starts_with(query) {
        return Some(80);
    }
    if name.contains(query) {
        return Some(60);
    }
    if description.contains(query) {
        return Some(40);
    }
    if is_subsequence(query, name) {
        return Some(20);
    }
    None
}

/// True when every char of `needle` appears in `haystack` in order.
fn is_subsequence(needle: &str, haystack: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let mut chars = haystack.chars();
    needle.chars().all(|c| chars.any(|h| h == c))
}

/// A parsed `find` invocation.
///
/// The flags follow grep's, since `find` already follows its exit-status
/// convention: `-m` caps results, `--case-sensitive` stops folding case, and
/// a kind flag narrows which lists are searched.
#[derive(Debug, PartialEq, Eq)]
pub struct Query {
    /// The words to search for, joined, so `find crate info` is one phrase.
    pub text: String,
    /// Cap on results, applied after ranking so the best survive.
    pub limit: Option<usize>,
    /// Fold case (the default) or match exactly.
    pub case_sensitive: bool,
    /// Which lists to search. Empty means all of them.
    pub kinds: Vec<Kind>,
}

/// Parse `find [flags] <words...>`.
pub fn parse_query(tokens: &[&str]) -> Result<Query, String> {
    let mut text: Vec<String> = Vec::new();
    let mut limit = None;
    let mut case_sensitive = false;
    let mut kinds: Vec<Kind> = Vec::new();
    let mut rest = tokens.iter().copied();

    while let Some(token) = rest.next() {
        match token {
            "--tools" => kinds.push(Kind::Tool),
            "--prompts" => kinds.push(Kind::Prompt),
            "--resources" => kinds.push(Kind::Resource),
            "--templates" => kinds.push(Kind::Template),
            "--builtins" => kinds.push(Kind::Builtin),
            "--case-sensitive" => case_sensitive = true,
            "-m" | "--max" => {
                let raw = rest
                    .next()
                    .ok_or_else(|| format!("{token} needs a count"))?;
                limit = Some(parse_limit(token, raw)?);
            }
            _ => {
                if let Some(raw) = token.strip_prefix("--max=") {
                    limit = Some(parse_limit("--max", raw)?);
                } else if let Some(raw) = token.strip_prefix("-m") {
                    // `-m5`, the way grep also accepts it.
                    limit = Some(parse_limit("-m", raw)?);
                } else if token.starts_with('-') && token.len() > 1 {
                    return Err(format!(
                        "unknown option `{token}` (find takes -m/--max, --case-sensitive, \
                         and --tools/--prompts/--resources/--templates/--builtins)"
                    ));
                } else {
                    text.push(token.to_string());
                }
            }
        }
    }

    let text = text.join(" ");
    if text.is_empty() {
        return Err("usage: find [-m N] [--case-sensitive] [--tools|...] <keyword>".to_string());
    }
    kinds.sort_by_key(|kind| kind.order());
    kinds.dedup();
    Ok(Query {
        text,
        limit,
        case_sensitive,
        kinds,
    })
}

fn parse_limit(flag: &str, raw: &str) -> Result<usize, String> {
    match raw.parse::<usize>() {
        Ok(0) => Err(format!("{flag} must be at least 1")),
        Ok(n) => Ok(n),
        Err(_) => Err(format!("{flag} expects a number, got `{raw}`")),
    }
}

/// Every entry of the surface matching the parsed query, best first.
pub fn search_query(surface: &Surface, query: &Query) -> Vec<Hit> {
    let mut hits = search_filtered(surface, &query.text, query.case_sensitive, &query.kinds);
    if let Some(limit) = query.limit {
        hits.truncate(limit);
    }
    hits
}

/// Every entry of the surface matching `query`, best first.
///
/// Case-insensitive across tool, prompt, resource, and template names and
/// descriptions. Ties break on name so repeated searches print in a stable
/// order.
/// The search `find` actually runs: case folding and kind filtering are
/// options rather than fixed behaviour.
fn search_filtered(
    surface: &Surface,
    query: &str,
    case_sensitive: bool,
    kinds: &[Kind],
) -> Vec<Hit> {
    // Folding both sides is what makes the match case-insensitive; keeping
    // both as typed is what makes it exact.
    let query = if case_sensitive {
        query.to_string()
    } else {
        query.to_lowercase()
    };
    let wanted = |kind: Kind| kinds.is_empty() || kinds.contains(&kind);
    let mut hits = Vec::new();

    let mut push = |kind: Kind, name: &str, description: &str| {
        if !wanted(kind) {
            return;
        }
        let (name_key, description_key) = if case_sensitive {
            (name.to_string(), description.to_string())
        } else {
            (name.to_lowercase(), description.to_lowercase())
        };
        if let Some(score) = score_prepared(&query, &name_key, &description_key) {
            hits.push(Hit {
                kind,
                name: name.to_string(),
                description: description.to_string(),
                score,
            });
        }
    };

    for t in &surface.tools {
        push(Kind::Tool, &t.name, t.description.as_deref().unwrap_or(""));
    }
    for p in &surface.prompts {
        push(
            Kind::Prompt,
            &p.name,
            p.description.as_deref().unwrap_or(""),
        );
    }
    for r in &surface.resources {
        // A resource is read by URI, so that is the typed name; its own name
        // reads as description when it has none of its own.
        let description = r.description.clone().unwrap_or_else(|| r.name.clone());
        push(Kind::Resource, &r.uri, &description);
    }
    for t in &surface.templates {
        let description = t.description.clone().unwrap_or_else(|| t.name.clone());
        push(Kind::Template, &t.uri_template, &description);
    }
    for (name, description) in BUILTINS {
        push(Kind::Builtin, name, description);
    }

    hits.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| a.kind.order().cmp(&b.kind.order()))
            .then_with(|| a.name.cmp(&b.name))
    });
    hits
}

/// Hits grouped by kind, groups in [`Kind::order`], each group's hits still
/// ranked. This is what the `find` command prints.
pub fn grouped(hits: Vec<Hit>) -> Vec<(Kind, Vec<Hit>)> {
    let mut groups: Vec<(Kind, Vec<Hit>)> = Vec::new();
    for hit in hits {
        match groups.iter_mut().find(|(kind, _)| *kind == hit.kind) {
            Some((_, group)) => group.push(hit),
            None => groups.push((hit.kind, vec![hit])),
        }
    }
    groups.sort_by_key(|(kind, _)| kind.order());
    groups
}

/// Levenshtein distance, two rows.
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() {
        return b.len();
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// How far off a typo may be before a suggestion stops being useful. Short
/// words tolerate less: at distance 2, `read` is as close to `refresh` as to
/// half the surface.
fn tolerance(word: &str) -> usize {
    match word.chars().count() {
        0..=3 => 1,
        4..=8 => 2,
        _ => 3,
    }
}

/// The nearest command word to `word`, if one is close enough to be worth
/// printing. Considers built-ins and every tool and prompt name.
pub fn did_you_mean(surface: &Surface, word: &str) -> Option<String> {
    let lowered = word.to_lowercase();
    let max = tolerance(&lowered);
    let candidates = BUILTINS
        .iter()
        .map(|(name, _)| (*name).to_string())
        .chain(surface.tools.iter().map(|t| t.name.clone()))
        .chain(surface.prompts.iter().map(|p| p.name.clone()));

    let mut best: Option<(usize, String)> = None;
    for candidate in candidates {
        let distance = edit_distance(&lowered, &candidate.to_lowercase());
        if distance > max {
            continue;
        }
        // Ties go to the shorter name, then alphabetically, so the
        // suggestion does not depend on list order.
        let better = match &best {
            None => true,
            Some((best_distance, best_name)) => {
                (distance, candidate.len(), &candidate)
                    < (*best_distance, best_name.len(), best_name)
            }
        };
        if better {
            best = Some((distance, candidate));
        }
    }
    best.map(|(_, name)| name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_mcp::protocol::ToolDefinition;

    fn tool(name: &str, description: &str) -> ToolDefinition {
        serde_json::from_value(serde_json::json!({
            "name": name,
            "description": description,
            "inputSchema": {"type": "object"},
        }))
        .unwrap()
    }

    fn surface() -> Surface {
        Surface {
            tools: vec![
                tool("get_downloads", "Get download statistics"),
                tool("get_version_downloads", "Daily stats for one version"),
                tool("search_crates", "Find crates by name or keywords"),
                tool("get_owners", "Crate owners and maintainers"),
            ],
            prompts: vec![
                serde_json::from_value(serde_json::json!({
                    "name": "analyze_crate",
                    "description": "Comprehensive crate analysis",
                }))
                .unwrap(),
            ],
            resources: vec![
                serde_json::from_value(serde_json::json!({
                    "uri": "crates://serde/readme",
                    "name": "serde readme",
                }))
                .unwrap(),
            ],
            templates: vec![
                serde_json::from_value(serde_json::json!({
                    "uriTemplate": "crates://{name}/info",
                    "name": "crate info",
                    "description": "Registry metadata for a crate",
                }))
                .unwrap(),
            ],
        }
    }

    /// A plain search, the way `find` runs one with no flags.
    fn search(surface: &Surface, query: &str) -> Vec<Hit> {
        search_filtered(surface, query, false, &[])
    }

    fn q(tokens: &[&str]) -> Query {
        parse_query(tokens).expect("parses")
    }

    #[test]
    fn flags_are_separated_from_the_query() {
        let parsed = q(&["-m", "5", "--tools", "--case-sensitive", "crate", "info"]);
        // Non-flag words stay a phrase, in order.
        assert_eq!(parsed.text, "crate info");
        assert_eq!(parsed.limit, Some(5));
        assert!(parsed.case_sensitive);
        assert_eq!(parsed.kinds, vec![Kind::Tool]);
    }

    #[test]
    fn a_limit_is_accepted_the_ways_grep_accepts_it() {
        assert_eq!(q(&["-m", "3", "x"]).limit, Some(3));
        assert_eq!(q(&["-m3", "x"]).limit, Some(3));
        assert_eq!(q(&["--max", "3", "x"]).limit, Some(3));
        assert_eq!(q(&["--max=3", "x"]).limit, Some(3));
        assert_eq!(q(&["x"]).limit, None);
    }

    #[test]
    fn a_malformed_invocation_says_what_is_wrong() {
        for bad in [
            vec!["-m"],
            vec!["-m", "zero", "x"],
            vec!["-m", "0", "x"],
            vec!["--nope", "x"],
            vec!["--tools"],
        ] {
            assert!(parse_query(&bad).is_err(), "{bad:?} should not parse");
        }
    }

    #[test]
    fn kind_filters_narrow_the_search() {
        let s = surface();
        // `info` names a built-in and appears in a template URI.
        let all = search_query(&s, &q(&["info"]));
        assert!(all.iter().any(|h| h.kind == Kind::Builtin));
        assert!(all.iter().any(|h| h.kind == Kind::Template));

        let templates_only = search_query(&s, &q(&["--templates", "info"]));
        assert!(!templates_only.is_empty());
        assert!(templates_only.iter().all(|h| h.kind == Kind::Template));

        // Several filters union rather than intersect.
        let two = search_query(&s, &q(&["--templates", "--builtins", "info"]));
        assert!(two.len() > templates_only.len());
    }

    #[test]
    fn a_limit_keeps_the_best_matches() {
        let s = surface();
        let all = search_query(&s, &q(&["download"]));
        assert!(all.len() >= 2, "fixture should match more than one");
        let capped = search_query(&s, &q(&["-m", "1", "download"]));
        assert_eq!(capped.len(), 1);
        // The cap applies after ranking, so the survivor is the best one.
        assert_eq!(capped[0].name, all[0].name);
    }

    #[test]
    fn case_sensitivity_is_opt_in() {
        let s = surface();
        assert!(!search_query(&s, &q(&["GET_DOWNLOADS"])).is_empty());
        assert!(
            search_query(&s, &q(&["--case-sensitive", "GET_DOWNLOADS"])).is_empty(),
            "an exact-case search must not fold"
        );
        assert!(!search_query(&s, &q(&["--case-sensitive", "get_downloads"])).is_empty());
    }

    #[test]
    fn find_matches_names_across_every_kind() {
        let s = surface();
        let names = |q: &str| -> Vec<String> {
            search(&s, q)
                .into_iter()
                .map(|h| h.name)
                .collect::<Vec<_>>()
        };

        assert_eq!(
            names("download"),
            vec!["get_downloads", "get_version_downloads"]
        );
        assert_eq!(names("analyze"), vec!["analyze_crate"]);
        assert_eq!(names("readme"), vec!["crates://serde/readme"]);
        // `info` is also a built-in now. The exact name match ranks first,
        // ahead of the template that merely contains the word, which is the
        // documented ranking rather than a preference for either source.
        assert_eq!(names("info"), vec!["info", "crates://{name}/info"]);
    }

    #[test]
    fn built_ins_are_searchable_too() {
        let hits = search(&surface(), "alias");
        let names: Vec<&str> = hits.iter().map(|h| h.name.as_str()).collect();
        assert!(names.contains(&"alias"), "{names:?}");
        assert!(names.contains(&"unalias"), "{names:?}");
        assert!(hits.iter().all(|h| h.kind == Kind::Builtin));
    }

    #[test]
    fn find_matches_descriptions_too() {
        let hits = search(&surface(), "keywords");
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].name, "search_crates");
        // Prose-only, so it must rank below any name match.
        assert!(hits[0].score < 60);
    }

    #[test]
    fn find_is_case_insensitive() {
        assert_eq!(search(&surface(), "OWNERS").len(), 1);
    }

    #[test]
    fn a_subsequence_matches_but_ranks_last() {
        let hits = search(&surface(), "gtown");
        let names: Vec<&str> = hits.iter().map(|h| h.name.as_str()).collect();
        assert!(names.contains(&"get_owners"), "{names:?}");
        assert_eq!(
            hits.iter().find(|h| h.name == "get_owners").unwrap().score,
            20
        );
    }

    #[test]
    fn ranking_puts_the_literal_match_first() {
        let hits = search(&surface(), "get_owners");
        assert_eq!(hits[0].name, "get_owners");
        assert_eq!(hits[0].score, 100);
    }

    #[test]
    fn grouping_orders_kinds_and_keeps_rank_within_a_group() {
        let s = surface();
        let mut hits = search(&s, "crate");
        hits.push(Hit {
            kind: Kind::Tool,
            name: "zzz".to_string(),
            description: String::new(),
            score: 1,
        });
        let groups = grouped(hits);
        let kinds: Vec<Kind> = groups.iter().map(|(k, _)| *k).collect();
        assert_eq!(kinds[0], Kind::Tool, "{kinds:?}");
        let tools = &groups[0].1;
        assert!(
            tools.windows(2).all(|w| w[0].score >= w[1].score),
            "{tools:?}"
        );
    }

    #[test]
    fn no_match_is_empty() {
        assert!(search(&surface(), "kubernetes").is_empty());
    }

    #[test]
    fn did_you_mean_finds_the_near_miss() {
        let s = surface();
        assert_eq!(
            did_you_mean(&s, "serch_crates").as_deref(),
            Some("search_crates")
        );
        // Built-ins are candidates too.
        assert_eq!(did_you_mean(&s, "descrbe").as_deref(), Some("describe"));
        // And prompts.
        assert_eq!(
            did_you_mean(&s, "analyze_crat").as_deref(),
            Some("analyze_crate")
        );
    }

    #[test]
    fn did_you_mean_stays_quiet_when_nothing_is_close() {
        assert_eq!(did_you_mean(&surface(), "kubectl"), None);
    }

    #[test]
    fn short_words_get_a_tighter_tolerance() {
        // `read` is a built-in, so an exact word suggests itself; the point
        // here is that a 4-char typo does not reach a 7-char command.
        assert_eq!(did_you_mean(&surface(), "reab").as_deref(), Some("read"));
        assert_eq!(did_you_mean(&surface(), "xyz"), None);
    }

    #[test]
    fn edit_distance_basics() {
        assert_eq!(edit_distance("", "abc"), 3);
        assert_eq!(edit_distance("abc", ""), 3);
        assert_eq!(edit_distance("kitten", "sitting"), 3);
        assert_eq!(edit_distance("same", "same"), 0);
    }
}
