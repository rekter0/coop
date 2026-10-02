# Keep the installer cleanup local when provisioning snippets are concatenated.
(
    set -euo pipefail
    : "${GUEST_USER:?GUEST_USER must be set by the orchestrator}"

    OMP_BIN="/home/${GUEST_USER}/.local/bin/omp"

    # A profile post_install may already provide omp (e.g. a test stub).
    if [ -x "$OMP_BIN" ]; then
        echo '  [guest] omp already installed, skipping download.'
    else
        echo '  [guest] Installing omp...'
        case "$(uname -m)" in
            x86_64 | amd64) OMP_ARCH=x64 ;;
            aarch64 | arm64) OMP_ARCH=arm64 ;;
            *)
                echo "  [guest] ERROR: omp has no Linux build for $(uname -m)." >&2
                exit 1
                ;;
        esac
        OMP_ASSET="omp-linux-${OMP_ARCH}"
        OMP_RELEASES="https://github.com/can1357/oh-my-pi/releases"
        OMP_TMP=$(mktemp -d)
        trap 'rm -rf "$OMP_TMP"' EXIT

        # Resolve the latest tag once so the binary and SHA256SUMS.txt come
        # from the same release even if a new one is published mid-install.
        OMP_LATEST_URL=$(curl -fsSL --retry 3 --retry-all-errors \
            -o /dev/null -w '%{url_effective}' "$OMP_RELEASES/latest")
        OMP_TAG="${OMP_LATEST_URL##*/tag/}"
        if ! printf '%s\n' "$OMP_TAG" | grep -Eq '^v[0-9]+\.[0-9]+\.[0-9]+$'; then
            echo "  [guest] ERROR: could not resolve the latest omp release (got '$OMP_LATEST_URL')." >&2
            exit 1
        fi

        curl -fsSL --retry 3 --retry-all-errors \
            -o "$OMP_TMP/$OMP_ASSET" "$OMP_RELEASES/download/$OMP_TAG/$OMP_ASSET"
        curl -fsSL --retry 3 --retry-all-errors \
            -o "$OMP_TMP/SHA256SUMS.txt" "$OMP_RELEASES/download/$OMP_TAG/SHA256SUMS.txt"

        # omp's own installer skips this check; coop verifies the binary
        # against the checksum file published in the same release.
        OMP_EXPECTED=$(awk -v asset="$OMP_ASSET" '$2 == asset { print $1 }' "$OMP_TMP/SHA256SUMS.txt")
        OMP_ACTUAL=$(sha256sum "$OMP_TMP/$OMP_ASSET" | awk '{ print $1 }')
        if [ -z "$OMP_EXPECTED" ] || [ "$OMP_EXPECTED" != "$OMP_ACTUAL" ]; then
            echo "  [guest] ERROR: omp $OMP_TAG $OMP_ASSET failed SHA-256 verification." >&2
            echo "  [guest]   expected: ${OMP_EXPECTED:-<missing from SHA256SUMS.txt>}" >&2
            echo "  [guest]   actual:   $OMP_ACTUAL" >&2
            exit 1
        fi

        # ~/.local/bin already exists, owned by the guest user, from guest config;
        # without -D a missing directory fails here instead of turning root-owned.
        install -m 0755 -o "$GUEST_USER" -g "$GUEST_USER" "$OMP_TMP/$OMP_ASSET" "$OMP_BIN"
        echo "  [guest] Installed omp $OMP_TAG."
    fi

    # The release binary links against glibc; run it as the guest user so a
    # build that cannot start fails provisioning instead of the first launch.
    if ! su - "$GUEST_USER" -c "'$OMP_BIN' --version" </dev/null >/dev/null; then
        echo "  [guest] ERROR: $OMP_BIN is installed but cannot run." >&2
        exit 1
    fi

    echo '  [guest] Symlinking omp into system PATH...'
    ln -sf "$OMP_BIN" /usr/local/bin/omp

    echo '  [guest] Installing omp-yolo shortcut...'
    cat >/usr/local/bin/omp-yolo <<'YOLOEOF'
#!/bin/bash
exec omp --yolo "$@"
YOLOEOF
    chmod 755 /usr/local/bin/omp-yolo
)
