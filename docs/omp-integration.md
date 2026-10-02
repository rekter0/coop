# omp Integration

coop installs [omp](https://github.com/can1357/oh-my-pi) (oh-my-pi) into
every guest image and gives you a dedicated `coop omp` launcher. This guide
covers the command, the configuration that controls what gets injected into
the guest, and the bootstrap sequence that runs when a VM starts.

## Launching omp

```bash
coop omp [instance-name] [-- extra-args...]
```

This SSHes into the guest and runs `omp --yolo` in `/workspace`. The VM is
the isolation boundary, so omp's approval prompts are redundant. omp already
defaults to its `yolo` approval mode; coop passes the flag explicitly so a
stricter `tools.approvalMode` in a copied `config.yml` does not apply.

To restore prompts for a single session, pass `--ask`. coop then passes
`--approval-mode always-ask`, which prompts before writes and command
execution:

```bash
coop omp --ask
```

Trailing arguments go straight through to the `omp` CLI:

```bash
coop omp -- --model opus
coop omp -- -p "list the failing tests"
```

omp ignores launch flags that precede a subcommand, so subcommands work
unchanged:

```bash
coop omp -- login
coop omp -- plugin list
```

Inside the guest, `omp-yolo` runs `omp --yolo` from any directory.

## Configuration

omp settings live under the `[omp]` section in `config.toml`:

```toml
[omp]
env_forward = ["OPENROUTER_API_KEY", "GEMINI_API_KEY"]
config_dir = "~/.omp/agent"

[omp.mcp_servers.playwright]
command = "npx"
args = ["-y", "@playwright/mcp@latest"]
```

Every field is optional. An empty `[omp]` section (or omitting it entirely)
still installs omp in the image and still copies host config on boot.

### Provider credentials

omp works with many model providers and reads their API keys from the
environment. coop already forwards `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, and
`XAI_API_KEY` on every SSH session when they are configured under `[claude]`,
`[codex]`, or `[grok]` or set on the host, so omp sees them without extra
configuration. Proxy mode (`[proxy.anthropic]` / `[proxy.openai]`) and
`codex.auth = "chatgpt"` keep the matching key out of the guest; omp then
needs credentials from the copied `agent.db` or a guest login.

List any other provider key (`OPENROUTER_API_KEY`, `GEMINI_API_KEY`, …) in
`env_forward`. The values travel over SSH `SendEnv` and are never written to
guest disk. An active VM PAT assignment rejects `GITHUB_TOKEN` and `GH_TOKEN`
in this list.

omp stores subscription logins and API keys entered with `/login` in its
`SQLite` credential store, `agent.db`. When the host has one, coop copies it
into the guest on every boot (see [Config directory](#config-directory)), so
host logins work in the guest immediately. Without a host copy, sign in from
the guest:

```bash
coop omp -- login
```

### Config directory

`config_dir` selects the host omp agent directory to overlay into the guest's
`~/.omp/agent/` on every agent bootstrap (`coop up` or `coop start`, without
`--no-agents`). The default is `~/.omp/agent`; a custom path supports `~`
expansion. The copied entries are:

- `AGENTS.md`, `SYSTEM.md`, `APPEND_SYSTEM.md`, `RULES.md`
- `config.yml`, `models.yml`, `lsp.json`, `keybindings.yml`
- `agent.db` and, when present, its `agent.db-wal` journal
- `skills/`, `rules/`, `commands/`, `prompts/`, `instructions/`, `hooks/`,
  `tools/`, `extensions/`, `agents/`

Directory symlinks, hidden directories, and bare git repos (`*.git`) inside a
copied tree stay on the host. Sessions, caches, `secrets.yml`, and installed
plugins (`~/.omp/plugins/`) are not copied. Host `mcp.json` is merged rather
than copied; see [MCP server registration](#mcp-server-registration).

The copied `agent.db` holds every provider credential in the host store. coop
removes any guest `agent.db-wal` and `agent.db-shm` before the copy, so an
older journal is never replayed onto the new database, and sets the copied
files to owner-only (`0600`). When the host has an `agent.db`, it is reapplied
on every boot, so a login made inside the guest lasts until the next restart;
without a host copy, guest logins persist. Providers that rotate OAuth refresh
tokens allow only one client to keep refreshing a shared login; if the guest
login stops working, sign in again on the host and restart the VM. If host omp
is writing while the VM starts, the database and its journal can be copied
from slightly different moments; restarting the VM copies them again.

`config.yml` and `models.yml` are copied verbatim. Host-absolute paths and
`localhost` endpoints in them refer to the guest, not the host.

Files follow an overlay lifecycle: restart overwrites files still present on
the host, but host deletions do not delete previous guest copies.
`config_dir = false` stops copying and retains previous copies. A missing
default source likewise retains previous copies; custom paths must exist at
config validation time. To remove retained content, remove it in the guest
or recreate the VM.

Project files under `/workspace` (`AGENTS.md`, `.omp/`) are already in the
workspace and do not need to be copied.

### MCP server registration

`mcp_servers` maps server names to definitions using the same schema as
Claude, Codex, and Grok integration. coop merges them into the guest
`~/.omp/agent/mcp.json`:

1. The guest file is the base; its other keys and servers are kept.
2. Servers in host `mcp.json` replace guest servers with the same name.
3. Servers in `[omp.mcp_servers]` replace either.

Removing a server from coop config does not delete it from the guest file.
Stdio `env` values are host variable names in coop config; they are written
as `${NAME}` so omp expands them from the guest environment, and those host
names are forwarded automatically. HTTP header values that use `cmd:` are
resolved on the host and written into the guest file, which is owner-only.
When neither host `mcp.json` nor `[omp.mcp_servers]` exists, coop leaves the
guest file untouched.

omp also discovers MCP servers and skills from other agents' config, including
`~/.claude*` and `~/.codex/config.toml`. In the guest those files are managed
by coop's Claude and Codex integration, so omp sees the servers configured
there as well.

### Plugin marketplaces

`marketplaces` and `plugins` declare omp plugin marketplaces and the plugins
to install from them:

```toml
[omp]
marketplaces = ["anthropics/claude-plugins-official"]
plugins = ["code-review@claude-plugins-official"]
```

Each marketplace source is registered with `omp plugin marketplace add` and
each plugin installed with `omp plugin install --scope user`. omp reads
Claude Code-compatible catalogs. A source that is an absolute local directory
is copied into the guest first.

These are baked into the golden image during `coop setup` (on the Lima/macOS
backend) and recorded in the image's template config. On a VM's first boot
coop installs only the delta not already baked in; on the Firecracker/Linux
backend, where nothing is baked, the full set installs on first boot.

## Bootstrap sequence

When `coop up` creates/restarts a project VM or `coop start` restarts a
stopped VM (without `--no-agents`), coop executes the following steps after
the VM boots and SSH becomes available:

1. **User content**: Overlay the allowlisted entries from `config_dir` into
   `~/.omp/agent/`, including the `agent.db` credential store.
2. **MCP servers**: Merge host and configured servers into
   `~/.omp/agent/mcp.json`.
3. **Marketplaces & plugins** (first boot only): Install the configured
   `marketplaces`/`plugins` not already baked into the golden image.

### Skipping bootstrap

```bash
coop up . --no-agents
coop start --no-agents
```

This skips the guest bootstrap sequence entirely. The VM still includes omp
because it is baked into the image during `coop setup`.

## Installation and updates

`coop setup` installs the latest omp release that exists when the image is
built. It downloads the prebuilt `omp-linux-x64` or `omp-linux-arm64` binary
and that release's `SHA256SUMS.txt` from GitHub, verifies the checksum, and
installs the binary to `~/.local/bin/omp` for the guest user with a
`/usr/local/bin/omp` link. Images built before omp support need
`coop setup --rebuild`; existing VMs also need
`coop restore <vm> --image <image> --reprovision` (or destroy/recreate) to
pick up the binary.

omp checks for updates at startup but does not install them on its own. To
update a running VM, run `omp update` inside it, or from the host:

```bash
coop agent update --omp
coop agent update --check   # compare against the latest GitHub release
```

`omp update` replaces the binary under the guest user's home, so it needs no
sudo. See [`agent update`](commands.md#agent-update).
