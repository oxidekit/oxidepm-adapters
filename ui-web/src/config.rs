//! `config.toml` in the adapter's state directory.
//!
//! ```toml
//! listen = "127.0.0.1:9615"          # an IP address and a port; the default
//! key = "..."                        # generated the first time the adapter starts
//! origin = "http://localhost:3000"   # optional: the one web page allowed to call the API
//! allowed_hosts = ["pm.example.org"] # optional: names the Host header may carry besides the bound address
//! ```
//!
//! A file that does not parse is an error: the adapter then listens nowhere and says why. It is
//! never replaced by the defaults. The key is required on every address, loopback included.

use std::net::SocketAddr;
use std::path::Path;

use serde::{Deserialize, Serialize};

pub const DEFAULT_LISTEN: &str = "127.0.0.1:9615";
/// The shortest key accepted (a generated one is 64 characters).
pub const MIN_KEY_CHARS: usize = 24;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub listen: SocketAddr,
    pub key: Option<String>,
    pub origin: Option<String>,
    pub allowed_hosts: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            listen: DEFAULT_LISTEN.parse().expect("the default address parses"),
            key: None,
            origin: None,
            allowed_hosts: Vec::new(),
        }
    }
}

#[derive(Deserialize, Serialize, Default)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    listen: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    origin: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    allowed_hosts: Vec<String>,
}

/// The configuration in `path`; `None` when there is no such file.
pub fn load(path: &Path) -> Result<Option<Config>, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse(&text)
            .map(Some)
            .map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: cannot be read: {e}", path.display())),
    }
}

/// A key is a token of ASCII letters, digits and `-._~`: it travels in a header and, from a browser,
/// as a WebSocket subprotocol, where nothing else is allowed.
pub fn check_key(key: &str) -> Result<(), String> {
    if key.chars().count() < MIN_KEY_CHARS {
        return Err(format!("key must be at least {MIN_KEY_CHARS} characters"));
    }
    if !key
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "-._~".contains(c))
    {
        return Err("key may contain only letters, digits and - . _ ~".to_string());
    }
    Ok(())
}

/// An origin as a browser sends it: scheme, host and optional port, nothing else.
pub fn check_origin(origin: &str) -> Result<(), String> {
    let bad = || {
        format!("origin = {origin:?} must look like \"http://localhost:3000\" (a scheme, a host and maybe a port; no path)")
    };
    let rest = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
        .ok_or_else(bad)?;
    if rest.is_empty()
        || rest.contains(['/', '?', '#', '@'])
        || rest.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(bad());
    }
    if origin != origin.to_ascii_lowercase() {
        return Err(format!(
            "origin = {origin:?} must be in lower case, as a browser sends it"
        ));
    }
    Ok(())
}

pub fn parse(text: &str) -> Result<Config, String> {
    let file: File = toml::from_str(text).map_err(|e| e.message().to_string())?;
    let listen = file.listen.as_deref().unwrap_or(DEFAULT_LISTEN);
    let listen: SocketAddr = listen.parse().map_err(|_| {
        format!("listen = {listen:?} is not an IP address and a port, such as \"127.0.0.1:9615\"")
    })?;
    if listen.port() == 0 {
        return Err("listen needs a port, not 0".to_string());
    }
    if let Some(key) = &file.key {
        check_key(key)?;
    }
    if let Some(origin) = &file.origin {
        check_origin(origin)?;
    }
    let mut allowed_hosts = Vec::new();
    for host in &file.allowed_hosts {
        if host.is_empty()
            || host.contains(['/', '@', '?', '#'])
            || host.chars().any(|c| c.is_whitespace() || c.is_control())
        {
            return Err(format!("allowed_hosts entry {host:?} must be a host name, with a port if the proxy forwards one"));
        }
        allowed_hosts.push(host.to_ascii_lowercase());
    }
    Ok(Config {
        listen,
        key: file.key,
        origin: file.origin,
        allowed_hosts,
    })
}

