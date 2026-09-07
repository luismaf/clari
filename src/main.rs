use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local, Utc};
use clap::Parser;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::io::{self, IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::process::Command;
use tokio::time::sleep;
use tracing::{debug, info, warn};

// ─────────────────────────────────────────────
// Colors (only when stdout is a terminal)
// ─────────────────────────────────────────────
const GREEN: &str = "\x1b[0;32m";
const YELLOW: &str = "\x1b[0;33m";
const RED: &str = "\x1b[0;31m";
const CYAN: &str = "\x1b[0;36m";
const RESET: &str = "\x1b[0m";

fn painted(s: &str, code: &str) -> String {
    if io::stdout().is_terminal() {
        format!("{code}{s}{RESET}")
    } else {
        s.to_string()
    }
}

// ─────────────────────────────────────────────
// Config
// ─────────────────────────────────────────────
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Config {
    /// Usage % that fires the delegation and the fine-grained 5s
    /// monitoring (default 90). Applies with or without resets_at.
    #[serde(default = "default_threshold")]
    threshold_pct: f64,
    /// Seconds before the block to inject the warning (default 300s = 5 min)
    #[serde(default = "default_warning_lead")]
    warning_lead_time_secs: u64,
    /// Extra seconds after resets_at before sending the resume.
    /// Kept deliberately small: you want to wake up as soon as the quota
    /// frees up (the reset usually lands on time; 15s covers clock skew).
    #[serde(default = "default_margin")]
    safety_margin_secs: u64,
    /// Force the reset window (epoch, seconds), ignoring the hook JSON one.
    /// Useful if you have several agents with different windows, a stale
    /// hook, or you want to align the wake-up to a specific reset.
    /// 'null' (default) = use the hook's.
    #[serde(default)]
    forced_resets_at: Option<i64>,
    /// How often (seconds) the JSON is checked. Clamped at runtime to a
    /// sane minimum so it never hammers the CPU or herdr's socket.
    #[serde(default = "default_poll")]
    poll_interval_secs: u64,

    /// herdr binary to invoke (in case it isn't on PATH with that exact name).
    #[serde(default = "default_herdr_bin")]
    herdr_bin: String,
    /// Pin ONE named herdr session (passed as HERDR_SESSION). None
    /// (default) = EVERY running herdr session (`herdr session list`) is
    /// watched, so a limit wakes the whole fleet, not just one session.
    herdr_session: Option<String>,
    /// The agent kind we look for with `herdr agent list` when there is
    /// no explicit target (see herdr_agent_target). See `herdr agent start --help`
    /// for the supported kinds; ours is "claude".
    #[serde(default = "default_herdr_agent_kind")]
    herdr_agent_kind: String,
    /// Alive agent name or explicit pane_id (e.g. "w1:p1" or the name you
    /// gave it with `herdr agent start <name> ...` / `agent rename`).
    /// If set, NO autodetection is done — it is used as-is.
    /// Needed if you run more than one Claude agent at once.
    herdr_agent_target: Option<String>,

    /// If true (default), resume and delegation reach ALL alive
    /// kind='claude' agents, not only the pin (herdr_agent_target).
    /// If false, delegation/resume goes ONLY to the pin (or the first
    /// one if no pin). Configurable with -a/--all and -o/--no-all.
    #[serde(default = "default_true")]
    resume_all: bool,

    /// Exact delegation prompt text. If not in the config, the embedded
    /// default is used (DEFAULT_DELEGATION_PROMPT).
    #[serde(default = "default_delegation_prompt")]
    delegation_prompt: String,
    /// If false (default), DELEGATION is off: nothing is injected into the
    /// agent before the block (only auto-resume at the reset).
    /// Turn it on with `--set delegation=true`: before the hard limit the
    /// `delegation_prompt` is injected into the main agent (see the
    /// default embedded in DEFAULT_DELEGATION_PROMPT).
    #[serde(default)]
    delegation: bool,
    #[serde(default = "default_resume_msg")]
    resume_message: String,
    #[serde(default = "default_state_path")]
    state_path: PathBuf,
    #[serde(default = "default_statusline_path")]
    statusline_json_path: PathBuf,

    /// If true (default), on daemon start it verifies that the Claude Code
    /// settings.json has clari's statusLine hook and installs it if
    /// missing. Never overwrites a statusLine pointing to another command.
    #[serde(default = "default_true")]
    install_statusline_hook: bool,
    /// Path of the user Claude Code settings.json. Default:
    /// $CLAUDE_CONFIG_DIR/settings.json o ~/.claude/settings.json.
    claude_settings_path: Option<PathBuf>,

    /// If true (default), the moment the hard limit hits, every claude
    /// agent stuck on Claude Code's "Usage limit reached" screen gets the
    /// `/low-priority` command, so it keeps working in the background at
    /// lower priority instead of freezing until the reset. The screen is
    /// read first (never sent blind: the command is a toggle) and the
    /// result is verified on the next poll. Off with --no-low-priority.
    #[serde(default = "default_true")]
    low_priority: bool,
    /// Slash command sent to a blocked agent (default "/low-priority").
    #[serde(default = "default_low_priority_command")]
    low_priority_command: String,
}

fn default_threshold() -> f64 {
    90.0
}
fn default_warning_lead() -> u64 {
    300
}
fn default_margin() -> u64 {
    15
}
fn default_poll() -> u64 {
    10
}
fn default_herdr_bin() -> String {
    "herdr".to_string()
}
fn default_herdr_agent_kind() -> String {
    "claude".to_string()
}
fn default_delegation_prompt() -> String {
    DEFAULT_DELEGATION_PROMPT.to_string()
}
fn default_resume_msg() -> String {
    "continue".into()
}
fn default_low_priority_command() -> String {
    "/low-priority".into()
}
fn default_state_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".local/state/clari/state.json")
}
fn default_true() -> bool {
    true
}
fn default_statusline_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".claude/statusline-cache.json")
}

// ─────────────────────────────────────────────
// Budget limits (limites.json)
// ─────────────────────────────────────────────
#[derive(Debug, Clone, Serialize, Deserialize)]
struct LimitesConfig {
    /// Maximum concurrent dev agents. `None` = unlimited.
    max_devs: Option<u32>,
    /// Maximum USD spent per day. `None` = unlimited.
    usd_dia: Option<f64>,
    /// Maximum tokens per session (default 50 000).
    #[serde(default = "default_tokens_sesion")]
    tokens_sesion: u64,
}

fn default_tokens_sesion() -> u64 {
    50_000
}

impl Default for LimitesConfig {
    fn default() -> Self {
        Self {
            max_devs: None,
            usd_dia: None,
            tokens_sesion: default_tokens_sesion(),
        }
    }
}

fn limites_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_default()
        .join("clari/limites.json")
}

fn load_limites() -> LimitesConfig {
    let path = limites_path();
    if !path.exists() {
        return LimitesConfig::default();
    }
    fs::read_to_string(&path)
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok())
        .unwrap_or_default()
}

fn save_limites(cfg: &LimitesConfig) -> Result<()> {
    let path = limites_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let tmp = path.with_extension("tmp");
    let json = serde_json::to_string_pretty(cfg)
        .context("serializing limites.json")?;
    fs::write(&tmp, &json)
        .with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, &path)
        .with_context(|| format!("renaming to {}", path.display()))?;
    Ok(())
}

impl Default for Config {
    fn default() -> Self {
        Self {
            threshold_pct: default_threshold(),
            warning_lead_time_secs: default_warning_lead(),
            safety_margin_secs: default_margin(),
            forced_resets_at: None,
            poll_interval_secs: default_poll(),
            herdr_bin: default_herdr_bin(),
            herdr_session: None,
            herdr_agent_kind: default_herdr_agent_kind(),
            herdr_agent_target: None,
            resume_all: default_true(),
            delegation_prompt: default_delegation_prompt(),
            delegation: false,
            resume_message: default_resume_msg(),
            state_path: default_state_path(),
            statusline_json_path: default_statusline_path(),
            install_statusline_hook: default_true(),
            claude_settings_path: None,
            low_priority: default_true(),
            low_priority_command: default_low_priority_command(),
        }
    }
}

const DEFAULT_DELEGATION_PROMPT: &str = r#"URGENT — YOUR RATE-LIMIT WINDOW IS ABOUT TO EXPIRE. This Claude Code session is about to hit the hard block (the window comes from your plan/usage — not always 5 hours — and it won't recover until it resets). You have ONE job: maximize the autonomous work of the other agents without you. NO implementing, NO chatting, NO asking permission, NO tokens spent describing what you do.
MANDATORY PLAN (do it NOW, before the block):
1. Prepare your session: stop any long-running task and guarantee your session is left easy to resume (check the working tree; no half-made changes without a commit).
2. Write a work plan of at least 200 hours (or the maximum that makes sense for the current project) — enough that every delegated agent stays busy and advancing for the WHOLE block (~5 hours, while you are gone) — in a single file:
   - PWD/<PROJECT>-delegation-plan.md
   - Clear tasks, prioritized by value and independence, each with a verifiable "done" criterion, in the exact order a fresh agent should execute them.
   - Include at the end a "CONTEXT" block: where the code lives, repo conventions, how to run build/tests, and the current state of the work.
3. Delegate ALL of that with herdr (you have HERDR_ENV=1 and the skill):
   - herdr pane split --current --direction right --cwd "$PWD" --no-focus
     → capture the pane-id that the output returns.
   - herdr agent start opc-deleg --kind opencode --pane <pane-id> --timeout 300000
     → wait for success (agent ready, detected in the pane).
   - herdr agent prompt opc-deleg "Work autonomously. Read <plan> and execute it. Done criteria are in the plan. Update the state file (below) at the end of each task. If you hit a blocker that needs a business decision, resolve it the best reasonable way and keep going, documenting the decision." --wait
     → confirm that the prompt was taken (don't assume it).
   - If there are two or more fully independent areas, repeat the split/start/prompt per area (max 3 agents). NEVER split one task between two agents.
4. If the herdr skill is not available, delegate through whatever fallback you have configured (opencode another route, subagent, coding MCP). Don't stop delegating.
5. Update HANDOFF.md (or the root-project equivalent):
   - How to resume your own work when you return (what you did, where you left it, what you tried).
   - What was delegated and to whom (agent name + pane-id), with which plan, and the state of each area.
   - What remains when the quota comes back.
6. When everything is delegated and confirmed: wait calmly for the block. Don't keep consuming tokens; don't redo delegated work; don't write long summaries. A short line is enough.
When the reset comes (the guard handles it automatically), the first action will be to resume from HANDOFF.md, respecting that the delegated work stays with the other agent."#;

/// Appended to the resume message when DELEGATION is on: while it was blocked
/// the agent handed its work over to the other herdr agents, so on return it
/// must tell the team lead that it is back.
const RESUME_DELEGATION_NOTICE: &str =
    "Notify the team lead on herdr that you are back and ready to resume the work you delegated.";

// ─────────────────────────────────────────────
// State
// ─────────────────────────────────────────────
#[derive(Debug, Default, Serialize, Deserialize)]
struct GuardState {
    last_injected_reset_at: Option<i64>,
    last_hard_limit_reset_at: Option<i64>,
    /// The window (resets_at) the fleet is blocked on. Recorded the moment
    /// the hard limit is seen and cleared only once every agent has been
    /// resumed after the reset. The resume is driven by THIS field and the
    /// clock, never by the hook JSON: the JSON flips to 0% the instant the
    /// window opens (any pane render refreshes it), so "still blocked at
    /// reset + margin" is not a signal that can be waited for.
    #[serde(default)]
    blocked_reset_at: Option<i64>,
    /// Window for which we already warned that resets_at went stale
    /// (avoids repeating the warning every poll).
    warned_stale_reset_at: Option<i64>,
    /// Woken per agent: target -> resets_at of the last window for which
    /// the resume was already sent (multi-target dedup).
    #[serde(default)]
    woken_targets: HashMap<String, i64>,
    /// Delegation prompt injected per agent: target -> resets_at of the
    /// window for which the prompt was already delivered (so a failed/incomplete
    /// injection is retried without spamming the ones that succeeded).
    #[serde(default)]
    injected_targets: HashMap<String, i64>,
    /// `/low-priority` handled per agent: target -> resets_at of the
    /// window in which the agent was switched to (or found already in)
    /// lower-priority mode, or in which the mode turned out unavailable.
    #[serde(default)]
    low_priority_targets: HashMap<String, i64>,
    /// Last time (epoch secs) the command was sent per agent, so retries
    /// are spaced out (the command is a toggle: never fire it twice fast).
    #[serde(default)]
    low_priority_attempts: HashMap<String, i64>,
}

// ─────────────────────────────────────────────
// Pause state (~/.config/clari/pause.json)
// ─────────────────────────────────────────────
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
struct PauseState {
    global: bool,
    agents: Vec<String>,
}

fn pause_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_default()
        .join("clari/pause.json")
}

fn load_pause_state(path: &Path) -> PauseState {
    let p = resolve_path(path);
    if p.exists() {
        fs::read_to_string(&p)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    } else {
        PauseState::default()
    }
}

fn save_pause_state(path: &Path, state: &PauseState) -> Result<()> {
    let p = resolve_path(path);
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = p.with_extension("tmp");
    fs::write(&tmp, serde_json::to_string_pretty(state)?)
        .with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, &p).with_context(|| format!("renaming to {}", p.display()))?;
    Ok(())
}

// ─────────────────────────────────────────────
// Rate Info (from the hook JSON, not from the screen)
// ─────────────────────────────────────────────
#[derive(Debug, Clone, Default)]
struct RateInfo {
    used_pct: f64,
    resets_at: Option<i64>,
    hard_limit_hit: bool,
}

// ─────────────────────────────────────────────
// CLI
// ─────────────────────────────────────────────
const AFTER_HELP: &str = r#"MODES:
  (no flags)         start the daemon (installing the service first if it
                     is missing) AND print the status (default)
  -z, --start        explicit daemon start (same as no flags)
  -s, --status       read-only status (never starts the daemon)
  -q, --stop         stop the daemon (the service, or the process; never
                     uninstalls) · --rehearsal  dry run (no effects)
  --write-statusline Claude Code hook (stdin JSON -> cache) · --install/--uninstall service
  -d[=MSG] / -n      DELEGATION on (custom message) / off · -t <name> pin agent
  -a / -o            resume+delegate ALL claude agents / only the pin
  pause              pause clari: no wake or delegation · --agent <name> pauses one
  resume             resume a paused clari · --agent <name> resumes one

PAUSE: when paused globally, clari keeps running (reads quota) but sends nothing
to any agent — no delegation, no resume. Per-agent pause skips only that agent.
State persists in ~/.config/clari/pause.json and is hot-reloaded by the daemon.

