use glob::Pattern;
use std::path::Path;

use crate::config::DEFAULT_IGNORE_PATTERNS;

pub struct Ignore(Vec<Pattern>);

impl Ignore {
    pub fn new(extra: &[String]) -> Result<Self, glob::PatternError> {
        DEFAULT_IGNORE_PATTERNS
            .iter()
            .copied()
            .chain(extra.iter().map(String::as_str))
            .map(Pattern::new)
            .collect::<Result<_, _>>()
            .map(Self)
    }

    pub fn matches(&self, root: &Path, path: &Path) -> bool {
        let Ok(relative) = path.strip_prefix(root) else {
            return false;
        };
        !relative.as_os_str().is_empty()
            && self.0.iter().any(|pattern| {
                pattern.matches_path(relative)
                    || relative
                        .iter()
                        .any(|part| pattern.matches_path(Path::new(part)))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignore_matches_relative_to_root_not_parent_dirs() {
        let ignore = Ignore::new(&["src/generated/*".into()]).unwrap();
        let root = Path::new("/srv/target/app");
        assert!(!ignore.matches(root, &root.join("src/main.rs")));
        assert!(ignore.matches(root, &root.join("target/debug/bin")));
        assert!(ignore.matches(root, &root.join("src/generated/foo.rs")));
    }

    #[test]
    fn default_ignore_covers_python_js_build_and_logs() {
        let ignore = Ignore::new(&[]).unwrap();
        let root = Path::new("/app");
        for path in [
            "py/__pycache__",
            "foo.pyc",
            "app.log",
            ".venv",
            "dist",
            "build",
            ".next",
        ] {
            assert!(ignore.matches(root, &root.join(path)), "{path}");
        }
    }
}
