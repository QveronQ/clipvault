#!/bin/sh
# Installe clipvault-daemon comme LaunchAgent (équivalent macOS du service
# systemd user) : démarré à l'ouverture de session, relancé s'il meurt.
#
#   ./dist/macos/install-launchagent.sh            # installe et démarre
#   ./dist/macos/install-launchagent.sh --uninstall
set -eu

label=ovh.qdev.clipvault.daemon
plist=$HOME/Library/LaunchAgents/$label.plist
bin=$HOME/.local/bin/clipvault-daemon
log=$HOME/Library/Logs/clipvault-daemon.log

# Signature. Avec une identité Apple, l'exigence retenue par macOS ne porte que
# sur l'identifiant et le certificat — jamais sur le binaire lui-même — si bien
# que l'autorisation « Saisie de contenu » survit aux recompilations. En
# signature ad-hoc elle meurt à chaque déploiement et il faut la redonner.
sign_daemon() {
	identity=$(security find-identity -v -p codesigning 2>/dev/null |
		grep -oE '"(Apple Development|Developer ID Application): [^"]+"' | head -1 | tr -d '"')
	if [ -n "$identity" ]; then
		codesign --force --sign "$identity" \
			--identifier ovh.qdev.clipvault.daemon --timestamp=none "$1"
		echo "signé: $identity"
	else
		codesign --force --sign - "$1" 2>/dev/null || true
		echo "signé ad-hoc (pas d'identité Apple) — l'autorisation sera à redonner" >&2
	fi
}

if [ "${1:-}" = "--uninstall" ]; then
	launchctl bootout "gui/$(id -u)/$label" 2>/dev/null || true
	rm -f "$plist"
	echo "désinstallé: $plist"
	exit 0
fi

root=$(cd "$(dirname "$0")/../.." && pwd)
cargo build --release --manifest-path "$root/Cargo.toml" -p clipvault-daemon
install -d "$HOME/.local/bin" "$HOME/Library/LaunchAgents" "$HOME/Library/Logs"
install -m 755 "$root/target/release/clipvault-daemon" "$bin"
sign_daemon "$bin"

cat > "$plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>$label</string>
	<key>ProgramArguments</key>
	<array>
		<string>$bin</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<true/>
	<key>ProcessType</key>
	<string>Background</string>
	<key>EnvironmentVariables</key>
	<dict>
		<key>RUST_LOG</key>
		<string>info</string>
	</dict>
	<key>StandardOutPath</key>
	<string>$log</string>
	<key>StandardErrorPath</key>
	<string>$log</string>
</dict>
</plist>
PLIST

# bootout d'abord : recharge propre si le service tournait déjà.
launchctl bootout "gui/$(id -u)/$label" 2>/dev/null || true
launchctl bootstrap "gui/$(id -u)" "$plist"

echo "installé: $plist"
echo "binaire : $bin"
echo "logs    : $log"
echo
echo "Easy-Switch Logitech : si la bascule échoue en 0xE00002E2, autoriser"
echo "$bin dans Réglages > Confidentialité et sécurité > Saisie de contenu."
echo "Signé par une identité Apple, l'autorisation ne se donne qu'une fois ;"
echo "en ad-hoc, elle est à REDONNER après chaque déploiement (retirer puis"
echo "rajouter l'entrée — la décocher/recocher ne suffit pas)."
echo
echo "  launchctl print gui/$(id -u)/$label   # état"
echo "  tail -f $log                          # suivre les logs"
