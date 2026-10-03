//! `coop agent update` — refresh the coding-agent binaries inside a running VM.
//!
//! The agents are installed "latest at build time" during `coop setup`, so
//! they go stale in long-running VMs and in VMs created from an old image.
//! This command updates them in place against a *running* instance, without
//! rebuilding the golden image (`coop setup --rebuild` remains the path for
//! refreshing the image itself).
//!
//! The agents differ in how they update, and the difference is encoded in
//! [`UpdateStrategy`] (chosen by [`strategy`]) so no caller can run the
//! wrong one:
//!
//! - **Codex** uses a native per-user installation. coop re-runs its
//!   installer wrapper ([`guest::SCRIPT_CODEX`]) with `COOP_FORCE_INSTALL=1`
//!   to refresh the package or migrate an older direct-binary installation.
//!   The guest user can also run `codex update` directly.
//! - **Claude Code** lives in the guest user's `~/.local/bin` and already
//!   auto-updates in the background. `coop agent update --claude` just runs
//!   `claude update` synchronously as the guest user — a convenience, not a
//!   fix.
//! - **Grok Build** lives in the guest user's `~/.grok/bin` and already
//!   auto-updates in the background. `coop agent update --grok` runs
//!   `grok update` synchronously as the guest user.
//! - **omp** lives in the guest user's `~/.local/bin` and only checks for
//!   updates at startup. `coop agent update --omp` runs `omp update`, which
//!   downloads and checksums the release binary, as the guest user; `--check`
//!   compares against the latest GitHub release like Codex.
//! - **pi** is an npm package under the guest user's `~/.local` prefix.
//!   `coop agent update --pi` runs `pi update`, which reinstalls it with npm
//!   into that prefix as the guest user; `--check` compares against the
//!   latest GitHub release.

use std::io::Write as _;

use anyhow::{Context, Result, bail};
use semver::Version;

use crate::agents::AgentKind;
use crate::backend::{self, SshSession};
use crate::paths::GuestPath;
use crate::remote_command::RemoteCommand;
use crate::{config, guest, prompt, update};

use super::{prepare_session_from_target, resolve_running};

/// Options for `coop agent update`, parsed from the CLI flags.
pub(crate) struct AgentUpdateOpts {
    pub selection: AgentSelection,
    pub check: bool,
    pub yes: bool,
}

/// The release feeds coop compares guest binaries against for agents that
/// do not update themselves in the background.
const CODEX_REPO: &str = "openai/codex";
const OMP_REPO: &str = "can1357/oh-my-pi";
const PI_REPO: &str = "earendil-works/pi";

// ── Domain types ──────────────────────────────────────────────

/// How `agent`'s binary is refreshed inside the guest. The root-vs-user
/// asymmetry lives here so a caller can't run Claude's or Grok's
/// self-update as root or Codex's reinstall without sudo.
fn strategy(agent: AgentKind) -> UpdateStrategy {
    match agent {
        AgentKind::Claude | AgentKind::Grok | AgentKind::Omp | AgentKind::Pi => {
            UpdateStrategy::SelfUpdate
        }
        AgentKind::Codex => UpdateStrategy::ReinstallAsRoot {
            script: guest::SCRIPT_CODEX,
        },
    }
}

/// Whether `agent` keeps itself current in the background, so coop tracks
/// no "latest" version for it.
fn auto_updates(agent: AgentKind) -> bool {
    match agent {
        AgentKind::Claude | AgentKind::Grok => true,
        AgentKind::Codex | AgentKind::Omp | AgentKind::Pi => false,
    }
}

/// How an agent's binary is refreshed in the guest.
enum UpdateStrategy {
    /// Run the installer wrapper with sudo so it can replace the system link.
    /// The wrapper runs the native installer as the configured guest user.
    ReinstallAsRoot { script: &'static str },
    /// Invoke the agent's own updater as the guest user (no sudo).
    SelfUpdate,
}

/// Which agents a single `coop agent update` invocation targets, in
/// [`AgentKind::ALL`] order. No construction can represent "update
/// nothing", so [`agents`](Self::agents) is always non-empty.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AgentSelection {
    agents: Vec<AgentKind>,
}

