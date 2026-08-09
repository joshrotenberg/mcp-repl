#!/usr/bin/env bash
set -euo pipefail

coverage_command=(cargo llvm-cov)
rustc_sysroot=$(rustc --print sysroot)
rustc_host=$(rustc -vV | sed -n 's/^host: //p')
llvm_profdata="$rustc_sysroot/lib/rustlib/$rustc_host/bin/llvm-profdata"

# A package-manager Rust can precede rustup on PATH while cargo-llvm-cov still
# asks rustup to install llvm-tools. In that case, compile with rustup's active
# toolchain too so the profiler and compiler come from the same sysroot.
if [[ ! -x "$llvm_profdata" ]] && command -v rustup > /dev/null 2>&1; then
  rustup_toolchain=$(rustup show active-toolchain)
  rustup_toolchain=${rustup_toolchain%% *}
  coverage_command=(rustup run "$rustup_toolchain" cargo llvm-cov)
fi

if ! "${coverage_command[@]}" --version > /dev/null 2>&1; then
  echo "cargo-llvm-cov is required; install it with: cargo install cargo-llvm-cov" >&2
  exit 2
fi

# HTML is useful for finding untested branches locally. Pass
# `--summary-only` for a compact terminal report (and in automation).
if [[ $# -eq 0 ]]; then
  set -- --html
fi

"${coverage_command[@]}" --workspace --all-features "$@"
