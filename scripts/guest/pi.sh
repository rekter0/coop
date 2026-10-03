# Keep the installer cleanup local when provisioning snippets are concatenated.
(
    set -euo pipefail
    : "${GUEST_USER:?GUEST_USER must be set by the orchestrator}"

    PI_PREFIX="/home/${GUEST_USER}/.local"
    PI_BIN="${PI_PREFIX}/bin/pi"

    # pi needs Node.js 22.19 or newer. The `node` profile may already have
    # installed it from NodeSource; otherwise add the same repository.
    if ! command -v node >/dev/null 2>&1 \
        || ! node -e 'const [major, minor] = process.versions.node.split(".").map(Number); process.exit(major > 22 || (major === 22 && minor >= 19) ? 0 : 1)'; then
        echo '  [guest] Installing Node.js 22 for pi...'
        PI_NODESOURCE=$(mktemp)
        trap 'rm -f "$PI_NODESOURCE"' EXIT
        curl -fsSL --retry 3 --retry-all-errors -o "$PI_NODESOURCE" https://deb.nodesource.com/setup_22.x
        bash "$PI_NODESOURCE" </dev/null
        DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends nodejs </dev/null
    fi

    # A profile post_install may already provide pi (e.g. a test stub).
    if [ -x "$PI_BIN" ]; then
        echo '  [guest] pi already installed, skipping npm install.'
    else
        echo '  [guest] Installing pi...'
        # The guest-owned prefix lets `pi update` reinstall itself without
        # sudo. --ignore-scripts keeps dependency lifecycle scripts from
        # running; pi's published npm-shrinkwrap.json pins its dependencies.
        su - "$GUEST_USER" -c \
            "npm install -g --ignore-scripts --no-fund --no-audit --prefix '${PI_PREFIX}' @earendil-works/pi-coding-agent" \
            </dev/null
    fi

    if ! su - "$GUEST_USER" -c "'$PI_BIN' --version" </dev/null >/dev/null; then
        echo "  [guest] ERROR: $PI_BIN is installed but cannot run." >&2
        exit 1
    fi

    echo '  [guest] Symlinking pi into system PATH...'
    ln -sf "$PI_BIN" /usr/local/bin/pi

    echo '  [guest] Installing pi-yolo shortcut...'
    cat >/usr/local/bin/pi-yolo <<'YOLOEOF'
#!/bin/bash
# pi reads subcommands only as the first argument, so --approve goes in
# front of session launches and nowhere else.
case "${1:-}" in
    auth | install | remove | uninstall | update | list | config | mcp) exec pi "$@" ;;
esac
exec pi --approve "$@"
YOLOEOF
    chmod 755 /usr/local/bin/pi-yolo
)
