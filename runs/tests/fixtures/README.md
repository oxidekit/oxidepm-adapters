# Runs Fake Host Test Fixtures

Portable POSIX shell stand-ins for `ssh` and `rsync` backed by a local target
directory. These fixtures let deploy, publish, and device tests run without a
network or a remote machine.

## Programs

- `fake-ssh` - stub `ssh` client that logs every invocation and either runs the
  remote command locally inside `FAKE_HOST_ROOT` or emits canned output.
- `fake-rsync` - stub `rsync` client that logs every invocation and copies files
  into `FAKE_HOST_ROOT` when the destination uses `host:/path` syntax.

To use them as drop-in `ssh` and `rsync` binaries on `PATH`, symlink or copy
them into a temporary `bin/` directory (or use `oxidepm_runtime_testkit::fixtures::FakeHost`).

## Environment Knobs

| Variable | Default | Description |
| --- | --- | --- |
| `FAKE_MODE` | `success` | Behavior mode: `success`, `nonzero`, `conflict`, `hang`, or `missing-marker`. |
| `FAKE_EXIT_CODE` | `1` | Exit status used when `FAKE_MODE=nonzero`. |
| `FAKE_MUTEX_HASH` | `0123456789abcdef0123456789abcdef01234567` | 40-char hex hash inserted into the `ConflictingInstance` message when `FAKE_MODE=conflict`. |
| `FAKE_DELAY_SECONDS` | `0` | Optional delay (passed to `sleep`) before executing (slow mode). |
| `FAKE_HANG_SECONDS` | `300` | Sleep duration in `hang` mode before self-exit if not killed by the caller. |
| `FAKE_HOST_ROOT` | *(unset)* | Local directory representing the remote host root. Required by `fake-rsync` for `host:/path` destinations and used by `fake-ssh` as the working directory when executing remote commands. |
| `FAKE_SSH_RUN_LOCAL` | `1` | When `1` and `FAKE_HOST_ROOT` is set, `fake-ssh` runs the remote command in `FAKE_HOST_ROOT` via `/bin/sh -c`. Set to `0` to skip execution and emit `FAKE_MARKER`. |
| `FAKE_SSH_STDOUT` | *(unset)* | Fixed stdout text emitted by `fake-ssh` in `success` mode (skips local execution). |
| `FAKE_SSH_STDERR` | *(unset)* | Fixed stderr text emitted by `fake-ssh`. |
| `FAKE_SSH_LOG` | *(unset)* | File path where `fake-ssh` appends tab-separated argv lines. Falls back to `FAKE_ARGS_LOG`. |
| `FAKE_RSYNC_LOG` | *(unset)* | File path where `fake-rsync` appends tab-separated argv lines. Falls back to `FAKE_ARGS_LOG`. |
| `FAKE_ARGS_LOG` | *(unset)* | Shared fallback argv log file path. |
| `FAKE_MARKER` | `REMOTE_OK` / `RSYNC_OK` | Pass marker line printed in `success` mode (when `fake-ssh` is not executing a local command). |
| `FAKE_FORBIDDEN_MARKER` | *(unset)* | Optional extra line printed in `success` mode for negative marker tests. |

## Modes

- `success`: executes the local fake-host action (or prints the pass marker) and exits `0`.
- `nonzero`: prints an error to stderr and exits with `FAKE_EXIT_CODE`.
- `conflict`: prints `A conflicting instance of Global\UnrealBuildTool_Mutex_<hash> is already running.` and exits `10`.
- `hang`: prints startup output and `FAKE_HANG_READY`, then `exec`s `sleep` until killed by the caller (no orphan child processes).
- `missing-marker`: exits `0` without printing the pass marker line.
