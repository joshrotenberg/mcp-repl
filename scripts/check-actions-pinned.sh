#!/usr/bin/env bash
# Refuse mutable external Action refs before repository policy does.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
failures=0

while IFS= read -r entry; do
  spec=${entry#*uses:}
  spec=${spec#"${spec%%[![:space:]]*}"}
  spec=${spec%%[[:space:]#]*}
  case "$spec" in
    ./*) continue ;;
  esac

  ref=${spec##*@}
  if [[ ! "$ref" =~ ^[0-9a-f]{40}$ ]]; then
    echo "External Action is not pinned to a full commit SHA: $entry" >&2
    failures=$((failures + 1))
  fi
done < <(grep -RHnE '^[[:space:]]*(-[[:space:]]+)?uses:[[:space:]]+' \
  "$root/.github/workflows" --include='*.yml' --include='*.yaml')

if [[ "$failures" -ne 0 ]]; then
  exit 1
fi
echo "all external Actions are pinned to full commit SHAs"
