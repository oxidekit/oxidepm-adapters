# OxidePM adapters

OxidePM adapters are separate programs that `oxidepmd` starts, supervises and talks to over the
adapter protocol (JSON lines on stdin and stdout). Adapters run only on Linux. The core runs
on macOS with the built-in `cmd` runtime.

Install and enable an adapter from an OxidePM release with `oxidepm adapter import --bundled <name> --enable`.
Review the permissions and confirm the grant at the prompt. Bundled imports work offline using
the release checksums. For source builds, check out the core repository next to this one as
`../oxidepm` so Cargo can resolve the shared crates.

| Directory | What |
|---|---|
| `sdk/` | `oxidepm-adapter-sdk`: the main loop of an adapter (handshake, `ping`, events, relayed verbs, `shutdown`, and the session socket of a verb that owns the terminal) |
| `notifier/` | the `notifier` adapter: Telegram, Discord, Slack and webhook notifications, and the `oxidepm notify` verb |
| `exporter-prometheus/` | the `exporter-prometheus` adapter: `GET /metrics` for Prometheus |
| `ui-web/` | the `ui-web` adapter: the REST API and WebSocket for the apps, and the `oxidepm web` verb |
| `ui-tui/` | the `ui-tui` adapter: the terminal dashboard, the `oxidepm monit` session verb |
| `runtime-node/` | the `runtime-node` adapter: the `node`, `npm`, `pnpm` and `yarn` runtimes |
| `runtime-python/` | the `runtime-python` adapter: the `python` runtime |
| `runtime-go/` | the `runtime-go` adapter: the `go` runtime |
| `runtime-rust/` | the `runtime-rust` adapter: the `rust` and `cargo` runtimes |
| `watch/` | Restarts opted-in apps when their files change |
| `check/` | Checks project readiness and applies optional fixes |
| `init/` | Generates OxidePM project configuration |
| `git-start/` | Clones or updates a Git checkout and prepares it for launch |
| `deploy/` | Deploys and inspects remote Git checkouts over SSH |
| `bundle/` | Builds adapter archives, a catalog, and `SHA256SUMS` |
| `runtime-testkit/` | what the integration tests of the runtime adapters share (not an adapter) |

## Tests

`cargo test` runs the unit tests and the integration tests. The integration tests build and start
the real `oxidepmd` and `oxidepm` of the OxidePM checkout (`../oxidepm`, or `OXIDEPM_CORE_DIR`;
their target directory is `OXIDEPM_CORE_TARGET_DIR`, by default the target directory of that
checkout), package and import adapters from release-style bundles or by their pinned hashes,
enable them, and drive them with the daemon's own lifecycle events. Every test uses a private home.

## The notifier

Import and enable it with `oxidepm adapter import --bundled notifier --enable`. Its configuration is
`config.toml` in its state directory (`oxidepm notify status` prints the path):

```toml
events = ["crash", "restart"]   # empty or absent: every event
min_severity = "warning"        # info | warning | critical
timeout_secs = 10               # per request, 1 to 60

[telegram]
bot_token_env = "OXIDEPM_TELEGRAM_TOKEN"   # read from the daemon's environment; or bot_token = "..."
chat_id = "-100123456789"

[discord]
webhook_url = "https://discord.com/api/webhooks/..."

[slack]
webhook_url = "https://hooks.slack.com/services/..."

[webhook]
url = "https://example.org/alerts"
secret = "..."   # signs every request: X-OxidePM-Timestamp and X-OxidePM-Signature: v1=HMAC-SHA256(secret, "<timestamp>.<body>")
```

A file that does not parse is an error that `oxidepm notify status` shows; it is never replaced, and
nothing is sent until it is fixed. The file is read again when it changes.

## The Prometheus exporter

`exporter-prometheus` is a service with read access to the daemon. It listens on `127.0.0.1:9616`
and, on each scrape, asks the daemon for the status of the apps (`host.request` `Status`; at most
one question a second, however many scrapers there are). Import and enable it with
`oxidepm adapter import --bundled exporter-prometheus --enable`.

```yaml
# prometheus.yml
scrape_configs:
  - job_name: oxidepm
    static_configs: [{ targets: ["127.0.0.1:9616"] }]
    # with a key (below):
    # authorization: { credentials: "the key" }
```

