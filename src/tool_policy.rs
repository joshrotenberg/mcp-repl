//! Tool-call policy: allow and deny lists of raw MCP tool names, from a
//! server profile (`allow_tools`, `deny_tools`) and from the invocation
//! (`--allow-tool`, `--deny-tool`).
//!
//! Enforcement has exactly one home: tower-mcp's client-side
//! `McpClientBuilder::tool_call_policy` hook, which every `tools/call` the
//! client prepares passes through before a request id is allocated. A denied
//! call is never sent. This module supplies the decision and nothing else
//! enforces it. Listings consult the same [`ToolPolicy::decide`] only to
//! decide what to show.
//!
//! Matching is exact and case-sensitive against the raw tool name. There are
//! no patterns: an allow list written as a glob would silently admit tools a
//! server adds later. Entries containing `*`, `?`, `[` or `]` are refused so
//! that syntax stays available for a deliberate future design.
//!
//! Precedence: an invocation can narrow a profile but never widen it. The
//! effective allow list is the intersection of every allow list that is set;
//! the effective deny list is the union. Deny wins over allow.
//!
//! This is a client-side control. It is not a sandbox, it does not replace
//! server authorization, and server annotations such as `readOnlyHint` play
//! no part in it.

use std::collections::BTreeSet;
use std::sync::{Arc, RwLock};

use tower_mcp::{ToolCallContext, ToolCallDecision, ToolCallPolicy};

/// One source of allow and deny entries: a profile, or one invocation flag.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layer {
    /// How the source is named in diagnostics: `profile work`,
    /// `--allow-tool`, `--deny-tool`.
    pub source: String,
    /// `None` means this source does not restrict by allow list. An empty
    /// set allows nothing.
    pub allow: Option<BTreeSet<String>>,
    pub deny: BTreeSet<String>,
}

impl Layer {
    fn new(source: String, allow: Option<&[String]>, deny: &[String]) -> Result<Self, String> {
        let check = |list: &[String]| -> Result<BTreeSet<String>, String> {
            list.iter()
                .map(|entry| validate_entry(&source, entry).map(|()| entry.clone()))
                .collect()
        };
        Ok(Self {
            allow: allow.map(check).transpose()?,
            deny: check(deny)?,
            source,
        })
    }

    fn is_empty(&self) -> bool {
        self.allow.is_none() && self.deny.is_empty()
    }
}

/// A tool or profile name as it appears in a human-facing line, with control
/// characters (newlines included) made visible. Matching, the hook's reason,
/// and JSON keep the raw value.
fn inline(text: &str) -> std::borrow::Cow<'_, str> {
    crate::untrusted::sanitize_inline(text)
}

fn inline_list<'a>(items: impl IntoIterator<Item = &'a str>) -> String {
    items
        .into_iter()
        .map(|item| inline(item).into_owned())
        .collect::<Vec<_>>()
        .join(", ")
}

fn validate_entry(source: &str, entry: &str) -> Result<(), String> {
    if entry.is_empty() {
        return Err(format!("{}: a tool policy entry is empty", inline(source)));
    }
    if entry.contains(['*', '?', '[', ']']) {
        return Err(format!(
            "{}: `{}` looks like a pattern; tool policy entries are exact tool names",
            inline(source),
            inline(entry)
        ));
    }
    Ok(())
}

/// The invocation-level lists, from `--allow-tool` and `--deny-tool`.
#[derive(Clone, Debug, Default)]
pub struct Flags {
    pub allow: Vec<String>,
    pub deny: Vec<String>,
}

/// Which list decided a denial.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rule {
    /// An allow list is in effect and does not name the tool.
    Allow,
    /// A deny list names the tool.
    Deny,
}

impl Rule {
    pub fn label(self) -> &'static str {
        match self {
            Rule::Allow => "allow",
            Rule::Deny => "deny",
        }
    }
}

/// Why a tool call is denied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Denial {
    pub rule: Rule,
    /// The sources responsible, in precedence order.
    pub sources: Vec<String>,
}

impl Denial {
    /// The reason handed to tower-mcp and shown after the tool name.
    pub fn reason(&self) -> String {
        let what = match self.rule {
            Rule::Allow => "not in the allow list",
            Rule::Deny => "in the deny list",
        };
        format!("{what} ({})", self.sources.join(", "))
    }
}

/// The effective policy for one connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolPolicy {
    layers: Vec<Layer>,
    allow: Option<BTreeSet<String>>,
    deny: BTreeSet<String>,
}

