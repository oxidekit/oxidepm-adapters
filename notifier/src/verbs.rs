//! The `notify` verb: `oxidepm notify status | test | events | severity | telegram | remove`.
//!
//! A relayed verb has no standard input and no terminal, so a secret is never asked for here: the
//! bot token is read from the daemon's environment (`OXIDEPM_TELEGRAM_TOKEN`, which the manifest
//! passes on) or from `bot_token` in `config.toml`, and the webhook URLs are written into that
//! file by hand. Nothing a person types on the command line is a secret.

use oxidepm_adapter_proto::messages::{InvokeParams, InvokeResult};
use oxidepm_adapter_sdk::Output;
use serde_json::{json, Value};

use crate::channels::channels;
use crate::config::{self, Config, Telegram, CHANNELS, TOKEN_ENV};
use crate::event::{Alert, Severity, VALID_EVENTS};
use crate::service::Notifier;

type Done = (i32, Value);

const USAGE: &str = "usage: oxidepm notify <command>
  status                  show the channels, the filters and whether the configuration is usable
  test                    send a test message to every channel now, whatever the filters say
  events [--set LIST]     show, or set, the event types to send (empty LIST: all)
  severity LEVEL          send only events of this severity or above: info, warning, critical, or none
  telegram --chat ID      set up Telegram; the token comes from OXIDEPM_TELEGRAM_TOKEN or bot_token
  remove CHANNEL          remove a channel: telegram, discord, slack or webhook";

pub async fn run(n: &Notifier, call: InvokeParams, out: Output) -> InvokeResult {
    let (exit_code, result) = dispatch(n, &call.argv, &out).await;
    InvokeResult {
        exit_code,
        result: Some(result),
    }
}

fn fail(out: &Output, message: impl AsRef<str>) -> Done {
    out.stderr(format!("{}\n", message.as_ref()));
    (1, json!({ "ok": false, "error": message.as_ref() }))
}

fn usage(out: &Output, why: &str) -> Done {
    out.stderr(format!("{why}\n{USAGE}\n"));
    (2, json!({ "ok": false, "error": why }))
}

async fn dispatch(n: &Notifier, argv: &[String], out: &Output) -> Done {
    let Some(command) = argv.first() else {
        return usage(out, "which command?");
    };
    let rest = &argv[1..];
    match command.as_str() {
        "status" if rest.is_empty() => status(n, out),
        "test" if rest.is_empty() => test(n, out).await,
        "events" => events(n, rest, out).await,
        "severity" => severity(n, rest, out).await,
        "telegram" => telegram(n, rest, out).await,
        "remove" => remove(n, rest, out).await,
        "help" | "--help" | "-h" => {
            out.stdout(format!("{USAGE}\n"));
            (0, json!({ "ok": true }))
        }
        other => usage(out, &format!("unknown command '{}'", other.escape_debug())),
    }
}

/// The configuration for a verb: the file as it is, and a broken file stops the verb.
fn strict(n: &Notifier, out: &Output) -> Result<Config, Done> {
    match config::load(&n.path) {
        Ok(config) => Ok(config),
        Err(e) => Err(fail(
            out,
            format!("the configuration is not usable, so nothing was changed: {e}"),
        )),
    }
}

