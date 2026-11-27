# Unreal Test Fixtures

Portable POSIX shell stand-ins for Unreal Engine build and runtime tools, plus
tiny scrubbed log samples. These fixtures let tests run without an engine
installation.

## Programs

- `fake-ubt` - stand-in for UnrealBuildTool (`UnrealBuildTool.dll`).
- `fake-uat` - stand-in for Unreal AutomationTool (`BuildCookRun`).
- `fake-editor-cmd` - stand-in for `UnrealEditor-Cmd` (headless commandlets).
- `fake-packaged-game` - stand-in for a packaged Linux game binary.

## Environment Knobs

All four programs accept the same environment variables:

| Variable | Default | Description |
| --- | --- | --- |
| `FAKE_MODE` | `success` | Behavior mode: `success`, `nonzero`, `conflict`, `hang`, or `missing-marker`. |
| `FAKE_EXIT_CODE` | `1` | Exit status used when `FAKE_MODE=nonzero`. |
| `FAKE_MUTEX_HASH` | `0123456789abcdef0123456789abcdef01234567` | 40-char hex hash inserted into the `ConflictingInstance` mutex name when `FAKE_MODE=conflict`. |
| `FAKE_DELAY_SECONDS` | `0` | Optional delay (passed to `sleep`) before emitting output (slow mode). |
| `FAKE_HANG_SECONDS` | `300` | Sleep duration in `hang` mode before self-exit if not killed by the caller. |
| `FAKE_ARGS_LOG` | *(unset)* | File path to append tab-separated argv lines for each invocation. |
| `FAKE_MARKER` | *(per fixture)* | Pass marker line printed in `success` mode (`Result: Succeeded`, `BUILD SUCCESSFUL`, or `FIXTURE_MARKER: PASS`). |
| `FAKE_FORBIDDEN_MARKER` | *(unset)* | Optional extra line printed in `success` mode for negative marker tests. |
| `FAKE_PROJECT` | `SampleGame` | Generic project name used in log lines. |
| `FAKE_ENGINE_VERSION` | `5.5.0` | Engine release version string printed by `fake-packaged-game`. |

## Command-Line Flags Recognized

- `-log=<path>` and `-abslog=<path>`: mirror emitted lines into `<path>`.
- `-archivedirectory=<path>` (`fake-uat` in `success` mode): creates a minimal
  `<path>/Linux/<project>/Content/Paks/<project>-Linux.pak` tree.

## Modes

- `success`: prints realistic output including the expected pass marker and exits `0`.
- `nonzero`: prints an error summary and exits with `FAKE_EXIT_CODE`.
- `conflict`: prints `A conflicting instance of Global\UnrealBuildTool_Mutex_<hash> is already running.` and exits `10`.
- `hang`: prints startup output and `FAKE_HANG_READY`, then `exec`s `sleep` until killed by the caller (no orphan child processes).
- `missing-marker`: exits `0` with normal output but omits the expected pass marker line.

## Scrubbed Log Samples

`samples/` contains tiny, scrubbed excerpts of UBT, UAT, commandlet, and
packaged game output for parser and judge tests. All paths, project names, user
names, and machine identifiers have been replaced with generic placeholders.