Metrics: `oxidepm_up` (1 when the daemon answered), `oxidepm_processes_total`, and per app, with the
labels `name`, `id` and `mode`, `oxidepm_process_up`, `oxidepm_process_uptime_seconds`,
`oxidepm_process_restarts_total`, `oxidepm_process_memory_bytes` and `oxidepm_process_cpu_percent`.
Label values are escaped. If the daemon cannot be asked (it is stopped, busy, or refused the
request) the answer is HTTP 503 with only `oxidepm_up 0`, never an empty list that looks healthy.

Optional `config.toml` in the adapter's state directory, read when the adapter starts (disable and
enable it again after a change):

```toml
listen = "127.0.0.1:9616"       # an IP address and a port; the default
key = "at least 16 characters"  # required for any address that is not loopback
```

* An address that is not loopback (`0.0.0.0` included) without a `key` is refused: the adapter
  listens nowhere and `adapter info` shows why. With a key, every request needs
  `Authorization: Bearer <key>`; so does a loopback bind that has a key.
* Without a key (loopback only) the `Host` header must be `localhost` or a loopback address with the
  port that is bound, so a web page whose name was re-pointed at `127.0.0.1` cannot read the metrics.
* Nothing but `/metrics` exists. There is no TLS: put a proxy in front for anything but a trusted network.
* The sandbox lets the adapter bind the ports its manifest lists (`network.listen`), which is `9616`
  as shipped. Another port needs a build whose manifest lists it, and a new consent at `enable`.

## The web API

`ui-web` is a service with the `lifecycle` level and the scope `opted-in`. It listens on
`127.0.0.1:9615` and answers REST and WebSocket requests by asking the daemon (`host.request`) as the
actor `adapter:ui-web`. The daemon's policy and audit log apply to every call. Import and enable it
with `oxidepm adapter import --bundled ui-web --enable`.

* **A key on every request, loopback included.** Without one, any web page the person visits could
  send POST and DELETE requests to `http://127.0.0.1:9615`. The first start makes a random key in
  `config.toml` (mode 0600, the previous file kept as `config.toml.bak`); `oxidepm web` prints it
  once, `oxidepm web new-key` replaces it (the old one stops working at once) and prints the new
  one. The key travels in the `X-API-Key` header. A browser WebSocket cannot set a header, so it
  may offer the subprotocols `oxidepm.v1` and `oxidepm-key.<key>` (never the URL). Only
  `/api/health`, which answers `{"ok":true}`, needs no key.
* **`Host` must be the bound address** (or `localhost` on a loopback bind, or a name listed in
  `allowed_hosts`), so a page whose name was re-pointed at 127.0.0.1 is refused. **`Origin` must be
  absent or the configured `origin`** on every request that changes something and on the WebSocket
  upgrade. The one configured origin gets CORS answers (a preflight needs no key); nobody else does.
* **Scope.** Only apps that list `ui-web` under `adapters` can be restarted, stopped or reloaded
  (the daemon refuses the others with 403 `E_POLICY_DENIED`); every app can be looked at.
* **No app creation, no delete, no flush** at the default level (`lifecycle`): `POST /api/processes`
  answers 403 with the reason, and the daemon refuses deletes. This is a change from the web server
  that was part of the core. An adapter given the `admin` level creates apps (an absolute `cwd`, a
  valid name, the `runtime` defaulting to `cmd`; nothing is guessed from the directory) and deletes
  them; `DELETE /api/processes/all` needs `{"confirm":"all"}` in the body or `?confirm=all`.
