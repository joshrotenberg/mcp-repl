//! HTTP authorization precedence and conflict validation.
//!
//! Connection orchestration chooses a profile and transport; this module
//! owns the security-sensitive decision about which credential source may
//! reach that transport. The functions take resolved inputs, which keeps the
//! precedence rules testable without process environment or network state.

use std::time::Duration;

use tower_mcp::client::HttpClientConfig;

pub(crate) fn build_http_config(
    bearer: Option<String>,
    headers: &[String],
    profile_bearer: Option<String>,
    profile_headers: &[(String, String)],
    env_bearer: Option<String>,
    request_timeout: Option<Duration>,
) -> Result<HttpClientConfig, String> {
    let mut config = HttpClientConfig::default();
    for (name, value) in profile_headers {
        config = config.header(name.as_str(), value.as_str());
    }
    let selected_has_authorization = profile_headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("authorization"));
    if let Some(token) = bearer.or(profile_bearer).or_else(|| {
        (!selected_has_authorization)
            .then_some(env_bearer)
            .flatten()
    }) {
        config = config.bearer_token(token);
    }
    for raw in headers {
        let (name, value) = raw
            .split_once(':')
            .ok_or_else(|| format!("invalid --header {raw:?}: expected `Name: Value`"))?;
        config = config.header(name.trim(), value.trim());
    }
    // The framework applies its own 30s HTTP request timeout. Leaving it in
    // place would make `--timeout 300` a lie on HTTP and `--timeout 0` still
    // give up at 30s, so the transport is told the same deadline the REPL is
    // using. "Indefinitely" becomes a year, which no interactive session
    // outlives, because the transport requires some duration.
    config.request_timeout = request_timeout.unwrap_or(Duration::from_secs(365 * 24 * 60 * 60));
    Ok(config)
}

pub(crate) fn raw_header_is_authorization(header: &str) -> bool {
    header
        .split_once(':')
        .is_some_and(|(name, _)| name.trim().eq_ignore_ascii_case("authorization"))
}

fn selected_header_is_authorization((name, _): &(String, String)) -> bool {
    name.eq_ignore_ascii_case("authorization")
}

fn bearer_fd_conflict_result(conflicts: Vec<&'static str>) -> Result<(), String> {
    if conflicts.is_empty() {
        return Ok(());
    }
    Err(format!(
        "--bearer-fd cannot be combined with other authorization sources: {}. Remove the other source instead of relying on credential precedence",
        conflicts.join(", ")
    ))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn validate_bearer_fd_exclusive(
    enabled: bool,
    cli_bearer: bool,
    env_bearer: bool,
    cli_headers: &[String],
    selected_bearer: bool,
    selected_headers: &[(String, String)],
    cli_oauth: bool,
    selected_oauth: bool,
) -> Result<(), String> {
    if !enabled {
        return Ok(());
    }
    let mut conflicts = Vec::new();
    if cli_bearer {
        conflicts.push("--bearer");
    }
    if env_bearer {
        conflicts.push("MCP_BEARER");
    }
    if selected_bearer {
        conflicts.push("profile `bearer`/`bearer_env`");
    }
    if cli_headers
        .iter()
        .any(|header| raw_header_is_authorization(header))
    {
        conflicts.push("--header Authorization");
    }
    if selected_headers
        .iter()
        .any(selected_header_is_authorization)
    {
        conflicts.push("profile/import Authorization header");
    }
    if cli_oauth {
        conflicts.push("--oauth");
    }
    if selected_oauth {
        conflicts.push("profile OAuth");
    }
    bearer_fd_conflict_result(conflicts)
}

pub(crate) fn validate_profile_bearer_fd_exclusive(
    enabled: bool,
    profile: &crate::config::Profile,
) -> Result<(), String> {
    if !enabled {
        return Ok(());
    }
    let mut conflicts = Vec::new();
    if profile.bearer.is_some() {
        conflicts.push("profile `bearer`");
    }
    if profile.bearer_env.is_some() {
        conflicts.push("profile `bearer_env`");
    }
    if profile.oauth.is_some() {
        conflicts.push("profile OAuth");
    }
    if profile
        .headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("authorization"))
    {
        conflicts.push("profile Authorization header");
    }
    bearer_fd_conflict_result(conflicts)
}

pub(crate) fn selected_oauth_profile(
    cli_oauth: Option<&str>,
    profile_oauth: Option<&str>,
    cli_bearer: bool,
    cli_headers: &[String],
) -> Option<String> {
    let explicit_authorization = cli_bearer
        || cli_headers
            .iter()
            .any(|header| raw_header_is_authorization(header));
    (!explicit_authorization)
        .then(|| cli_oauth.or(profile_oauth).map(str::to_string))
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_and_header_precedence_are_explicit_inputs() {
        let config = build_http_config(
            None,
            &["X-Source: cli".to_string()],
            Some("profile-token".to_string()),
            &[("X-Source".to_string(), "profile".to_string())],
            Some("environment-token".to_string()),
            Some(Duration::from_secs(17)),
        )
        .unwrap();
        assert_eq!(
            config.headers.get("Authorization").map(String::as_str),
            Some("Bearer profile-token")
        );
        assert_eq!(
            config.headers.get("X-Source").map(String::as_str),
            Some("cli")
        );
        assert_eq!(config.request_timeout, Duration::from_secs(17));
    }

    #[test]
    fn explicit_authorization_suppresses_implicit_credentials() {
        let config = build_http_config(
            None,
            &[],
            None,
            &[("Authorization".to_string(), "Basic selected".to_string())],
            Some("environment-token".to_string()),
            None,
        )
        .unwrap();
        assert_eq!(
            config.headers.get("Authorization").map(String::as_str),
            Some("Basic selected")
        );
    }
}
