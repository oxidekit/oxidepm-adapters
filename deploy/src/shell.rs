pub fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

pub fn valid_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && !host.starts_with('-')
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._@:-".contains(&b))
}

pub fn valid_branch(branch: &str) -> bool {
    !branch.is_empty()
        && !branch.starts_with('-')
        && !branch.contains("..")
        && !branch.chars().any(|c| c.is_whitespace() || c.is_control())
}

pub fn cwd_command(cwd: Option<&str>) -> String {
    let Some(dir) = cwd else {
        return String::new();
    };
    let quoted = if dir.starts_with('-') {
        shell_quote(&format!("./{dir}"))
    } else {
        shell_quote(dir)
    };
    format!("cd {quoted} && ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_quote_escapes_single_quotes_and_metacharacters() {
        let dir = "a'b; rm -rf /";
        let expected = r#"'a'"'"'b; rm -rf /'"#;
        assert_eq!(shell_quote(dir), expected);
        assert_eq!(
            crate::deploy::pull_command(&cwd_command(Some(dir)), "main"),
            format!("cd {expected} && git pull origin -- 'main'"),
        );
        assert_eq!(
            crate::diff::commands(&cwd_command(Some(dir)), 5)[0].2,
            format!("cd {expected} && git log --oneline --no-decorate -n 5"),
        );
        assert_eq!(cwd_command(Some("-")), "cd './-' && ");
    }

    #[test]
    fn host_validation_rejects_leading_dash_and_shell_chars() {
        for host in ["-oProxyCommand=x", "host;id", "", "host name", "höst"] {
            assert!(!valid_host(host), "{host:?}");
        }
        for host in ["user@host.example.com", "10.0.0.1", "my-alias", "::1"] {
            assert!(valid_host(host), "{host:?}");
        }
        assert!(valid_host(&"a".repeat(253)));
        assert!(!valid_host(&"a".repeat(254)));
    }

    #[test]
    fn branch_validation_rejects_option_like_refs() {
        for branch in ["-x", "--upload-pack=x", "a..b", "a b", "a\n", "a\x1b", ""] {
            assert!(!valid_branch(branch), "{branch:?}");
        }
        for branch in ["main", "release/1.0"] {
            assert!(valid_branch(branch), "{branch:?}");
        }
    }
}