impl AgentSelection {
    /// Select the agents named on the command line. Selection is additive:
    /// naming none means every agent.
    pub(crate) fn new(named: impl IntoIterator<Item = AgentKind>) -> Self {
        let named: Vec<AgentKind> = named.into_iter().collect();
        let agents: Vec<AgentKind> = AgentKind::ALL
            .into_iter()
            .filter(|agent| named.contains(agent))
            .collect();
        Self {
            agents: if agents.is_empty() {
                AgentKind::ALL.to_vec()
            } else {
                agents
            },
        }
    }

    fn agents(&self) -> &[AgentKind] {
        &self.agents
    }
}

/// A parsed agent version. The constructor extracts the first semver-looking
/// token from arbitrary `--version` / `tag_name` output, so callers compare
/// with `<` rather than string-diffing.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct AgentVersion(Version);

impl AgentVersion {
    /// Extract a semver version from free-form text. Handles bare `1.2.3`,
    /// `v1.2.3`, tool-prefixed `codex-cli 0.5.0`, and dash-separated tags
    /// like `rust-v0.42.0`. Returns `Err` when no token parses.
    ///
    /// The *first* semver-parseable whitespace token wins, which matches the
    /// `--version` output of both agents today (the version leads or is the
    /// second token). A future format that emitted another semver-shaped
    /// token first would mis-pick; the unit tests pin the current shapes.
    fn parse(raw: &str) -> Result<Self> {
        raw.split_whitespace()
            .find_map(Self::from_token)
            .map(Self)
            .with_context(|| format!("no semver version found in {raw:?}"))
    }

    /// Try to read a version out of one whitespace-delimited token, first as
    /// the whole token (minus a leading `v`), then as the suffix after the
    /// last `v` (for tags such as `rust-v0.42.0`). A `name/` prefix, as in
    /// omp's `omp/18.4.12`, is dropped first.
    fn from_token(token: &str) -> Option<Version> {
        let token = token.rsplit_once('/').map_or(token, |(_, rest)| rest);
        let stripped = token.strip_prefix('v').unwrap_or(token);
        if let Ok(v) = Version::parse(stripped) {
            return Some(v);
        }
        let after_v = token.rsplit_once('v').map(|(_, rest)| rest)?;
        Version::parse(after_v).ok()
    }
}

impl std::fmt::Display for AgentVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Result of updating one agent.
enum UpdateOutcome {
    Updated {
        from: Option<AgentVersion>,
        to: AgentVersion,
    },
    AlreadyCurrent {
        version: AgentVersion,
    },
}

/// Version-comparison result for `--check`, never a sentinel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CheckStatus {
    UpToDate,
    UpdateAvailable,
    /// Claude Code and Grok Build update themselves in the background —
    /// coop does not track a "latest" for them.
    AutoUpdates,
    /// Installed or latest version could not be determined.
    Unknown,
}

/// One row of the `--check` report.
struct CheckRow {
    agent: AgentKind,
    installed: Option<AgentVersion>,
    latest: Option<AgentVersion>,
    status: CheckStatus,
}

// ── Command entry point ───────────────────────────────────────

pub(crate) fn cmd_agent_update(
    be: &backend::PlatformBackend,
    cfg: &config::CoopConfig,
    name: Option<&config::InstanceName>,
    opts: &AgentUpdateOpts,
) -> Result<()> {
    // Resolve the running instance once (a stopped/missing one errors here
    // with the shared "not running" guidance), then build the SSH session
    // from it — this mirrors `open_ssh_session` but keeps the instance name
    // for the confirmation prompt and messages without a second lookup.
    let running = resolve_running(be, cfg, name)?;
    let inst_name = running.instance().name.to_string();
    let repo = backend::detect_instance_repo(running.instance());
    let (inst, target) = running.into_parts();
    let session = prepare_session_from_target(cfg, Some(&inst), target, repo.as_ref())?;

    if opts.check {
        return run_check(&session, &opts.selection);
    }

    if !opts.yes
        && !prompt::confirm(&format!(
            "Update {} in '{inst_name}' to the latest version?",
            selection_phrase(&opts.selection),
        ))?
    {
        tracing::info!("Update cancelled");
        return Ok(());
    }

    run_updates(&session, &opts.selection)
}

