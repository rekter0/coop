# pi Integration

coop installs [pi](https://github.com/earendil-works/pi) into every guest
image and gives you a dedicated `coop pi` launcher. This guide covers the
command, the configuration that controls what gets injected into the guest,
and the bootstrap sequence that runs when a VM starts.

## Launching pi

```bash
coop pi [instance-name] [-- extra-args...]
```

This SSHes into the guest and runs `pi --approve` in `/workspace`. pi does not
ask before each tool call; its only prompt is whether to trust a project's
`.pi/` settings, extensions, skills, and MCP servers. `--approve` trusts them
for the session, since the VM is the isolation boundary.

To get that prompt back for a single session, pass `--ask`. coop then omits
`--approve`, and pi asks before loading project-local `.pi/` resources. Tool
calls still run without prompts:

```bash
coop pi --ask
```

Trailing arguments go straight through to the `pi` CLI:

```bash
coop pi -- --model opus
coop pi -- -p "list the failing tests"
```

pi reads its subcommands (`auth`, `install`, `remove`, `uninstall`, `update`,
`list`, `config`, `mcp`) only as the first argument, so coop does not add
`--approve` in front of them:

```bash
coop pi -- install npm:@example/pi-tools
coop pi -- list
```

Inside the guest, `pi-yolo` follows the same rule from any directory.

## Configuration

pi settings live under the `[pi]` section in `config.toml`:

```toml
[pi]
env_forward = ["OPENROUTER_API_KEY", "GEMINI_API_KEY"]
config_dir = "~/.pi/agent"
packages = ["npm:@example/pi-tools@1.0.0", "git:github.com/example/pi-skills@v1"]

[pi.mcp_servers.playwright]
command = "npx"
args = ["-y", "@playwright/mcp@latest"]
```

Every field is optional. An empty `[pi]` section (or omitting it entirely)
still installs pi in the image and still copies host config on boot.

### Provider credentials

pi works with many model providers and reads their API keys from the
environment. coop already forwards `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, and
`XAI_API_KEY` on every SSH session when they are configured under `[claude]`,
`[codex]`, or `[grok]` or set on the host, so pi sees them without extra
configuration. Proxy mode (`[proxy.anthropic]` / `[proxy.openai]`) and
`codex.auth = "chatgpt"` keep the matching key out of the guest; pi then needs
credentials from the copied `auth.json` or a guest login.

List any other provider key (`OPENROUTER_API_KEY`, `GEMINI_API_KEY`, …) in
`env_forward`. The values travel over SSH `SendEnv` and are never written to
guest disk. An active VM PAT assignment rejects `GITHUB_TOKEN` and `GH_TOKEN`
in this list.

pi stores API keys and subscription logins entered with `/login` in
`auth.json`. When the host has one, coop copies it into the guest on every boot
(see [Config directory](#config-directory)), so host logins work in the guest
immediately. An entry whose `key` is a `!command` runs that command inside the
guest, so a command that reads a host secret manager does not resolve there;
forward that key with `env_forward` instead. Without a host copy, sign in from
the guest with `/login` inside `coop pi`.

### Config directory

`config_dir` selects the host pi agent directory to overlay into the guest's
`~/.pi/agent/` on every agent bootstrap (`coop up` or `coop start`, without
`--no-agents`). The default is `~/.pi/agent`; a custom path supports `~`
expansion. The copied entries are:

- `AGENTS.md`, `AGENTS.override.md`, `CLAUDE.md`, `SYSTEM.md`,
  `APPEND_SYSTEM.md`
- `keybindings.json`, `models.json`, `auth.json`
- `extensions/`, `skills/`, `prompts/`, `themes/`

Directory symlinks, hidden directories, and bare git repos (`*.git`) inside a
copied tree stay on the host. Sessions, `trust.json` (host project paths), and
installed packages are not copied. Host `settings.json` and `mcp.json` are
merged rather than copied; see [Settings](#settings) and
[MCP server registration](#mcp-server-registration).

A copied `auth.json` is set to owner-only (`0600`). When the host has one, it
is reapplied on every boot, so a login made inside the guest lasts until the
next restart; without a host copy, guest logins persist. Providers that rotate
OAuth refresh tokens allow only one client to keep refreshing a shared login;
if the guest login stops working, sign in again on the host and restart the VM.

`models.json` is copied verbatim. Host-absolute paths and `localhost`
endpoints in it refer to the guest, not the host.

Files follow an overlay lifecycle: restart overwrites files still present on
the host, but host deletions do not delete previous guest copies.
`config_dir = false` stops copying and retains previous copies. A missing
default source likewise retains previous copies; custom paths must exist at
config validation time. To remove retained content, remove it in the guest or
recreate the VM.

Project files under `/workspace` (`AGENTS.md`, `.pi/`) are already in the
workspace and do not need to be copied.

### Settings

When the host has a `settings.json`, coop overlays it onto the guest
`~/.pi/agent/settings.json`: host keys replace guest keys, and nested objects
merge with the host winning on a conflict. The host `packages` list is dropped
because it names host installs and paths; the guest keeps its own `packages`
list, so packages installed in the guest stay loaded across restarts. List the
packages the guest should install in `[pi] packages`.

### MCP server registration

`mcp_servers` maps server names to definitions using the same schema as the
other agents. coop merges them into the guest `~/.pi/agent/mcp.json`:

1. The guest file is the base; its other keys and servers are kept.
2. Servers in host `mcp.json` replace guest servers with the same name.
3. Servers in `[pi.mcp_servers]` replace either.

pi supports stdio and streamable-HTTP servers but not the legacy SSE transport,
so a `type = "sse"` entry under `[pi.mcp_servers]` fails config validation;
use `type = "http"`. Removing a server from coop config does not delete it from
the guest file. Stdio `env` values are host variable names in coop config;
they are written as `${NAME}` so pi expands them from the guest environment,
and those host names are forwarded automatically. HTTP header values that use
`cmd:` are resolved on the host and written into the guest file, which is
owner-only. When neither host `mcp.json` nor `[pi.mcp_servers]` exists, coop
leaves the guest file untouched.

### Packages

`packages` lists [pi packages](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/packages.md)
to install in the guest with `pi install`. Each entry is an npm source
(`npm:<package>[@version]`), a git source (`git:<host>/<repo>[@ref]`), or a git
URL (`https://`, `ssh://`, `git@`). A local path names a host directory, so it
fails config validation. pi records each install in the guest
`settings.json`. Packages can run extension code inside the guest.

Packages are baked into the golden image during `coop setup` (on the
Lima/macOS backend) and recorded in the image's template config. On a VM's
first boot coop installs only the packages not already baked in; on the
Firecracker/Linux backend, where nothing is baked, the full list installs on
first boot.

## Bootstrap sequence

When `coop up` creates/restarts a project VM or `coop start` restarts a
stopped VM (without `--no-agents`), coop executes the following steps after
the VM boots and SSH becomes available:

1. **User content**: Overlay the allowlisted entries from `config_dir` into
   `~/.pi/agent/`, including `auth.json`.
2. **Settings**: Overlay host `settings.json` onto the guest file, keeping the
   guest `packages` list.
3. **MCP servers**: Merge host and configured servers into
   `~/.pi/agent/mcp.json`.
4. **Packages** (first boot only): Install the `packages` not already baked
   into the golden image.

### Skipping bootstrap

```bash
coop up . --no-agents
coop start --no-agents
```

This skips the guest bootstrap sequence entirely. The VM still includes pi
because it is baked into the image during `coop setup`.

## Installation and updates

pi needs Node.js 22.19 or newer. `coop setup` installs Node.js 22 from the
NodeSource repository (the same source as the `node` profile) unless a
suitable `node` is already present, then installs the latest
`@earendil-works/pi-coding-agent` from npm as the guest user with
`npm install -g --ignore-scripts --prefix ~/.local`. pi publishes an
`npm-shrinkwrap.json`, so its dependency versions are pinned by the release.
The binary is linked at `/usr/local/bin/pi`. Images built before pi support
need `coop setup --rebuild`; existing VMs also need
`coop restore <vm> --image <image> --reprovision` (or destroy/recreate) to pick
up pi and Node.js.

pi checks for new versions at startup but does not install them on its own. To
update a running VM, run `pi update` inside it, or from the host:

```bash
coop agent update --pi
coop agent update --check   # compare against the latest GitHub release
```

`pi update` reinstalls the package with npm into the guest user's `~/.local`
prefix, so it needs no sudo. See [`agent update`](commands.md#agent-update).
