#!/bin/sh
# Installs gllu-server as a systemd service, e.g. in a Debian/Ubuntu LXC container (Proxmox).
#   sh install-lxc.sh                 # downloads the latest release binary
#   sh install-lxc.sh ./gllu-server   # or installs a local binary
set -eu

REPO="NUCL3ARN30N/genius-librelinkup"
BIN=/usr/local/bin/gllu-server
HERE=$(cd "$(dirname "$0")" && pwd)

[ "$(id -u)" -eq 0 ] || { echo "run as root"; exit 1; }

if [ "${1:-}" != "" ]; then
    install -m 0755 "$1" "$BIN"
else
    command -v curl >/dev/null || { apt-get update && apt-get install -y curl ca-certificates; }
    url=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
        | grep -o '"browser_download_url": *"[^"]*gllu-server-[^"]*-linux-x64"' | head -n1 | cut -d'"' -f4)
    [ -n "$url" ] || { echo "no gllu-server binary found in the latest release"; exit 1; }
    sums=$(dirname "$url")/SHA256SUMS.txt
    tmp=$(mktemp -d)
    curl -fsSL -o "$tmp/gllu-server" "$url"
    curl -fsSL -o "$tmp/SHA256SUMS.txt" "$sums"
    expected=$(grep " $(basename "$url")\$" "$tmp/SHA256SUMS.txt" | cut -d' ' -f1)
    actual=$(sha256sum "$tmp/gllu-server" | cut -d' ' -f1)
    [ "$expected" = "$actual" ] || { echo "checksum mismatch – aborting"; exit 1; }
    install -m 0755 "$tmp/gllu-server" "$BIN"
    rm -rf "$tmp"
fi

id gllu >/dev/null 2>&1 || useradd --system --home-dir /var/lib/gllu-server --shell /usr/sbin/nologin gllu

# upgrade an installation from before the rename (bllu-server, "Better LibreLinkUp")
if [ -f /etc/bllu-server.env ] && [ ! -f /etc/gllu-server.env ]; then
    echo "→ migrating bllu-server to gllu-server"
    systemctl disable --now bllu-server 2>/dev/null || true
    sed 's/^BLLU_/GLLU_/' /etc/bllu-server.env > /etc/gllu-server.env
    chmod 0600 /etc/gllu-server.env
    rm -f /etc/bllu-server.env
    if [ -d /var/lib/bllu-server ] && [ ! -e /var/lib/gllu-server ]; then
        mv /var/lib/bllu-server /var/lib/gllu-server
    fi
    [ -d /var/lib/gllu-server ] && chown -R gllu:gllu /var/lib/gllu-server
    rm -f /etc/systemd/system/bllu-server.service /usr/local/bin/bllu-server /usr/local/bin/bllu-pair
    userdel bllu 2>/dev/null || true
fi
if [ ! -f /etc/gllu-server.env ]; then
    install -m 0600 "$HERE/gllu-server.env.example" /etc/gllu-server.env
    echo "→ edit /etc/gllu-server.env (LibreLinkUp login, public URL), then: systemctl restart gllu-server"
fi
install -m 0644 "$HERE/gllu-server.service" /etc/systemd/system/gllu-server.service
systemctl daemon-reload
systemctl enable --now gllu-server
cat > /usr/local/bin/gllu-pair <<'EOF'
#!/bin/sh
# gllu-pair [pair|clients|revoke ID|fingerprint] – runs gllu-server with the service's settings
set -a
. /etc/gllu-server.env
set +a
export GLLU_DATA_DIR=/var/lib/gllu-server
[ "$#" -gt 0 ] || set -- pair
exec /usr/local/bin/gllu-server "$@"
EOF
chmod 0755 /usr/local/bin/gllu-pair
echo
echo "Create a pairing code with:  gllu-pair"
echo "List / revoke clients with:  gllu-pair clients  |  gllu-pair revoke <id>"
echo "(a code is also printed to the log on the first start: journalctl -u gllu-server)"