impl ToolPolicy {
    /// Combine a profile's lists (if the target is a profile) with the
    /// invocation flags. `Ok(None)` means nothing is configured, so no hook
    /// is installed and behavior is exactly as without this feature.
    pub fn resolve(
        profile: Option<(&str, &crate::config::Profile)>,
        flags: &Flags,
    ) -> Result<Option<Self>, String> {
        let mut layers = Vec::new();
        if let Some((name, profile)) = profile {
            layers.push(Layer::new(
                format!("profile {name}"),
                profile.allow_tools.as_deref(),
                &profile.deny_tools,
            )?);
        }
        if !flags.allow.is_empty() {
            layers.push(Layer::new("--allow-tool".into(), Some(&flags.allow), &[])?);
        }
        if !flags.deny.is_empty() {
            layers.push(Layer::new("--deny-tool".into(), None, &flags.deny)?);
        }
        Ok(Self::from_layers(layers))
    }

    /// The effective policy of these layers, or `None` when none restricts.
    pub fn from_layers(layers: Vec<Layer>) -> Option<Self> {
        let layers: Vec<Layer> = layers.into_iter().filter(|l| !l.is_empty()).collect();
        if layers.is_empty() {
            return None;
        }
        let allow = layers
            .iter()
            .filter_map(|layer| layer.allow.clone())
            .reduce(|kept, next| kept.intersection(&next).cloned().collect());
        let deny = layers
            .iter()
            .flat_map(|layer| layer.deny.iter().cloned())
            .collect();
        Some(Self {
            layers,
            allow,
            deny,
        })
    }

    /// `None` allows the call. Deny is checked first, so a name on both
    /// lists is denied.
    pub fn decide(&self, name: &str) -> Option<Denial> {
        if self.deny.contains(name) {
            return Some(Denial {
                rule: Rule::Deny,
                sources: self
                    .layers
                    .iter()
                    .filter(|layer| layer.deny.contains(name))
                    .map(|layer| layer.source.clone())
                    .collect(),
            });
        }
        match &self.allow {
            Some(allow) if !allow.contains(name) => Some(Denial {
                rule: Rule::Allow,
                sources: self
                    .layers
                    .iter()
                    .filter(|layer| layer.allow.as_ref().is_some_and(|a| !a.contains(name)))
                    .map(|layer| layer.source.clone())
                    .collect(),
            }),
            _ => None,
        }
    }

    pub fn permits(&self, name: &str) -> bool {
        self.decide(name).is_none()
    }

    pub fn sources(&self) -> Vec<&str> {
        self.layers.iter().map(|l| l.source.as_str()).collect()
    }

    /// Configuration that cannot mean what it says: a name both allowed and
    /// denied, or a flag that tries to widen a profile's allow list.
    ///
    /// Overlaps are found in the configured lists, before any intersection:
    /// a name on both of a profile's lists is reported even when a flag's
    /// allow list has already narrowed it out of the effective allow list.
    pub fn warnings(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut allowed_by: std::collections::BTreeMap<&str, Vec<&str>> = Default::default();
        for layer in &self.layers {
            for name in layer.allow.iter().flatten() {
                allowed_by
                    .entry(name.as_str())
                    .or_default()
                    .push(layer.source.as_str());
            }
        }
        for (name, allow_sources) in &allowed_by {
            if !self.deny.contains(*name) {
                continue;
            }
            let deny_sources = self
                .layers
                .iter()
                .filter(|layer| layer.deny.contains(*name))
                .map(|layer| layer.source.as_str());
            out.push(format!(
                "tool policy: `{}` is both allowed ({}) and denied ({}); deny wins",
                inline(name),
                inline_list(allow_sources.iter().copied()),
                inline_list(deny_sources)
            ));
        }
        let mut seen = BTreeSet::new();
        for layer in &self.layers {
            for name in layer.allow.iter().flatten() {
                if self.permits(name) || self.deny.contains(name) || !seen.insert(name) {
                    continue;
                }
                let narrower: Vec<&str> = self
                    .layers
                    .iter()
                    .filter(|other| other.allow.as_ref().is_some_and(|a| !a.contains(name)))
                    .map(|other| other.source.as_str())
                    .collect();
                out.push(format!(
                    "tool policy: `{}` from {} stays denied: it is not in the allow list of {}, \
                     and an allow list can only be narrowed",
                    inline(name),
                    inline(&layer.source),
                    inline_list(narrower)
                ));
            }
        }
        out
    }

