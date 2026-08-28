# clari

> **CLA**ude **R**un **I**nitiator. Keeps Claude running. That's it.

**The pain:** your Claude Code quota window closes, your agent freezes,
and it stays frozen until you happen to look at the terminal. The window
reopened forty minutes ago. Nobody pressed anything. You paid for those
forty minutes.

**The fix:** `clari` watches the rate-limit window through Claude Code's
official JSON `statusLine` hook — no UI scraping — and wakes your agent(s)
the second the window opens again. Optionally, right before the quota runs
out, it lets your main agent hand its pending work to other agents so the
fleet keeps moving while the main one waits.

One small binary. It does this one thing and goes back to sleep.

Want the whole fleet daemon — work board, dispatch, budgets, team
reconciliation, the works? That's clari's big sibling,
[climax](https://github.com/luismaf/climax). Every great session deserves
one.

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/luismaf/clari/master/scripts/install.sh | bash
```

Detects your system (Ubuntu/Debian via apt, Arch via PKGBUILD, macOS or
other Linux as a binary in `~/.local/bin`, Windows via cargo) and never
touches your services or config.

```bash
clari --install  # systemd user service, start on boot (opt-in)
```

## Quick start

```bash
clari          # start the daemon and print status
clari -s       # read-only status: ON/OFF, quota, hook, agents
clari -q       # stop the daemon (the service stays installed)
clari -d       # delegation on: hand work over right before the block
clari -n       # delegation off
clari -p       # just the usage %, for your scripts
```

Everything important has a short flag.

## How it works

1. Registers the `statusLine` hook in Claude Code's `settings.json`.
2. On every render, reads the quota window from the hook's JSON payload.
3. Before the block it warns (and delegates, if you turned that on).
4. At `resets_at + margin` it resumes every alive agent — once per window.

## Configuration

Flags write `~/.config/clari/config.toml` (hot-reloaded by the daemon).
Use `null` to clear any optional value.

| Flag | Config key | Default |
| --- | --- | --- |
| `-d[=MSG]` / `-n` | `delegation` (`delegation_prompt`) | `false` |
| `-t <name>` | `herdr_agent_target` | all `claude` agents |
| `-a` / `-o` | `resume_all` | `true` |
| `-p <pct>` / `--threshold` | `threshold_pct` | `90` |
| `-r <text>` | `resume_message` | `continue` |
| `--poll <secs>` | `poll_interval_secs` | `10` |
| `--margin <secs>` | `safety_margin_secs` | `15` |
| `--warning <secs>` | `warning_lead_time_secs` | `300` |
| `--herdr <bin>` | `herdr_bin` | `herdr` |
| `--session <name>` | `herdr_session` | — |
| `--kind <kind>` | `herdr_agent_kind` | `claude` |

## Uninstall

```bash
clari --uninstall  # removes only the service; hook, config and binary kept
rm ~/.local/bin/clari
```

## License

MIT