DELEGATION (off by default): before the window ends, asks the main agent to hand
its work over to the other herdr agents; at the reset the auto-resume wakes them
("continue", editable with -r/--resume and, with delegation on, tells the team
lead that the agent is back).

EXAMPLES:
  clari                 Start daemon + status    clari -s             Status (read-only)
  clari -z              Start daemon (explicit)  clari -q             Stop daemon
  clari -d 'delegate now'  Delegation on         clari -r 'continue'  Custom resume
  clari -t w5:p2        Watch one                clari --rehearsal    Dry run
  clari -p              Usage % (machine)        clari -t/-l          List targets/panels
  clari --blocked       0 or the blocked target(s) (for scripts/apps)
  clari pause           Pause globally           clari resume         Unpause all
  clari pause --agent w1:p1  Pause one agent     clari resume --agent w1:p1  Unpause one

NOTES:
  Config flags write ~/.config/clari/config.toml (hot-reloaded; edit by hand too).
  Service logs: journalctl --user -u clari.service -f
  Install: curl -fsSL https://raw.githubusercontent.com/luismaf/clari/master/scripts/install.sh | bash"#;

#[derive(Parser, Debug)]
#[command(
    name = "clari",
    version,
    disable_version_flag = true,
    about = "Claude Code quota guard (JSON hook + auto-resume, orchestrated over herdr)",
    after_help = AFTER_HELP
)]
struct Cli {
    /// Print version.
    #[arg(short = 'v', long = "version")]
    version: bool,

    /// Path to the TOML config file (default: ~/.config/clari/config.toml).
    #[arg(short, long, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Show status: % used, reset, window, agents (read-only).
    #[arg(short = 's', long)]
    status: bool,

    /// Machine-readable hard-limit status for scripts/apps: prints
    /// "1" when blocked, "0" otherwise.
    #[arg(long = "blocked")]
    blocked: bool,

    /// Rehearsal: full daemon cycle WITHOUT running herdr or sending prompts.
    #[arg(long = "rehearsal")]
    dry_run: bool,

    /// Start the daemon (same as a bare `clari`): if the systemd user
    /// service is not installed it is installed first, then started, and
    /// the command returns (no foreground loop). Otherwise it prints the
    /// full status and runs in the foreground.
    #[arg(short = 'z', long = "start")]
    start: bool,

    /// Stop the running clari daemon (the systemd user service when it is
    /// installed, otherwise the daemon process). Never uninstalls the
    /// service. Prints the full status after.
    #[arg(short = 'q', long = "stop")]
    stop: bool,

    /// Run the guard loop unconditionally in this process (no systemd
    /// delegation). Internal: used by the generated service unit, whose
    /// process must never hand the work back to systemd.
    #[arg(long, hide = true)]
    daemon_loop: bool,

    /// Turn DELEGATION on (writes to the config file, hot-reloaded).
    /// The custom delegation message can be given inline with `-d=MSG`
    /// (or `--delegate=MSG`) or as trailing arguments (no quotes needed).
    /// Without it, the embedded default is used. The active message is
    /// printed to stdout.
    #[arg(short = 'd', long, value_name = "MSG", require_equals = true)]
    delegate: Option<Option<String>>,

    /// Turn DELEGATION off (default; writes to the config file).
    #[arg(short = 'n', long)]
    no_delegate: bool,

    /// Custom delegation message: every remaining argument, joined with
    /// spaces, becomes the delegation_prompt. Only meaningful together
    /// with -d/--delegate (the shell needs no quotes).
    #[arg(value_name = "MSG...")]
    message: Vec<String>,

    /// Watch ONLY that agent/pane of herdr. "null" clears the pin and
    /// goes back to watching ALL kind='claude' agents. Without a value,
    /// prints the targets that would be resumed/delegated.
    #[arg(short = 't', long, value_name = "AGENT", num_args = 0..=1, default_missing_value = "")]
    target: Option<String>,

    /// List every alive `claude` panel (target name or pane_id), one per line.
    #[arg(short = 'l', long = "list")]
    list: bool,

    /// Resume/delegate to ALL kind='claude' windows (default: on), not
    /// only the pinned herdr_agent_target.
    #[arg(short = 'a', long)]
    all: bool,

    /// Resume/delegate ONLY to the pinned herdr_agent_target (or the
    /// first detected one). Opposite of --all (which is the default).
    #[arg(short = 'o', long)]
    no_all: bool,

    /// When the limit hits, send /low-priority to every claude agent stuck
    /// on the "Usage limit reached" screen so it keeps working at lower
    /// priority (default: on; writes to the config file).
    #[arg(short = 'L', long = "low-priority")]
    low_priority: bool,

    /// Don't send /low-priority: blocked agents wait for the reset.
    #[arg(long = "no-low-priority")]
    no_low_priority: bool,

    /// statusLine hook for Claude Code: receives JSON on stdin and stores
    /// it in statusline_json_path (the guard reads it afterwards).
    #[arg(long)]
    write_statusline: bool,

    /// Install the systemd user service (boot autorun) and start it.
    /// Writes only the unit file: the binary stays where the installer
    /// put it (clari never copies itself). Prints the full status after.
    #[arg(long = "install")]
    install_service: bool,

    /// Uninstall the systemd service. Removes ONLY the service: the
    /// statusLine hook, the config and the binary are kept. Prints the
    /// full status after.
    #[arg(long = "uninstall")]
    uninstall_service: bool,

    /// Check frequency in seconds (minimum 5; default 10).
    #[arg(long, value_name = "SECS")]
    poll: Option<u64>,

    /// Seconds after the reset before sending the resume (default 15).
    #[arg(long, value_name = "SECS")]
    margin: Option<u64>,

    /// How many seconds before the block to warn (default 300).
    #[arg(long, value_name = "SECS")]
    warning: Option<u64>,

    /// Usage % that fires the delegation (and the fine-grained 5s monitoring).
    /// Applies with or without resets_at in the hook JSON (default 90).
    #[arg(long, value_name = "PCT")]
    threshold: Option<f64>,

    /// Same as --threshold, with a short flag: % of usage that fires
    /// the delegation prompt (default 90). Without a number it prints
    /// the current usage % instead.
    #[arg(short = 'p', long = "percent", value_name = "PCT", num_args = 0..=1, default_missing_value = "")]
    percent: Option<String>,

    /// Force the reset window (epoch seconds). "null" clears it.
    #[arg(long, value_name = "EPOCH")]
    forced_reset: Option<String>,

    /// herdr binary (path or name) to invoke.
    #[arg(long, value_name = "BIN")]
    herdr: Option<String>,

    /// Pin ONE named herdr session (passed as HERDR_SESSION). "null"
    /// removes the pin: every running herdr session is watched (default).
    #[arg(long, value_name = "NAME")]
    session: Option<String>,

    /// herdr agent kind to watch (default: claude).
    #[arg(long, value_name = "KIND")]
    kind: Option<String>,

    /// Resume text sent when the window opens (default: continue).
    #[arg(short = 'r', long = "resume", value_name = "TEXT")]
    resume_msg: Option<String>,

    /// Don't auto-install the statusLine hook on daemon start.
    #[arg(long)]
    no_install_hook: bool,

    /// Path of the guard state file (default: ~/.local/state/clari/state.json).
    #[arg(long, value_name = "PATH")]
    state_file: Option<PathBuf>,

    /// Path of the statusline cache written by the hook.
    #[arg(long, value_name = "PATH")]
    statusline: Option<PathBuf>,

    /// Path of the Claude Code settings.json. "null" restores the default.
    #[arg(long, value_name = "PATH")]
    settings: Option<String>,

    /// Budget-limit subcommand (show/edit limites.json).
    #[command(subcommand)]
    command: Option<Subcommands>,
}

#[derive(clap::Subcommand, Debug)]
enum Subcommands {
    /// Show or edit budget limits (~/.config/clari/limites.json).
    ///
    /// Without flags: display the current limits.
    /// With flags: update the specified fields (null clears).
    Limites {
        /// Maximum concurrent dev agents. Pass without a value to clear (unlimited).
        #[arg(long)]
        max_devs: Option<Option<u32>>,

        /// Max USD per day. Pass without a value to clear (unlimited).
        #[arg(long)]
        usd_dia: Option<Option<f64>>,

        /// Max tokens per session (default 50 000). Pass without a value to reset.
        #[arg(long)]
        tokens_sesion: Option<Option<u64>>,
    },

    /// Pause clari: no wake or delegation to agents.
    ///
    /// Without --agent: pauses globally (all agents).
    /// With --agent: pauses only that specific agent.
    Pause {
        /// Agent name to pause (if omitted, pauses globally).
        #[arg(long = "agent", value_name = "NAME")]
        agent: Option<String>,
    },

    /// Resume a paused clari (undo pause).
    ///
    /// Without --agent: clears all pause state.
    /// With --agent: unpauses only that specific agent.
    Resume {
        /// Agent name to unpause (if omitted, clears all pause state).
        #[arg(long = "agent", value_name = "NAME")]
        agent: Option<String>,
    },
}

