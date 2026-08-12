#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
temporary="$(mktemp)"
trap 'rm -f "${temporary}"' EXIT

FORGE_REVISION="abcdef1234567890" \
FORGE_VERSION="1.2.3" \
ARM64_SHA256="aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" \
X86_64_SHA256="bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb" \
FORMULA_PATH="${temporary}" \
  bash "${script_dir}/update-homebrew-formula.sh"

ruby -c "${temporary}" >/dev/null
grep -F 'version "1.2.3-edge.abcdef1"' "${temporary}" >/dev/null
grep -F 'sha256 "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"' "${temporary}" >/dev/null
grep -F 'sha256 "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"' "${temporary}" >/dev/null
