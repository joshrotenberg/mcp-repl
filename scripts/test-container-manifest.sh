#!/usr/bin/env bash
# Exercise immutable version-tag and latest-tag container publication behavior.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
publish="$root/scripts/publish-container-manifest.sh"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT INT TERM
mkdir -p "$work/bin"

export TEST_TAG=v1.2.3
export TEST_NEXT_TAG=v1.2.4
export TEST_DIGEST_A=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
export TEST_DIGEST_B=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
export TEST_DIGEST_C=cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc
export TEST_DIGEST_D=dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd

cat > "$work/bin/gh" <<'STUB'
#!/bin/sh
set -eu

[ "${1:-}" = api ] || {
  echo "unexpected gh command: $*" >&2
  exit 1
}
endpoint=${2:-}
mode=${GH_MODE:-valid}
state=${TEST_STATE:?}

release_json() {
  tag=$1
  immutable=true
  author='github-actions[bot]'
  author_type=Bot
  name=$tag
  case "$2" in
    mutable) immutable=false ;;
    foreign) author=octocat; author_type=User ;;
    wrong_name) name=wrong ;;
  esac
  printf '{"id":42,"tag_name":"%s","name":"%s","draft":false,"prerelease":false,"immutable":%s,"author":{"login":"%s","type":"%s"}}\n' \
    "$tag" "$name" "$immutable" "$author" "$author_type"
}

case "$endpoint" in
  "repos/test/project/releases/tags/$TEST_TAG")
    case "$mode" in
      current_mutable) release_json "$TEST_TAG" mutable ;;
      current_foreign) release_json "$TEST_TAG" foreign ;;
      current_wrong_name) release_json "$TEST_TAG" wrong_name ;;
      current_wrong_tag) release_json v9.9.9 valid ;;
      current_malformed) printf '{}\n' ;;
      *) release_json "$TEST_TAG" valid ;;
    esac
    ;;
  repos/test/project/releases/latest)
    count_file="$state/latest_calls"
    count=0
    [ ! -f "$count_file" ] || count=$(cat "$count_file")
    count=$((count + 1))
    printf '%s\n' "$count" > "$count_file"
    latest=$TEST_TAG
    latest_state=valid
    case "$mode" in
      newer) latest=$TEST_NEXT_TAG ;;
      race)
        [ "$count" -le 2 ] || latest=$TEST_NEXT_TAG
        ;;
      latest_mutable) latest_state=mutable ;;
      latest_foreign) latest_state=foreign ;;
      latest_wrong_name) latest_state=wrong_name ;;
      latest_malformed) printf '{}\n'; exit 0 ;;
    esac
    release_json "$latest" "$latest_state"
    ;;
  *)
    echo "unexpected API endpoint: $endpoint" >&2
    exit 1
    ;;
esac
STUB

cat > "$work/bin/docker" <<'STUB'
#!/bin/sh
set -eu

state=${TEST_STATE:?}
mode=${DOCKER_MODE:-valid}
image=ghcr.io/test/project

manifest() {
  printf '{"schemaVersion":2,"manifests":[{"digest":"sha256:%s","platform":{"os":"linux","architecture":"amd64"}},{"digest":"sha256:%s","platform":{"os":"linux","architecture":"arm64"}}]}\n' "$1" "$2"
}

