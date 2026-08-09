//! Deterministic property-test generator and regression corpus.
//!
//! This stays dependency-free and bounded so the same corpus runs on every
//! ordinary CI invocation. When a boundary bug is fixed, add its smallest
//! reproducer to the appropriate corpus below before changing the seed.

pub const GENERATED_CASES: usize = 1_024;
pub const PROPERTY_SEED: u64 = 0x6d63_702d_7265_706c;

pub struct Generator {
    state: u64,
}

impl Generator {
    pub fn new(domain: u64) -> Self {
        Self {
            state: PROPERTY_SEED ^ domain,
        }
    }

    pub fn next(&mut self) -> u64 {
        // xorshift64*: deterministic on every target and Rust version.
        let mut value = self.state;
        value ^= value >> 12;
        value ^= value << 25;
        value ^= value >> 27;
        self.state = value;
        value.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    pub fn index(&mut self, upper: usize) -> usize {
        debug_assert!(upper > 0);
        (self.next() % upper as u64) as usize
    }

    pub fn text(&mut self, max_chars: usize) -> String {
        const ALPHABET: &[char] = &[
            'a', 'b', 'x', 'Z', '0', '1', '_', '-', '.', ':', '=', '$', ' ', '\t', '\n', '\r',
            '\0', '\\', '\'', '"', '{', '}', '[', ']', '(', ')', '&', '|', ',', ';', '/', 'é',
            '中', '🦀', '\u{1b}', '\u{9b}', '\u{9d}',
        ];
        let len = self.index(max_chars + 1);
        (0..len)
            .map(|_| ALPHABET[self.index(ALPHABET.len())])
            .collect()
    }
}

pub const COMMAND_REGRESSIONS: &[&str] = &[
    r#"tool a="two words""#,
    r#"tool a="say \"hi\"" path='C:\tmp' note=two\ words"#,
    r#"call run.start {"instruction": "Reply with exactly hello", "items": [1, 2]} &"#,
    r#"tool a="trailing &""#,
    "tool literal='&' &",
    "tool a={1]",
    "tool a=[1,",
    "tool a='unterminated",
    "tool note=two\\",
];

pub const ROUTING_REGRESSIONS: &[&str] = &[
    "x = search query=serde",
    "get_crate name=serde | crates[0].name",
    "y = call foo | .id",
    r#"echo message="left | right""#,
    r#"call echo {"message":"left | right"} | .content"#,
    "query=serde",
];

pub const INVALID_PATH_REGRESSIONS: &[&str] = &[
    ".",
    ".[0]",
    "items.",
    "items..name",
    "items[",
    "items[]",
    "items[-1]",
    "items[nope]",
    "items[0]name",
    "items[0].",
    "items]",
];

pub const SELECTOR_REGRESSIONS: &[&str] = &[
    "registry:serve",
    "path/to/.mcp.json:server",
    "path/to/MCP.JSON:server",
    "path/to/.mcp.json:",
    "ordinary-command",
];

pub const INTERPOLATION_REGRESSIONS: &[&str] = &[
    "${env:MISSING}",
    "${MISSING}",
    "${input:token}",
    "${workspaceFolder}/server",
    "${workspaceFolderBasename}",
    "${userHome}",
    "${env:}",
    "${unterminated",
];

pub const WIRE_REGRESSIONS: &[(&str, &[&str])] = &[
    (
        r#"{"params":{"password":"hunter2","taskToken":"visible"}"#,
        &["hunter2"],
    ),
    (
        r#"[{"api_token":"ghp_one"},{"clientSecret":"stripe_two"},{"note":"keep me"}"#,
        &["ghp_one", "stripe_two"],
    ),
    (
        "Authorization: Bearer first-secret\r\nX-Api-Key: second-secret\nnote: keep",
        &["first-secret", "second-secret"],
    ),
    (
        "Bearer alpha, Basic beta; Digest gamma=delta\nkeep",
        &["alpha", "beta", "gamma=delta"],
    ),
    ("plain\u{9d}unterminated-osc", &[]),
    (r#"{"password":{"nested":"do-not-keep"}"#, &["do-not-keep"]),
];

pub const ALIAS_REGRESSIONS: &[(&[(&str, &str)], &str)] = &[
    (&[("a", "b"), ("b", "a")], "a x"),
    (&[("t", "tools"), ("lst", "t")], "lst"),
    (&[("sa", "slow_add a=1 b=2 &")], "sa"),
    (&[("w", "tool wait")], "w id=42"),
    (&[("tool", "tools")], "tool wait id=42"),
];
