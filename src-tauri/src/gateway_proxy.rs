//! Resolve an HTTP(S) proxy for the gateway without inheriting the shell environment.

use std::net::IpAddr;
use tauri::Url;
use winreg::enums::{HKEY_CURRENT_USER, KEY_READ};
use winreg::RegKey;

pub(crate) fn gateway_proxy_env(upstream: &str) -> Result<Vec<(String, String)>, &'static str> {
    let upstream = Url::parse(upstream).map_err(|_| "invalid upstream URL")?;
    let host = upstream.host_str().ok_or("missing upstream host")?;
    let address = host.trim_matches(['[', ']']);
    if host.eq_ignore_ascii_case("localhost")
        || address.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
    {
        return Ok(Vec::new());
    }

    // Node falls back to HTTP_PROXY for HTTPS. ALL_PROXY is an application fallback.
    let explicit = if upstream.scheme() == "https" {
        proxy_env("HTTPS_PROXY").or_else(|| proxy_env("HTTP_PROXY"))
    } else {
        proxy_env("HTTP_PROXY")
    }
    .or_else(|| proxy_env("ALL_PROXY"));
    let proxy = explicit.or_else(|| windows_proxy(&upstream));
    let Some(proxy) = proxy else {
        return Ok(Vec::new());
    };
    let proxy = normalize_proxy(&proxy)?;
    let mut bypass = proxy_env("NO_PROXY").unwrap_or_default();
    if !bypass.is_empty() {
        bypass.push(',');
    }
    bypass.push_str("localhost,127.0.0.1,::1,[::1]");

    // The gateway has one upstream origin and does not follow redirects.
    Ok(vec![
        ("HTTP_PROXY".into(), proxy.clone()),
        ("HTTPS_PROXY".into(), proxy),
        ("NO_PROXY".into(), bypass),
        ("NODE_USE_ENV_PROXY".into(), "1".into()),
    ])
}

fn proxy_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn normalize_proxy(value: &str) -> Result<String, &'static str> {
    let value = value.trim();
    if value.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("invalid proxy URL");
    }
    let url = if value.contains("://") {
        value.to_string()
    } else {
        format!("http://{value}")
    };
    let url = Url::parse(&url).map_err(|_| "invalid proxy URL")?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("only HTTP(S) proxy URLs are supported");
    }
    if url.host_str().is_none()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("invalid proxy URL");
    }
    Ok(url.to_string())
}

fn windows_proxy(upstream: &Url) -> Option<String> {
    let settings = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(
            r"Software\Microsoft\Windows\CurrentVersion\Internet Settings",
            KEY_READ,
        )
        .ok()?;
    if settings.get_value::<u32, _>("ProxyEnable").ok()? != 1 {
        return None;
    }
    let host = upstream.host_str()?.to_ascii_lowercase();
    let authority = format!("{host}:{}", upstream.port_or_known_default()?);
    let bypass: String = settings.get_value("ProxyOverride").unwrap_or_default();
    for pattern in bypass
        .split(';')
        .map(|value| value.trim().to_ascii_lowercase())
    {
        if pattern.is_empty() {
            continue;
        }
        if (pattern == "<local>" && !host.contains('.') && !host.contains(':'))
            || wildcard_matches(&pattern, &host)
            || wildcard_matches(&pattern, &authority)
        {
            return None;
        }
    }

    let server: String = settings.get_value("ProxyServer").ok()?;
    // An unqualified server applies to both protocols; a protocol map does not.
    if !server.contains('=') {
        return (!server.trim().is_empty()).then(|| server.trim().to_string());
    }
    server.split(';').find_map(|entry| {
        let (scheme, value) = entry.split_once('=')?;
        (scheme.trim().eq_ignore_ascii_case(upstream.scheme()) && !value.trim().is_empty())
            .then(|| value.trim().to_string())
    })
}

fn wildcard_matches(pattern: &str, value: &str) -> bool {
    let Some((prefix, rest)) = pattern.split_once('*') else {
        return pattern == value;
    };
    let Some(mut remaining) = value.strip_prefix(prefix) else {
        return false;
    };
    let mut parts = rest.split('*').peekable();
    while let Some(part) = parts.next() {
        if parts.peek().is_none() {
            return remaining.ends_with(part);
        }
        let Some(index) = remaining.find(part) else {
            return false;
        };
        remaining = &remaining[index + part.len()..];
    }
    false
}