[ "${1:-}" = buildx ] && [ "${2:-}" = imagetools ] || {
  echo "unexpected docker command: $*" >&2
  exit 1
}
operation=${3:-}
case "$operation" in
  inspect)
    reference=${5:-}
    case "$reference" in
      "$image:1.2.3")
        case "$mode" in
          inspect_error)
            echo "registry connection reset" >&2
            exit 1
            ;;
          ambiguous_not_found)
            echo "proxy route not found" >&2
            exit 1
            ;;
          existing_bad)
            manifest "$TEST_DIGEST_A" "$TEST_DIGEST_C"
            ;;
          existing)
            manifest "$TEST_DIGEST_A" "$TEST_DIGEST_B"
            ;;
          *)
            if [ -f "$state/version_created" ]; then
              if [ "$mode" = new_bad ]; then
                manifest "$TEST_DIGEST_A" "$TEST_DIGEST_C"
              else
                manifest "$TEST_DIGEST_A" "$TEST_DIGEST_B"
              fi
            else
              echo "ERROR: $reference: not found" >&2
              exit 1
            fi
            ;;
        esac
        ;;
      "$image:1.2.4")
        if [ "$mode" = newer_missing ]; then
          echo "manifest unknown: not found" >&2
          exit 1
        fi
        manifest "$TEST_DIGEST_C" "$TEST_DIGEST_D"
        ;;
      "$image:latest")
        [ -f "$state/latest_source" ] || {
          echo "manifest unknown: not found" >&2
          exit 1
        }
        if [ "$mode" = latest_bad ]; then
          manifest "$TEST_DIGEST_A" "$TEST_DIGEST_D"
        elif [ "$(cat "$state/latest_source")" = "$image:1.2.4" ]; then
          manifest "$TEST_DIGEST_C" "$TEST_DIGEST_D"
        else
          manifest "$TEST_DIGEST_A" "$TEST_DIGEST_B"
        fi
        ;;
      *)
        echo "unexpected inspect reference: $reference" >&2
        exit 1
        ;;
    esac
    ;;
  create)
    printf '%s\n' "$*" >> "$state/docker.log"
    shift 3
    [ "${1:-}" = -t ] || {
      echo "missing manifest target" >&2
      exit 1
    }
    target=${2:-}
    shift 2
    case "$target" in
      "$image:1.2.3")
        [ "$mode" != create_version_fail ] || {
          echo "version create failed" >&2
          exit 1
        }
        [ "$#" -eq 2 ] || {
          echo "wrong architecture reference count" >&2
          exit 1
        }
        printf '%s\n' "$@" > "$state/version_refs"
        : > "$state/version_created"
        ;;
      "$image:latest")
        [ "$mode" != create_latest_fail ] || {
          echo "latest create failed" >&2
          exit 1
        }
        [ "$#" -eq 1 ] || {
          echo "wrong latest source count" >&2
          exit 1
        }
        printf '%s\n' "$1" > "$state/latest_source"
        ;;
      *)
        echo "unexpected create target: $target" >&2
        exit 1
        ;;
    esac
    ;;
  *)
    echo "unexpected imagetools operation: $operation" >&2
    exit 1
    ;;
esac
STUB

chmod +x "$work/bin/gh" "$work/bin/docker"
export PATH="$work/bin:$PATH"
export GH_REPO=test/project

setup_case() {
  local name=$1
  TEST_STATE="$work/$name"
  TEST_DIGEST_DIR="$TEST_STATE/digests"
  export TEST_STATE TEST_DIGEST_DIR
  mkdir -p "$TEST_DIGEST_DIR"
  : > "$TEST_STATE/docker.log"
  : > "$TEST_DIGEST_DIR/$TEST_DIGEST_A"
  : > "$TEST_DIGEST_DIR/$TEST_DIGEST_B"
  unset GH_MODE DOCKER_MODE
}

expect_failure() {
  local label=$1
  if "$publish" "$TEST_TAG" "$TEST_DIGEST_DIR" \
      > "$TEST_STATE/stdout" 2> "$TEST_STATE/stderr"; then
    echo "$label unexpectedly succeeded" >&2
    exit 1
  fi
}

setup_case fresh
"$publish" "$TEST_TAG" "$TEST_DIGEST_DIR"
grep -Fq "create -t ghcr.io/test/project:1.2.3" "$TEST_STATE/docker.log"
grep -Fxq "ghcr.io/test/project@sha256:$TEST_DIGEST_A" "$TEST_STATE/version_refs"
grep -Fxq "ghcr.io/test/project@sha256:$TEST_DIGEST_B" "$TEST_STATE/version_refs"
grep -Fxq 'ghcr.io/test/project:1.2.3' "$TEST_STATE/latest_source"

setup_case existing
DOCKER_MODE=existing
export DOCKER_MODE
"$publish" "$TEST_TAG" "$TEST_DIGEST_DIR"
if grep -Fq "create -t ghcr.io/test/project:1.2.3" "$TEST_STATE/docker.log"; then
  echo "An exact existing version manifest was replaced" >&2
  exit 1
fi

setup_case existing_bad
DOCKER_MODE=existing_bad
export DOCKER_MODE
expect_failure "mismatched existing version manifest"
if [[ -s "$TEST_STATE/docker.log" ]]; then
  echo "A mismatched existing version manifest triggered a registry write" >&2
  exit 1
