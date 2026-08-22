#!/bin/sh
# Construit Clipvault.app à partir du binaire release de l'UI.
#
#   ./dist/macos/make-app.sh              -> ./target/Clipvault.app
#   ./dist/macos/make-app.sh ~/Applications -> ~/Applications/Clipvault.app
#
# Le bundle sert à deux choses : donner le focus clavier au popup (un binaire
# lancé depuis un terminal ne passe pas au premier plan) et le rendre lançable
# par `open -a Clipvault`, donc par n'importe quel raccourci système.
set -eu

root=$(cd "$(dirname "$0")/../.." && pwd)
dest=${1:-$root/target}
app=$dest/Clipvault.app

cargo build --release --manifest-path "$root/Cargo.toml" -p clipvault-ui

rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$root/target/release/clipvault" "$app/Contents/MacOS/clipvault"
cp "$root/dist/macos/Info.plist" "$app/Contents/Info.plist"

# Icône optionnelle : dist/macos/icon.png (1024x1024) -> clipvault.icns
icon=$root/dist/macos/icon.png
if [ -f "$icon" ]; then
	set=$(mktemp -d)/clipvault.iconset
	mkdir -p "$set"
	for size in 16 32 128 256 512; do
		sips -z $size $size "$icon" --out "$set/icon_${size}x${size}.png" >/dev/null
		sips -z $((size * 2)) $((size * 2)) "$icon" \
			--out "$set/icon_${size}x${size}@2x.png" >/dev/null
	done
	iconutil -c icns "$set" -o "$app/Contents/Resources/clipvault.icns"
	rm -rf "$set"
else
	echo "note: pas de dist/macos/icon.png, icône générique" >&2
fi

# Sans signature, macOS retue l'app à chaque reconstruction. Une identité Apple
# donne en plus une identité stable, à laquelle les autorisations accordées
# restent attachées d'une compilation à l'autre.
identity=$(security find-identity -v -p codesigning 2>/dev/null |
	grep -oE '"(Apple Development|Developer ID Application): [^"]+"' | head -1 | tr -d '"')
if [ -n "$identity" ]; then
	codesign --force --sign "$identity" --timestamp=none "$app"
else
	codesign --force --sign - "$app" 2>/dev/null || true
fi

# Le cache de Launch Services garde l'ancien chemin après un déplacement.
touch "$app"

echo "$app"