/// Comma/and-joined agent names for the confirmation prompt.
fn selection_phrase(selection: &AgentSelection) -> String {
    let labels: Vec<&str> = selection.agents().iter().map(|a| a.display()).collect();
    match labels.as_slice() {
        [] => unreachable!("AgentSelection is never empty"),
        [one] => (*one).to_string(),
        [a, b] => format!("{a} and {b}"),
        [rest @ .., last] => format!("{}, and {last}", rest.join(", ")),
    }
}

// ── Update path ───────────────────────────────────────────────

/// Update every selected agent, printing each result. Continues past a
/// per-agent failure and returns an error only after all have run, so a
/// multi-agent update reports every outcome even when one fails.
fn run_updates(session: &SshSession, selection: &AgentSelection) -> Result<()> {
    let out = &mut std::io::stdout();
    let mut failed = false;
    for &agent in selection.agents() {
        match update_one(session, agent) {
            Ok(outcome) => {
                writeln!(out, "{}", outcome_line(agent, &outcome))?;
                if auto_updates(agent) {
                    writeln!(
                        out,
                        "  note: {} also auto-updates in the background.",
                        agent.display()
                    )?;
                }
            }
            Err(e) => {
                failed = true;
                writeln!(out, "{}: update failed — {e:#}", agent.display())?;
            }
        }
    }
    if failed {
        bail!("one or more agents failed to update");
    }
    Ok(())
}

/// Update a single agent and verify the result by re-reading its version.
fn update_one(session: &SshSession, agent: AgentKind) -> Result<UpdateOutcome> {
    let before = capture_version(session, agent);
    match strategy(agent) {
        UpdateStrategy::ReinstallAsRoot { script } => reinstall_as_root(session, script)
            .with_context(|| format!("failed to reinstall {}", agent.display()))?,
        UpdateStrategy::SelfUpdate => {
            self_update(session, agent)
                .with_context(|| format!("failed to update {}", agent.display()))?;
        }
    }
    // A readable version after the update doubles as the executable check:
    // `capture_version` runs `<bin> --version` over SSH, which fails if the
    // binary is missing or not runnable.
    let after = capture_version(session, agent).with_context(|| {
        format!(
            "could not read {} version after update — the binary may be missing or broken",
            agent.display(),
        )
    })?;
    Ok(if before.as_ref() == Some(&after) {
        UpdateOutcome::AlreadyCurrent { version: after }
    } else {
        UpdateOutcome::Updated {
            from: before,
            to: after,
        }
    })
}

/// Re-run an embedded installer script as root with the force flag set,
/// piping the script over stdin so it never lands on argv.
fn reinstall_as_root(session: &SshSession, script: &str) -> Result<()> {
    let user = guest::GuestUser::new(session.target.user.as_ref())?;
    session.target.exec_with_stdin(
        RemoteCommand::new()
            .literal("sudo env GUEST_USER=")
            .arg(user.as_str())
            .literal(" COOP_FORCE_INSTALL=1 bash -s"),
        script.as_bytes().to_vec(),
    )
}

/// Run an agent's own updater as the guest user (no sudo).
fn self_update(session: &SshSession, agent: AgentKind) -> Result<()> {
    let bin = agent_binary(session, agent)?;
    session
        .target
        .exec(RemoteCommand::new().arg(bin).literal(" update"))
}

// ── Check path ────────────────────────────────────────────────

/// Report installed-vs-latest versions for the selected agents. Mutates
/// nothing; degrades to `Unknown` when a version can't be determined.
fn run_check(session: &SshSession, selection: &AgentSelection) -> Result<()> {
    let rows: Vec<CheckRow> = selection
        .agents()
        .iter()
        .map(|&agent| check_row(session, agent))
        .collect();
    let out = &mut std::io::stdout();
    for line in check_report(&rows) {
        writeln!(out, "{line}")?;
    }
    Ok(())
}

