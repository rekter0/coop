set -euo pipefail

# This runs as root before the guest user exists, so a default rustup install
# would land in /root, where the guest's `cargo`/`rustc` proxies cannot find a
# toolchain. Install into shared locations instead and give them to uid 1000,
# which coop always assigns to the guest user, so `rustup update` and
# `cargo install` work without sudo.
echo '  [guest] Installing Rust via rustup...'
export RUSTUP_HOME=/usr/local/rustup
export CARGO_HOME=/usr/local/cargo
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs |
    sh -s -- -y --no-modify-path --default-toolchain stable --component rust-analyzer
chown -R 1000 "$RUSTUP_HOME" "$CARGO_HOME"

# pam_env applies /etc/environment to every SSH session, interactive or not.
# Guest config later prepends the guest user's own bin dirs to the same PATH.
touch /etc/environment
for var in RUSTUP_HOME CARGO_HOME; do
    sed -i "/^${var}=/d" /etc/environment
    echo "${var}=${!var}" >>/etc/environment
done
if ! grep -q '^PATH=.*/usr/local/cargo/bin' /etc/environment; then
    if grep -q '^PATH="' /etc/environment; then
        sed -i 's|^PATH="|PATH="/usr/local/cargo/bin:|' /etc/environment
    else
        echo 'PATH="/usr/local/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:/usr/games:/usr/local/games"' >>/etc/environment
    fi
fi