    /// The machine form shown by `info`. Tool and source names only.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "allow": self.allow,
            "deny": self.deny,
            "sources": self.sources(),
        })
    }

    /// One human line for the banner and `info`.
    pub fn summary(&self) -> String {
        let allow = match &self.allow {
            None => "all tools allowed".to_string(),
            Some(allow) if allow.is_empty() => "no tools allowed".to_string(),
            Some(allow) => format!("allow {}", inline_list(allow.iter().map(String::as_str))),
        };
        let deny = if self.deny.is_empty() {
            String::new()
        } else {
            format!(
                "; deny {}",
                inline_list(self.deny.iter().map(String::as_str))
            )
        };
        format!(
            "tool policy: {allow}{deny} (from {})",
            inline_list(self.sources())
        )
    }
}

#[async_trait::async_trait]
impl ToolCallPolicy for ToolPolicy {
    async fn authorize(&self, call: ToolCallContext<'_>) -> ToolCallDecision {
        match self.decide(call.name) {
            None => ToolCallDecision::Allow,
            Some(denial) => ToolCallDecision::Deny {
                reason: denial.reason(),
            },
        }
    }
}

/// The policy of the live connection, for presentation only: listings,
/// `info`, and the machine error. Installed alongside each client built with
/// the same policy, and replaced on `connect`.
static CURRENT: RwLock<Option<Arc<ToolPolicy>>> = RwLock::new(None);

pub fn install(policy: Option<Arc<ToolPolicy>>) {
    *CURRENT.write().unwrap_or_else(|e| e.into_inner()) = policy;
}

pub fn current() -> Option<Arc<ToolPolicy>> {
    CURRENT.read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Whether listings should leave this tool out: only tools whose calls the
/// current policy denies.
pub fn hides(name: &str) -> bool {
    current().is_some_and(|policy| !policy.permits(name))
}

/// The tools a listing shows, and how many it leaves out: every tool when
/// `all` is set or no policy applies, otherwise only those whose calls the
/// policy permits. Presentation only; dispatch always sees the full surface,
/// so a hidden tool still reaches the hook and gets the policy error.
pub fn listed_tools<'a>(
    tools: &'a [tower_mcp::protocol::ToolDefinition],
    policy: Option<&ToolPolicy>,
    all: bool,
) -> (Vec<&'a tower_mcp::protocol::ToolDefinition>, usize) {
    let shown: Vec<_> = tools
        .iter()
        .filter(|tool| all || policy.is_none_or(|policy| policy.permits(&tool.name)))
        .collect();
    let hidden = tools.len() - shown.len();
    (shown, hidden)
}

