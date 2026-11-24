use std::path::Path;

use crate::git::Runner;

pub fn install_args(allow_scripts: bool, offline: bool) -> Vec<&'static str> {
    let mut args = vec!["install"];
    if !allow_scripts {
        args.push("--ignore-scripts");
    }
    if offline {
        args.push("--offline");
    }
    args
}

pub fn install(
    runner: &impl Runner,
    dir: &Path,
    allow_scripts: bool,
    offline: bool,
) -> Result<(), String> {
    runner.run(dir, "npm", &install_args(allow_scripts, offline))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::tests::GitRunner;

    #[test]
    fn install_uses_ignore_scripts_by_default() {
        let runner = GitRunner::default();
        install(&runner, Path::new("/project"), false, false).unwrap();
        install(&runner, Path::new("/project"), true, false).unwrap();
        install(&runner, Path::new("/project"), false, true).unwrap();
        assert_eq!(
            *runner.calls.borrow(),
            [
                vec!["npm", "install", "--ignore-scripts"],
                vec!["npm", "install"],
                vec!["npm", "install", "--ignore-scripts", "--offline"],
            ]
        );
    }
}