/// Gather one agent's installed/latest versions and classify them.
fn check_row(session: &SshSession, agent: AgentKind) -> CheckRow {
    let installed = capture_version(session, agent);
    let latest = release_repo(agent).and_then(|repo| latest_release(agent, repo));
    let status = check_status(agent, installed.as_ref(), latest.as_ref());
    CheckRow {
        agent,
        installed,
        latest,
        status,
    }
}

/// GitHub repo whose newest release `--check` compares against. Agents that
/// update themselves in the background have none.
fn release_repo(agent: AgentKind) -> Option<&'static str> {
    match agent {
        AgentKind::Claude | AgentKind::Grok => None,
        AgentKind::Codex => Some(CODEX_REPO),
        AgentKind::Omp => Some(OMP_REPO),
        AgentKind::Pi => Some(PI_REPO),
    }
}

/// Best-effort lookup of an agent's newest release tag. Network failures
/// degrade to `None` (reported as `Unknown`) rather than aborting the check.
fn latest_release(agent: AgentKind, repo: &str) -> Option<AgentVersion> {
    match update::latest_release_tag(repo) {
        Ok(tag) => AgentVersion::parse(&tag).ok(),
        Err(e) => {
            tracing::debug!(
                "Failed to look up latest {} release: {e:#}",
                agent.display()
            );
            None
        }
    }
}

/// Classify an agent's installed version against the latest known one.
fn check_status(
    agent: AgentKind,
    installed: Option<&AgentVersion>,
    latest: Option<&AgentVersion>,
) -> CheckStatus {
    if auto_updates(agent) {
        return CheckStatus::AutoUpdates;
    }
    match (installed, latest) {
        (Some(i), Some(l)) if i < l => CheckStatus::UpdateAvailable,
        (Some(_), Some(_)) => CheckStatus::UpToDate,
        _ => CheckStatus::Unknown,
    }
}

// ── Pure formatting ───────────────────────────────────────────

/// One line describing an update outcome.
fn outcome_line(agent: AgentKind, outcome: &UpdateOutcome) -> String {
    let label = agent.display();
    match outcome {
        UpdateOutcome::Updated {
            from: Some(from),
            to,
        } => {
            format!("{label}: updated {from} → {to}")
        }
        UpdateOutcome::Updated { from: None, to } => format!("{label}: updated to {to}"),
        UpdateOutcome::AlreadyCurrent { version } => {
            format!("{label}: already at the latest version ({version})")
        }
    }
}

/// Build the `--check` report lines: one aligned row per agent.
fn check_report(rows: &[CheckRow]) -> Vec<String> {
    rows.iter().map(check_line).collect()
}

fn check_line(row: &CheckRow) -> String {
    let installed = row
        .installed
        .as_ref()
        .map_or_else(|| "?".to_string(), AgentVersion::to_string);
    let (version_col, note) = match row.status {
        CheckStatus::UpToDate => (installed, "up to date".to_string()),
        CheckStatus::UpdateAvailable => {
            let latest = row
                .latest
                .as_ref()
                .map_or_else(|| "?".to_string(), AgentVersion::to_string);
            (
                format!("{installed} → {latest}"),
                format!(
                    "update available — run: coop agent update --{}",
                    row.agent.cli_name()
                ),
            )
        }
        CheckStatus::AutoUpdates => (
            installed,
            "up to date (auto-updates in background)".to_string(),
        ),
        CheckStatus::Unknown => (installed, "could not determine latest version".to_string()),
    };
    let label = row.agent.display();
    format!("{label:<12} {version_col:<16} {note}")
}

// ── Guest binary resolution + version capture (IO) ────────────

/// Absolute guest path of an agent's binary for the session's guest user.
fn agent_binary(session: &SshSession, agent: AgentKind) -> Result<GuestPath> {
    Ok(agent.binary(&guest::GuestUser::new(session.target.user.as_ref())?))
}