/// The footer for a listing that left `hidden` tools out.
pub fn hidden_note(hidden: usize, all_flag: Option<&str>) -> String {
    let noun = if hidden == 1 { "tool" } else { "tools" };
    match all_flag {
        Some(flag) => {
            format!("{hidden} {noun} hidden: calls are denied by tool policy; `{flag}` lists them")
        }
        None => format!("{hidden} {noun} hidden: calls are denied by tool policy"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Profile;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn profile(allow: Option<&[&str]>, deny: &[&str]) -> Profile {
        Profile {
            allow_tools: allow.map(names),
            deny_tools: names(deny),
            ..Profile::default()
        }
    }

    fn flags(allow: &[&str], deny: &[&str]) -> Flags {
        Flags {
            allow: names(allow),
            deny: names(deny),
        }
    }

    #[test]
    fn nothing_configured_installs_no_policy() {
        let p = profile(None, &[]);
        assert_eq!(
            ToolPolicy::resolve(Some(("work", &p)), &Flags::default()),
            Ok(None)
        );
        assert_eq!(ToolPolicy::resolve(None, &Flags::default()), Ok(None));
    }

    #[test]
    fn allow_only_admits_exactly_the_listed_names() {
        let p = profile(Some(&["search", "get_issue"]), &[]);
        let policy = ToolPolicy::resolve(Some(("work", &p)), &Flags::default())
            .unwrap()
            .unwrap();
        assert!(policy.permits("search"));
        assert!(policy.permits("get_issue"));
        let denial = policy.decide("delete_repo").unwrap();
        assert_eq!(denial.rule, Rule::Allow);
        assert_eq!(denial.sources, ["profile work"]);
        assert_eq!(denial.reason(), "not in the allow list (profile work)");
        // A tool the server adds later is not in the list either.
        assert!(!policy.permits("new_tool"));
    }

    #[test]
    fn deny_only_blocks_exactly_the_listed_names() {
        let policy = ToolPolicy::resolve(None, &flags(&[], &["delete_repo"]))
            .unwrap()
            .unwrap();
        assert!(policy.permits("search"));
        assert!(policy.permits("new_tool"));
        let denial = policy.decide("delete_repo").unwrap();
        assert_eq!(denial.rule, Rule::Deny);
        assert_eq!(denial.reason(), "in the deny list (--deny-tool)");
    }

    #[test]
    fn deny_wins_when_both_name_a_tool() {
        let p = profile(Some(&["search", "get_issue"]), &["get_issue"]);
        let policy = ToolPolicy::resolve(Some(("work", &p)), &Flags::default())
            .unwrap()
            .unwrap();
        assert!(policy.permits("search"));
        assert_eq!(policy.decide("get_issue").unwrap().rule, Rule::Deny);
        assert_eq!(
            policy.warnings(),
            [
                "tool policy: `get_issue` is both allowed (profile work) and denied (profile work); deny wins"
            ]
        );
    }

    #[test]
    fn an_empty_allow_list_denies_every_call() {
        let p = profile(Some(&[]), &[]);
        let policy = ToolPolicy::resolve(Some(("browse", &p)), &Flags::default())
            .unwrap()
            .unwrap();
        assert!(!policy.permits("anything"));
        assert!(policy.summary().contains("no tools allowed"));
    }

    #[test]
    fn flags_narrow_a_profile_but_never_widen_it() {
        let p = profile(Some(&["search", "get_issue"]), &["purge"]);
        let policy = ToolPolicy::resolve(
            Some(("work", &p)),
            &flags(&["search", "delete_repo"], &["get_issue"]),
        )
        .unwrap()
        .unwrap();
        assert_eq!(policy.to_json()["allow"], serde_json::json!(["search"]));
        assert!(policy.permits("search"));
        // Allowed by the flag, but the profile's allow list does not name it.
        let widened = policy.decide("delete_repo").unwrap();
        assert_eq!(widened.rule, Rule::Allow);
        assert_eq!(widened.sources, ["profile work"]);
        // Denied by the flag.
        assert_eq!(policy.decide("get_issue").unwrap().sources, ["--deny-tool"]);
        assert_eq!(policy.decide("purge").unwrap().sources, ["profile work"]);
        assert_eq!(
            policy.warnings(),
            [
                "tool policy: `get_issue` is both allowed (profile work) and denied \
                 (--deny-tool); deny wins",
                "tool policy: `delete_repo` from --allow-tool stays denied: it is not in the \
                 allow list of profile work, and an allow list can only be narrowed",
            ]
        );
        assert_eq!(
            policy.sources(),
            ["profile work", "--allow-tool", "--deny-tool"]
        );
    }

    #[test]
    fn a_non_profile_target_uses_the_flags_alone() {
        let policy = ToolPolicy::resolve(None, &flags(&["search"], &[]))
            .unwrap()
            .unwrap();
        assert!(policy.permits("search"));
        assert_eq!(
            policy.decide("other").unwrap().reason(),
            "not in the allow list (--allow-tool)"
        );
    }

    #[test]
    fn matching_is_exact_and_case_sensitive() {
        let policy = ToolPolicy::resolve(None, &flags(&["get_issue"], &[]))
            .unwrap()
            .unwrap();
        for near_miss in [
            "Get_Issue",
            "get_issue ",
            " get_issue",
            "get-issue",
            "get_issu",
        ] {
            assert!(!policy.permits(near_miss), "{near_miss:?}");
        }
    }

    #[test]
    fn patterns_and_empty_entries_are_refused() {
        for entry in ["git_*", "get?", "[ab]", ""] {
            let err = ToolPolicy::resolve(None, &flags(&[entry], &[])).unwrap_err();
            assert!(err.starts_with("--allow-tool: "), "{err}");
        }
        let p = profile(None, &["x*"]);
        let err = ToolPolicy::resolve(Some(("work", &p)), &Flags::default()).unwrap_err();
        assert!(err.contains("profile work"), "{err}");
        assert!(err.contains("exact tool names"), "{err}");
    }

    #[test]
    fn the_machine_form_names_lists_and_sources_only() {
        let p = profile(Some(&["search"]), &[]);
        let policy = ToolPolicy::resolve(Some(("work", &p)), &flags(&[], &["purge"]))
            .unwrap()
            .unwrap();
        assert_eq!(
            policy.to_json(),
            serde_json::json!({
                "allow": ["search"],
                "deny": ["purge"],
                "sources": ["profile work", "--deny-tool"],
            })
        );
        let deny_only = ToolPolicy::resolve(None, &flags(&[], &["purge"]))
            .unwrap()
            .unwrap();
        assert!(deny_only.to_json()["allow"].is_null());
        assert_eq!(
            deny_only.summary(),
            "tool policy: all tools allowed; deny purge (from --deny-tool)"
        );
    }

    #[tokio::test]
    async fn the_hook_decision_matches_decide() {
        let policy = ToolPolicy::resolve(None, &flags(&["search"], &["purge"]))
            .unwrap()
            .unwrap();
        let allowed = serde_json::json!({ "name": "search", "arguments": {} });
        assert_eq!(
            policy
                .authorize(ToolCallContext::new("search", &allowed, None, 0))
                .await,
            ToolCallDecision::Allow
        );
        let denied = serde_json::json!({ "name": "purge", "arguments": {} });
        assert_eq!(
            policy
                .authorize(ToolCallContext::new("purge", &denied, None, 0))
                .await,
            ToolCallDecision::Deny {
                reason: "in the deny list (--deny-tool)".to_string()
            }
        );
    }

    #[test]
    fn listings_leave_out_only_denied_tools() {
        let tool = |name: &str| tower_mcp::protocol::ToolDefinition {
            name: name.to_string(),
            title: None,
            description: None,
            input_schema: serde_json::json!({ "type": "object" }),
            output_schema: None,
            icons: None,
            annotations: None,
            execution: None,
            meta: None,
        };
        let tools = vec![tool("search"), tool("purge"), tool("get_issue")];
        let names = |listed: &[&tower_mcp::protocol::ToolDefinition]| {
            listed.iter().map(|t| t.name.clone()).collect::<Vec<_>>()
        };

        let (shown, hidden) = listed_tools(&tools, None, false);
        assert_eq!((shown.len(), hidden), (3, 0));

        let policy = ToolPolicy::resolve(None, &flags(&[], &["purge"]))
            .unwrap()
            .unwrap();
        let (shown, hidden) = listed_tools(&tools, Some(&policy), false);
        assert_eq!(names(&shown), ["search", "get_issue"]);
        assert_eq!(hidden, 1);

        let (shown, hidden) = listed_tools(&tools, Some(&policy), true);
        assert_eq!((shown.len(), hidden), (3, 0));
    }

    /// The overlap is in the configured lists, so it is reported even when a
    /// flag has already narrowed the name out of the effective allow list.
    #[test]
    fn an_overlap_is_reported_before_intersecting() {
        let p = profile(Some(&["x", "y"]), &["x"]);
        let policy = ToolPolicy::resolve(Some(("work", &p)), &flags(&["y"], &[]))
            .unwrap()
            .unwrap();
        assert_eq!(policy.to_json()["allow"], serde_json::json!(["y"]));
        assert_eq!(
            policy.warnings(),
            [
                "tool policy: `x` is both allowed (profile work) and denied (profile work); deny wins"
            ]
        );
    }

    /// Names are escaped in human-facing text and raw everywhere else.
    #[test]
    fn names_with_control_characters_stay_on_one_line() {
        let p = profile(Some(&["ok\nerror: forged"]), &["ok\nerror: forged"]);
        let policy = ToolPolicy::resolve(Some(("pro\nfile", &p)), &Flags::default())
            .unwrap()
            .unwrap();
        for line in policy.warnings().iter().chain([&policy.summary()]) {
            assert!(!line.contains('\n'), "{line:?}");
            assert!(line.contains("ok\\nerror: forged"), "{line:?}");
            assert!(line.contains("profile pro\\nfile"), "{line:?}");
        }
        // Matching, the hook reason, and JSON keep the raw values.
        let denial = policy.decide("ok\nerror: forged").unwrap();
        assert_eq!(denial.sources, ["profile pro\nfile"]);
        assert_eq!(
            policy.to_json()["deny"],
            serde_json::json!(["ok\nerror: forged"])
        );
        let err = ToolPolicy::resolve(None, &flags(&["a\n*"], &[])).unwrap_err();
        assert!(!err.contains('\n'), "{err:?}");
    }

    #[test]
    fn the_hidden_note_explains_omission() {
        assert_eq!(
            hidden_note(1, Some("tools --all")),
            "1 tool hidden: calls are denied by tool policy; `tools --all` lists them"
        );
        assert_eq!(
            hidden_note(2, None),
            "2 tools hidden: calls are denied by tool policy"
        );
    }

    /// Every decision matches a direct set model of the documented rules:
    /// invalid entries are refused, nothing configured installs nothing, and
    /// otherwise a name is allowed exactly when no deny list holds it and
    /// every allow list that is set does.
    #[test]
    fn property_tool_policy_decisions_match_the_set_model() {
        use crate::property::{GENERATED_CASES, Generator, TOOL_POLICY_REGRESSIONS};

        for (listed, probe) in TOOL_POLICY_REGRESSIONS {
            let allow = ToolPolicy::resolve(None, &flags(&[listed], &[]))
                .unwrap()
                .unwrap();
            assert!(allow.permits(listed), "{listed:?}");
            assert!(!allow.permits(probe), "{listed:?} admitted {probe:?}");
            let deny = ToolPolicy::resolve(None, &flags(&[], &[listed]))
                .unwrap()
                .unwrap();
            assert!(!deny.permits(listed), "{listed:?}");
            assert!(deny.permits(probe), "{listed:?} denied {probe:?}");
        }

        let mut generator = Generator::new(0x746f_6f6c_706f_6c69);
        let (mut refused, mut unconfigured, mut decided) = (0, 0, 0);
        for _ in 0..GENERATED_CASES {
            let pool: Vec<String> = (0..4).map(|_| generator.text(4)).collect();
            let pick = |generator: &mut Generator| -> Vec<String> {
                let count = generator.index(3);
                (0..count)
                    .map(|_| pool[generator.index(pool.len())].clone())
                    .collect()
            };
            let profile_allow = (generator.index(2) == 0).then(|| pick(&mut generator));
            let profile_deny = pick(&mut generator);
            let flag_allow = pick(&mut generator);
            let flag_deny = pick(&mut generator);
            let profile = Profile {
                allow_tools: profile_allow.clone(),
                deny_tools: profile_deny.clone(),
                ..Profile::default()
            };
            let resolved = ToolPolicy::resolve(
                Some(("p", &profile)),
                &Flags {
                    allow: flag_allow.clone(),
                    deny: flag_deny.clone(),
                },
            );

            let every_entry = profile_allow
                .iter()
                .flatten()
                .chain(&profile_deny)
                .chain(&flag_allow)
                .chain(&flag_deny);
            let valid = every_entry
                .clone()
                .all(|e| !e.is_empty() && !e.contains(['*', '?', '[', ']']));
            if !valid {
                assert!(resolved.is_err(), "accepted an invalid entry: {resolved:?}");
                refused += 1;
                continue;
            }
            let resolved = resolved.expect("valid entries resolve");
            let configured = profile_allow.is_some() || every_entry.clone().next().is_some();
            let Some(policy) = resolved else {
                assert!(!configured, "configured lists installed no policy");
                unconfigured += 1;
                continue;
            };
            decided += 1;
            assert!(configured, "an empty configuration installed a policy");

            let allow_lists: Vec<&Vec<String>> = profile_allow
                .iter()
                .chain((!flag_allow.is_empty()).then_some(&flag_allow))
                .collect();
            for probe in pool.iter().chain(std::iter::once(&generator.text(4))) {
                let denied = profile_deny.contains(probe) || flag_deny.contains(probe);
                let allowed = !denied && allow_lists.iter().all(|list| list.contains(probe));
                assert_eq!(
                    policy.permits(probe),
                    allowed,
                    "probe {probe:?} with allow {profile_allow:?}/{flag_allow:?} \
                     deny {profile_deny:?}/{flag_deny:?}"
                );
                if let Some(denial) = policy.decide(probe) {
                    assert_eq!(denial.rule == Rule::Deny, denied, "{probe:?}");
                    assert!(!denial.sources.is_empty(), "{probe:?}");
                }
            }
        }
        // Each branch of the model is actually exercised by the seed.
        assert!(
            refused > 0 && unconfigured > 0 && decided > GENERATED_CASES / 4,
            "refused {refused}, unconfigured {unconfigured}, decided {decided}"
        );
    }
}