// ─────────────────────────────────────────────
// Main
// ─────────────────────────────────────────────
#[tokio::main]
async fn main() -> Result<()> {
    // Don't panic when stdout is a closed pipe (e.g. `clari -s | head`).
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();

    if cli.version {
        println!("clari {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    let config_path: PathBuf = cli.config.clone().unwrap_or_else(default_config_path);

    // ── Subcommands (handled early, before the config path) ──
    if let Some(Subcommands::Limites { max_devs, usd_dia, tokens_sesion }) = &cli.command {
        return handle_limites_subcommand(max_devs, usd_dia, tokens_sesion);
    }
    if let Some(Subcommands::Pause { agent }) = &cli.command {
        let p = pause_path();
        let mut pause_state = load_pause_state(&p);
        if let Some(name) = agent {
            if !pause_state.agents.contains(name) {
                pause_state.agents.push(name.clone());
            }
            println!("Agent '{}' paused.", name);
        } else {
            pause_state.global = true;
            println!("Global pause activated.");
        }
        save_pause_state(&p, &pause_state)?;
        println!();
        let cfg = load_config_from(&config_path)?;
        print_full_status(&cfg).await?;
        return Ok(());
    }
    if let Some(Subcommands::Resume { agent }) = &cli.command {
        let p = pause_path();
        let mut pause_state = load_pause_state(&p);
        if let Some(name) = agent {
            pause_state.agents.retain(|a| a != name);
            println!("Agent '{}' unpaused.", name);
        } else {
            pause_state.global = false;
            pause_state.agents.clear();
            println!("Pause cleared.");
        }
        save_pause_state(&p, &pause_state)?;
        println!();
        let cfg = load_config_from(&config_path)?;
        print_full_status(&cfg).await?;
        return Ok(());
    }

    if cli.install_service || cli.uninstall_service {
        if cli.install_service && cli.uninstall_service {
            bail!("--install and --uninstall are mutually exclusive");
        }
        if cli.status || cli.start || cli.stop || cli.dry_run || cli.write_statusline {
            bail!("--install/--uninstall don't combine with other modes");
        }
        if cli.install_service {
            install_service()?;
        } else {
            uninstall_service()?;
        }
        println!();
        print_full_status(&load_config_from(&config_path)?).await?;
        return Ok(());
    }

    if [cli.status, cli.start, cli.stop]
        .iter()
        .filter(|&&on| on)
        .count()
        > 1
    {
        bail!("--status, --start and --stop are mutually exclusive");
    }

    // `-p` / `--percent` without a number: report the current usage %.
    if cli.percent.as_deref() == Some("") {
        let config = load_config_from(&config_path)?;
        let info = gather_rate_info(&config)?;
        println!("{:.1}", info.used_pct);
        return Ok(());
    }

    // `-t` / `--target` without a value: list the targets that would be
    // resumed/delegated (the pin, or every alive claude agent), one per line.
    if cli.target.as_deref() == Some("") {
        let config = load_config_from(&config_path)?;
        let targets = resolve_wake_targets(&config).await?;
        for t in &targets {
            println!("{}", t.key());
        }
        return Ok(());
    }

    // `-l` / `--list`: list every alive `claude` panel (target/pane_id), one per line.
    if cli.list {
        let config = load_config_from(&config_path)?;
        let agents = list_kind_agents(&config).await?;
        for a in &agents {
            println!("{}", a.key());
        }
        return Ok(());
    }

    // `--blocked`: machine-readable. Prints "0" when nothing is blocked, or
    // the blocked target(s) (one per line) when some agent can't work.
    if cli.blocked {
        let config = load_config_from(&config_path)?;
        let info = gather_rate_info(&config)?;
        let targets = match resolve_wake_targets(&config).await {
            Ok(t) => t,
            Err(_) => Vec::new(),
        };
        let blocked: Vec<Target> = if info.hard_limit_hit {
            // Global quota block: every target is blocked.
            targets
        } else {
            let mut b = Vec::new();
            for t in &targets {
                let status = get_agent_status(&config, t).await.unwrap_or_default();
                if agent_is_blocked(&status) {
                    b.push(t.clone());
                }
            }
            b
        };
        if blocked.is_empty() {
            println!("0");
        } else {
            for t in &blocked {
                println!("{}", t.key());
            }
        }
        return Ok(());
    }

    // Direct config flags (write to the TOML, hot-reloaded).
    let settings = collect_settings(&cli)?;
    if !settings.is_empty() {
        if cli.status || cli.start || cli.stop || cli.dry_run || cli.write_statusline {
            bail!(
                "config flags don't combine with --status/--start/--stop/--rehearsal/--write-statusline"
            );
        }
        apply_config_settings(&config_path, &settings)?;
        return Ok(());
    }

    let mut config = load_config_from(&config_path)?;
    let dry_run = cli.dry_run;

    if cli.write_statusline {
        return write_statusline(&config);
    }

    // Polling interval hard floor: never under 5s, whatever the config says.
    clamp_poll(&mut config);

    // `--stop`: stop the daemon (systemd service or the daemon process),
    // then report the full status so you see it actually went off.
    if cli.stop {
        stop_daemon()?;
        println!();
        print_full_status(&config).await?;
        return Ok(());
    }

    // Read-only status: the only mode that never touches the daemon.
    if cli.status {
        print_full_status(&config).await?;
        return Ok(());
    }

    // Daemon mode (no flags, -z/--start or --rehearsal).
    //
    // A fresh start must not fail in any situation: make sure the config
    // file exists (with defaults) and the statusLine hook is in place.
    // Both are best-effort.
    ensure_config(&config_path);
    if let Err(e) = run_hook_install(&config, dry_run) {
        warn!("Hook check/install: {:#}", e);
    }

    //
    // `clari` / `-z` / `--start` always end up with the systemd user
    // service up: if the unit is missing it is installed first, then
    // started, and the command returns (the terminal is not occupied).
    // The service itself runs with --daemon-loop, so its process never
    // delegates back to systemd. --rehearsal always stays foreground.
    if !cli.dry_run && !cli.daemon_loop {
        let unit_installed = user_systemd_dir().join(SERVICE_UNIT_NAME).exists();
        if unit_installed {
            let was_active = is_service_active();
            match run_systemctl(&["--user", "start", SERVICE_UNIT_NAME]) {
                Ok(()) => {
                    // Only announce the start when it actually made a change
                    // (the service was down): if it was already running the
                    // message is just noise.
                    if !was_active {
                        println!(
                            "{}",
                            painted(
                                &format!(
                                    "Daemon started via systemd user service {}.",
                                    SERVICE_UNIT_NAME
                                ),
                                GREEN,
                            )
                        );
                        println!();
                    }
                    print_full_status(&config).await?;
                    return Ok(());
                }
                Err(e) => warn!(
                    "Could not start the systemd user service ({:#}); \
                     running the daemon in the foreground instead.",
                    e
                ),
            }
        } else {
            // Not installed yet: install it first, so a bare `clari` or
            // `--start` leaves the guard running as a service, never as a
            // hanging foreground process.
            match install_service() {
                Ok(()) => {
                    println!();
                    print_full_status(&config).await?;
                    return Ok(());
                }
                Err(e) => warn!(
                    "Could not install the systemd user service ({:#}); \
                     running the daemon in the foreground instead.",
                    e
                ),
            }
        }
    }

    // Foreground daemon (--rehearsal, the service's own --daemon-loop
    // process, or when systemd is unavailable): report the full status
    // first, then run the guard loop.
    print_full_status(&config).await?;
    println!();

    info!("clari daemon started (JSON hook + herdr, no UI scraping)");
    info!(
        "warning_lead = {}s | poll = {}s | margin = {}s | agent_kind = {}",
        config.warning_lead_time_secs,
        config.poll_interval_secs,
        config.safety_margin_secs,
        config.herdr_agent_kind
    );

    if !resolve_path(&config.statusline_json_path).exists() {
        warn!(
            "{} does not exist yet: the hook will generate it with the first \
             Claude Code render.",
            config.statusline_json_path.display()
        );
    }

    let mut state = load_state(&config.state_path)?;
    let mut last_config_mtime = file_mtime(&config_path);

    loop {
        // Hot-reload: if the config file changed (via --set or by hand),
        // reload it on the next cycle without restarting the service.
        if let Some(mtime) = file_mtime(&config_path) {
            if Some(mtime) != last_config_mtime {
                last_config_mtime = Some(mtime);
                match load_config_from(&config_path) {
                    Ok(new_config) => {
                        config = new_config;
                        clamp_poll(&mut config);
                        info!(
                            "Config hot-reloaded from {} (poll={}s, delegation={})",
                            config_path.display(),
                            config.poll_interval_secs,
                            config.delegation
                        );
                    }
                    Err(e) => warn!("Could not reload config: {:#}", e),
                }
            }
        }

        match run_once(&config, &mut state, dry_run).await {
            Ok(Action::Continue) => {
                sleep(Duration::from_secs(config.poll_interval_secs)).await;
            }
            Ok(Action::SleepSeconds(secs)) => {
                // Always a 2s floor on any loop.
                sleep(Duration::from_secs(secs.max(2))).await;
            }
            Err(e) => {
                warn!("Error in cycle: {:#}", e);
                sleep(Duration::from_secs(config.poll_interval_secs)).await;
            }
        }
    }
}

enum Action {
    Continue,
    SleepSeconds(u64),
}

/// How long after the reset the resume keeps being retried for agents that
/// did not turn `working` (send failed, stale screen). Generous on purpose:
/// a daemon (re)started hours after the reset must still wake the fleet.
const RESUME_RETRY_WINDOW_SECS: i64 = 3 * 3600;

/// Wakes the fleet for the window recorded in `blocked_reset_at` once the
/// clock says reset + margin has passed — whatever the hook JSON shows.
/// Returns Some(action) when it acted this cycle.
async fn pending_resume(
    config: &Config,
    state: &mut GuardState,
    now: i64,
    dry_run: bool,
) -> Result<Option<Action>> {
    let Some(reset_at) = state.blocked_reset_at else {
        return Ok(None);
    };
    if now < reset_at + config.safety_margin_secs as i64 {
        return Ok(None);
    }
    if now - reset_at > RESUME_RETRY_WINDOW_SECS {
        warn!(
            "Window {} reset {}s ago and some agent never turned working: giving up on it",
            reset_at,
            now - reset_at
        );
        state.blocked_reset_at = None;
        state.last_hard_limit_reset_at = Some(reset_at);
        save_state(&config.state_path, state)?;
        return Ok(None);
    }
    if state.last_hard_limit_reset_at != Some(reset_at) {
        info!(
            "Window {} opened ({}s ago): resuming every agent that is still waiting",
            reset_at,
            now - reset_at
        );
        state.last_hard_limit_reset_at = Some(reset_at);
        save_state(&config.state_path, state)?;
    }
    match resume_targets(config, state, reset_at, dry_run).await {
        Ok(true) => {
            info!("Every agent is resumed for window {}", reset_at);
            state.blocked_reset_at = None;
            save_state(&config.state_path, state)?;
            Ok(Some(Action::Continue))
        }
        Ok(false) => {
            debug!("Resume still pending for some target of window {}", reset_at);
            Ok(Some(Action::SleepSeconds(config.poll_interval_secs.max(5))))
        }
        Err(e) => {
            warn!("Resume of window {} incomplete: {:#} — retrying next cycle", reset_at, e);
            Ok(Some(Action::SleepSeconds(config.poll_interval_secs.max(5))))
        }
    }
}

async fn run_once(
    config: &Config,
    state: &mut GuardState,
    dry_run: bool,
) -> Result<Action> {
    let pause = load_pause_state(&pause_path());
    if pause.global {
        debug!("Paused globally: skipping all actions");
        return Ok(Action::Continue);
    }

    let info = gather_rate_info(config)?;
    let now = Utc::now().timestamp();
    debug!(
        "used={:.1}% | resets_at={:?} | hard={}",
        info.used_pct, info.resets_at, info.hard_limit_hit
    );

    // 0. A window we were blocked on has opened: wake the fleet. This runs
    //    first and on the clock alone, because the hook JSON already shows
    //    the fresh window by now and would never say "blocked" again.
    if let Some(action) = pending_resume(config, state, now, dry_run).await? {
        return Ok(action);
    }

    // 1. Hard limit: sleep and resume (all targets at the reset).
    //    Important: NEVER sleep through the warning window. If the hard
    //    limit is detected early (used>=99.9 with the window still far),
    //    sleep only until the warning window starts and from there monitor
    //    closely: the delegation injection MUST happen before the limit
    //    screen (once it shows, Claude accepts no input).
    if info.hard_limit_hit {
        if let Some(reset_at) = info.resets_at {
            if state.last_hard_limit_reset_at == Some(reset_at) {
                // Window already resumed (targets still pending are retried
                // by pending_resume above while blocked_reset_at is set).
                //
                // If way past the reset and resets_at stayed the same,
                // the hook JSON is stale (or the window moved):
                // warn ONCE per window and keep polling.
                if now - reset_at > (config.safety_margin_secs as i64) + 60
                    && state.warned_stale_reset_at != Some(reset_at)
                {
                    state.warned_stale_reset_at = Some(reset_at);
                    save_state(&config.state_path, state)?;
                    warn!(
                        "Resume already sent for reset_at={} but the limit is still \
                         active: the hook resets_at is stale or the window moved. \
                         The next poll will pick up the new resets_at and retry.",
                        reset_at
                    );
                }
                return Ok(Action::Continue);
            }
            // NEW window in hard limit: remember it, the resume is driven
            // by the clock from here on (see pending_resume).
            if state.blocked_reset_at != Some(reset_at) {
                state.blocked_reset_at = Some(reset_at);
                save_state(&config.state_path, state)?;
                info!(
                    "Hard limit: fleet blocked until {} (+{}s margin); every agent \
                     still waiting will be resumed then",
                    reset_at, config.safety_margin_secs
                );
            }
            let remaining = reset_at - now;
            if now >= reset_at + (config.safety_margin_secs as i64) {
                // The reset already passed (+margin) but the JSON is stale:
                // wake up right now.
                if config.delegation
                    && state
                        .last_injected_reset_at
                        .map_or(true, |l| (l - reset_at).abs() > 120)
                {
                    warn!(
                        "Window {reset_at} reset before the delegation was injected \
                         (missed the warning window): the agent was already at the hard \
                         limit so the hand-over can't go in now. Check the daemon was \
                         running and the poll interval was small enough."
                    );
                }
                return Ok(pending_resume(config, state, now, dry_run)
                    .await?
                    .unwrap_or(Action::Continue));
            }
            // Still inside the blocked window: don't let the fleet freeze.
            // Every claude agent sitting on the "Usage limit reached"
            // screen gets /low-priority so it keeps working right now at
            // lower priority; the ones already in that mode are left alone.
            if config.low_priority {
                if let Err(e) = low_priority_pass(config, state, reset_at, &pause, dry_run).await {
                    warn!("Low-priority pass: {:#}", e);
                }
            }
            if remaining > (config.warning_lead_time_secs as i64) {
                if config.low_priority {
                    // Keep polling: agents that hit the screen later (or a
                    // new session) must get the command too, and a sent
                    // command is verified on the next pass.
                    return Ok(Action::SleepSeconds(config.poll_interval_secs.max(5)));
                }
                // Hard limit detected EARLY (the JSON showed 99.9 with the
                // window far away): sleep ONLY until the warning window
                // starts, to arrive awake and inject on time.
                let secs = (remaining - config.warning_lead_time_secs as i64).max(2) as u64;
                info!(
                    "Hard limit detected with the window still {}s away: \
                     sleeping {}s until the warning window (delegation must \
                     go in before the limit screen)",
                    remaining, secs
                );
                return Ok(Action::SleepSeconds(secs));
            }
            // Inside the warning window: inject NOW (the agents still
            // accept input) and keep monitoring closely.
            if config.delegation {
                if let Err(e) = inject_delegation_prompt(config, state, reset_at, dry_run).await {
                    warn!("Delegation prompt injection: {:#}", e);
                }
            }
            return Ok(Action::SleepSeconds(config.poll_interval_secs.max(5)));
        }
    }

    // 2. Warning "a few minutes before" (warning_lead_time_secs). With
    // threshold_pct: fires as soon as the usage % reaches it, with or
    // without resets_at in the JSON (the % always wins; before, it was
    // only a fallback when resets_at was missing, so it "never fired").
    let should_inject = if let Some(reset_at) = info.resets_at {
        let remaining = reset_at - now;
        remaining <= (config.warning_lead_time_secs as i64) || info.used_pct >= config.threshold_pct
    } else {
        info.used_pct >= config.threshold_pct
    };

    if should_inject {
        let reset_at = info
            .resets_at
            .unwrap_or(now + config.warning_lead_time_secs as i64);

        if !config.delegation {
            if let Some(last) = state.last_injected_reset_at {
                if (reset_at - last).abs() <= 120 {
                    debug!("Already marked this window, skip");
                    return Ok(Action::Continue);
                }
            }
            info!(
                "Delegation disabled (delegation=false): skipping the \
                 delegation prompt (auto-resume stays active)"
            );
            state.last_injected_reset_at = Some(reset_at);
            save_state(&config.state_path, state)?;
            return Ok(Action::Continue);
        }

        // If the whole window was already injected, don't re-enter it.
        if let Some(last) = state.last_injected_reset_at {
            if (reset_at - last).abs() <= 120 {
                debug!("Already injected for this window, skip");
                return Ok(Action::Continue);
            }
        }

        let why = if let Some(r) = info.resets_at {
            let remaining = r - now;
            if remaining > 0 && remaining <= (config.warning_lead_time_secs as i64) {
                format!("{remaining}s left")
            } else {
                format!("{:.0}% used", info.used_pct)
            }
        } else {
            format!("{:.0}% used, no resets_at", info.used_pct)
        };
        info!(
            "Delegation trigger: {why} — injecting urgent delegation prompt",
        );

        if let Err(e) = inject_delegation_prompt(config, state, reset_at, dry_run).await {
            warn!("Delegation prompt injection: {:#}", e);
        }
    }

    // Danger zone: with used >= threshold (default 90%) it monitors every
    // 5s instead of every poll_interval_secs, so the warning window is
    // never missed.
    if info.used_pct >= config.threshold_pct {
        return Ok(Action::SleepSeconds(5));
    }

    Ok(Action::Continue)
}

/// Sends the delegation prompt to the targets that haven't received it
/// yet for this window (dedup per target, multi-agent). Only marks the
/// window as injected in `last_injected_reset_at` when ALL of them got
/// it: the ones that failed are retried on the next poll without
/// spamming the ones that already have it.
async fn inject_delegation_prompt(
    config: &Config,
    state: &mut GuardState,
    reset_at: i64,
    dry_run: bool,
) -> Result<()> {
    // Budget enforcement: refuse to open/delegate if max_devs is reached.
    if let Err(e) = check_max_devs(config, "delegation").await {
        warn!("Delegation blocked by budget: {:#}", e);
        return Ok(()); // skip this cycle, retry on next poll
    }

    let pause = load_pause_state(&pause_path());
    let all_targets = resolve_wake_targets(config).await?;
    let targets: Vec<Target> = all_targets
        .into_iter()
        .filter(|t| !t.is_paused(&pause))
        .collect();
    let mut all_injected = true;

    for t in &targets {
        if state
            .injected_targets
            .get(&t.key())
            .is_some_and(|&r| (reset_at - r).abs() <= 120)
        {
            debug!("'{}' already injected for window {}", t, reset_at);
            continue;
        }

        info!("Injecting urgent delegation prompt into '{}' (window {})", t, reset_at);

        let ok = if dry_run {
            info!("[rehearsal] would inject the delegation prompt into '{}'", t);
            true
        } else {
            match send_to_herdr(
                config,
                t,
                &config.delegation_prompt,
                Some(&[]),
                Some(600_000),
            )
            .await
            {
                Ok(v) => {
                    // Don't trust a blind success: if the agent was at the
                    // limit screen (or otherwise couldn't take the prompt),
                    // herdr reports it as stalled. Treat that as NOT
                    // delivered so the next poll retries, instead of
                    // silently recording the window as delegated.
                    if delegation_stalled(&v) {
                        warn!(
                            "'{}' did not accept the delegation prompt (stalled/limit screen) — will retry",
                            t
                        );
                        false
                    } else {
                        info!("Delegation prompt accepted by '{}'", t);
                        true
                    }
                }
                Err(e) => {
                    // A timeout doesn't mean it wasn't delivered (the
                    // agent stays busy delegating the work and can take
                    // longer than the timeout to settle to idle/done).
                    if format!("{e:#}").contains("timeout") {
                        warn!(
                            "'{}' didn't settle in time after the delegation prompt; \
                             assuming it was delivered",
                            t
                        );
                        true
                    } else {
                        warn!("Delegation to '{}' failed: {:#} — will retry", t, e);
                        false
                    }
                }
            }
        };

        if ok {
            state.injected_targets.insert(t.key(), reset_at);
        } else {
            all_injected = false;
        }
    }

    if all_injected {
        state.last_injected_reset_at = Some(reset_at);
    }
    save_state(&config.state_path, state)?;
    Ok(())
}

// ─────────────────────────────────────────────
// JSON reading (Statusline Hook) — 100% independent of the pane manager
// ─────────────────────────────────────────────
fn gather_rate_info(config: &Config) -> Result<RateInfo> {
    let path = resolve_path(&config.statusline_json_path);
    if !path.exists() {
        debug!("statusline JSON does not exist yet: {:?}", path);
        return Ok(RateInfo::default());
    }
    let data = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let v: Value = serde_json::from_str(&data).context("parsing statusline JSON")?;

    let used = v
        .pointer("/rate_limits/five_hour/used_percentage")
        .and_then(|x| x.as_f64())
        .unwrap_or(0.0);
    let resets_raw = v
        .pointer("/rate_limits/five_hour/resets_at")
        .and_then(|x| x.as_i64())
        .or_else(|| {
            v.pointer("/rate_limits/five_hour/reset_at")
                .and_then(|x| x.as_i64())
        });

    // Claude sometimes sends ms instead of s. If forced_resets_at is set
    // in the config, it always wins (user-forced window).
    let resets_at = config.forced_resets_at.or_else(|| {
        resets_raw.map(|ts| {
            if ts > 1_000_000_000_000 {
                ts / 1000
            } else {
                ts
            }
        })
    });
    let now = Utc::now().timestamp();
    let hard_limit_hit = used >= 99.9 || resets_at.is_some_and(|r| now >= r);

    Ok(RateInfo {
        used_pct: used,
        resets_at,
        hard_limit_hit,
    })
}

/// True when a `herdr agent prompt` response means the prompt was NOT
/// accepted (stale limit screen, non-interactive agent), so clari should
/// retry instead of marking the window as delegated. Handles the shapes
/// herdr returns across versions (a stall boolean and/or a status string
/// containing "stall").
fn delegation_stalled(v: &Value) -> bool {
    const BOOL_KEYS: &[&str] = &[
        "/result/agent_prompt_stalled",
        "/result/stalled",
        "/agent_prompt_stalled",
        "/stalled",
    ];
    if BOOL_KEYS
        .iter()
        .any(|k| v.pointer(k).and_then(|x| x.as_bool()).unwrap_or(false))
    {
        return true;
    }
    const STATUS_KEYS: &[&str] = &[
        "/result/agent/agent_status",
        "/result/agent/status",
        "/result/status",
        "/status",
    ];
    STATUS_KEYS
        .iter()
        .any(|k| {
            v.pointer(k)
                .and_then(|x| x.as_str())
                .map(|s| s.to_ascii_lowercase().contains("stall"))
                .unwrap_or(false)
        })
}

// ─────────────────────────────────────────────
// herdr: discovery and sending (the whole agent interaction layer)
// ─────────────────────────────────────────────

/// The actual resume text sent to an agent: the configured `resume_message`
/// (default "continue"), plus — when DELEGATION is on — an extra instruction
/// telling the agent to notify the team lead that it is back (the work was
/// handed over while it was blocked, so the lead should know it resumed).
fn effective_resume_message(config: &Config) -> String {
    if !config.delegation {
        return config.resume_message.clone();
    }
    let base = config.resume_message.trim().trim_end_matches('.');
    format!("{base}. {RESUME_DELEGATION_NOTICE}")
}

#[derive(Debug, Clone)]
struct HerdrAgentEntry {
    target: String, // name if present, else pane_id
    kind: Option<String>,
}

/// A herdr session reachable from this daemon (`herdr session list`).
#[derive(Debug, Clone, PartialEq, Eq)]
struct HerdrSession {
    /// None = herdr's default session (no HERDR_SESSION needed).
    name: Option<String>,
    /// The session socket, when known: the unambiguous way to address it.
    socket: Option<PathBuf>,
}

impl HerdrSession {
    fn label(&self) -> &str {
        self.name.as_deref().unwrap_or("default")
    }
}

/// An agent to wake: which pane/name, inside which herdr session. Agents
/// live in sessions, so the same pane id can exist in two sessions; the
/// key ("name@session") is what state files and pause lists refer to.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Target {
    id: String,
    session: Option<String>,
    socket: Option<PathBuf>,
}

impl Target {
    fn key(&self) -> String {
        match &self.session {
            Some(s) => format!("{}@{}", self.id, s),
            None => self.id.clone(),
        }
    }

    /// A pause entry may name the agent bare ("w1:p1A") or fully
    /// qualified ("w1:p1A@super"); both pause this target.
    fn is_paused(&self, pause: &PauseState) -> bool {
        pause.agents.iter().any(|p| p == &self.id || *p == self.key())
    }
}

impl std::fmt::Display for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.key())
    }
}

