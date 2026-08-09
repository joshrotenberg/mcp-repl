#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_root"

for tool in cargo vhs jq; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "error: $tool is required to regenerate the README recordings" >&2
    exit 1
  fi
done

cargo build --release

for tape in hero describe elicitation scripting; do
  echo "recording docs/tapes/$tape.tape"
  vhs "docs/tapes/$tape.tape"
done
