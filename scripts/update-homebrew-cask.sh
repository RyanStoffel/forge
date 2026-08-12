#!/usr/bin/env bash
set -euo pipefail

: "${FORGE_REVISION:?FORGE_REVISION is required}"
: "${FORGE_VERSION:?FORGE_VERSION is required}"
: "${ARM64_SHA256:?ARM64_SHA256 is required}"
: "${X86_64_SHA256:?X86_64_SHA256 is required}"
: "${CASK_PATH:?CASK_PATH is required}"

short_revision="${FORGE_REVISION:0:7}"

cat >"${CASK_PATH}" <<CASK
cask "forge-app" do
  version "${FORGE_VERSION},${short_revision}"

  on_arm do
    sha256 "${ARM64_SHA256}"
    url "https://github.com/RyanStoffel/forge/releases/download/edge-#{version.csv.second}/Forge-aarch64-apple-darwin.zip"
  end
  on_intel do
    sha256 "${X86_64_SHA256}"
    url "https://github.com/RyanStoffel/forge/releases/download/edge-#{version.csv.second}/Forge-x86_64-apple-darwin.zip"
  end

  name "Forge"
  desc "Native terminal, editor, Git, and coding-agent workspace"
  homepage "https://github.com/RyanStoffel/forge"

  app "Forge.app"
  binary "#{appdir}/Forge.app/Contents/MacOS/forge"

  caveats <<~EOS
    Forge is an unsigned edge build. On first launch, macOS may require
    System Settings → Privacy & Security → Open Anyway.
  EOS

  zap trash: [
    "~/Library/Application Support/Forge",
    "~/Library/Preferences/dev.ryanstoffel.forge.plist",
    "~/Library/Saved Application State/dev.ryanstoffel.forge.savedState",
  ]
end
CASK
