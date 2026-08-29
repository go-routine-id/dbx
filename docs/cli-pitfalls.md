# dbx — Common CLI Tool Mistakes & How We Avoid Them

Companion to [pitfalls.md](pitfalls.md) (DB-client specific). This one covers
mistakes common to **CLI tools generally**, per [clig.dev](https://clig.dev/)
and real-world CLI bug reports. dbx is a TUI, but it's still launched and
scripted as a CLI — these rules apply at the boundary.

---

## 1. Terminal detection & TTY

| # | Mistake | Why it hurts | dbx mitigation |
|---|---------|-------------|----------------|
| 1.1 | Launching a TUI when stdout isn't a TTY (piped, CI, `ssh host dbx` without `-t`) | Garbage escape codes flood the pipe / CI log | **GAP →** check `is_terminal()` on stdout+stdin at startup; if not a TTY, print a clear error to stderr and exit non-zero |
| 1.2 | Progress bars/spinners when output is piped | "Christmas tree" CI logs | TUI only starts on a real TTY (1.1 makes this impossible) |
| 1.3 | `TERM=dumb` not honored | Escape soup on limited terminals | **GAP →** treat `TERM=dumb` as not-a-TUI-terminal: refuse with guidance |

## 2. Color conventions

| # | Mistake | dbx mitigation |
|---|---------|----------------|
| 2.1 | Ignoring `NO_COLOR` (any non-empty value disables color) | **GAP →** honor `NO_COLOR` + `DBX_NO_COLOR` + `--no-color`: theme falls back to plain monochrome styles |
| 2.2 | Overusing color until it means nothing | Already designed in: single accent color, muted palette (docs/ui-ux.md) |
| 2.3 | Disabling color globally when only stdout is piped | N/A for v0.1 (TUI requires TTY everywhere) — but keep per-stream checks in mind for future headless mode |

## 3. Exit codes & signals

| # | Mistake | dbx mitigation |
|---|---------|----------------|
| 3.1 | Exit 0 on failure | **GAP →** exit code contract: `0` clean quit, `1` runtime error, `2` usage/config error (clap's default for parse errors) |
| 3.2 | Ctrl+C dead in raw mode | ✅ Fixed in M0 (bug-hunt #2) |
| 3.3 | SIGTERM/SIGHUP leaves terminal in raw mode | **GAP →** install signal handlers that restore the terminal before exiting (panic hook + Drop guard don't catch signals) |

## 4. Config discovery

| # | Mistake | dbx mitigation |
|---|---------|----------------|
| 4.1 | Hardcoding `~/.dbx`, ignoring `XDG_CONFIG_HOME` | `dirs` crate follows XDG; **GAP →** document precedence: `--config` flag > `$DBX_CONFIG` > `$XDG_CONFIG_HOME/dbx/` > `~/.config/dbx/` |
| 4.2 | macOS GUI path (`~/Library/Application Support`) for a CLI | CLI convention is `~/.config` even on macOS (gh, git, kubectl do this) — `dirs::config_dir` behavior verified, not assumed |
| 4.3 | Leaking `XDG_CONFIG_HOME` to spawned children | dbx doesn't spawn XDG-aware children; if we ever do (e.g. `$EDITOR`), never mutate process-wide env |

## 5. Help & discoverability

| # | Mistake | dbx mitigation |
|---|---------|----------------|
| 5.1 | Minimal `--help` without examples | **GAP →** clap `after_help` with 3-4 real examples (connect, custom config, version) |
| 5.2 | No shell completions | **GAP →** `clap_complete` for zsh/bash/fish — small crate, big daily QoL. Backlog (post-v0.1) |
| 5.3 | First run with zero config = dead end | ✅ Designed: S1 empty state teaches `n` for new connection |

## 6. Output discipline (for future headless/scripting mode)

| # | Mistake | dbx mitigation |
|---|---------|----------------|
| 6.1 | Human text on stdout, machine output mixed | When a headless mode exists (`dbx query "..."`), machine output → stdout, diagnostics → stderr. Backlog item, rule recorded now |
| 6.2 | No machine-readable option | `--json` for any future headless command |
| 6.3 | Logging to stdout/stderr while TUI owns the screen | **GAP →** `DBX_LOG=<path>` file logging for debugging; zero log output to the terminal while the TUI runs |

## 7. Crash & bug reports

| # | Mistake | dbx mitigation |
|---|---------|----------------|
| 7.1 | Panic prints into the alternate screen → user sees nothing after restore | Panic hook restores terminal FIRST, then prints (✅ M0); **GAP →** also write panic to a crash log (`~/.local/state/dbx/crash.log`) with version + OS for bug reports |
| 7.2 | No `--version` | ✅ clap `version` since M0 |

---

## Summary: new gaps found

| # | Gap | Proposed placement |
|---|-----|--------------------|
| 1 | TTY detection + graceful refusal | M1 task 1.4 area (startup path) |
| 2 | `TERM=dumb` refusal | same |
| 3 | `NO_COLOR` / `DBX_NO_COLOR` / `--no-color` | theme system extension (M4 polish) |
| 4 | Exit code contract (0/1/2) | M1 (first real error paths) |
| 5 | SIGTERM/SIGHUP → terminal restore | M1 (runtime hardening) |
| 6 | Config precedence documentation | M1 (config loader) |
| 7 | `--help` examples | M1 CLI touch-up |
| 8 | `DBX_LOG` file logging | M1 (debuggability) |
| 9 | Panic → crash log file | M1 |
| 10 | Shell completions (`clap_complete`) | backlog post-v0.1 |
| 11 | Headless mode (`dbx query`) + `--json` | backlog post-v0.1 |

## Sources

- [Command Line Interface Guidelines (clig.dev)](https://clig.dev/)
- [atmos — macOS XDG CLI conventions](https://atmos.tools/changelog/macos-xdg-cli-conventions)
- [GitHub Copilot CLI — XDG_CONFIG_HOME ignored](https://github.com/github/copilot-cli/issues/2035)
- [openclaw — XDG leak to child processes](https://github.com/openclaw/openclaw/issues/79847)
- [Veracode CLI exit codes](https://docs.veracode.com/r/CLI_exit_codes)