fi

setup_case one_digest
rm "$TEST_DIGEST_DIR/$TEST_DIGEST_B"
expect_failure "one digest"

setup_case invalid_digest
mv "$TEST_DIGEST_DIR/$TEST_DIGEST_B" "$TEST_DIGEST_DIR/not-a-digest"
expect_failure "invalid digest"

setup_case hidden_extra
: > "$TEST_DIGEST_DIR/.unexpected"
expect_failure "hidden extra digest artifact"

setup_case symlink_digest
rm "$TEST_DIGEST_DIR/$TEST_DIGEST_B"
ln -s "$TEST_DIGEST_A" "$TEST_DIGEST_DIR/$TEST_DIGEST_B"
expect_failure "symlink digest artifact"

setup_case newer
GH_MODE=newer
export GH_MODE
"$publish" "$TEST_TAG" "$TEST_DIGEST_DIR"
grep -Fxq 'ghcr.io/test/project:1.2.4' "$TEST_STATE/latest_source"
if grep -Fq 'create -t ghcr.io/test/project:latest ghcr.io/test/project:1.2.3' \
    "$TEST_STATE/docker.log"; then
  echo "An older rerun rolled latest back" >&2
  exit 1
fi

setup_case race
GH_MODE=race
export GH_MODE
"$publish" "$TEST_TAG" "$TEST_DIGEST_DIR"
grep -Fxq 'ghcr.io/test/project:1.2.4' "$TEST_STATE/latest_source"
grep -Fq 'create -t ghcr.io/test/project:latest ghcr.io/test/project:1.2.3' \
  "$TEST_STATE/docker.log"
grep -Fq 'create -t ghcr.io/test/project:latest ghcr.io/test/project:1.2.4' \
  "$TEST_STATE/docker.log"

for mode in current_mutable current_foreign current_wrong_name \
  current_wrong_tag current_malformed; do
  setup_case "$mode"
  GH_MODE=$mode
  export GH_MODE
  expect_failure "$mode current release"
  if [[ -s "$TEST_STATE/docker.log" ]]; then
    echo "$mode current release triggered a registry write" >&2
    exit 1
  fi
done

for mode in latest_mutable latest_foreign latest_wrong_name latest_malformed; do
  setup_case "$mode"
  GH_MODE=$mode
  export GH_MODE
  expect_failure "$mode latest release"
  if grep -Fq 'create -t ghcr.io/test/project:latest' "$TEST_STATE/docker.log"; then
    echo "$mode latest release changed latest" >&2
    exit 1
  fi
done

setup_case inspect_error
DOCKER_MODE=inspect_error
export DOCKER_MODE
expect_failure "registry inspection error"
if [[ -s "$TEST_STATE/docker.log" ]]; then
  echo "A registry inspection error triggered a registry write" >&2
  exit 1
fi

setup_case ambiguous_not_found
DOCKER_MODE=ambiguous_not_found
export DOCKER_MODE
expect_failure "ambiguous not-found inspection error"
if [[ -s "$TEST_STATE/docker.log" ]]; then
  echo "An ambiguous not-found error triggered a registry write" >&2
  exit 1
fi

setup_case bad_new_version
DOCKER_MODE=new_bad
export DOCKER_MODE
expect_failure "bad newly created version manifest"
if grep -Fq 'create -t ghcr.io/test/project:latest' "$TEST_STATE/docker.log"; then
  echo "A bad new version manifest changed latest" >&2
  exit 1
fi

setup_case missing_newer
GH_MODE=newer
DOCKER_MODE=newer_missing
export GH_MODE DOCKER_MODE
expect_failure "missing current latest version image"
if grep -Fq 'create -t ghcr.io/test/project:latest' "$TEST_STATE/docker.log"; then
  echo "A missing latest version image changed latest" >&2
  exit 1
fi

setup_case create_version_failure
DOCKER_MODE=create_version_fail
export DOCKER_MODE
expect_failure "version publication failure"

setup_case create_latest_failure
DOCKER_MODE=create_latest_fail
export DOCKER_MODE
expect_failure "latest reconciliation failure"

setup_case bad_latest_result
DOCKER_MODE=latest_bad
export DOCKER_MODE
expect_failure "bad latest publication result"

echo "container manifest behavior tests passed"
