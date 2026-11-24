//! `config.toml` in the adapter's state directory: where to listen and the access key.
//!
//! ```toml
//! listen = "127.0.0.1:9616"   # an IP address and a port; the default
//! key = "a long random string" # required unless `listen` is a loopback address
//! ```
//!
//! A file that does not parse, or that would put `/metrics` on a network without a key, is an
//! error: the exporter then listens nowhere and says why. It is never replaced by the defaults.

use std::net::SocketAddr;
use std::path::Path;

use serde::Deserialize;

pub const DEFAULT_LISTEN: &str = "127.0.0.1:9616";
/// The shortest key accepted. The key travels in a header and guards a port on a network, so a
/// guessable one is no better than none.
pub const MIN_KEY_CHARS: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub listen: SocketAddr,
    pub key: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct File {
    listen: Option<String>,
    key: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            listen: DEFAULT_LISTEN.parse().expect("the default address parses"),
            key: None,
        }
    }
}

/// The configuration in `path`; the defaults when there is no such file.
pub fn load(path: &Path) -> Result<Config, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse(&text).map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(format!("{}: cannot be read: {e}", path.display())),
    }
}

pub fn parse(text: &str) -> Result<Config, String> {
    let file: File = toml::from_str(text).map_err(|e| e.message().to_string())?;
    let listen = file.listen.as_deref().unwrap_or(DEFAULT_LISTEN);
    let listen: SocketAddr = listen.parse().map_err(|_| {
        format!("listen = {listen:?} is not an IP address and a port, such as \"127.0.0.1:9616\"")
    })?;
    if listen.port() == 0 {
        return Err("listen needs a port, not 0".to_string());
    }
    let key = match file.key {
        None => None,
        Some(key) => {
            if key.chars().count() < MIN_KEY_CHARS {
                return Err(format!("key must be at least {MIN_KEY_CHARS} characters"));
            }
            if key.chars().any(|c| c.is_control() || c.is_whitespace()) {
                return Err("key must not contain spaces or control characters".to_string());
            }
            Some(key)
        }
    };
    if !listen.ip().is_loopback() && key.is_none() {
        return Err(format!(
            "refusing to listen on {listen}: that address is not loopback, so /metrics would be open to the network. \
             Set key = \"...\" (at least {MIN_KEY_CHARS} characters; Prometheus sends it as a bearer token) or listen on 127.0.0.1"
        ));
    }
    Ok(Config { listen, key })
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "0123456789abcdef-key";

    #[test]
    fn the_default_is_loopback_without_a_key() {
        let c = parse("").unwrap();
        assert_eq!(c.listen.to_string(), "127.0.0.1:9616");
        assert!(c.key.is_none());
        assert_eq!(c, Config::default());
    }

    #[test]
    fn a_missing_file_is_the_default() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            load(&dir.path().join("config.toml")).unwrap(),
            Config::default()
        );
    }

    #[test]
    fn an_address_that_is_not_loopback_needs_a_key() {
        for listen in [
            "0.0.0.0:9616",
            "192.168.1.5:9616",
            "[::]:9616",
            "[2001:db8::1]:9616",
        ] {
            let e = parse(&format!("listen = \"{listen}\"")).unwrap_err();
            assert!(
                e.contains("not loopback") && e.contains("key"),
                "{listen}: {e}"
            );
            let c = parse(&format!("listen = \"{listen}\"\nkey = \"{KEY}\"")).unwrap();
            assert_eq!(c.key.as_deref(), Some(KEY));
        }
    }

    #[test]
    fn loopback_addresses_need_no_key() {
        for listen in ["127.0.0.1:9616", "127.0.0.5:80", "[::1]:9616"] {
            assert!(
                parse(&format!("listen = \"{listen}\""))
                    .unwrap()
                    .key
                    .is_none(),
                "{listen}"
            );
        }
    }

    #[test]
    fn a_bad_value_is_an_error_that_names_it() {
        assert!(parse("listen = \"localhost:9616\"")
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
            .contains("at least 16"));
        assert!(parse("key = \"has a space in it, long enough\"")
            .unwrap_err()
            .contains("spaces"));
        assert!(parse("keey = \"x\"").unwrap_err().contains("unknown field"));
        assert!(parse("listen = ").is_err());
    }

    #[test]
    fn a_file_that_cannot_be_used_is_an_error_not_the_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "listen = \"0.0.0.0:9616\"\n").unwrap();
        let e = load(&path).unwrap_err();
        assert!(e.contains("config.toml") && e.contains("refusing"), "{e}");
    }
}
