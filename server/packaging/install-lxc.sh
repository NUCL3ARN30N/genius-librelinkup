#!/bin/sh
# Installs bllu-server as a systemd service, e.g. in a Debian/Ubuntu LXC container (Proxmox).
#   sh install-lxc.sh                 # downloads the latest release binary
#   sh install-lxc.sh ./bllu-server   # or installs a local binary
set -eu

REPO="NUCL3ARN30N/better-librelinkup"
BIN=/usr/local/bin/bllu-server
HERE=$(cd "$(dirname "$0")" && pwd)

[ "$(id -u)" -eq 0 ] || { echo "run as root"; exit 1; }

if [ "${1:-}" != "" ]; then
    install -m 0755 "$1" "$BIN"
else
    command -v curl >/dev/null || { apt-get update && apt-get install -y curl ca-certificates; }
    url=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
        | grep -o '"browser_download_url": *"[^"]*bllu-server-[^"]*-linux-x64"' | head -n1 | cut -d'"' -f4)
    [ -n "$url" ] || { echo "no bllu-server binary found in the latest release"; exit 1; }
    sums=$(dirname "$url")/SHA256SUMS.txt
    tmp=$(mktemp -d)
    curl -fsSL -o "$tmp/bllu-server" "$url"
    curl -fsSL -o "$tmp/SHA256SUMS.txt" "$sums"
    expected=$(grep " $(basename "$url")\$" "$tmp/SHA256SUMS.txt" | cut -d' ' -f1)
    actual=$(sha256sum "$tmp/bllu-server" | cut -d' ' -f1)
    [ "$expected" = "$actual" ] || { echo "checksum mismatch – aborting"; exit 1; }
    install -m 0755 "$tmp/bllu-server" "$BIN"
    rm -rf "$tmp"
fi

id bllu >/dev/null 2>&1 || useradd --system --home-dir /var/lib/bllu-server --shell /usr/sbin/nologin bllu
if [ ! -f /etc/bllu-server.env ]; then
    install -m 0600 "$HERE/bllu-server.env.example" /etc/bllu-server.env
    echo "→ edit /etc/bllu-server.env (LibreLinkUp login, public URL), then: systemctl restart bllu-server"
fi
install -m 0644 "$HERE/bllu-server.service" /etc/systemd/system/bllu-server.service
systemctl daemon-reload
systemctl enable --now bllu-server
cat > /usr/local/bin/bllu-pair <<'EOF'
#!/bin/sh
# bllu-pair [pair|clients|revoke ID|fingerprint] – runs bllu-server with the service's settings
set -a
. /etc/bllu-server.env
set +a
export BLLU_DATA_DIR=/var/lib/bllu-server
[ "$#" -gt 0 ] || set -- pair
exec /usr/local/bin/bllu-server "$@"
EOF
chmod 0755 /usr/local/bin/bllu-pair
echo
echo "Create a pairing code with:  bllu-pair"
echo "List / revoke clients with:  bllu-pair clients  |  bllu-pair revoke <id>"
echo "(a code is also printed to the log on the first start: journalctl -u bllu-server)"