fn status(n: &Notifier, out: &Output) -> Done {
    out.stdout("Notification configuration\n".to_string());
    out.stdout(format!("Config file: {}\n", n.path.display()));
    let config = match config::load(&n.path) {
        Ok(c) => c,
        Err(e) => {
            out.stdout(format!(
                "Configuration: NOT USABLE, nothing is sent until it is fixed\n  {e}\n"
            ));
            return (1, json!({ "ok": false, "error": e }));
        }
    };
    out.stdout("Configuration: ok\n".to_string());
    let mut channels_json = Vec::new();
    match &config.telegram {
        Some(t) => {
            let token = match (t.resolve_token(), &t.bot_token_env) {
                (Some(token), Some(env)) if std::env::var(env).is_ok_and(|v| !v.is_empty()) => {
                    format!("from {env} ({})", config::mask(&token))
                }
                (Some(token), _) => format!("from config.toml ({})", config::mask(&token)),
                (None, Some(env)) => format!(
                    "MISSING: {env} is not set for the adapter, and config.toml has no bot_token"
                ),
                (None, None) => "MISSING: no bot_token and no bot_token_env".to_string(),
            };
            out.stdout(format!(
                "Telegram: configured\n  Chat ID: {}\n  Bot token: {token}\n",
                t.chat_id
            ));
            channels_json.push("telegram");
        }
        None => out.stdout("Telegram: not configured\n".to_string()),
    }
    for (name, present) in [
        ("Discord", config.discord.is_some()),
        ("Slack", config.slack.is_some()),
        ("Webhook", config.webhook.is_some()),
    ] {
        if present {
            channels_json.push(match name {
                "Discord" => "discord",
                "Slack" => "slack",
                _ => "webhook",
            });
        }
        out.stdout(format!(
            "{name}: {}\n",
            if present {
                "configured"
            } else {
                "not configured"
            }
        ));
    }
    for channel in channels(&config) {
        out.stdout(format!(
            "  {} sends to {}\n",
            channel.name(),
            channel.host()
        ));
    }
    if config.events.is_empty() {
        out.stdout("Events: all\n".to_string());
    } else {
        out.stdout(format!("Events: {}\n", config.events.join(", ")));
    }
    out.stdout(format!(
        "Minimum severity: {}\n",
        config.min_severity.as_deref().unwrap_or("none")
    ));
    (
        0,
        json!({ "ok": true, "channels": channels_json, "events": config.events, "min_severity": config.min_severity }),
    )
}

async fn test(n: &Notifier, out: &Output) -> Done {
    let config = match strict(n, out) {
        Ok(c) => c,
        Err(done) => return done,
    };
    let channels = channels(&config);
    if channels.is_empty() {
        return fail(out, "No channel is configured. Run 'oxidepm notify status' to see where the configuration is.");
    }
    out.stdout(format!(
        "Sending a test message to {} channel(s)...\n",
        channels.len()
    ));
    // a test goes to every channel whatever the filters and the rate limit say, and says what each one did
    let alert = Alert::own(
        "test",
        "OxidePM test message: if you can read this, this channel works.",
    );
    let http = crate::channels::client(config.timeout());
    let sent = Notifier::send_all(&http, &channels, &alert, config.timeout()).await;
    let mut failed = 0;
    let mut results = Vec::new();
    for s in &sent {
        match &s.result {
            Ok(()) => {
                out.stdout(format!("{}: ok\n", s.channel));
                results.push(json!({ "channel": s.channel, "ok": true }));
            }
            Err(why) => {
                failed += 1;
                out.stdout(format!("{}: FAILED ({why})\n", s.channel));
                results.push(json!({ "channel": s.channel, "ok": false, "error": why }));
            }
        }
    }
    if failed > 0 {
        out.stderr(format!("{failed} of {} channel(s) failed\n", sent.len()));
    }
    (
        i32::from(failed > 0),
        json!({ "ok": failed == 0, "channels": results }),
    )
}

/// Change the file: the load must have worked, the result must validate, and the old file is kept.
async fn change(
    n: &Notifier,
    out: &Output,
    edit: impl FnOnce(&mut Config) -> Result<String, String>,
) -> Done {
    let _one_at_a_time = n.edit.lock().await;
    let mut config = match strict(n, out) {
        Ok(c) => c,
        Err(done) => return done,
    };
    let message = match edit(&mut config) {
        Ok(m) => m,
        Err(why) => return fail(out, why),
    };
    match config::save(&n.path, &config) {
        Ok(()) => {
            out.stdout(format!("{message}\n"));
            (0, json!({ "ok": true }))
        }
        Err(e) => fail(out, format!("nothing was changed: {e}")),
    }
}