/// Running sessions from `herdr session list --json`; the default session
/// is addressed by socket only (no HERDR_SESSION).
fn parse_session_list(v: &Value) -> Vec<HerdrSession> {
    v.pointer("/sessions")
        .or_else(|| v.pointer("/result/sessions"))
        .and_then(|x| x.as_array())
        .map(|arr| {
            arr.iter()
                .filter(|s| s.get("running").and_then(|r| r.as_bool()).unwrap_or(false))
                .filter_map(|s| {
                    let name = s.get("name").and_then(|x| x.as_str())?.to_string();
                    let is_default = s.get("default").and_then(|x| x.as_bool()).unwrap_or(false);
                    let socket = s
                        .get("socket_path")
                        .and_then(|x| x.as_str())
                        .map(PathBuf::from);
                    Some(HerdrSession {
                        name: if is_default { None } else { Some(name) },
                        socket,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The sessions the guard works on: the pinned `herdr_session` if set,
/// otherwise EVERY running herdr session. If herdr can't list sessions
/// (older herdr, monolithic mode) the default session is used.
async fn running_sessions(config: &Config) -> Vec<HerdrSession> {
    if let Some(name) = &config.herdr_session {
        return vec![HerdrSession {
            name: Some(name.clone()),
            socket: None,
        }];
    }
    let fallback = vec![HerdrSession {
        name: None,
        socket: None,
    }];
    let output = herdr_command_in(config, None, None)
        .args(["session", "list", "--json"])
        .output()
        .await;
    match output {
        Ok(o) if o.status.success() => {
            let v: Value = serde_json::from_slice(&o.stdout).unwrap_or(Value::Null);
            let sessions = parse_session_list(&v);
            if sessions.is_empty() {
                debug!("herdr lists no running session: using the default one");
                fallback
            } else {
                sessions
            }
        }
        Ok(o) => {
            debug!(
                "'herdr session list' failed ({}): using the default session",
                herdr_error_message(&o.stderr)
            );
            fallback
        }
        Err(e) => {
            debug!("'herdr session list' could not run ({e}): using the default session");
            fallback
        }
    }
}

/// Resolves the herdr binary to invoke: if `herdr_bin` is a path
/// (absolute or with /), it is used as-is. If it is just a name, it is
/// looked up first in ~/.local/bin (user systemd services run with
/// a minimal PATH and can't see $HOME bins) and then falls back to the process
/// PATH. Returns path-or-name, always usable by Command::new.
fn herdr_bin_resolved(config: &Config) -> PathBuf {
    let bin = Path::new(&config.herdr_bin);
    if bin.components().count() > 1 {
        return bin.to_path_buf();
    }
    if let Some(home) = dirs::home_dir() {
        let candidate = home.join(".local/bin").join(&config.herdr_bin);
        if candidate.exists() {
            return candidate;
        }
    }
    bin.to_path_buf()
}

/// herdr invocation addressed to one session. The daemon's own
/// environment (it may have been started from inside a herdr pane) must
/// never leak HERDR_SESSION/HERDR_SOCKET_PATH into a call meant for
/// another session, so both are cleared before the explicit ones are set.
fn herdr_command_in(config: &Config, session: Option<&str>, socket: Option<&Path>) -> Command {
    let mut cmd = Command::new(herdr_bin_resolved(config));
    if session.is_some() || socket.is_some() {
        cmd.env_remove("HERDR_SESSION");
        cmd.env_remove("HERDR_SOCKET_PATH");
    }
    if let Some(s) = session {
        cmd.env("HERDR_SESSION", s);
    }
    if let Some(p) = socket {
        cmd.env("HERDR_SOCKET_PATH", p);
    }
    cmd
}

fn herdr_command_for(config: &Config, target: &Target) -> Command {
    herdr_command_in(config, target.session.as_deref(), target.socket.as_deref())
}

fn herdr_error_message(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return "(no error output)".to_string();
    }
    if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
        let err = v.get("error").cloned().unwrap_or(v);
        let code = err
            .get("code")
            .and_then(|x| x.as_str())
            .unwrap_or("unknown");
        let message = err
            .get("message")
            .and_then(|x| x.as_str())
            .unwrap_or(trimmed);
        return format!("[{code}] {message}");
    }
    trimmed.to_string()
}

fn parse_agent_list(v: &Value) -> Vec<HerdrAgentEntry> {
    let arr = v
        .pointer("/result/agents")
        .or_else(|| v.pointer("/agents"))
        .and_then(|x| x.as_array())
        .cloned()
        .or_else(|| v.pointer("/result").and_then(|x| x.as_array()).cloned())
        .or_else(|| v.as_array().cloned())
        .unwrap_or_default();

    arr.iter()
        .filter_map(|item| {
            let pane_id = item
                .get("pane_id")
                .and_then(|x| x.as_str())
                .or_else(|| item.get("id").and_then(|x| x.as_str()))?
                .to_string();
            let name = item
                .get("name")
                .and_then(|x| x.as_str())
                .map(str::to_string);
            let kind = item
                .get("kind")
                .and_then(|x| x.as_str())
                .or_else(|| item.get("agent").and_then(|x| x.as_str()))
                .or_else(|| item.get("agent_kind").and_then(|x| x.as_str()))
                .map(str::to_string);
            let target = name.unwrap_or(pane_id);
            Some(HerdrAgentEntry { target, kind })
        })
        .collect()
}

/// Resolves the targets for STATUS display: if `herdr_agent_target` is set
/// in the config, only that one is shown; otherwise every alive
/// kind=`herdr_agent_kind` agent from `herdr agent list` is a target.
/// Zero matches is an explicit error, not a guess.
async fn resolve_targets(config: &Config) -> Result<Vec<Target>> {
    if let Some(explicit) = &config.herdr_agent_target {
        return Ok(vec![pinned_target(config, explicit).await]);
    }
    auto_detect_targets(config).await
}

/// The pinned agent as a Target: looked up across the running sessions
/// (so "w1:p1A" finds its session); if it isn't listed, it is addressed
/// in the pinned/default session as-is.
async fn pinned_target(config: &Config, pin: &str) -> Target {
    if let Ok(all) = list_agents_everywhere(config).await {
        if let Some((s, _)) = all.iter().find(|(_, a)| a.target == pin) {
            return Target {
                id: pin.to_string(),
                session: s.name.clone(),
                socket: s.socket.clone(),
            };
        }
    }
    Target {
        id: pin.to_string(),
        session: config.herdr_session.clone(),
        socket: None,
    }
}

/// `herdr agent list` in EVERY running session (or the pinned one).
/// A session that fails is reported and skipped; it is an error only when
/// no session answered at all.
async fn list_agents_everywhere(config: &Config) -> Result<Vec<(HerdrSession, HerdrAgentEntry)>> {
    let sessions = running_sessions(config).await;
    let mut all = Vec::new();
    let mut errors = Vec::new();
    for s in &sessions {
        let output = herdr_command_in(config, s.name.as_deref(), s.socket.as_deref())
            .args(["agent", "list"])
            .output()
            .await;
        match output {
            Ok(o) if o.status.success() => match serde_json::from_slice::<Value>(&o.stdout) {
                Ok(v) => {
                    for a in parse_agent_list(&v) {
                        all.push((s.clone(), a));
                    }
                }
                Err(e) => errors.push(format!("{}: parsing 'herdr agent list' JSON: {e}", s.label())),
            },
            Ok(o) => errors.push(format!("{}: {}", s.label(), herdr_error_message(&o.stderr))),
            Err(e) => errors.push(format!(
                "{}: running 'herdr agent list' (is herdr in PATH and the server up?): {e}",
                s.label()
            )),
        }
    }
    if !errors.is_empty() && errors.len() == sessions.len() {
        bail!(
            "'herdr agent list' failed in every session: {}",
            errors.join(" | ")
        );
    }
    for e in errors {
        warn!("'herdr agent list' failed in session {}", e);
    }
    Ok(all)
}

fn kind_targets(config: &Config, all: &[(HerdrSession, HerdrAgentEntry)]) -> Vec<Target> {
    all.iter()
        .filter(|(_, a)| a.kind.as_deref() == Some(config.herdr_agent_kind.as_str()))
        .map(|(s, a)| Target {
            id: a.target.clone(),
            session: s.name.clone(),
            socket: s.socket.clone(),
        })
        .collect()
}

/// Autodetects every alive agent of `herdr_agent_kind` in every running
/// herdr session. NO pin here: this is the set that gets resumed/delegated.
async fn auto_detect_targets(config: &Config) -> Result<Vec<Target>> {
    let all = list_agents_everywhere(config).await?;
    let matches = kind_targets(config, &all);

    if matches.is_empty() {
        bail!(
            "No kind='{}' agent found alive in herdr. Seen: {:?}. \
             Set clari's herdr_agent_target if the detected kind is off.",
            config.herdr_agent_kind,
            all.iter()
                .map(|(s, a)| format!(
                    "{}@{}({})",
                    a.target,
                    s.label(),
                    a.kind.as_deref().unwrap_or("?")
                ))
                .collect::<Vec<_>>()
        );
    }
    // Note: ALL the matches (multi-target, multi-session), not just one.
    Ok(matches)
}

/// Lists every alive `herdr_agent_kind` agent across sessions. Unlike
/// `auto_detect_targets`, an empty match is a valid result (returns []
/// instead of bailing), which is what `--list` wants.
async fn list_kind_agents(config: &Config) -> Result<Vec<Target>> {
    let all = list_agents_everywhere(config).await?;
    Ok(kind_targets(config, &all))
}

/// Checks whether the `max_devs` budget limit allows waking more agents.
/// Returns Ok(()) when under the limit or when the limit is unset.
/// Returns Err with a clear log line when at or over the limit.
async fn check_max_devs(config: &Config, action: &str) -> Result<()> {
    let limits = load_limites();
    let max = match limits.max_devs {
        Some(m) => m,
        None => return Ok(()), // unlimited
    };

    let alive = list_kind_agents(config).await.unwrap_or_default();
    let count = alive.len() as u32;

    if count >= max {
        warn!(
            "BUDGET: max_devs={} — {} denied, {} agent(s) alive ({:?}). \
             Set a higher limit with `clari limites --max-devs N` or free a slot.",
            max, action, count, alive
        );
        bail!(
            "max_devs limit reached ({}/{}): {} denied",
            count,
            max,
            action
        );
    }
    debug!(
        "max_devs check OK: {}/{} alive — {} allowed",
        count, max, action
    );
    Ok(())
}

/// Stub for future USD/day budget tracking.
///
/// Will check `LimitesConfig::usd_dia` against the current day's spend
/// (sourced from the `medir-modelo` tracking pipeline). For now it always
/// returns `Ok(false)` — no budget is considered breached.
///
/// Expected integration:
///   1. `medir-modelo` writes a daily spend JSON (e.g. `~/.local/state/clari/spend_today.json`).
///   2. This function reads it, sums the USD, and compares against `usd_dia`.
///   3. Returns `true` when the daily spend exceeds the limit.
fn is_over_budget() -> bool {
    // TODO: read daily spend from medir-modelo and compare with load_limites().usd_dia
    false
}

/// Heuristic for `--blocked`: an agent "can't work right now" when its
/// status is unknown, stalled, at a limit, blocking or erroring — i.e. not
/// one of the healthy statuses herdr reports for idle/working agents.
fn agent_is_blocked(status: &str) -> bool {
    const HEALTHY: &[&str] = &[
        "working", "busy", "idle", "free", "waiting", "queued", "pending", "done", "ready",
    ];
    let s = status.to_ascii_lowercase();
    !HEALTHY.contains(&s.as_str())
        || s.contains("stall")
        || s.contains("limit")
        || s.contains("block")
        || s.contains("error")
}

/// The set that receives resumes and delegation prompts.
/// With `resume_all` (default ON) it is EVERY alive kind=claude agent —
/// the pin must not leave the other panels blocked. With `resume_all=false`
/// only the pinned `herdr_agent_target` is used (or the first detected).
/// If listing fails and a pin exists, fall back to the pin so the pinned
/// agent is never skipped.
async fn resolve_wake_targets(config: &Config) -> Result<Vec<Target>> {
    if !config.resume_all {
        if let Some(explicit) = &config.herdr_agent_target {
            return Ok(vec![pinned_target(config, explicit).await]);
        }
    }
    match auto_detect_targets(config).await {
        Ok(all) => Ok(all),
        Err(e) => {
            if let Some(pin) = &config.herdr_agent_target {
                warn!(
                    "herdr agent list failed ({:#}); falling back to the pinned target '{}'",
                    e, pin
                );
                Ok(vec![Target {
                    id: pin.clone(),
                    session: config.herdr_session.clone(),
                    socket: None,
                }])
            } else {
                Err(e)
            }
        }
    }
}

// ─────────────────────────────────────────────
// Low priority: keep blocked agents working during the block
// ─────────────────────────────────────────────

/// What a claude pane's visible screen says about the limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LimitScreen {
    /// "Lower priority until ..." — already working at lower priority.
    LowPriorityActive,
    /// Lower-priority mode ended / allowance used / not offered: nothing
    /// clari can do, the agent waits for the reset.
    LowPriorityUnavailable,
    /// "Usage limit reached" and NOT in lower-priority mode: send it.
    LimitReached,
    /// "Your usage limit has reset · press enter to continue".
    LimitReset,
    /// Not on the limit screen (working, idle, whatever).
    Other,
}

/// Classifies the visible screen of a claude pane. The bottom lines win:
/// Claude Code pins the current limit status right above the input box,
/// while older banners scroll up, so the scan runs bottom-up and stops at
/// the first line that speaks about the limit.
fn classify_limit_screen(screen: &str) -> LimitScreen {
    const ACTIVE: &[&str] = &[
        "Lower priority until",
        "Working at lower priority",
        "Continuing now at lower priority",
        "Lower-priority mode is back on",
    ];
    const UNAVAILABLE: &[&str] = &[
        "lower-priority allowance",
        "Lower-priority mode ended",
        "lower-priority mode ended",
        "lower-priority mode stopped",
        "Lower-priority mode is no longer available",
        "Lower-priority mode isn't available",
        "Lower-priority mode is taking a break",
    ];
    for raw in screen.lines().rev() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if ACTIVE.iter().any(|m| line.contains(m)) {
            return LimitScreen::LowPriorityActive;
        }
        if UNAVAILABLE.iter().any(|m| line.contains(m)) {
            return LimitScreen::LowPriorityUnavailable;
        }
        if line.contains("Your usage limit has reset") {
            return LimitScreen::LimitReset;
        }
        // The pinned banner ("Usage limit reached") or the API error a
        // turn/subagent dies with ("You've hit your session limit · resets
        // 2:20am"): both mean the agent can't work until the reset.
        if line.contains("Usage limit reached")
            || line.contains("You've hit your session limit")
            || line.contains("You've hit your usage limit")
        {
            return LimitScreen::LimitReached;
        }
    }
    LimitScreen::Other
}

/// Minimum spacing between two `/low-priority` sends to the same agent:
/// the command is a toggle, so a second send before the first one has
/// been observed on screen would turn the mode back OFF.
const LOW_PRIORITY_RETRY_SECS: i64 = 60;

/// The visible screen of an agent's pane as plain text.
async fn read_agent_screen(config: &Config, target: &Target) -> Result<String> {
    let output = herdr_command_for(config, target)
        .args([
            "agent", "read", &target.id, "--source", "visible", "--format", "text",
        ])
        .output()
        .await
        .with_context(|| format!("running 'herdr agent read {}'", target))?;
    if !output.status.success() {
        bail!(
            "'herdr agent read {}' failed: {}",
            target,
            herdr_error_message(&output.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// One pass over every claude agent, in every session, while the hard
/// limit is active: the ones stuck on "Usage limit reached" get the
/// `/low-priority` command (once, verified on the next pass by reading the
/// screen again); the ones already at lower priority are recorded and left
/// alone; the ones for which the mode is unavailable are recorded so they
/// are not poked again this window.
async fn low_priority_pass(
    config: &Config,
    state: &mut GuardState,
    reset_at: i64,
    pause: &PauseState,
    dry_run: bool,
) -> Result<()> {
    let targets: Vec<Target> = resolve_wake_targets(config)
        .await?
        .into_iter()
        .filter(|t| !t.is_paused(pause))
        .collect();
    let now = Utc::now().timestamp();
    let cmd = config.low_priority_command.trim();
    let mut changed = false;

    for t in &targets {
        let key = t.key();
        if state.low_priority_targets.get(&key) == Some(&reset_at) {
            continue;
        }
        let screen = match read_agent_screen(config, t).await {
            Ok(s) => s,
            Err(e) => {
                debug!("Could not read the screen of '{}': {:#}", t, e);
                continue;
            }
        };
        match classify_limit_screen(&screen) {
            LimitScreen::LowPriorityActive => {
                info!(
                    "'{}' is working at lower priority (window {}) — leaving it alone",
                    t, reset_at
                );
                state.low_priority_targets.insert(key, reset_at);
                changed = true;
            }
            LimitScreen::LowPriorityUnavailable => {
                warn!(
                    "'{}': lower-priority mode is not available (allowance used up or \
                     mode ended) — it waits for the reset at {}",
                    t, reset_at
                );
                state.low_priority_targets.insert(key, reset_at);
                changed = true;
            }
            LimitScreen::LimitReset => {
                debug!("'{}' already shows the limit reset — the resume path handles it", t);
            }
            LimitScreen::Other => {
                debug!("'{}' is not on the limit screen — nothing to do", t);
            }
            LimitScreen::LimitReached => {
                if let Some(last) = state.low_priority_attempts.get(&key) {
                    if now - last < LOW_PRIORITY_RETRY_SECS {
                        debug!(
                            "'{}' got {} {}s ago — waiting for the screen to reflect it",
                            t,
                            cmd,
                            now - last
                        );
                        continue;
                    }
                }
                info!(
                    "'{}' is stuck on 'Usage limit reached' → sending {} so it keeps \
                     working at lower priority",
                    t, cmd
                );
                state.low_priority_attempts.insert(key.clone(), now);
                changed = true;
                if dry_run {
                    info!("[rehearsal] would send {} to '{}'", cmd, t);
                    continue;
                }
                // Fire and forget (no --wait: herdr's --timeout needs it and the
                // toggle is verified by re-reading the screen next pass anyway).
                match send_to_herdr(config, t, cmd, None, None).await {
                    Ok(_) => info!("'{}': {} sent — verifying on the next pass", t, cmd),
                    Err(e) => warn!("'{}': sending {} failed: {:#} — will retry", t, cmd, e),
                }
            }
        }
    }

    if changed {
        save_state(&config.state_path, state)?;
    }
    Ok(())
}

/// Wakes the herdr targets when the window opens.
/// - Agents already `working` are not touched (marked as ok).
/// - The ones already resumed for this `reset_at` are skipped (dedup).
/// - The resume is verified with `--wait --until working`: if herdr does
///   NOT observe the agent turning working (input lost on a stale screen),
///   the target stays unmarked and the next cycle retries until it really
///   wakes up.
async fn resume_targets(
    config: &Config,
    state: &mut GuardState,
    reset_at: i64,
    dry_run: bool,
) -> Result<bool> {
    // Budget enforcement: refuse to wake agents if max_devs is reached.
    if let Err(e) = check_max_devs(config, "resume").await {
        warn!("Resume blocked by budget: {:#}", e);
        return Ok(false); // skip this cycle, retry on next poll
    }

    let pause = load_pause_state(&pause_path());
    let all_targets = resolve_wake_targets(config).await?;
    let targets: Vec<Target> = all_targets
        .into_iter()
        .filter(|t| !t.is_paused(&pause))
        .collect();
    info!(
        "Window {} opened: checking {} target(s)",
        reset_at,
        targets.len()
    );

    let msg = effective_resume_message(config);

    for t in &targets {
        if state.woken_targets.get(&t.key()) == Some(&reset_at) {
            debug!("'{}' already has resume for this window, skip", t);
            continue;
        }

        let ok = if dry_run {
            info!("[rehearsal] would send resume to '{}': {}", t, msg);
            true
        } else {
            wake_agent(config, t, &msg).await?
        };

        if ok {
            state.woken_targets.insert(t.key(), reset_at);
        }
    }

    save_state(&config.state_path, state)?;
    Ok(targets
        .iter()
        .all(|t| state.woken_targets.get(&t.key()) == Some(&reset_at)))
}

/// Sends the resume to an agent and VERIFIES it actually started:
/// `herdr agent prompt --wait --until working` returns
/// `agent_prompt_stalled` if the input wasn't accepted (stale limit
/// screen, non-interactive agent), and we treat that as pending.
async fn wake_agent(config: &Config, target: &Target, msg: &str) -> Result<bool> {
    match get_agent_status(config, target).await {
        Ok(s) if s == "working" => {
            info!("'{}' is already working — not touching", target);
            return Ok(true);
        }
        Ok(s) => debug!("'{}' is '{}' → sending resume", target, s),
        Err(e) => debug!("Could not verify '{}' ({:#}) → sending anyway", target, e),
    }

    info!("Sending resume to '{}': {}", target, msg);
    match send_to_herdr(config, target, msg, Some(&["working"]), Some(60_000)).await {
        Ok(v) => {
            let status = v
                .pointer("/result/agent/agent_status")
                .and_then(|x| x.as_str())
                .unwrap_or("");
            if status == "working" {
                info!("'{}' accepted the resume (now working)", target);
                Ok(true)
            } else {
                warn!(
                    "'{}' did not turn working after the resume (status: '{}') — will retry",
                    target, status
                );
                Ok(false)
            }
        }
        Err(e) => {
            warn!("Resume of '{}' failed: {:#} — will retry on the next cycle", target, e);
            Ok(false)
        }
    }
}

/// Sends text + Enter atomically via `herdr agent prompt`.
/// No shell, no buffers, no C-c beforehand: `agent prompt` works
/// even while the agent is working.
async fn send_to_herdr(
    config: &Config,
    target: &Target,
    text: &str,
    wait_until: Option<&[&str]>,
    timeout_ms: Option<u64>,
) -> Result<Value> {
    let mut cmd = herdr_command_for(config, target);
    cmd.args(["agent", "prompt", &target.id, text]);
    if let Some(states) = wait_until {
        cmd.arg("--wait");
        for s in states {
            cmd.args(["--until", *s]);
        }
    }
    if let Some(ms) = timeout_ms {
        cmd.args(["--timeout", &ms.to_string()]);
    }

    let output = cmd
        .output()
        .await
        .with_context(|| format!("running 'herdr agent prompt {target}'"))?;

    if !output.status.success() {
        bail!(
            "'herdr agent prompt' to '{}' failed: {}",
            target,
            herdr_error_message(&output.stderr)
        );
    }

    Ok(serde_json::from_slice(&output.stdout).unwrap_or(Value::Null))
}

async fn get_agent_status(config: &Config, target: &Target) -> Result<String> {
    let output = herdr_command_for(config, target)
        .args(["agent", "get", &target.id])
        .output()
        .await
        .context("running 'herdr agent get'")?;

    if !output.status.success() {
        bail!("{}", herdr_error_message(&output.stderr));
    }

    let v: Value = serde_json::from_slice(&output.stdout).unwrap_or(Value::Null);
    let status = v
        .pointer("/result/agent/agent_status")
        .or_else(|| v.pointer("/result/agent/status"))
        .or_else(|| v.pointer("/result/agent_status"))
        .or_else(|| v.pointer("/result/status"))
        .or_else(|| v.pointer("/status"))
        .or_else(|| v.pointer("/agent_status"))
        .and_then(|x| x.as_str())
        .map(str::to_string);

    match status {
        Some(s) => Ok(s),
        None => {
            let raw = serde_json::to_string(&v).unwrap_or_default();
            bail!(
                "could not extract the agent status; raw response: {}",
                raw.chars().take(300).collect::<String>()
            )
        }
    }
}

// ─────────────────────────────────────────────
// Helpers (config/state, no background changes)
// ─────────────────────────────────────────────
fn resolve_path(p: &Path) -> PathBuf {
    if let Some(s) = p.to_str() {
        if let Some(stripped) = s.strip_prefix("~/") {
            if let Some(home) = dirs::home_dir() {
                return home.join(stripped);
            }
        }
    }
    p.to_path_buf()
}

/// Handles the `clari limites` subcommand: display or update limits.
fn handle_limites_subcommand(
    max_devs: &Option<Option<u32>>,
    usd_dia: &Option<Option<f64>>,
    tokens_sesion: &Option<Option<u64>>,
) -> Result<()> {
    let editing = max_devs.is_some() || usd_dia.is_some() || tokens_sesion.is_some();
    let mut cfg = load_limites();

    if editing {
        if let Some(v) = max_devs {
            cfg.max_devs = *v;
        }
        if let Some(v) = usd_dia {
            cfg.usd_dia = *v;
        }
        if let Some(v) = tokens_sesion {
            cfg.tokens_sesion = v.unwrap_or(default_tokens_sesion());
        }
        save_limites(&cfg)?;
        println!("Limits updated at {}", limites_path().display());
    }

    // Always print the current state.
    println!("{:<15}: {}", "max_devs", fmt_opt_u32(cfg.max_devs));
    println!("{:<15}: {}", "usd_dia", fmt_opt_f64(cfg.usd_dia));
    println!("{:<15}: {}", "tokens_sesion", cfg.tokens_sesion);
    Ok(())
}

fn fmt_opt_u32(v: Option<u32>) -> String {
    match v {
        Some(n) => n.to_string(),
        None => "unlimited".to_string(),
    }
}

fn fmt_opt_f64(v: Option<f64>) -> String {
    match v {
        Some(n) => format!("{n:.2}"),
        None => "unlimited".to_string(),
    }
}

fn default_config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_default()
        .join("clari/config.toml")
}

fn load_config_from(path: &Path) -> Result<Config> {
    if path.exists() {
        let content = fs::read_to_string(path)?;
        Ok(toml::from_str(&content)?)
    } else {
        warn!("No config at {:?}, using defaults", path);
        Ok(Config::default())
    }
}

/// Hard floor for the polling interval: never under 5s, no matter what
/// the config says. Avoids hammering the CPU/socket.
fn clamp_poll(config: &mut Config) {
    const MIN_POLL_SECS: u64 = 5;
    if config.poll_interval_secs < MIN_POLL_SECS {
        warn!(
            "poll_interval_secs={} is too low, clamping to {}s",
            config.poll_interval_secs, MIN_POLL_SECS
        );
        config.poll_interval_secs = MIN_POLL_SECS;
    }
}

/// mtime of a file, or None if missing/failed.
fn file_mtime(path: &Path) -> Option<std::time::SystemTime> {
    fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Collects the CLI direct config flags into a list of typed
/// (key, value) pairs. A `None` value = remove the key (null).
fn collect_settings(cli: &Cli) -> Result<Vec<(String, Option<toml::Value>)>> {
    let mut s: Vec<(String, Option<toml::Value>)> = Vec::new();
    if cli.delegate.is_some() && cli.no_delegate {
        bail!("--delegate and --no-delegate are mutually exclusive");
    }
    if !cli.message.is_empty() && cli.delegate.is_none() {
        bail!("a delegation message requires -d/--delegate");
    }
    if let Some(inline) = &cli.delegate {
        s.push(("delegation".into(), Some(toml::Value::Boolean(true))));
        let trailing = if cli.message.is_empty() {
            None
        } else {
            Some(cli.message.join(" "))
        };
        let text = match (inline.as_ref(), trailing) {
            (Some(_), Some(_)) => bail!(
                "delegation message given twice: use either '-d=MESSAGE' or trailing text, not both"
            ),
            (Some(a), None) => Some(a.clone()),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        };
        if let Some(t) = text {
            s.push(("delegation_prompt".into(), Some(toml::Value::String(t))));
        }
    }
    if cli.no_delegate {
        s.push(("delegation".into(), Some(toml::Value::Boolean(false))));
    }
    if let Some(t) = &cli.target {
        // Bare `-t` (empty value) is the "list targets" query handled in
        // main(); it is not a config setting.
        if !t.is_empty() {
            if t == "null" {
                s.push(("herdr_agent_target".into(), None));
            } else {
                s.push((
                    "herdr_agent_target".into(),
                    Some(toml::Value::String(t.clone())),
                ));
            }
        }
    }
    if cli.all && cli.no_all {
        bail!("--all and --no-all are mutually exclusive");
    }
    if cli.all {
        s.push(("resume_all".into(), Some(toml::Value::Boolean(true))));
    }
    if cli.no_all {
        s.push(("resume_all".into(), Some(toml::Value::Boolean(false))));
    }
    if cli.low_priority && cli.no_low_priority {
        bail!("--low-priority and --no-low-priority are mutually exclusive");
    }
    if cli.low_priority {
        s.push(("low_priority".into(), Some(toml::Value::Boolean(true))));
    }
    if cli.no_low_priority {
        s.push(("low_priority".into(), Some(toml::Value::Boolean(false))));
    }
    if let Some(v) = cli.poll {
        s.push((
            "poll_interval_secs".into(),
            Some(toml::Value::Integer(v as i64)),
        ));
    }
    if let Some(v) = cli.margin {
        s.push((
            "safety_margin_secs".into(),
            Some(toml::Value::Integer(v as i64)),
        ));
    }
    if let Some(v) = cli.warning {
        s.push((
            "warning_lead_time_secs".into(),
            Some(toml::Value::Integer(v as i64)),
        ));
    }
    if let Some(v) = cli.threshold {
        s.push(("threshold_pct".into(), Some(toml::Value::Float(v))));
    }
    if let Some(v) = &cli.percent {
        if !v.is_empty() {
            let pct: f64 = v
                .parse()
                .context("--percent expects a number (or no value to print the usage %)")?;
            s.push(("threshold_pct".into(), Some(toml::Value::Float(pct))));
        }
    }
    if let Some(v) = &cli.forced_reset {
        if v == "null" {
            s.push(("forced_resets_at".into(), None));
        } else {
            let epoch: i64 = v
                .parse()
                .with_context(|| format!("invalid epoch for --forced-reset: '{v}'"))?;
            s.push(("forced_resets_at".into(), Some(toml::Value::Integer(epoch))));
        }
    }
    if let Some(v) = &cli.herdr {
        s.push(("herdr_bin".into(), Some(toml::Value::String(v.clone()))));
    }
    if let Some(v) = &cli.session {
        if v == "null" {
            s.push(("herdr_session".into(), None));
        } else {
            s.push(("herdr_session".into(), Some(toml::Value::String(v.clone()))));
        }
    }
    if let Some(v) = &cli.kind {
        s.push((
            "herdr_agent_kind".into(),
            Some(toml::Value::String(v.clone())),
        ));
    }
    if let Some(v) = &cli.resume_msg {
        s.push((
            "resume_message".into(),
            Some(toml::Value::String(v.clone())),
        ));
    }
    if cli.no_install_hook {
        s.push((
            "install_statusline_hook".into(),
            Some(toml::Value::Boolean(false)),
        ));
    }
    if let Some(p) = &cli.state_file {
        s.push((
            "state_path".into(),
            Some(toml::Value::String(p.display().to_string())),
        ));
    }
    if let Some(p) = &cli.statusline {
        s.push((
            "statusline_json_path".into(),
            Some(toml::Value::String(p.display().to_string())),
        ));
    }
    if let Some(v) = &cli.settings {
        if v == "null" {
            s.push(("claude_settings_path".into(), None));
        } else {
            s.push((
                "claude_settings_path".into(),
                Some(toml::Value::String(v.clone())),
            ));
        }
    }
    Ok(s)
}

fn toml_inline(v: &toml::Value) -> String {
    match v {
        toml::Value::Boolean(b) => b.to_string(),
        toml::Value::Integer(i) => i.to_string(),
        toml::Value::Float(f) => f.to_string(),
        toml::Value::String(s) => format!("'{s}'"),
        other => other.to_string(),
    }
}

/// Writes the settings into the config file (creating it if it doesn't
/// exist) with an atomic write. "null" (None) removes the key. The
/// daemon reloads the file by mtime (hot-reload).
fn apply_config_settings(path: &Path, entries: &[(String, Option<toml::Value>)]) -> Result<()> {
    let mut table: toml::Value = if path.exists() {
        let raw =
            fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&raw).with_context(|| format!("{} is not valid TOML", path.display()))?
    } else {
        toml::Value::Table(Default::default())
    };
    let root = table
        .as_table_mut()
        .context("config root is not a TOML table")?;

    for (key, value) in entries {
        match value {
            Some(v) => {
                root.insert(key.clone(), v.clone());
            }
            None => {
                root.remove(key);
            }
        }
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, toml::to_string_pretty(&table)?)
        .with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("renaming to {}", path.display()))?;

    for (k, v) in entries {
        match v {
            Some(value) => println!("  {k} = {}", toml_inline(value)),
            None => println!("  {k} = {}", painted("(removed)", RED)),
        }
    }
    for (k, v) in entries {
        if k == "delegation" {
            match v {
                Some(toml::Value::Boolean(true)) => {
                    println!(
                        "{}",
                        painted(
                            "Delegation is now ON (the star; auto-resume keeps working).",
                            GREEN
                        )
                    );
                }
                Some(toml::Value::Boolean(false)) => {
                    println!(
                        "{}",
                        painted("Delegation is now OFF (plain auto-resume).", RED)
                    );
                }
                _ => {}
            }
        }
    }
    let delegation_on = entries
        .iter()
        .any(|(k, v)| k == "delegation" && matches!(v, Some(toml::Value::Boolean(true))));
    if delegation_on {
        let cfg = load_config_from(path)?;
        println!();
        println!(
            "{}",
            painted("Delegation message that will be injected:", CYAN)
        );
        println!("{}", cfg.delegation_prompt);
    }

    println!("Config updated at {}", path.display());
    Ok(())
}

fn load_state(path: &Path) -> Result<GuardState> {
    let p = resolve_path(path);
    if p.exists() {
        Ok(serde_json::from_str(&fs::read_to_string(p)?).unwrap_or_default())
    } else {
        Ok(GuardState::default())
    }
}

fn save_state(path: &Path, state: &GuardState) -> Result<()> {
    let p = resolve_path(path);
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(p, serde_json::to_string_pretty(state)?)?;
    Ok(())
}

/// statusLine hook mode for Claude Code: reads the JSON payload Claude
/// Code injects on stdin on every render and persists it atomically
/// (write a tmp + rename, so the daemon never reads a truncated file)
/// into statusline_json_path. Replaces the external statusline_writer.sh:
/// in ~/.claude/settings.json -> "statusLine": {"type": "command",
/// "command": "clari --write-statusline"}.
/// Prints nothing to stdout: Claude Code might use that output as the
/// statusline content.
fn write_statusline(config: &Config) -> Result<()> {
    let mut raw = String::new();
    io::stdin()
        .read_to_string(&mut raw)
        .context("reading stdin from the hook")?;
    let payload = raw.trim();
    if payload.is_empty() {
        bail!("empty stdin: Claude Code sent no statusLine payload");
    }
    serde_json::from_str::<Value>(payload).context("statusLine payload is not valid JSON")?;

    let path = resolve_path(&config.statusline_json_path);
    let tmp = path.with_extension("tmp");
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&tmp, payload).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, &path).with_context(|| format!("renaming to {}", path.display()))?;
    Ok(())
}

// ─────────────────────────────────────────────
// Auto-install of the statusLine hook in the Claude Code settings.json
// ─────────────────────────────────────────────

/// The exact command the hook must have to be considered "clari's".
const STATUSLINE_HOOK_COMMAND: &str = "clari --write-statusline";

/// Path of the user Claude Code settings.json. Respects
/// CLAUDE_CONFIG_DIR if set; otherwise ~/.claude/settings.json.
fn claude_settings_path(config: &Config) -> PathBuf {
    if let Some(p) = &config.claude_settings_path {
        return resolve_path(p);
    }
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        return PathBuf::from(dir).join("settings.json");
    }
    dirs::home_dir()
        .unwrap_or_default()
        .join(".claude/settings.json")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HookState {
    /// The hook is ours and active.
    Present,
    /// The settings file does not exist.
    NoSettings,
    /// Exists but without statusLine.
    Missing,
    /// The statusLine points to another command: never overwritten.
    Other,
    /// The file exists but is not valid JSON.
    Invalid,
}

/// Read-only inspection of the hook state (fitness for --status).
fn hook_state(settings: &Path) -> HookState {
    if !settings.exists() {
        return HookState::NoSettings;
    }
    let raw = match fs::read_to_string(settings) {
        Ok(r) => r,
        Err(_) => return HookState::Invalid,
    };
    let Ok(v) = serde_json::from_str::<Value>(&raw) else {
        return HookState::Invalid;
    };
    match v.get("statusLine") {
        None => HookState::Missing,
        Some(sl) => {
            let cmd = sl.get("command").and_then(|x| x.as_str()).unwrap_or("");
            let typ = sl.get("type").and_then(|x| x.as_str()).unwrap_or("");
            if typ == "command" && cmd == STATUSLINE_HOOK_COMMAND {
                HookState::Present
            } else {
                HookState::Other
            }
        }
    }
}

/// Installs the statusLine hook in settings.json preserving the rest of
/// the content. Writes with tmp+rename and makes a backup first
/// (settings.json.clari.bak) the first time. Fails without touching
/// anything if the existing file is not valid JSON.
fn install_statusline_hook(settings: &Path) -> Result<bool> {
    let mut value = if settings.exists() {
        let raw = fs::read_to_string(settings)
            .with_context(|| format!("reading {}", settings.display()))?;
        serde_json::from_str::<Value>(&raw)
            .with_context(|| format!("{} is not valid JSON", settings.display()))?
    } else {
        Value::Object(Default::default())
    };

    let obj = value
        .as_object_mut()
        .context("settings.json is not a JSON object")?;
    match obj.get("statusLine") {
        None => {}
        Some(sl) => {
            let cmd = sl.get("command").and_then(|x| x.as_str()).unwrap_or("");
            if cmd == STATUSLINE_HOOK_COMMAND {
                return Ok(false);
            }
            bail!(
                "statusLine already points to another command ({}); leaving it alone",
                cmd
            );
        }
    }
    obj.insert(
        "statusLine".to_string(),
        json!({ "type": "command", "command": STATUSLINE_HOOK_COMMAND }),
    );

    if settings.exists() {
        let backup = PathBuf::from(format!("{}.clari.bak", settings.display()));
        if !backup.exists() {
            fs::copy(settings, &backup)
                .with_context(|| format!("backing up {}", backup.display()))?;
            info!("settings.json backup created at {}", backup.display());
        }
    }
    if let Some(parent) = settings.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = settings.with_extension("tmp");
    fs::write(&tmp, serde_json::to_string_pretty(&value)?)
        .with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, settings).with_context(|| format!("renaming to {}", settings.display()))?;
    Ok(true)
}

/// Entry point of the auto-install when the daemon starts.
/// With dry_run it only reports what it would do, touching nothing.
fn run_hook_install(config: &Config, dry_run: bool) -> Result<()> {
    if !config.install_statusline_hook {
        debug!("install_statusline_hook=false, skip auto-install");
        return Ok(());
    }
    let settings = claude_settings_path(config);
    match hook_state(&settings) {
        HookState::Present => debug!(
            "statusLine hook already configured at {} (nothing to do)",
            settings.display()
        ),
        HookState::Other => warn!(
            "Claude Code statusLine points to another command in {}; \
             leaving it alone. If you intend to use clari as the quota \
             guard, configure it by hand.",
            settings.display()
        ),
        HookState::Invalid => warn!(
            "{} is not valid JSON: not touching the file (fix it or set \
             install_statusline_hook=false).",
            settings.display()
        ),
        HookState::NoSettings | HookState::Missing => {
            if dry_run {
                info!(
                    "[rehearsal] would install the statusLine hook in {}",
                    settings.display()
                );
                return Ok(());
            }
            match install_statusline_hook(&settings) {
                Ok(true) => info!("StatusLine hook installed at {}", settings.display()),
                Ok(false) => debug!("StatusLine hook was already there ({})", settings.display()),
                Err(e) => warn!("Could not install the hook: {:#}", e),
            }
        }
    }
    Ok(())
}

// ─────────────────────────────────────────────
// User systemd service (--install / --uninstall)
// ─────────────────────────────────────────────

const SERVICE_UNIT_NAME: &str = "clari.service";

fn user_systemd_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME") {
        PathBuf::from(dir).join("systemd/user")
    } else {
        dirs::home_dir()
            .unwrap_or_default()
            .join(".config/systemd/user")
    }
}