* **`/ws`** carries the daemon's `lifecycle` events (`{"type":"event","kind":"stopped","app":{...},
  "data":{...},"seq":N,...}`), including those made by the CLI or the supervisor, not only by this
  adapter. A client that reads too slowly gets `{"type":"lagged","missed":N}`. At most 16 streams.
* **Logs.** `GET /api/processes/@tag/logs?lines=N` returns the lines of every app with the tag (at most
  16 apps), each marked `[name]`.
* **Rate.** At most 20 requests a second go to the daemon (the daemon allows 50 and strikes an adapter
  that keeps asking over it); the list is asked for once a second however many clients poll it;
  more get 429. Request bodies are limited to 64 KiB, connections to 64.
* There is no `/metrics` (that is `exporter-prometheus`) and no TLS: put a proxy in front for anything
  but a trusted network, and list its host name in `allowed_hosts`.

```toml
# config.toml in the adapter's state directory (read when the adapter starts)
listen = "127.0.0.1:9615"          # an IP address and a port; the sandbox allows the ports of the manifest only
key = "..."                         # made on the first start
origin = "http://localhost:3000"    # optional: the one web page that may call the API
allowed_hosts = ["pm.example.org"]  # optional: names a proxy forwards in the Host header
```

`admin` cannot be requested at `enable` (a level can only be lowered from the manifest's), so the
shipped adapter never has it; an adapter build whose manifest asks for `admin` and an interactive
terminal at `enable` is the way to give it.

## The terminal dashboard

`oxidepm monit` is a session verb of the `ui-tui` adapter. Import and enable it with
`oxidepm adapter import --bundled ui-tui --enable`.

The adapter is a tool, not a service: it is started when `oxidepm monit` is run and ends with it.
`oxidepm` hands its own terminal (standard input and output) to the daemon, which starts the adapter
under its sandbox with those two descriptors; the adapter speaks the adapter protocol over a socket
made for the session, so the terminal is free for the dashboard. It holds the `lifecycle` level for
all apps: it can list them, read their logs, and stop and restart them, as the actor `adapter:ui-tui`
(the daemon's policy and audit log apply; a protected app is refused and the refusal is shown). It has
no network, no files and starts no programs.

Keys: `Tab` and `Shift-Tab` switch between Processes, Details and Logs; the arrow keys (or `j` and `k`)
select an app, or scroll the log on the Logs tab, where `PageUp`, `PageDown`, `Home` and `End` also
work; `l` opens the log of the selected app; `s` and `r` ask `y/n` before they stop or restart it;
`q`, `Esc` and `Ctrl-C` leave. The list is asked for again every second, and so is the log while the
Logs tab is open. The answer of the daemon to a stop or restart, and any problem reaching it, is
shown on the bottom line. Whatever an app writes to its log is shown with control characters
replaced, so it cannot move the cursor or change the screen. The terminal is put back (main screen,
cursor, input mode) when the dashboard ends, fails or panics.

It needs a terminal for both input and output and takes no arguments.

## The runtime adapters

A runtime adapter tells the daemon what to run for an app whose `runtime` it provides. It returns
data: the steps to run before the app starts (`runtime.prepare`), and the program, arguments and
directory to execute (`runtime.plan`). The daemon runs the steps and starts the app itself, in its
own process group, so the adapter has no network, starts nothing and reads none of your files. The
daemon runs only the programs the manifest lists in `provides.programs`.

Import and enable one with `oxidepm adapter import runtime-node --enable` once it is in the signed
index, or from the bundle of a release with `oxidepm adapter import --bundled --tag runtime
--enable --yes`.

### runtime-node

Provides `node`, `npm`, `pnpm` and `yarn`. `node` runs `node <script> <arguments>`, the script
being relative to the app's directory unless it is an absolute path. `npm`, `pnpm` and `yarn` run
the package script named by the command: `npm run <script> -- <arguments>` (the `--` keeps npm
from taking `--port 80` for an option of its own), `pnpm run <script> <arguments>` and `yarn run
<script> <arguments>`. There are no steps, so an app starts without waiting for a build. The
adapter does not look at the script or at `package.json`: a script that is not there is reported by
node or by the package tool when the app starts, in the app's own log.

### runtime-python

Provides `python`. The command is read like a shell would read it, so `app.py`, `-m uvicorn
app:app --host 0.0.0.0` and `'my script.py'` all work (a file name with a space has to be quoted),
and the app's arguments follow. The interpreter is the `venv` or `.venv` of the app's directory when
it has one, else `python3`, else `python`. The adapter cannot look at your files, so it has the
daemon run the system's Python for a moment, in the app's directory and without the app's own
environment, to say where the virtualenv is. The app is started with `PYTHONUNBUFFERED=1`, so
printed lines reach the log as they are printed.

### runtime-go

Provides `go`. The app is started with `go run <command> <arguments>` in the app's directory; the
command is the package (`.`) or the file (`main.go`). There are no steps: `go run` compiles when the
app starts. Integration tests use a stand-in `go` that records what it was given, so they do not
need the Go toolchain.

### runtime-rust

Provides `rust` and `cargo`; it asks the daemon to run `rustc` and `cargo`, and then the binary they
built (the daemon accepts that one program only when it is an executable file of its own user).
The builds run in the daemon with the app's environment, under a time limit, with the output kept
in `logs/<name>-build.log` and the last lines in the error.

* `rust`: one `.rs` file, compiled with `rustc -O` into `.oxidepm/bin/` of the app's directory and
  run from there.
* `cargo`: `cargo build --release` in the app's directory. The command names the binary to run (a
  workspace can have several), or is `.` for the project's own: the only binary of the package in
  the app's directory, or the one with the package's name, or the only binary of a workspace that
  has no package of its own. `cargo metadata --no-deps` says which binaries the project has and
  where its target directory is, so a workspace, a library with another name than the package and a
  `target-dir` in `.cargo/config.toml` work. A `CARGO_TARGET_DIR` in the app's own environment is
  refused, because the question to cargo is asked without the app's environment; use the
  configuration file.

## Development and deployment adapters

Import and enable each adapter as needed, for example
`oxidepm adapter import --bundled watch --enable`.

- **watch** restarts an app after a change under a configured path. Add
  `[apps.adapters.watch]` to the app with optional `paths` (default `["."]`),
  `ignore` and `debounce_ms` (default 200).
- **check** adds `oxidepm check [TARGET]`. Use `--strict` to fail on warnings.
  `--fix` can install Node dependencies and create `.env` from a template.
  `--set-env KEY=VALUE` edits `.env`. The write options are limited to projects
  within the current directory. `oxidepm start TARGET --check` opts into preflight.
  The adapter checks for loopback listeners. It warns when sandbox restrictions prevent
  it from determining port availability.
- **init** adds `oxidepm init [DIR]`. It writes a new `oxidepm.config.toml` in the
  project and preserves existing configs. Use `oxidepm --dry-run init [DIR]` to preview.
- **git-start** adds `oxidepm git-start URL [--branch BRANCH]`. It writes checkouts
  under `adapters/state/git-start/clones/`. Use `--install` to install Node dependencies,
  `--allow-scripts` to allow install scripts, `--offline` to use an existing checkout, and
  `--no-start` to prepare it without launching. `--dry-run` prints the plan.
- **deploy** adds `oxidepm deploy HOST` and `oxidepm diff HOST`. Both use noninteractive
  SSH and require an existing known-hosts entry. Deploy pulls from `origin` on the
  remote branch unless `--branch` selects one. `--remote-bin` sets the remote OxidePM
  executable. Restart failures exit with status 3. Both verbs support `--dry-run`.
  The daemon may pass an agent socket through `SSH_AUTH_SOCK` on Linux with Landlock ABI 9 or later.

## Release bundles

OxidePM release archives carry every adapter under `share/oxidepm/adapters`.
The bundle tool writes the archives, `index.json`, and `SHA256SUMS`. The CLI and
daemon verify the catalog and selected archives against `SHA256SUMS` on bundled import.
The release's `checksums.txt` covers the outer archive. A catalog intended for network
distribution needs a signature.

Use `--tag runtime` for the four runtimes, `--tag dev` for `watch`, `check`, and `init`,
`--tag deploy` for `git-start` and `deploy`, `--tag ui` for `ui-web` and `ui-tui`,
`--tag notifier` for `notifier`, or `--tag exporter` for `exporter-prometheus`.
On Linux, the installer enables only the runtimes with `oxidepm adapter import --bundled --tag runtime --enable --yes`.
`--yes` applies only to pure adapters. To enable an adapter with broader permissions
without a terminal, import it first, review the grant, and use the printed
`oxidepm adapter enable <name> --accept <fingerprint>` command.

### Import an adapter built from source

Build the executable and package it for your machine. For example, on Linux x86_64:

```sh
cargo build --release -p oxidepm-adapter-watch -p oxidepm-adapter-bundle
target/release/oxidepm-adapter-bundle --out target/watch-bundle \
    --target x86_64-unknown-linux-gnu --url-base https://example.org/adapters --sequence 1 \
    watch:target/release/oxidepm-watch
```

Use the archive digest from `target/watch-bundle/SHA256SUMS`:

```sh
oxidepm adapter import ./target/watch-bundle/watch-0.4.0-rc.5.tar.gz --sha256 <digest> --allow-reserved-name --enable
```

The bundle builder takes a catalog URL even for archives you import locally.
The archives are the same bytes for the same inputs. The sequence number of an index must be higher
than that of the last one published, and the index is valid for 90 days (`--valid-days`).

## License

Licensed under either the [MIT License](LICENSE-MIT) or the [Apache License, Version 2.0](LICENSE-APACHE), at your option.
