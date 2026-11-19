#!/bin/bash
# Build a native Russenger.app with Rust, Go and the macOS command-line tools.
set -euo pipefail

root="$(cd "$(dirname "$0")" && pwd)"
dist="$root/dist"
mkdir -p "$dist" "$root/bin"
stage="$(mktemp -d "$dist/.russenger-build.XXXXXX")"
trap 'rm -rf "$stage"' EXIT

(
    cd "$root/relay-helper"
    CGO_ENABLED=0 go build -ldflags '-s -w' -o "$root/bin/relay-helper" .
)
cargo build --locked --release --manifest-path "$root/relay-win/Cargo.toml" \
    --target-dir "$root/relay-win/target" --bin russenger --bin russenger-login --bin russenger-player

app="$stage/Russenger.app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
for binary in russenger russenger-login russenger-player; do
    install -m 755 "$root/relay-win/target/release/$binary" "$app/Contents/MacOS/"
done
install -m 755 "$root/bin/relay-helper" "$app/Contents/MacOS/"
cp "$root/LICENSE" "$root/THIRD-PARTY-NOTICES.md" "$app/Contents/Resources/"

iconset="$stage/Russenger.iconset"
mkdir "$iconset"
for size in 16 32 128 256 512; do
    sips -z "$size" "$size" "$root/assets/icon-512.png" --out "$iconset/icon_${size}x${size}.png" >/dev/null
    if [ "$size" -lt 512 ]; then
        retina="$((size * 2))"
        sips -z "$retina" "$retina" "$root/assets/icon-512.png" --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
    fi
done
iconutil --convert icns "$iconset" --output "$app/Contents/Resources/Russenger.icns"

version="$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$root/relay-win/Cargo.toml" | head -n 1)"
cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
    <key>CFBundleDisplayName</key><string>Russenger</string>
    <key>CFBundleName</key><string>Russenger</string>
    <key>CFBundleIdentifier</key><string>io.github.ducvd89.Russenger</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleExecutable</key><string>russenger</string>
    <key>CFBundleIconFile</key><string>Russenger.icns</string>
    <key>CFBundleShortVersionString</key><string>$version</string>
    <key>CFBundleVersion</key><string>$version</string>
    <key>NSHighResolutionCapable</key><true/>
</dict></plist>
PLIST

plutil -lint "$app/Contents/Info.plist"
for binary in "$app/Contents/MacOS/"*; do
    codesign --force --sign - "$binary"
done
codesign --force --sign - "$app"
codesign --verify --deep --strict "$app"

destination="$dist/Russenger.app"
if [ -d "$destination" ]; then
    mv "$destination" "$stage/previous.app"
fi
mv "$app" "$destination"
printf 'Built %s\n' "$destination"
