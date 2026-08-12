#!/usr/bin/env bash
set -euo pipefail

: "${FORGE_REVISION:?FORGE_REVISION is required}"
: "${FORGE_VERSION:?FORGE_VERSION is required}"
: "${ARM64_SHA256:?ARM64_SHA256 is required}"
: "${X86_64_SHA256:?X86_64_SHA256 is required}"
: "${FORMULA_PATH:?FORMULA_PATH is required}"

short_revision="${FORGE_REVISION:0:7}"

cat >"${FORMULA_PATH}" <<FORMULA
class Forge < Formula
  desc "Native terminal, editor, Git, and coding-agent workspace"
  homepage "https://github.com/RyanStoffel/forge"
  version "${FORGE_VERSION}-edge.${short_revision}"
  license :cannot_represent

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/RyanStoffel/forge/releases/download/edge/forge-aarch64-apple-darwin"
      sha256 "${ARM64_SHA256}"
    else
      url "https://github.com/RyanStoffel/forge/releases/download/edge/forge-x86_64-apple-darwin"
      sha256 "${X86_64_SHA256}"
    end
  end

  def install
    artifact = Dir["forge-*-apple-darwin"].first
    odie "Forge release artifact is missing" unless artifact

    bin.install artifact => "forge"
  end

  test do
    assert_match version.major_minor_patch.to_s, shell_output("#{bin}/forge --version")
  end
end
FORMULA
