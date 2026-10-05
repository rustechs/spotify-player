#!/usr/bin/env bash
# Local lint gate matching .github/workflows/ci.yml (fmt + clippy).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${ROOT}"

FEATURES="${RUST_FEATURES:-rodio-backend,media-control,system-audio-visualization,image,notify,fzf}"

cargo fmt --all -- --check
cargo clippy --no-default-features --features "${FEATURES}" -- -D warnings
cargo clippy --no-default-features -- -D warnings
if [[ "$(uname -s)" == "Linux" ]]; then
  # `daemonize` is Unix-only; lint the daemon/streaming paths where they build.
  cargo clippy --no-default-features --features "${FEATURES},daemon" -- -D warnings
fi

echo "lint ok (fmt + clippy with/without features)"
