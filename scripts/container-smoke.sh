#!/bin/sh
# Smoke-test an mcp-repl container image: it must report the expected version
# and answer a request against the bundled demo server, which needs no network.
#
# usage: scripts/container-smoke.sh <image> <expected-version>
set -eu

image=$1
expected=$2

version=$(docker run --rm "$image" --version)
if [ "$version" != "mcp-repl $expected" ]; then
  echo "version check failed for $image: got '$version', want 'mcp-repl $expected'" >&2
  exit 1
fi

reply=$(docker run --rm "$image" --demo --json -e 'echo message=smoke-ok')
case "$reply" in
  *'"text":"smoke-ok"'*) ;;
  *)
    echo "demo check failed for $image: got '$reply'" >&2
    exit 1
    ;;
esac

echo "$image: $version, demo request answered"
