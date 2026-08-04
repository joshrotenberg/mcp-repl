# Security Policy

## Supported Versions

| Version | Supported |
|---------|-----------|
| 0.2.x   | Yes       |
| < 0.2   | No        |

Versions before 0.2.0 were released from the
[tower-mcp](https://github.com/joshrotenberg/tower-mcp) repository, where
`mcp-repl` was developed until the 0.2.0 extraction.

## Reporting a Vulnerability

Please report security vulnerabilities via [GitHub Security Advisories](https://github.com/joshrotenberg/mcp-repl/security/advisories/new).

Do **not** open a public issue for security vulnerabilities.

You should receive an initial response within 72 hours. If a vulnerability is confirmed, a fix will be released as soon as possible, typically within 7 days.

## Scope

This policy covers:

- The `mcp-repl` crate, including its credential-store integration and OAuth
  profile handling

Vulnerabilities in the underlying protocol implementation belong to
[tower-mcp's security policy](https://github.com/joshrotenberg/tower-mcp/blob/main/SECURITY.md).