/// Where the installer usually puts the binary: $XDG_BIN_HOME,
/// $XDG_DATA_HOME/bin, or ~/.local/bin. clari itself never writes there
/// (that is the installer's job); it is only referenced in messages.
fn executable_install_path() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_BIN_HOME") {
        return PathBuf::from(dir).join("clari");
    }
    if let Some(dir) = std::env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(dir).join("bin/clari");
    }
    dirs::home_dir()
        .unwrap_or_default()
        .join(".local/bin/clari")
}

fn run_systemctl(args: &[&str]) -> Result<()> {
    let out = std::process::Command::new("systemctl")
        .args(args)
        .output()
        .context("running systemctl (is systemd installed?)")?;
    if !out.status.success() {
        bail!(
            "'systemctl {}' failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Installs clari as a systemd user service with boot autorun
/// (WantedBy=default.target: starts at login; with `loginctl enable-linger`
/// also without an open session). The unit points at the running binary:
/// clari never copies itself anywhere — placing the binary where it
/// belongs is the installer's job. Idempotent: each run rewrites the unit,
/// reloads systemd and restarts the service. Linux/systemd only:
/// on macOS/others use the direct binary (daemon).
fn install_service() -> Result<()> {
    if !cfg!(target_os = "linux") {
        bail!(
            "--install only applies to Linux with systemd. On this \
             system run the daemon directly (e.g.: nohup clari --start &)"
        );
    }
    let exe = std::env::current_exe().context("resolving the binary path")?;
    let exe = fs::canonicalize(&exe).unwrap_or(exe);
    println!("Service will run: {}", exe.display());

    // A fresh start must not fail: make sure the config file exists.
    ensure_config(&default_config_path());

    let exe_str = exe.display().to_string();
    let exe_quoted = if exe_str.contains(' ') {
        format!("\"{}\"", exe_str.replace('"', r#"\""#))
    } else {
        exe_str.clone()
    };

    let unit_dir = user_systemd_dir();
    fs::create_dir_all(&unit_dir).with_context(|| format!("creating {}", unit_dir.display()))?;
    let unit_path = unit_dir.join(SERVICE_UNIT_NAME);

    let unit = format!(
        "# Generated by 'clari --install' — do not edit by hand; it is \
         # regenerated on every install. The binary is left where the \
         # installer put it (clari never copies itself).\n\
         [Unit]\n\
         Description=Clari - Claude Code quota guard (JSON hook + herdr)\n\
         After=default.target\n\
         \n\
         [Service]\n\
         # User services start with a minimal PATH; without this they would \n         # not find herdr (or other ~/.local/bin tools).\n\
         Environment=PATH=%h/.local/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin\n\
         ExecStart={exe_quoted} --daemon-loop\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n"
    );
    fs::write(&unit_path, unit).with_context(|| format!("writing {}", unit_path.display()))?;
    println!("Unit created: {}", unit_path.display());

    run_systemctl(&["--user", "daemon-reload"])?;
    run_systemctl(&["--user", "enable", SERVICE_UNIT_NAME])?;
    println!("Enabled for boot (systemctl --user enable)");

    // Login-less autorun (boot): linger is best-effort, not critical.
    if let Ok(env_user) = std::env::var("USER") {
        match std::process::Command::new("loginctl")
            .args(["enable-linger", &env_user])
            .output()
        {
            Ok(o) if o.status.success() => {
                println!("Login-less autorun enabled (loginctl enable-linger)");
            }
            Ok(o) => println!(
                "Warning: 'loginctl enable-linger' could not complete: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            ),
            Err(e) => println!(
                "Warning: loginctl not available ({e}); the service will start at login anyway"
            ),
        }
    }

    run_systemctl(&["--user", "restart", SERVICE_UNIT_NAME])?;
    println!(
        "Service restarted with the new version: {}",
        SERVICE_UNIT_NAME
    );

    println!();
    println!("Status     : systemctl --user status clari.service");
    println!("Logs       : journalctl --user -u clari.service -f");
    println!("Uninstall : clari --uninstall");
    Ok(())
}

/// Stops, disables and removes the service unit. Idempotent:
/// if it was not installed, reports and exits without error.
/// Only the service is removed: the statusLine hook, the config and the
/// binary are kept (the daemon re-creates the hook/config if needed).
fn uninstall_service() -> Result<()> {
    let unit_path = user_systemd_dir().join(SERVICE_UNIT_NAME);

    for step in [
        vec!["--user", "stop", SERVICE_UNIT_NAME],
        vec!["--user", "disable", SERVICE_UNIT_NAME],
    ] {
        if let Err(e) = run_systemctl(&step) {
            println!("(non-critical) {:#}", e);
        }
    }
    let _ = run_systemctl(&["--user", "daemon-reload"]);

    if unit_path.exists() {
        fs::remove_file(&unit_path).with_context(|| format!("removing {}", unit_path.display()))?;
        println!("Unit removed: {}", unit_path.display());
    } else {
        println!("No unit was installed (nothing to remove).");
    }

    if let Ok(env_user) = std::env::var("USER") {
        let _ = std::process::Command::new("loginctl")
            .args(["disable-linger", &env_user])
            .output();
    }

    println!("clari.service uninstalled.");
    println!(
        "Note: the statusLine hook, the config and the binary ({}) are \
         kept. A bare `clari` runs the daemon manually again; remove \
         the binary by hand for a full cleanup.",
        executable_install_path().display()
    );
    Ok(())
}

/// Ensures the config file exists with all the defaults, creating it (and
/// its directory) when missing. Best-effort: the daemon runs on defaults
/// anyway, so a failure is only a warning. Called on daemon start so a
/// fresh install never fails for a missing config.
fn ensure_config(config_path: &Path) {
    if config_path.exists() {
        return;
    }
    if let Some(parent) = config_path.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            warn!(
                "Could not create the config directory {}: {:#}",
                parent.display(),
                e
            );
            return;
        }
    }
    match toml::to_string_pretty(&Config::default()) {
        Ok(text) => match fs::write(config_path, text) {
            Ok(()) => info!("Created default config at {}", config_path.display()),
            Err(e) => warn!(
                "Could not create the default config at {}: {:#}",
                config_path.display(),
                e
            ),
        },
        Err(e) => warn!("Could not serialize the default config: {:#}", e),
    }
}

/// Whether the systemd user service is currently active (not just
/// installed). Used to decide if starting it is a real change worth
/// announcing. Returns None when systemd cannot be queried.
fn is_service_active() -> bool {
    std::process::Command::new("systemctl")
        .args(["--user", "is-active", SERVICE_UNIT_NAME])
        .output()
        .ok()
        .map(|o| {
            let s = String::from_utf8_lossy(&o.stdout);
            let s = s.trim();
            s == "active" || s == "activating" || s == "reloading"
        })
        .unwrap_or(false)
}

/// Whether the clari daemon is running, for the status display. Prefers
/// the systemd user service when the unit exists and is active; otherwise
/// scans the process table for a clari daemon process (e.g. a bare
/// `clari` started by hand).
fn daemon_status() -> bool {
    let unit_installed = user_systemd_dir().join(SERVICE_UNIT_NAME).exists();
    let systemd_state = if unit_installed {
        std::process::Command::new("systemctl")
            .args(["--user", "is-active", SERVICE_UNIT_NAME])
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    } else {
        None
    };
    match systemd_state.as_deref() {
        Some("active" | "activating" | "reloading") => true,
        Some(_) => clari_daemon_process().is_some(),
        None => clari_daemon_process().is_some(),
    }
}

/// Finds a running clari daemon process (start mode), returning its pid
/// and the CLI mode. Ignores the current process. A bare `clari` (no
/// arguments) is also a daemon, since a flagless run starts the guard.
fn clari_daemon_process() -> Option<(u32, String)> {
    let self_pid = std::process::id();
    let entries = fs::read_dir("/proc").ok()?;
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        if pid == self_pid {
            continue;
        }
        let Ok(cmdline) = fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let args: Vec<&[u8]> = cmdline
            .split(|b| *b == 0)
            .filter(|a| !a.is_empty())
            .collect();
        if args.is_empty() {
            continue;
        }
        let exe = String::from_utf8_lossy(args[0]);
        let bin = Path::new(&*exe)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("");
        if !bin.contains("clari") {
            continue;
        }
        let has = |flag: &str| args.contains(&flag.as_bytes());
        if has("--start") || has("-z") {
            let mode = if has("--start") { "--start" } else { "-z" };
            return Some((pid, mode.to_string()));
        }
        if args.len() == 1 {
            return Some((pid, String::new()));
        }
    }
    None
}

/// Stops the running clari daemon: the systemd user service when the unit
/// is installed, plus a SIGTERM to any leftover daemon process. Idempotent.
fn stop_daemon() -> Result<()> {
    let unit_installed = user_systemd_dir().join(SERVICE_UNIT_NAME).exists();
    if unit_installed {
        match run_systemctl(&["--user", "stop", SERVICE_UNIT_NAME]) {
            Ok(()) => println!("Daemon stopped (systemd user service {}).", SERVICE_UNIT_NAME),
            Err(e) => println!("(already stopped?) {:#}", e),
        }
    }
    if let Some((pid, mode)) = clari_daemon_process() {
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGTERM);
        }
        if mode.is_empty() {
            println!("Daemon process stopped (SIGTERM sent to pid {pid}).");
        } else {
            println!(
                "Daemon process stopped (SIGTERM sent to pid {pid}, `clari {mode}`)."
            );
        }
    } else if !unit_installed {
        println!("No clari daemon is running (nothing to stop).");
    }
    Ok(())
}

/// Prints the full status: clari ON/OFF (the most important item), the
/// quota/window data, delegation, hook, wake targets and the per-agent
/// states. Used by -s/--status and reported after start/stop/install/
/// uninstall.
async fn print_full_status(config: &Config) -> Result<()> {
    let daemon_on = daemon_status();
    println!(
        "{:<15}: {}",
        "clari",
        if daemon_on {
            painted("ON", GREEN)
        } else {
            painted("OFF", RED)
        }
    );
    println!(
        "{:<15}: {}",
        "service",
        if user_systemd_dir().join(SERVICE_UNIT_NAME).exists() {
            painted("installed", GREEN)
        } else {
            painted("not installed", YELLOW)
        }
    );

    let pause = load_pause_state(&pause_path());
    let pause_info = if pause.global {
        painted("GLOBAL — no wake/delegation to any agent", RED)
    } else if pause.agents.is_empty() {
        painted("none", GREEN)
    } else {
        painted(&format!("agents paused: {}", pause.agents.join(", ")), YELLOW)
    };
    println!("{:<15}: {}", "pause", pause_info);

    let info = match gather_rate_info(config) {
        Ok(i) => i,
        Err(e) => {
            println!(
                "{:<15}: {}",
                "statusline",
                painted(&format!("(read error: {e:#})"), RED)
            );
            RateInfo::default()
        }
    };
    print_status(&info, config);
    println!(
        "{:<15}: {}",
        "delegation",
        if config.delegation {
            painted("enabled", GREEN)
        } else {
            painted("disabled (default)", RED)
        }
    );
    println!(
        "{:<15}: {}",
        "low_priority",
        if config.low_priority {
            painted(
                &format!(
                    "enabled — {} to agents stuck on the limit screen (default)",
                    config.low_priority_command
                ),
                GREEN,
            )
        } else {
            painted("disabled (--no-low-priority): blocked agents wait for the reset", YELLOW)
        }
    );
    let limits = load_limites();
    println!(
        "{:<15}: {}",
        "budget",
        format!(
            "max_devs={} usd_dia={} tokens_sesion={}",
            fmt_opt_u32(limits.max_devs),
            fmt_opt_f64(limits.usd_dia),
            limits.tokens_sesion,
        )
    );
    if let Some(forced) = config.forced_resets_at {
        println!(
            "{:<15}: {} (forced window; ignores the hook resets_at)",
            "resets_forced", forced
        );
    }
    println!(
        "{:<15}: {}",
        "statusline_hook",
        match hook_state(&claude_settings_path(config)) {
            HookState::Present => painted("installed", GREEN),
            HookState::NoSettings => painted("no settings.json", YELLOW),
            HookState::Missing => painted("missing (auto-install on daemon start)", YELLOW),
            HookState::Other => painted("points to another command", RED),
            HookState::Invalid => painted("invalid JSON", RED),
        }
    );
    println!(
        "{:<15}: {}",
        "wake_targets",
        if config.resume_all {
            painted(
                &format!(
                    "ALL kind='{}' claude windows (-a/--all, default)",
                    config.herdr_agent_kind
                ),
                GREEN,
            )
        } else {
            painted("only the pinned herdr_agent_target (--no-all)", YELLOW)
        }
    );
    let sessions = running_sessions(config).await;
    println!(
        "{:<15}: {}",
        "herdr_sessions",
        if config.herdr_session.is_some() {
            painted(
                &format!("pinned to '{}' (--session null to watch all)", sessions[0].label()),
                YELLOW,
            )
        } else {
            painted(
                &format!(
                    "ALL running ({}): {}",
                    sessions.len(),
                    sessions.iter().map(|s| s.label().to_string()).collect::<Vec<_>>().join(", ")
                ),
                GREEN,
            )
        }
    );
    match resolve_targets(config).await {
        Ok(targets) => {
            println!(
                "{:<15}: {}",
                "herdr_targets",
                targets.iter().map(|t| t.key()).collect::<Vec<_>>().join(", ")
            );
            for t in &targets {
                match get_agent_status(config, t).await {
                    Ok(status) => {
                        let code = match status.as_str() {
                            "working" | "busy" => GREEN,
                            "idle" | "free" | "waiting" => YELLOW,
                            "queued" | "pending" => CYAN,
                            _ => RED,
                        };
                        println!("  {t} : {}", painted(&status, code));
                    }
                    Err(e) => println!("  {t} : {}", painted(&format!("(error: {e:#})"), RED)),
                }
            }
        }
        Err(e) => println!(
            "{:<15}: {}",
            "herdr_targets",
            painted(&format!("(not resolved: {e:#})"), RED)
        ),
    }
    Ok(())
}

fn print_status(info: &RateInfo, config: &Config) {
    let state = load_state(&config.state_path).unwrap_or_default();
    let fmt_ts = |ts: i64| {
        DateTime::from_timestamp(ts, 0)
            .map(|d| d.with_timezone(&Local).format("%H:%M:%S").to_string())
            .unwrap_or_else(|| ts.to_string())
    };
    let mut injected: Vec<_> = state.injected_targets.iter().collect();
    injected.sort_by(|a, b| a.0.cmp(b.0));
    let mut woken: Vec<_> = state.woken_targets.iter().collect();
    woken.sort_by(|a, b| a.0.cmp(b.0));
    let render = |v: &Vec<(&String, &i64)>| {
        if v.is_empty() {
            painted("(none yet)", YELLOW)
        } else {
            painted(
                &v.iter()
                    .map(|(t, w)| format!("{t} @{}", fmt_ts(**w)))
                    .collect::<Vec<_>>()
                    .join(", "),
                GREEN,
            )
        }
    };

    println!(
        "used_pct       : {}",
        painted(
            &format!("{:.1}%", info.used_pct),
            if info.hard_limit_hit || info.used_pct >= 99.9 {
                RED
            } else if info.used_pct >= config.threshold_pct {
                YELLOW
            } else {
                GREEN
            }
        )
    );
    println!(
        "hard_limit     : {}",
        painted(
            if info.hard_limit_hit {
                "true (blocked)"
            } else {
                "false"
            },
            if info.hard_limit_hit { RED } else { GREEN }
        )
    );
    if let Some(ts) = info.resets_at {
        let dt = DateTime::from_timestamp(ts, 0)
            .map(|d| d.with_timezone(&Local).to_rfc3339())
            .unwrap_or_default();
        let remaining = ts - Utc::now().timestamp();
        println!("resets_at      : {} ({})", ts, dt);
        println!(
            "remaining_secs : {}",
            if remaining <= config.warning_lead_time_secs as i64 {
                painted(&format!("{remaining} (warning in ~{remaining}s)"), YELLOW)
            } else {
                remaining.to_string()
            }
        );
    } else {
        println!("resets_at      : (unknown)");
    }
    println!(
        "delegation_sent: {}",
        render(&injected)
    );
    println!("resume_sent     : {}", render(&woken));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resume_message_is_plain_when_delegation_off() {
        let cfg = Config {
            delegation: false,
            resume_message: "continue".into(),
            ..Config::default()
        };
        assert_eq!(effective_resume_message(&cfg), "continue");
    }

    #[test]
    fn resume_message_notifies_team_lead_when_delegation_on() {
        let cfg = Config {
            delegation: true,
            resume_message: "continue".into(),
            ..Config::default()
        };
        let m = effective_resume_message(&cfg);
        assert_eq!(
            m,
            format!("continue. {RESUME_DELEGATION_NOTICE}"),
            "delegated resume must keep 'continue' and tell the team lead it is back"
        );
    }

    #[test]
    fn custom_resume_message_is_preserved_with_the_notice() {
        let cfg = Config {
            delegation: true,
            resume_message: "continue and read HANDOFF.md.".into(),
            ..Config::default()
        };
        let m = effective_resume_message(&cfg);
        assert!(m.starts_with("continue and read HANDOFF.md. Notify the team lead"), "got: {m}");
    }

    #[test]
    fn delegation_stalled_detects_boolean_flag() {
        let v: Value = json!({ "result": { "agent_prompt_stalled": true } });
        assert!(delegation_stalled(&v), "explicit stall boolean must be caught");
    }

    #[test]
    fn delegation_stalled_detects_status_string() {
        let v: Value = json!({ "result": { "agent": { "agent_status": "agent_prompt_stalled" } } });
        assert!(delegation_stalled(&v), "status string containing 'stall' must be caught");
    }

    #[test]
    fn delegation_stalled_accepts_normal_response() {
        let v: Value = json!({ "result": { "agent": { "agent_status": "idle" } }, "ok": true });
        assert!(!delegation_stalled(&v), "a clean accept must not be treated as stalled");
    }

    #[test]
    fn agent_is_blocked_flags_bad_statuses() {
        for s in ["stalled", "blocked", "error", "limit_reached", "weird_unknown"] {
            assert!(agent_is_blocked(s), "{s} should be treated as blocked");
        }
    }

    #[test]
    fn agent_is_blocked_healthy_statuses_are_free() {
        for s in [
            "working", "busy", "idle", "free", "waiting", "queued", "pending", "done", "ready",
        ] {
            assert!(!agent_is_blocked(s), "{s} should NOT be blocked");
        }
    }

    #[test]
    fn limites_config_defaults() {
        let cfg = LimitesConfig::default();
        assert_eq!(cfg.max_devs, None);
        assert_eq!(cfg.usd_dia, None);
        assert_eq!(cfg.tokens_sesion, 50_000);
    }

    #[test]
    fn limites_config_roundtrip() {
        let cfg = LimitesConfig {
            max_devs: Some(3),
            usd_dia: Some(5.50),
            tokens_sesion: 100_000,
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let back: LimitesConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.max_devs, Some(3));
        assert!((back.usd_dia.unwrap() - 5.50).abs() < f64::EPSILON);
        assert_eq!(back.tokens_sesion, 100_000);
    }

    #[test]
    fn is_over_budget_stub_always_false() {
        assert!(!is_over_budget(), "stub must always return false");
    }

    #[test]
    fn pause_state_default_is_not_paused() {
        let p = PauseState::default();
        assert!(!p.global);
        assert!(p.agents.is_empty());
    }

    #[test]
    fn pause_state_serde_roundtrip() {
        let p = PauseState {
            global: true,
            agents: vec!["w1:p1".into(), "w2:p2".into()],
        };
        let json = serde_json::to_string(&p).unwrap();
        let q: PauseState = serde_json::from_str(&json).unwrap();
        assert!(q.global);
        assert_eq!(q.agents, vec!["w1:p1", "w2:p2"]);
    }

    #[test]
    fn pause_state_partial_agent() {
        let p = PauseState {
            global: false,
            agents: vec!["w1:p1".into()],
        };
        let json = serde_json::to_string(&p).unwrap();
        let q: PauseState = serde_json::from_str(&json).unwrap();
        assert!(!q.global);
        assert_eq!(q.agents, vec!["w1:p1"]);
    }

    #[test]
    fn pause_state_missing_agents_defaults_to_empty() {
        let json = r#"{"global": true}"#;
        let p: PauseState = serde_json::from_str(json).unwrap();
        assert!(p.global);
        assert!(p.agents.is_empty());
    }

    #[test]
    fn pause_state_empty_json() {
        let p: PauseState = serde_json::from_str("{}").unwrap();
        assert!(!p.global);
        assert!(p.agents.is_empty());
    }

    #[test]
    fn limit_screen_detects_the_stuck_state() {
        let screen = "some output\n\n  Usage limit reached · resets 2:20am · /low-priority to continue now at lower priority · uses your weekly limit\n\n  ❯ \n";
        assert_eq!(classify_limit_screen(screen), LimitScreen::LimitReached);
        let armed = "  Usage limit reached · continuing automatically at 2:20am · esc to cancel\n";
        assert_eq!(classify_limit_screen(armed), LimitScreen::LimitReached);
    }

    #[test]
    fn limit_screen_leaves_lower_priority_alone() {
        // The pinned status line is the LAST word: an older "Usage limit
        // reached" banner higher up must not trigger a second (toggling) send.
        let screen = "  Usage limit reached\n\n  ⚠ Lower priority until 2:20am · 28% allowance left · /low-priority to stop\n\n  ⏵⏵ bypass permissions on\n";
        assert_eq!(classify_limit_screen(screen), LimitScreen::LowPriorityActive);
        let just_sent = "Continuing now at lower priority until your limit resets at 2:20am. Your weekly limit still applies...\n";
        assert_eq!(classify_limit_screen(just_sent), LimitScreen::LowPriorityActive);
    }

    #[test]
    fn limit_screen_knows_when_the_mode_is_unavailable() {
        for s in [
            "You've used this week's lower-priority allowance. Lower-priority mode is offered again after your weekly limit resets.",
            "Lower-priority mode ended · you have reached your weekly usage limit",
            "No room for lower-priority work for a while · lower-priority mode stopped; new messages wait for your usage limit to reset",
            "Lower-priority mode isn't available right now.",
        ] {
            assert_eq!(classify_limit_screen(s), LimitScreen::LowPriorityUnavailable, "{s}");
        }
    }

    #[test]
    fn limit_screen_api_error_wording_is_the_limit_too() {
        let screen = "● Agent \"x\" failed: Agent terminated early due to an API error: You've hit your session limit · resets 2:20am (America/Argentina/Buenos_Aires)\n  ⎿  You've hit your session limit · resets 2:20am\n     /upgrade to increase your usage limit.\n❯ \n";
        assert_eq!(classify_limit_screen(screen), LimitScreen::LimitReached);
        // ...unless the allowance is gone: then the LAST word is "unavailable".
        let gone = "● You've used this week's lower-priority allowance · lower-priority mode ended; it is offered again after your weekly limit resets\n  ⎿  You've hit your session limit · resets 2:20am\n";
        assert_eq!(classify_limit_screen(gone), LimitScreen::LimitReached);
        let gone_last = "  ⎿  You've hit your session limit · resets 2:20am\n● You've used this week's lower-priority allowance · lower-priority mode ended\n";
        assert_eq!(classify_limit_screen(gone_last), LimitScreen::LowPriorityUnavailable);
    }

    #[test]
    fn pending_resume_state_roundtrip_and_default() {
        let s: GuardState = serde_json::from_str("{}").unwrap();
        assert_eq!(s.blocked_reset_at, None);
        let s: GuardState = serde_json::from_str(r#"{"blocked_reset_at": 1788758400}"#).unwrap();
        assert_eq!(s.blocked_reset_at, Some(1788758400));
    }

    #[test]
    fn limit_screen_other_states() {
        assert_eq!(
            classify_limit_screen("Your usage limit has reset · press enter to continue"),
            LimitScreen::LimitReset
        );
        assert_eq!(classify_limit_screen("✻ Thinking… (3s)\n❯ "), LimitScreen::Other);
        // "Lower-priority mode is off" = the user turned it off: never fight them.
        assert_eq!(
            classify_limit_screen("Lower-priority mode is off. New messages wait for your usage limit as usual."),
            LimitScreen::Other
        );
    }

    #[test]
    fn session_list_keeps_only_running_sessions_and_default_has_no_name() {
        let v: Value = serde_json::from_str(
            r#"{"sessions":[
                {"default":true,"name":"default","running":true,"socket_path":"/h/.config/herdr/herdr.sock"},
                {"default":false,"name":"jefe","running":false,"socket_path":"/h/.config/herdr/sessions/jefe/herdr.sock"},
                {"default":false,"name":"super","running":true,"socket_path":"/h/.config/herdr/sessions/super/herdr.sock"}
            ]}"#,
        )
        .unwrap();
        let s = parse_session_list(&v);
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].name, None);
        assert_eq!(s[0].socket.as_deref(), Some(Path::new("/h/.config/herdr/herdr.sock")));
        assert_eq!(s[1].name.as_deref(), Some("super"));
        assert_eq!(s[1].label(), "super");
        assert_eq!(s[0].label(), "default");
    }

    #[test]
    fn target_key_and_pause_matching() {
        let t = Target { id: "w1:p1A".into(), session: Some("super".into()), socket: None };
        assert_eq!(t.key(), "w1:p1A@super");
        assert_eq!(t.to_string(), "w1:p1A@super");
        let bare = Target { id: "w1:p1A".into(), session: None, socket: None };
        assert_eq!(bare.key(), "w1:p1A");
        let by_id = PauseState { global: false, agents: vec!["w1:p1A".into()] };
        let by_key = PauseState { global: false, agents: vec!["w1:p1A@super".into()] };
        let other = PauseState { global: false, agents: vec!["w1:p1A@jefe".into()] };
        assert!(t.is_paused(&by_id));
        assert!(t.is_paused(&by_key));
        assert!(!t.is_paused(&other));
    }

    #[test]
    fn low_priority_is_on_by_default_and_the_flag_turns_it_off() {
        let cfg = Config::default();
        assert!(cfg.low_priority);
        assert_eq!(cfg.low_priority_command, "/low-priority");
        let cli = Cli::parse_from(["clari", "--no-low-priority"]);
        let s = collect_settings(&cli).unwrap();
        assert_eq!(s, vec![("low_priority".to_string(), Some(toml::Value::Boolean(false)))]);
        let cli = Cli::parse_from(["clari", "-L"]);
        let s = collect_settings(&cli).unwrap();
        assert_eq!(s, vec![("low_priority".to_string(), Some(toml::Value::Boolean(true)))]);
        assert!(collect_settings(&Cli::parse_from(["clari", "-L", "--no-low-priority"])).is_err());
    }
}
