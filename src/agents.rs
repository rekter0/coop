//! The coding agents coop installs in every guest image.
//!
//! [`AgentKind`] is the single list of agents. Shared code that does
//! something per agent iterates [`AgentKind::ALL`] and matches on the kind
//! exhaustively, so adding a variant fails to compile at every site that
//! needs a decision for it. Behavior that belongs to one agent (bootstrap
//! details, proxy and local-model routing, launch flags) stays in the module
//! that owns it; this module holds the facts the shared code needs.

use crate::config::{CoopConfig, mcp_stdio_env_host_names};
use crate::guest::{self, GuestUser, ProfileDef};
use crate::guest_env_state::EnvVarName;
use crate::paths::GuestPath;

/// A coding agent installed in the guest image.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentKind {
    Claude,
    Codex,
    Grok,
    Omp,
    Pi,
}

impl AgentKind {
    /// Every agent, in install, bootstrap, and update order. The install
    /// order is part of the provisioning script, whose hash decides whether
    /// an existing golden image is stale.
    pub const ALL: [Self; 5] = [Self::Claude, Self::Codex, Self::Grok, Self::Omp, Self::Pi];

    /// Human-facing label used in prompts, reports, and errors.
    pub fn display(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Codex => "Codex",
            Self::Grok => "Grok Build",
            Self::Omp => "omp",
            Self::Pi => "pi",
        }
    }

    /// Name of the agent's `coop <agent>` subcommand and `coop agent update
    /// --<agent>` flag.
    pub fn cli_name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Grok => "grok",
            Self::Omp => "omp",
            Self::Pi => "pi",
        }
    }

    /// Absolute guest path coop invokes for this agent. Claude Code, Grok
    /// Build, omp, and pi live under the guest user's home; Codex uses its
    /// system compatibility link.
    pub fn binary(self, user: &GuestUser) -> GuestPath {
        match self {
            Self::Claude => user.claude_bin(),
            Self::Codex => guest::codex_bin(),
            Self::Grok => user.grok_bin(),
            Self::Omp => user.omp_bin(),
            Self::Pi => user.pi_bin(),
        }
    }

    /// Guest provisioning snippets, in the order they run. They are
    /// concatenated with the other installers into one script that runs as
    /// root after guest config has created the guest user, so none may
    /// `exit` early.
    pub fn install_scripts(self) -> &'static [&'static str] {
        match self {
            Self::Claude => &[guest::SCRIPT_CLAUDE_CODE],
            // The native installer keeps the full package under the guest
            // user's home; the account wrapper drives the guest keyring.
            Self::Codex => &[guest::SCRIPT_CODEX, guest::SCRIPT_CODEX_ACCOUNT],
            Self::Grok => &[guest::SCRIPT_GROK],
            Self::Omp => &[guest::SCRIPT_OMP],
            Self::Pi => &[guest::SCRIPT_PI],
        }
    }

    /// Binaries the golden image must contain for this agent.
    pub fn required_binaries(self, user: &GuestUser) -> Vec<GuestPath> {
        match self {
            Self::Claude | Self::Grok | Self::Omp => vec![self.binary(user)],
            // pi's entry point is a Node script, so the runtime is part of the
            // install.
            Self::Pi => vec![self.binary(user), GuestPath::new("/usr/bin/node")],
            Self::Codex => vec![
                guest::codex_bin(),
                guest::codex_code_mode_host_bin(),
                guest::codex_account_bin(),
                // The Secret Service stack `codex-account` drives. Checking
                // the wrapper alone proves nothing — the provision script
                // always writes it — so verify the three tools its
                // BASE_PACKAGES entries install.
                GuestPath::new("/usr/bin/dbus-run-session"),
                GuestPath::new("/usr/bin/gnome-keyring-daemon"),
                GuestPath::new("/usr/bin/secret-tool"),
            ],
        }
    }

    /// Host environment variable names forwarded into the guest for this
    /// agent: its `env_forward` list, plus the host names that Grok, omp,
    /// and pi stdio MCP `env` mappings reference (they expand them as
    /// `${NAME}` in the guest config, so they must exist there).
    pub fn env_forward_names(self, cfg: &CoopConfig) -> Vec<EnvVarName> {
        match self {
            Self::Claude => cfg.claude.env_forward.clone(),
            Self::Codex => cfg.codex.env_forward.clone(),
            Self::Grok => {
                let mut names = cfg.grok.env_forward.clone();
                names.extend(mcp_stdio_env_host_names(&cfg.grok.mcp_servers));
                names
            }
            Self::Omp => {
                let mut names = cfg.omp.env_forward.clone();
                names.extend(mcp_stdio_env_host_names(&cfg.omp.mcp_servers));
                names
            }
            Self::Pi => {
                let mut names = cfg.pi.env_forward.clone();
                names.extend(mcp_stdio_env_host_names(&cfg.pi.mcp_servers));
                names
            }
        }
    }

    /// Marketplaces and plugins configured in this agent's own config
    /// section.
    pub fn configured_plugins(self, cfg: &CoopConfig) -> (&[String], &[String]) {
        match self {
            Self::Claude => (&cfg.claude.marketplaces, &cfg.claude.plugins),
            Self::Codex => (&cfg.codex.marketplaces, &cfg.codex.plugins),
            Self::Grok => (&cfg.grok.marketplaces, &cfg.grok.plugins),
            Self::Omp => (&cfg.omp.marketplaces, &cfg.omp.plugins),
            // pi has no marketplaces; its packages are installed directly.
            Self::Pi => (&[], &cfg.pi.packages),
        }
    }

    /// Marketplaces and plugins to bake into a golden image for this agent,
    /// sorted and deduplicated. Profile plugin lists are Claude plugins, so
    /// only Claude merges them in.
    pub fn baked_lists(
        self,
        cfg: &CoopConfig,
        profiles: &[ProfileDef],
    ) -> (Vec<String>, Vec<String>) {
        let (configured_marketplaces, configured_plugins) = self.configured_plugins(cfg);
        let mut marketplaces = configured_marketplaces.to_vec();
        let mut plugins = configured_plugins.to_vec();

        match self {
            Self::Claude => {
                for def in profiles {
                    marketplaces.extend(def.marketplaces.iter().cloned());
                    plugins.extend(def.plugins.iter().cloned());
                }
            }
            Self::Codex | Self::Grok | Self::Omp | Self::Pi => {}
        }

        marketplaces.sort_unstable();
        marketplaces.dedup();
        plugins.sort_unstable();
        plugins.dedup();

        (marketplaces, plugins)
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests use unwrap for brevity")]
mod tests {
    use super::*;

    fn profile(marketplaces: &[&str], plugins: &[&str]) -> ProfileDef {
        ProfileDef {
            name: "custom".into(),
            apt_packages: vec![],
            pre_install: None,
            post_install: None,
            marketplaces: marketplaces.iter().map(|s| (*s).to_owned()).collect(),
            plugins: plugins.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    #[test]
    fn display_names_each_agent() {
        assert_eq!(AgentKind::Claude.display(), "Claude Code");
        assert_eq!(AgentKind::Codex.display(), "Codex");
        assert_eq!(AgentKind::Grok.display(), "Grok Build");
        assert_eq!(AgentKind::Omp.display(), "omp");
        assert_eq!(AgentKind::Pi.display(), "pi");
    }

    #[test]
    fn cli_name_matches_each_subcommand() {
        let names: Vec<&str> = AgentKind::ALL.iter().map(|a| a.cli_name()).collect();
        assert_eq!(names, ["claude", "codex", "grok", "omp", "pi"]);
    }

    #[test]
    fn binary_paths_match_each_installer() {
        let user = GuestUser::new("dev").unwrap();
        assert_eq!(
            AgentKind::Claude.binary(&user).to_string(),
            "/home/dev/.local/bin/claude"
        );
        assert_eq!(
            AgentKind::Codex.binary(&user).to_string(),
            "/usr/local/bin/codex"
        );
        assert_eq!(
            AgentKind::Grok.binary(&user).to_string(),
            "/home/dev/.grok/bin/grok"
        );
        assert_eq!(
            AgentKind::Omp.binary(&user).to_string(),
            "/home/dev/.local/bin/omp"
        );
        assert_eq!(
            AgentKind::Pi.binary(&user).to_string(),
            "/home/dev/.local/bin/pi"
        );
    }

    #[test]
    fn install_scripts_keep_the_hashed_order() {
        // Reordering changes the provisioning script hash, which marks every
        // existing golden image stale.
        let scripts: Vec<&str> = AgentKind::ALL
            .iter()
            .flat_map(|agent| agent.install_scripts())
            .copied()
            .collect();
        assert_eq!(
            scripts,
            [
                guest::SCRIPT_CLAUDE_CODE,
                guest::SCRIPT_CODEX,
                guest::SCRIPT_CODEX_ACCOUNT,
                guest::SCRIPT_GROK,
                guest::SCRIPT_OMP,
                guest::SCRIPT_PI,
            ]
        );
    }

    #[test]
    fn required_binaries_cover_each_agent() {
        let user = GuestUser::default();
        let paths = |agent: AgentKind| -> Vec<String> {
            agent
                .required_binaries(&user)
                .iter()
                .map(ToString::to_string)
                .collect()
        };
        assert_eq!(paths(AgentKind::Claude), ["/home/ubuntu/.local/bin/claude"]);
        assert_eq!(paths(AgentKind::Grok), ["/home/ubuntu/.grok/bin/grok"]);
        assert_eq!(paths(AgentKind::Omp), ["/home/ubuntu/.local/bin/omp"]);
        assert_eq!(
            paths(AgentKind::Pi),
            ["/home/ubuntu/.local/bin/pi", "/usr/bin/node"]
        );
        assert_eq!(
            paths(AgentKind::Codex),
            [
                "/usr/local/bin/codex",
                "/usr/local/bin/codex-code-mode-host",
                "/usr/local/bin/codex-account",
                "/usr/bin/dbus-run-session",
                "/usr/bin/gnome-keyring-daemon",
                "/usr/bin/secret-tool",
            ]
        );
    }

    #[test]
    fn env_forward_names_read_each_section() {
        let mut cfg = CoopConfig::default();
        cfg.claude.env_forward = vec![EnvVarName::new("CLAUDE_ONLY").unwrap()];
        cfg.codex.env_forward = vec![EnvVarName::new("CODEX_ONLY").unwrap()];
        cfg.grok.env_forward = vec![EnvVarName::new("GROK_ONLY").unwrap()];
        cfg.omp.env_forward = vec![EnvVarName::new("OMP_ONLY").unwrap()];
        cfg.pi.env_forward = vec![EnvVarName::new("PI_ONLY").unwrap()];
        let mut env = std::collections::BTreeMap::new();
        env.insert(
            EnvVarName::new("TOKEN").unwrap(),
            EnvVarName::new("GROK_MCP_HOST").unwrap(),
        );
        cfg.grok.mcp_servers.insert(
            "tool".into(),
            crate::config::McpServerDef::Stdio {
                command: "npx".into(),
                args: vec![],
                env,
            },
        );
        let mut omp_env = std::collections::BTreeMap::new();
        omp_env.insert(
            EnvVarName::new("TOKEN").unwrap(),
            EnvVarName::new("OMP_MCP_HOST").unwrap(),
        );
        cfg.omp.mcp_servers.insert(
            "tool".into(),
            crate::config::McpServerDef::Stdio {
                command: "npx".into(),
                args: vec![],
                env: omp_env,
            },
        );
        let mut pi_env = std::collections::BTreeMap::new();
        pi_env.insert(
            EnvVarName::new("TOKEN").unwrap(),
            EnvVarName::new("PI_MCP_HOST").unwrap(),
        );
        cfg.pi.mcp_servers.insert(
            "tool".into(),
            crate::config::McpServerDef::Stdio {
                command: "npx".into(),
                args: vec![],
                env: pi_env,
            },
        );

        let names = |agent: AgentKind| -> Vec<String> {
            agent
                .env_forward_names(&cfg)
                .iter()
                .map(|n| n.as_ref().to_owned())
                .collect()
        };
        assert_eq!(names(AgentKind::Claude), ["CLAUDE_ONLY"]);
        assert_eq!(names(AgentKind::Codex), ["CODEX_ONLY"]);
        assert_eq!(names(AgentKind::Grok), ["GROK_ONLY", "GROK_MCP_HOST"]);
        assert_eq!(names(AgentKind::Omp), ["OMP_ONLY", "OMP_MCP_HOST"]);
        assert_eq!(names(AgentKind::Pi), ["PI_ONLY", "PI_MCP_HOST"]);
    }

    #[test]
    fn configured_plugins_read_each_section() {
        let mut cfg = CoopConfig::default();
        cfg.claude.marketplaces = vec!["claude-m".into()];
        cfg.claude.plugins = vec!["claude-p".into()];
        cfg.codex.marketplaces = vec!["codex-m".into()];
        cfg.codex.plugins = vec!["codex-p".into()];
        cfg.grok.marketplaces = vec!["grok-m".into()];
        cfg.grok.plugins = vec!["grok-p".into()];
        cfg.omp.marketplaces = vec!["omp-m".into()];
        cfg.omp.plugins = vec!["omp-p".into()];

        let expected = [
            (AgentKind::Claude, "claude-m", "claude-p"),
            (AgentKind::Codex, "codex-m", "codex-p"),
            (AgentKind::Grok, "grok-m", "grok-p"),
            (AgentKind::Omp, "omp-m", "omp-p"),
        ];
        for (agent, marketplace, plugin) in expected {
            let (marketplaces, plugins) = agent.configured_plugins(&cfg);
            assert_eq!(marketplaces, [marketplace], "{agent:?}");
            assert_eq!(plugins, [plugin], "{agent:?}");
        }

        cfg.pi.packages = vec!["npm:pi-p".into()];
        let (marketplaces, packages) = AgentKind::Pi.configured_plugins(&cfg);
        assert!(marketplaces.is_empty());
        assert_eq!(packages, ["npm:pi-p"]);
    }

    #[test]
    fn claude_baked_lists_merge_profiles_sorted_and_deduplicated() {
        let mut cfg = CoopConfig::default();
        cfg.claude.marketplaces = vec!["z".into(), "a".into(), "a".into()];
        cfg.claude.plugins = vec!["p2@z".into(), "p1@a".into()];
        cfg.codex.marketplaces = vec!["codex-only".into()];
        cfg.codex.plugins = vec!["codex-only-plugin".into()];
        let profiles = [profile(&["b", "a"], &["p3@b", "p1@a"])];

        let (marketplaces, plugins) = AgentKind::Claude.baked_lists(&cfg, &profiles);
        assert_eq!(marketplaces, ["a", "b", "z"]);
        assert_eq!(plugins, ["p1@a", "p2@z", "p3@b"]);
    }

    #[test]
    fn non_claude_baked_lists_ignore_profiles() {
        let mut cfg = CoopConfig::default();
        cfg.codex.marketplaces = vec!["b".into(), "a".into(), "a".into()];
        cfg.codex.plugins = vec!["p2@b".into(), "p1@a".into(), "p2@b".into()];
        cfg.grok.marketplaces = vec!["d".into(), "c".into(), "c".into()];
        cfg.grok.plugins = vec!["p4@d".into(), "p3@c".into(), "p4@d".into()];
        cfg.omp.marketplaces = vec!["f".into(), "e".into(), "e".into()];
        cfg.omp.plugins = vec!["p6@f".into(), "p5@e".into(), "p6@f".into()];
        cfg.pi.packages = vec!["npm:b".into(), "npm:a".into(), "npm:b".into()];
        let profiles = [profile(&["profile-m"], &["profile-p"])];

        let (marketplaces, plugins) = AgentKind::Codex.baked_lists(&cfg, &profiles);
        assert_eq!(marketplaces, ["a", "b"]);
        assert_eq!(plugins, ["p1@a", "p2@b"]);

        let (marketplaces, plugins) = AgentKind::Grok.baked_lists(&cfg, &profiles);
        assert_eq!(marketplaces, ["c", "d"]);
        assert_eq!(plugins, ["p3@c", "p4@d"]);

        let (marketplaces, plugins) = AgentKind::Omp.baked_lists(&cfg, &profiles);
        assert_eq!(marketplaces, ["e", "f"]);
        assert_eq!(plugins, ["p5@e", "p6@f"]);

        let (marketplaces, packages) = AgentKind::Pi.baked_lists(&cfg, &profiles);
        assert!(marketplaces.is_empty());
        assert_eq!(packages, ["npm:a", "npm:b"]);
    }
}
