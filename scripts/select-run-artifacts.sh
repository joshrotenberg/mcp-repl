#!/usr/bin/env bash
# Merge the newest complete producer artifacts available at or before a
# workflow run attempt. Partial reruns can retain successful producers from an
# earlier attempt. actions/download-artifact v5+ creates named directories when
# a pattern matches multiple artifacts, but flattens a sole match directly into
# the requested path. Accept that flat layout only when the caller requested one
# logical artifact, without ever mixing two versions of one logical artifact.
set -euo pipefail

if [[ $# -lt 4 ]]; then
  echo "usage: $0 <current-attempt> <download-dir> <output-dir> <artifact-base>..." >&2
  exit 2
fi

current_attempt=$1
download_dir=$2
output_dir=$3
shift 3
bases=("$@")

fail() {
  echo "run-artifacts: $*" >&2
  exit 1
}

if [[ ! "$current_attempt" =~ ^[1-9][0-9]*$ ||
      ! -d "$download_dir" || -L "$download_dir" ||
      ! -d "$output_dir" || -L "$output_dir" ]]; then
  echo "run-artifacts: attempt and directory arguments are invalid" >&2
  exit 2
fi
download_dir=$(cd "$download_dir" && pwd -P)
output_dir=$(cd "$output_dir" && pwd -P)
if [[ "$download_dir" == "$output_dir" ]]; then
  echo "run-artifacts: download and output directories must be distinct" >&2
  exit 2
fi

best_attempts=()
best_paths=()
for base in "${bases[@]}"; do
  if [[ ! "$base" =~ ^[A-Za-z0-9][A-Za-z0-9._-]*$ ]]; then
    echo "run-artifacts: unsafe artifact base: $base" >&2
    exit 2
  fi
  best_attempts+=(0)
  best_paths+=("")
done
if [[ ${#bases[@]} -ne $(printf '%s\n' "${bases[@]}" | LC_ALL=C sort -u | wc -l | tr -d '[:space:]') ]]; then
  echo "run-artifacts: artifact bases must be unique" >&2
  exit 2
fi

layout=
while IFS= read -r -d '' entry; do
  name=${entry##*/}
  if [[ -L "$entry" ]]; then
    fail "download directory contains an unsafe entry: $name"
  elif [[ -d "$entry" ]]; then
    entry_layout=directories
  elif [[ -f "$entry" ]]; then
    entry_layout=files
    if [[ ! "$name" =~ ^[A-Za-z0-9][A-Za-z0-9._+-]*$ || ! -s "$entry" ]]; then
      fail "flattened artifact contains an unsafe or empty file: $name"
    fi
  else
    fail "download directory contains an unsafe entry: $name"
  fi
  if [[ -n "$layout" && "$layout" != "$entry_layout" ]]; then
    fail "download directory mixes flattened files and artifact directories"
  fi
  layout=$entry_layout
done < <(find "$download_dir" -mindepth 1 -maxdepth 1 -print0)

if [[ "$layout" == files ]]; then
  if [[ ${#bases[@]} -ne 1 ]]; then
    fail "a flattened single artifact cannot satisfy multiple artifact bases"
  fi
  best_paths[0]=$download_dir
else
  while IFS= read -r -d '' entry; do
    name=${entry##*/}
    if [[ ! -d "$entry" || -L "$entry" ||
          ! "$name" =~ ^[A-Za-z0-9][A-Za-z0-9._-]*$ ]]; then
      fail "download directory contains an unsafe entry: $name"
    fi
    matched_index=-1
    matched_attempt=
    for index in "${!bases[@]}"; do
      prefix=${bases[$index]}-
      if [[ "$name" == "$prefix"* ]]; then
        suffix=${name#"$prefix"}
        if [[ "$suffix" =~ ^[1-9][0-9]*$ ]]; then
          if [[ $matched_index -ne -1 ]]; then
            fail "artifact directory matches more than one base: $name"
          fi
          matched_index=$index
          matched_attempt=$suffix
        fi
      fi
    done
    if [[ $matched_index -eq -1 ]]; then
      fail "unexpected artifact directory: $name"
    fi
    if ((matched_attempt > current_attempt)); then
      fail "artifact $name is from future attempt $matched_attempt"
    fi
    if ((matched_attempt > best_attempts[matched_index])); then
      best_attempts[matched_index]=$matched_attempt
      best_paths[matched_index]=$entry
    fi
  done < <(find "$download_dir" -mindepth 1 -maxdepth 1 -print0)
fi

if find "$output_dir" -mindepth 1 -maxdepth 1 -print -quit | grep -q .; then
  fail "output directory must be empty"
fi

for index in "${!bases[@]}"; do
  selected=${best_paths[$index]}
  if [[ -z "$selected" ]]; then
    fail "no artifact is available for ${bases[$index]} at attempt $current_attempt"
  fi
  file_count=0
  while IFS= read -r -d '' source; do
    name=${source##*/}
    if [[ ! "$name" =~ ^[A-Za-z0-9][A-Za-z0-9._+-]*$ ||
          ! -f "$source" || -L "$source" || ! -s "$source" ]]; then
      fail "${selected##*/} contains an unsafe, linked, empty, or non-file entry: $name"
    fi
    destination="$output_dir/$name"
    if [[ -e "$destination" || -L "$destination" ]]; then
      fail "selected artifacts collide on output file: $name"
    fi
    install -m 0644 "$source" "$destination"
    file_count=$((file_count + 1))
  done < <(find "$selected" -mindepth 1 -maxdepth 1 -print0)
  if [[ $file_count -eq 0 ]]; then
    fail "selected artifact ${selected##*/} is empty"
  fi
  if [[ "$selected" == "$download_dir" ]]; then
    echo "Selected sole flattened artifact for ${bases[$index]}"
  else
    echo "Selected ${selected##*/}"
  fi
done