async fn events(n: &Notifier, rest: &[String], out: &Output) -> Done {
    let list = match rest {
        [] => None,
        [flag, list] if flag == "--set" => Some(list.clone()),
        [one] if one.starts_with("--set=") => Some(one["--set=".len()..].to_string()),
        _ => return usage(out, "events takes --set LIST or nothing"),
    };
    let Some(list) = list else {
        return match strict(n, out) {
            Ok(c) => {
                out.stdout(format!(
                    "Events: {}\nValid event types: {}\n",
                    if c.events.is_empty() {
                        "all".to_string()
                    } else {
                        c.events.join(", ")
                    },
                    VALID_EVENTS.join(", ")
                ));
                (
                    0,
                    json!({ "ok": true, "events": c.events, "valid": VALID_EVENTS }),
                )
            }
            Err(done) => done,
        };
    };
    let events: Vec<String> = list
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    change(n, out, move |c| {
        let message = if events.is_empty() {
            "Event filter cleared: every event is sent".to_string()
        } else {
            format!("Events set to: {}", events.join(", "))
        };
        c.events = events;
        Ok(message)
    })
    .await
}

async fn severity(n: &Notifier, rest: &[String], out: &Output) -> Done {
    let [level] = rest else {
        return usage(
            out,
            "severity takes one level: info, warning, critical or none",
        );
    };
    let level = level.to_lowercase();
    if level != "none" && level.parse::<Severity>().is_err() {
        return usage(
            out,
            &format!(
                "'{}' is not a severity: use info, warning, critical or none",
                level.escape_debug()
            ),
        );
    }
    change(n, out, move |c| {
        if level == "none" {
            c.min_severity = None;
            Ok("Minimum severity cleared: every event is sent".to_string())
        } else {
            let parsed: Severity = level.parse().map_err(|_| "bad severity".to_string())?;
            c.min_severity = Some(parsed.as_str().to_string());
            Ok(format!(
                "Only events of severity {} or above are sent",
                parsed.as_str()
            ))
        }
    })
    .await
}

async fn telegram(n: &Notifier, rest: &[String], out: &Output) -> Done {
    let mut chat = None;
    let mut it = rest.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--chat" => chat = it.next().cloned(),
            "--token" => return fail(out, format!("A token on the command line would be kept in shell history and in the process list, so it is not accepted. Set {TOKEN_ENV} in the daemon's environment, or put bot_token in {}", n.path.display())),
            other if other.starts_with("--chat=") => chat = Some(other["--chat=".len()..].to_string()),
            other => return usage(out, &format!("unexpected argument '{}'", other.escape_debug())),
        }
    }
    let Some(chat) = chat.filter(|c| !c.trim().is_empty()) else {
        return usage(out, "telegram needs --chat ID");
    };
    change(n, out, move |c| {
        let kept = c.telegram.take();
        c.telegram = Some(Telegram {
            bot_token: kept.as_ref().map(|t| t.bot_token.clone()).unwrap_or_default(),
            bot_token_env: Some(TOKEN_ENV.to_string()),
            chat_id: chat.clone(),
            api_base: kept.and_then(|t| t.api_base),
        });
        let have = c.telegram.as_ref().and_then(Telegram::resolve_token).is_some();
        Ok(format!(
            "Telegram set up for chat {chat}.{}",
            if have { String::new() } else { format!(" There is no bot token yet: set {TOKEN_ENV} in the daemon's environment, or bot_token in the config file.") }
        ))
    })
    .await
}

async fn remove(n: &Notifier, rest: &[String], out: &Output) -> Done {
    let [channel] = rest else {
        return usage(
            out,
            "remove takes one channel: telegram, discord, slack or webhook",
        );
    };
    let channel = channel.to_lowercase();
    if !CHANNELS.contains(&channel.as_str()) {
        return usage(
            out,
            &format!(
                "'{}' is not a channel: telegram, discord, slack or webhook",
                channel.escape_debug()
            ),
        );
    }
    change(n, out, move |c| {
        let had = match channel.as_str() {
            "telegram" => c.telegram.take().is_some(),
            "discord" => c.discord.take().is_some(),
            "slack" => c.slack.take().is_some(),
            _ => c.webhook.take().is_some(),
        };
        Ok(if had {
            format!("{channel} removed")
        } else {
            format!("{channel} was not configured")
        })
    })
    .await
}
