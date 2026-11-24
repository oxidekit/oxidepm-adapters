use std::fs;
use std::io::{self, ErrorKind};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::time::Duration;

#[derive(Debug, PartialEq, Eq)]
pub enum PortStatus {
    Available,
    InUse,
    Unknown,
}

#[derive(Debug)]
pub struct PortCheckResult {
    pub desired_port: u16,
    pub status: PortStatus,
    pub available_port: Option<u16>,
}

fn detect_project_port(dir: &Path) -> Option<u16> {
    for file in [".env", ".env.local"] {
        if let Ok(content) = fs::read_to_string(dir.join(file)) {
            if let Some(port) = parse_port_from_env(&content) {
                return Some(port);
            }
        }
    }

    let content = fs::read_to_string(dir.join("package.json")).ok()?;
    let package: serde_json::Value = serde_json::from_str(&content).ok()?;
    ["start", "dev"].into_iter().find_map(|name| {
        package["scripts"][name]
            .as_str()
            .and_then(parse_port_from_script)
    })
}

fn parse_port(value: &str) -> Option<u16> {
    let port: u16 = value.trim().trim_matches(['"', '\'']).parse().ok()?;
    (port != 0).then_some(port)
}

fn parse_port_from_env(content: &str) -> Option<u16> {
    content
        .lines()
        .find_map(|line| parse_port(line.trim().strip_prefix("PORT=")?))
}

fn parse_port_from_script(script: &str) -> Option<u16> {
    let mut words = script.split_whitespace();
    while let Some(word) = words.next() {
        let value = match word {
            "--port" | "-p" => words.next(),
            _ => word
                .strip_prefix("PORT=")
                .or_else(|| word.strip_prefix("--port=")),
        };
        if let Some(port) = value.and_then(parse_port) {
            return Some(port);
        }
    }
    None
}

fn bind_status<T>(result: io::Result<T>, ipv6: bool) -> PortStatus {
    match result {
        Ok(_) => PortStatus::Available,
        Err(error) if error.kind() == ErrorKind::AddrInUse => PortStatus::InUse,
        Err(error) if ipv6 && ipv6_unavailable(&error) => PortStatus::Available,
        Err(_) => PortStatus::Unknown,
    }
}

fn ipv6_unavailable(error: &io::Error) -> bool {
    matches!(error.kind(), ErrorKind::AddrNotAvailable | ErrorKind::Unsupported)
        // Linux EAFNOSUPPORT, when AF_INET6 is unavailable.
        || (cfg!(target_os = "linux") && error.raw_os_error() == Some(97))
}

fn probe_binds(port: u16) -> PortStatus {
    let ipv4 = bind_status(TcpListener::bind((Ipv4Addr::UNSPECIFIED, port)), false);
    if ipv4 == PortStatus::InUse {
        return PortStatus::InUse;
    }
    let ipv6 = bind_status(TcpListener::bind((Ipv6Addr::UNSPECIFIED, port)), true);
    match (ipv4, ipv6) {
        (_, PortStatus::InUse) => PortStatus::InUse,
        (PortStatus::Available, PortStatus::Available) => PortStatus::Available,
        _ => PortStatus::Unknown,
    }
}

fn probe_port(port: u16) -> PortStatus {
    let loopback = [
        SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
        SocketAddr::from((Ipv6Addr::LOCALHOST, port)),
    ];
    if loopback
        .iter()
        .any(|addr| TcpStream::connect_timeout(addr, Duration::from_millis(100)).is_ok())
    {
        return PortStatus::InUse;
    }
    // A refused connection doesn't prove the port is free on other interfaces.
    probe_binds(port)
}

fn find_available_port(start_port: u16) -> Option<u16> {
    for port in start_port.max(1)..=u16::MAX {
        match probe_binds(port) {
            PortStatus::Available => return Some(port),
            PortStatus::InUse => {}
            PortStatus::Unknown => break,
        }
    }
    None
}

pub fn check_port_conflict(dir: &Path) -> Option<PortCheckResult> {
    let desired_port = detect_project_port(dir)?;
    let status = probe_port(desired_port);
    let available_port = if status == PortStatus::InUse {
        desired_port.checked_add(1).and_then(find_available_port)
    } else {
        None
    };
    Some(PortCheckResult {
        desired_port,
        status,
        available_port,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_project_port_returns_none_without_explicit_port() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("package.json"),
            r#"{"scripts":{"start":"node index.js"}}"#,
        )
        .unwrap();
        assert_eq!(detect_project_port(dir.path()), None);

        fs::remove_file(dir.path().join("package.json")).unwrap();
        fs::write(
            dir.path().join("Cargo.toml"),
            "[dependencies]\naxum = \"0.7\"",
        )
        .unwrap();
        assert_eq!(detect_project_port(dir.path()), None);
    }

    #[test]
    fn detects_ports_from_env_and_scripts_in_order() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("package.json"),
            r#"{"scripts":{"start":"serve -p 1234","dev":"serve --port 3210"}}"#,
        )
        .unwrap();
        assert_eq!(detect_project_port(dir.path()), Some(1234));
        fs::write(dir.path().join(".env.local"), "# PORT=5555\nPORT='2345'\n").unwrap();
        assert_eq!(detect_project_port(dir.path()), Some(2345));
        fs::write(dir.path().join(".env"), "PORT=3456\n").unwrap();
        assert_eq!(detect_project_port(dir.path()), Some(3456));
        assert_eq!(
            parse_port_from_script("PORT=4567 node index.js"),
            Some(4567)
        );
        assert_eq!(parse_port_from_script("serve --port 5678"), Some(5678));
        assert_eq!(parse_port_from_script("SPORT=1234 node index.js"), None);
        assert_eq!(parse_port_from_script("serve --port 0"), None);
    }

    #[test]
    fn check_port_conflict_on_65535_does_not_panic_or_return_zero() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".env"), "PORT=65535\n").unwrap();
        let listener = match TcpListener::bind(("0.0.0.0", u16::MAX)) {
            Ok(listener) => Some(listener),
            Err(e) if e.kind() == ErrorKind::AddrInUse => None,
            Err(e) => panic!("cannot bind test port: {e}"),
        };
        let conflict = check_port_conflict(dir.path()).unwrap();
        assert_eq!(conflict.status, PortStatus::InUse);
        assert_eq!(conflict.available_port, None);
        drop(listener);
        assert_ne!(find_available_port(0), Some(0));
    }

    #[test]
    fn ipv6_unsupported_error_is_not_reported_as_in_use() {
        assert_eq!(
            bind_status(Err::<(), _>(ErrorKind::AddrNotAvailable.into()), true),
            PortStatus::Available
        );
        assert_eq!(
            bind_status(Err::<(), _>(ErrorKind::Unsupported.into()), true),
            PortStatus::Available
        );
        // AF_INET6 is unavailable on Linux when socket() returns EAFNOSUPPORT.
        #[cfg(target_os = "linux")]
        assert_eq!(
            bind_status(Err::<(), _>(io::Error::from_raw_os_error(97)), true),
            PortStatus::Available
        );
        assert_eq!(
            bind_status(Err::<(), _>(ErrorKind::AddrInUse.into()), true),
            PortStatus::InUse
        );
    }
}
