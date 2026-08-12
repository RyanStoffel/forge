#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
temporary="$(mktemp -d)"
trap 'rm -rf "${temporary}"' EXIT

cat >"${temporary}/forge" <<'BINARY'
#!/usr/bin/env bash
echo "forge 1.2.3 (abcdef1234567890)"
BINARY
chmod 755 "${temporary}/forge"

FORGE_BINARY="${temporary}/forge" \
FORGE_VERSION="1.2.3" \
FORGE_REVISION="abcdef1234567890" \
OUTPUT_DIR="${temporary}/bundle" \
  bash "${script_dir}/package-macos-app.sh"

plutil -lint "${temporary}/bundle/Forge.app/Contents/Info.plist" >/dev/null
codesign --verify --deep --strict "${temporary}/bundle/Forge.app"
test "$(defaults read "${temporary}/bundle/Forge.app/Contents/Info" CFBundleIdentifier)" = "dev.ryanstoffel.forge"
test "$("${temporary}/bundle/Forge.app/Contents/MacOS/forge")" = "forge 1.2.3 (abcdef1234567890)"

ARM64_SHA256="aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" \
X86_64_SHA256="bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb" \
FORGE_VERSION="1.2.3" \
FORGE_REVISION="abcdef1234567890" \
CASK_PATH="${temporary}/forge.rb" \
  bash "${script_dir}/update-homebrew-cask.sh"

ruby -c "${temporary}/forge.rb" >/dev/null
grep -F 'cask "forge-app"' "${temporary}/forge.rb" >/dev/null
grep -F 'version "1.2.3-edge.abcdef1"' "${temporary}/forge.rb" >/dev/null
grep -F 'releases/download/forge-app-#{version}/Forge-aarch64-apple-darwin.zip' "${temporary}/forge.rb" >/dev/null
grep -F 'app "Forge.app"' "${temporary}/forge.rb" >/dev/null
