use std::path::{Path, PathBuf};

use oxidepm_adapter_proto::text::hex;
use sha2::{Digest, Sha256};
use url::Url;

pub struct RepoUrl<'a> {
    pub original: &'a str,
    pub normalized: String,
}

impl<'a> RepoUrl<'a> {
    pub fn parse(input: &'a str) -> Result<Self, String> {
        let invalid = || "Expected an https://, ssh:// or git@ repository URL".to_string();
        if input.starts_with('-')
            || input.chars().any(|c| c.is_whitespace() || c.is_control())
            || input.contains('\\')
        {
            return Err(invalid());
        }

        let (host, path) = if let Some(scp) = input.strip_prefix("git@") {
            let host_end = if scp.starts_with('[') {
                scp.find(']').ok_or_else(invalid)? + 1
            } else {
                scp.find(':').ok_or_else(invalid)?
            };
            let (host, rest) = scp.split_at(host_end);
            let path = rest.strip_prefix(':').ok_or_else(invalid)?;
            let host = url::Host::parse(host).map_err(|_| invalid())?;
            (host.to_string(), path.to_string())
        } else {
            let (scheme, rest) = input.split_once("://").ok_or_else(invalid)?;
            if !matches!(scheme, "https" | "ssh") || rest.starts_with('/') {
                return Err(invalid());
            }
            let url = Url::parse(input).map_err(|_| invalid())?;
            if url.query().is_some() || url.fragment().is_some() {
                return Err(invalid());
            }
            let hostname = url.host_str().ok_or_else(invalid)?;
            let host = match url.port() {
                Some(port) if !(scheme == "ssh" && port == 22) => format!("{hostname}:{port}"),
                _ => hostname.to_string(),
            };
            (host, url.path().to_string())
        };
        let path = path.trim_matches('/');
        let path = path.strip_suffix(".git").unwrap_or(path);
        if path.is_empty() || path.rsplit('/').next().is_some_and(str::is_empty) {
            return Err(invalid());
        }
        Ok(Self {
            original: input,
            normalized: format!("{}/{path}", host.to_ascii_lowercase()),
        })
    }

    pub fn name(&self) -> &str {
        self.normalized.rsplit('/').next().expect("repository path")
    }

    pub fn clone_dir(&self, state: &Path) -> PathBuf {
        let digest = hex(&Sha256::digest(self.normalized.as_bytes()));
        state.join("clones").join(&digest[..16])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distinct_repos_with_same_name_map_to_different_clone_dirs() {
        let path = |url| RepoUrl::parse(url).unwrap().clone_dir(Path::new("/state"));
        assert_ne!(
            path("https://a.com/x/app.git"),
            path("https://b.com/y/app.git")
        );
        assert_ne!(
            path("https://a.com/x/app.git"),
            path("https://a.com/y/app.git")
        );
        assert_eq!(path("https://A.COM/x/app.git/"), path("git@a.com:x/app"));
    }

    #[test]
    fn extract_repo_name_handles_scp_style_without_slash() {
        for url in ["git@github.com:repo", "git@github.com:org/repo.git"] {
            assert_eq!(RepoUrl::parse(url).unwrap().name(), "repo");
        }
    }

    #[test]
    fn url_validation_rejects_bad_schemes_and_leading_dash() {
        for url in [
            "--upload-pack=x",
            "file:///etc",
            "http://a/b",
            "ext::sh",
            "git@:repo",
            "ssh://host",
            "https:///host/repo",
            "https://host/.git",
            "https://host/path with spaces",
            "git@host:repo\n",
            "git@host:repo\0",
        ] {
            assert!(RepoUrl::parse(url).is_err(), "{url:?}");
        }
        for url in [
            "https://host/org/repo",
            "ssh://git@host/org/repo",
            "git@host:org/repo",
        ] {
            assert!(RepoUrl::parse(url).is_ok(), "{url}");
        }
    }
}