/// Read an agent's installed version over SSH, or `None` if the binary is
/// absent or its output doesn't parse. The path is derived from a validated
/// guest user, so it carries no shell metacharacters.
fn capture_version(session: &SshSession, agent: AgentKind) -> Option<AgentVersion> {
    let bin = agent_binary(session, agent).ok()?;
    let raw = session.target.capture(&format!("{bin} --version")).ok()?;
    AgentVersion::parse(&raw).ok()
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test code — panics are assertions")]
mod tests {
    use super::*;

    fn ver(s: &str) -> AgentVersion {
        AgentVersion::parse(s).unwrap()
    }

    // ── selection ──────────────────────────────────────────────

    fn select(named: &[AgentKind]) -> AgentSelection {
        AgentSelection::new(named.iter().copied())
    }

    #[test]
    fn selection_keeps_named_agents_in_canonical_order() {
        use AgentKind::{Claude, Codex, Grok, Omp, Pi};
        assert_eq!(select(&[Claude]).agents(), &[Claude]);
        assert_eq!(select(&[Codex]).agents(), &[Codex]);
        assert_eq!(select(&[Grok]).agents(), &[Grok]);
        assert_eq!(select(&[Claude, Codex]).agents(), &[Claude, Codex]);
        assert_eq!(select(&[Claude, Grok]).agents(), &[Claude, Grok]);
        assert_eq!(select(&[Grok, Codex]).agents(), &[Codex, Grok]);
        assert_eq!(select(&[Omp, Codex]).agents(), &[Codex, Omp]);
        assert_eq!(select(&[Pi, Omp]).agents(), &[Omp, Pi]);
        assert_eq!(
            select(&[Pi, Omp, Grok, Claude, Codex]).agents(),
            &AgentKind::ALL
        );
    }

    #[test]
    fn selection_of_nothing_means_every_agent() {
        assert_eq!(select(&[]).agents(), &AgentKind::ALL);
    }

    #[test]
    fn selection_lists_a_repeated_agent_once() {
        assert_eq!(
            select(&[AgentKind::Codex, AgentKind::Codex]).agents(),
            &[AgentKind::Codex]
        );
    }

    #[test]
    fn selection_phrase_joins_with_and() {
        assert_eq!(
            selection_phrase(&select(&[AgentKind::Claude])),
            "Claude Code"
        );
        assert_eq!(selection_phrase(&select(&[AgentKind::Codex])), "Codex");
        assert_eq!(
            selection_phrase(&select(&[AgentKind::Claude, AgentKind::Codex])),
            "Claude Code and Codex"
        );
        assert_eq!(
            selection_phrase(&select(&[])),
            "Claude Code, Codex, Grok Build, omp, and pi"
        );
    }

    // ── version parsing ────────────────────────────────────────

    #[test]
    fn parse_handles_bare_and_v_prefixed() {
        assert_eq!(ver("1.2.3"), ver("v1.2.3"));
        assert_eq!(ver("0.5.0").to_string(), "0.5.0");
    }

    #[test]
    fn parse_extracts_version_from_tool_prefixed_output() {
        assert_eq!(ver("codex-cli 0.5.0"), ver("0.5.0"));
        assert_eq!(ver("claude 1.2.3 (Claude Code)"), ver("1.2.3"));
    }

    #[test]
    fn parse_extracts_version_after_a_name_slash() {
        assert_eq!(ver("omp/18.4.12"), ver("18.4.12"));
        assert_eq!(ver("omp/v18.4.12"), ver("18.4.12"));
    }

    #[test]
    fn parse_extracts_version_from_dashed_tag() {
        assert_eq!(ver("rust-v0.42.0"), ver("0.42.0"));
    }

    #[test]
    fn parse_preserves_prerelease() {
        let v = ver("1.0.0-rc.1");
        assert_eq!(v.to_string(), "1.0.0-rc.1");
        assert!(ver("1.0.0") > v, "release must sort above its prerelease");
    }

    #[test]
    fn parse_rejects_garbage() {
        assert!(AgentVersion::parse("no version here").is_err());
        assert!(AgentVersion::parse("").is_err());
        assert!(AgentVersion::parse("v1").is_err());
    }

    #[test]
    fn version_ordering_is_semver_not_lexical() {
        assert!(ver("0.10.0") > ver("0.9.0"));
        assert!(ver("1.0.0") > ver("0.42.0"));
    }

    // ── check classification ───────────────────────────────────

    #[test]
    fn claude_always_reports_auto_updates() {
        assert_eq!(
            check_status(AgentKind::Claude, Some(&ver("1.2.3")), None),
            CheckStatus::AutoUpdates
        );
    }

    #[test]
    fn grok_reports_auto_updates() {
        assert_eq!(
            check_status(AgentKind::Grok, Some(&ver("1.0.24")), None),
            CheckStatus::AutoUpdates
        );
    }

    #[test]
    fn codex_update_available_when_installed_is_older() {
        assert_eq!(
            check_status(AgentKind::Codex, Some(&ver("0.4.1")), Some(&ver("0.5.0"))),
            CheckStatus::UpdateAvailable
        );
    }

    #[test]
    fn codex_up_to_date_when_equal_or_newer() {
        assert_eq!(
            check_status(AgentKind::Codex, Some(&ver("0.5.0")), Some(&ver("0.5.0"))),
            CheckStatus::UpToDate
        );
        assert_eq!(
            check_status(AgentKind::Codex, Some(&ver("0.6.0")), Some(&ver("0.5.0"))),
            CheckStatus::UpToDate
        );
    }

    #[test]
    fn codex_unknown_when_either_version_missing() {
        assert_eq!(
            check_status(AgentKind::Codex, None, Some(&ver("0.5.0"))),
            CheckStatus::Unknown
        );
        assert_eq!(
            check_status(AgentKind::Codex, Some(&ver("0.5.0")), None),
            CheckStatus::Unknown
        );
    }

    #[test]
    fn omp_compares_against_its_latest_release() {
        assert_eq!(
            check_status(AgentKind::Omp, Some(&ver("18.4.11")), Some(&ver("18.4.12"))),
            CheckStatus::UpdateAvailable
        );
        assert_eq!(
            check_status(AgentKind::Omp, Some(&ver("18.4.12")), Some(&ver("18.4.12"))),
            CheckStatus::UpToDate
        );
    }

    #[test]
    fn release_repo_names_only_agents_without_background_updates() {
        assert_eq!(release_repo(AgentKind::Codex), Some("openai/codex"));
        assert_eq!(release_repo(AgentKind::Omp), Some("can1357/oh-my-pi"));
        assert_eq!(release_repo(AgentKind::Pi), Some("earendil-works/pi"));
        assert_eq!(release_repo(AgentKind::Claude), None);
        assert_eq!(release_repo(AgentKind::Grok), None);
    }

    // ── report lines ───────────────────────────────────────────

    #[test]
    fn check_line_update_available_names_the_agents_flag() {
        let row = CheckRow {
            agent: AgentKind::Omp,
            installed: Some(ver("18.4.11")),
            latest: Some(ver("18.4.12")),
            status: CheckStatus::UpdateAvailable,
        };
        let line = check_line(&row);
        assert!(line.contains("coop agent update --omp"), "{line}");
        assert!(!line.contains("--codex"), "{line}");
    }

    #[test]
    fn check_line_update_available_shows_arrow_and_command() {
        let row = CheckRow {
            agent: AgentKind::Codex,
            installed: Some(ver("0.4.1")),
            latest: Some(ver("0.5.0")),
            status: CheckStatus::UpdateAvailable,
        };
        let line = check_line(&row);
        assert!(line.contains("0.4.1 → 0.5.0"), "{line}");
        assert!(line.contains("coop agent update --codex"), "{line}");
    }

    #[test]
    fn check_line_up_to_date_shows_installed_only() {
        let row = CheckRow {
            agent: AgentKind::Codex,
            installed: Some(ver("0.5.0")),
            latest: Some(ver("0.5.0")),
            status: CheckStatus::UpToDate,
        };
        let line = check_line(&row);
        assert!(line.contains("0.5.0"), "{line}");
        assert!(line.contains("up to date"), "{line}");
        assert!(!line.contains("→"), "{line}");
    }

    #[test]
    fn check_line_auto_updates_notes_background() {
        let row = CheckRow {
            agent: AgentKind::Claude,
            installed: Some(ver("1.2.3")),
            latest: None,
            status: CheckStatus::AutoUpdates,
        };
        let line = check_line(&row);
        assert!(line.contains("Claude Code"), "{line}");
        assert!(line.contains("1.2.3"), "{line}");
        assert!(line.contains("auto-updates in background"), "{line}");
    }

    #[test]
    fn check_line_auto_updates_names_grok_build() {
        let row = CheckRow {
            agent: AgentKind::Grok,
            installed: Some(ver("1.0.24")),
            latest: None,
            status: CheckStatus::AutoUpdates,
        };
        let line = check_line(&row);
        assert!(line.contains("Grok Build"), "{line}");
        assert!(line.contains("1.0.24"), "{line}");
        assert!(line.contains("auto-updates in background"), "{line}");
    }

    #[test]
    fn check_line_unknown_shows_placeholder() {
        let row = CheckRow {
            agent: AgentKind::Codex,
            installed: None,
            latest: None,
            status: CheckStatus::Unknown,
        };
        let line = check_line(&row);
        assert!(line.contains('?'), "{line}");
        assert!(line.contains("could not determine"), "{line}");
    }

    #[test]
    fn check_report_has_one_line_per_agent() {
        let rows = vec![
            CheckRow {
                agent: AgentKind::Claude,
                installed: Some(ver("1.2.3")),
                latest: None,
                status: CheckStatus::AutoUpdates,
            },
            CheckRow {
                agent: AgentKind::Codex,
                installed: Some(ver("0.4.1")),
                latest: Some(ver("0.5.0")),
                status: CheckStatus::UpdateAvailable,
            },
            CheckRow {
                agent: AgentKind::Grok,
                installed: Some(ver("1.0.24")),
                latest: None,
                status: CheckStatus::AutoUpdates,
            },
        ];
        let lines = check_report(&rows);
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("Claude Code"), "{}", lines[0]);
        assert!(lines[1].contains("Codex"), "{}", lines[1]);
        assert!(lines[2].contains("Grok Build"), "{}", lines[2]);
    }

    // ── outcome lines ──────────────────────────────────────────

    #[test]
    fn outcome_line_updated_from_known_version() {
        let outcome = UpdateOutcome::Updated {
            from: Some(ver("0.4.1")),
            to: ver("0.5.0"),
        };
        let line = outcome_line(AgentKind::Codex, &outcome);
        assert!(line.contains("Codex"), "{line}");
        assert!(line.contains("0.4.1 → 0.5.0"), "{line}");
    }

    #[test]
    fn outcome_line_updated_from_unknown_version() {
        let outcome = UpdateOutcome::Updated {
            from: None,
            to: ver("0.5.0"),
        };
        let line = outcome_line(AgentKind::Codex, &outcome);
        assert!(line.contains("updated to 0.5.0"), "{line}");
    }

    #[test]
    fn outcome_line_already_current() {
        let outcome = UpdateOutcome::AlreadyCurrent {
            version: ver("0.5.0"),
        };
        let line = outcome_line(AgentKind::Codex, &outcome);
        assert!(line.contains("already at the latest"), "{line}");
        assert!(line.contains("0.5.0"), "{line}");
    }

    #[test]
    fn strategy_matches_agent_asymmetry() {
        assert!(matches!(
            strategy(AgentKind::Codex),
            UpdateStrategy::ReinstallAsRoot { .. }
        ));
        assert!(matches!(
            strategy(AgentKind::Claude),
            UpdateStrategy::SelfUpdate
        ));
        assert!(matches!(
            strategy(AgentKind::Grok),
            UpdateStrategy::SelfUpdate
        ));
        assert!(matches!(
            strategy(AgentKind::Omp),
            UpdateStrategy::SelfUpdate
        ));
        assert!(matches!(
            strategy(AgentKind::Pi),
            UpdateStrategy::SelfUpdate
        ));
    }
}
