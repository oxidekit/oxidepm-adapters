pub const USAGE: &str = "Usage: oxidepm check [TARGET] [--fix] [--strict] [--set-env KEY=VALUE]\n\
    --fix and --set-env can only write within the current working directory.";

pub struct Args<'a> {
    pub target: &'a str,
    pub fix: bool,
    pub strict: bool,
    pub set_env: Vec<(&'a str, &'a str)>,
}

impl<'a> Args<'a> {
    pub fn parse(argv: &'a [String]) -> Result<Self, String> {
        let mut args = Self {
            target: ".",
            fix: false,
            strict: false,
            set_env: Vec::new(),
        };
        let mut target = None;
        let mut literal = false;
        let mut items = argv.iter().map(String::as_str);
        while let Some(item) = items.next() {
            match item {
                "--" if !literal => literal = true,
                "--fix" if !literal => args.fix = true,
                "--strict" if !literal => args.strict = true,
                "--set-env" if !literal => {
                    let value = items.next().ok_or("--set-env expects KEY=VALUE")?;
                    args.set_env.push(env_entry(value)?);
                }
                flag if !literal && flag.starts_with("--set-env=") => {
                    args.set_env.push(env_entry(&flag["--set-env=".len()..])?);
                }
                flag if !literal && flag.starts_with('-') => {
                    return Err(format!("unknown argument: {flag}"));
                }
                word if target.is_none() => target = Some(word),
                word => return Err(format!("unexpected argument: {word}")),
            }
        }
        if let Some(target) = target {
            args.target = target;
        }
        Ok(args)
    }
}

fn env_entry(entry: &str) -> Result<(&str, &str), String> {
    let (key, value) = entry.split_once('=').ok_or("--set-env expects KEY=VALUE")?;
    let mut bytes = key.bytes();
    if !bytes
        .next()
        .is_some_and(|b| b == b'_' || b.is_ascii_alphabetic())
        || !bytes.all(|b| b == b'_' || b.is_ascii_alphanumeric())
    {
        return Err("--set-env expects a valid environment variable name".into());
    }
    if value.contains(['\r', '\n', '\0']) {
        return Err("--set-env values must be on one line".into());
    }
    Ok((key, value))
}