/// The file text for `config`.
pub fn render(config: &Config) -> String {
    let file = File {
        listen: Some(config.listen.to_string()),
        key: config.key.clone(),
        origin: config.origin.clone(),
        allowed_hosts: config.allowed_hosts.clone(),
    };
    let body = toml::to_string(&file).unwrap_or_default();
    format!(
        "# OxidePM web API. The key below guards every request; keep this file private (mode 0600).\n\
         # Change it with: oxidepm web new-key\n{body}"
    )
}

/// A new random key: 32 bytes from the system, as 64 hex characters.
pub fn generate_key() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes)
        .map_err(|e| format!("the system has no random numbers for a key: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "0123456789abcdef-0123456789abcdef";

    #[test]
    fn the_default_is_loopback_without_a_key() {
        let c = parse("").unwrap();
        assert_eq!(c, Config::default());
        assert_eq!(c.listen.to_string(), "127.0.0.1:9615");
        assert!(c.key.is_none() && c.origin.is_none() && c.allowed_hosts.is_empty());
    }

    #[test]
    fn a_missing_file_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(&dir.path().join("config.toml")).unwrap(), None);
    }

    #[test]
    fn a_full_file_is_read() {
        let c = parse(&format!(
            "listen = \"0.0.0.0:8080\"\nkey = \"{KEY}\"\norigin = \"http://localhost:3000\"\nallowed_hosts = [\"PM.example.org\", \"10.0.0.5:8080\"]\n"
        ))
        .unwrap();
        assert_eq!(c.listen.to_string(), "0.0.0.0:8080");
        assert_eq!(c.key.as_deref(), Some(KEY));
        assert_eq!(c.origin.as_deref(), Some("http://localhost:3000"));
        assert_eq!(c.allowed_hosts, ["pm.example.org", "10.0.0.5:8080"]);
    }

    #[test]
    fn a_bad_value_is_an_error_that_names_it() {
        assert!(parse("listen = \"localhost:9615\"")
            .unwrap_err()
            .contains("not an IP address"));
        assert!(parse("listen = \"127.0.0.1\"")
            .unwrap_err()
            .contains("not an IP address"));
        assert!(parse("listen = \"127.0.0.1:0\"")
            .unwrap_err()
            .contains("not 0"));
        assert!(parse("key = \"short\"")
            .unwrap_err()
            .contains("at least 24"));
        assert!(parse("key = \"has a space in it, long enough to pass\"")
            .unwrap_err()
            .contains("only letters"));
        assert!(parse("key = \"quote\\\"quote-quote-quote-quote\"")
            .unwrap_err()
            .contains("only letters"));
        assert!(parse("origin = \"localhost:3000\"")
            .unwrap_err()
            .contains("must look like"));
        assert!(parse("origin = \"http://localhost:3000/\"")
            .unwrap_err()
            .contains("must look like"));
        assert!(parse("origin = \"HTTP://Localhost\"").is_err());
        assert!(parse("allowed_hosts = [\"a b\"]")
            .unwrap_err()
            .contains("allowed_hosts"));
        assert!(parse("allowed_hosts = [\"\"]").is_err());
        assert!(parse("keey = \"x\"").unwrap_err().contains("unknown field"));
        assert!(parse("listen = ").is_err());
    }

    #[test]
    fn a_file_that_cannot_be_used_is_an_error_not_the_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "key = \"short\"\n").unwrap();
        let e = load(&path).unwrap_err();
        assert!(e.contains("config.toml") && e.contains("key"), "{e}");
    }

    #[test]
    fn render_and_parse_agree() {
        let c = Config {
            listen: "127.0.0.1:9615".parse().unwrap(),
            key: Some(KEY.to_string()),
            origin: Some("https://pm.example.org".to_string()),
            allowed_hosts: vec!["pm.example.org".to_string()],
        };
        assert_eq!(parse(&render(&c)).unwrap(), c);
        let bare = Config {
            key: None,
            origin: None,
            allowed_hosts: vec![],
            ..c
        };
        assert_eq!(parse(&render(&bare)).unwrap(), bare);
    }

    #[test]
    fn generated_keys_are_long_distinct_and_valid() {
        let a = generate_key().unwrap();
        let b = generate_key().unwrap();
        assert_eq!(a.len(), 64);
        assert_ne!(a, b);
        check_key(&a).unwrap();
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
