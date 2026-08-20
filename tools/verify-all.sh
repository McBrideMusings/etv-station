#!/usr/bin/env bash
# Run a quality check across BOTH cargo workspaces in this repo.
#
# etv-station is two workspaces, not one:
#   * the parent at the repo root (crates/*, vendor/plexdb-reader)
#   * vendor/etv-next, which has its own [workspace] and is EXCLUDED from the
#     parent — a `cargo <anything> --workspace` at the root never touches it.
#
# Every check lives here exactly once. CLAUDE.md, .claude/skills/verify-project,
# and admin.toml all call this script rather than repeating the command list;
# a check duplicated across files is the seam that let a clippy error sit on
# main for three rounds (issue #305).
#
# The vendored tree's commands come from vendor/etv-next/CLAUDE.md and differ
# from the parent's (--locked, --all-features, --all-targets). Each workspace
# gets its own documented command, not a lowest-common-denominator merge.
#
# Usage:
#   tools/verify-all.sh              # test + lint + fmt-check, both workspaces
#   tools/verify-all.sh test         # tests only
#   tools/verify-all.sh lint         # clippy only
#   tools/verify-all.sh fmt-check    # rustfmt --check only (read-only)
#   tools/verify-all.sh fmt          # rustfmt, WRITES files
#
# Every selected check runs in both workspaces even if an earlier one fails;
# the exit status is non-zero if any of them failed, and the summary at the end
# names which.

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VENDOR="$ROOT/vendor/etv-next"

FAILED=()
PASSED=()

# run <label> <workspace-dir> <command...>
run() {
  local label="$1" dir="$2"
  shift 2
  printf '\n\033[1m==> %s\033[0m\n    (cd %s && %s)\n' "$label" "${dir#"$ROOT"/}" "$*"
  if (cd "$dir" && "$@"); then
    PASSED+=("$label")
  else
    FAILED+=("$label")
    printf '\033[31m!!! FAILED: %s\033[0m\n' "$label"
  fi
}

do_test() {
  run "parent: test" "$ROOT" \
    cargo test --workspace
  run "etv-next: test" "$VENDOR" \
    cargo test --workspace
}

do_lint() {
  run "parent: clippy" "$ROOT" \
    cargo clippy --workspace --all-features --all-targets -- -D clippy::all
  run "etv-next: clippy" "$VENDOR" \
    cargo clippy --locked --workspace --all-features --all-targets -- -D clippy::all
}

do_fmt_check() {
  run "parent: fmt --check" "$ROOT" \
    cargo +nightly fmt --all -- --check
  run "etv-next: fmt --check" "$VENDOR" \
    cargo +nightly fmt --all -- --check
}

do_fmt() {
  run "parent: fmt" "$ROOT" \
    cargo +nightly fmt --all
  run "etv-next: fmt" "$VENDOR" \
    cargo +nightly fmt --all
}

case "${1:-all}" in
  all)       do_test; do_lint; do_fmt_check ;;
  test)      do_test ;;
  lint)      do_lint ;;
  fmt-check) do_fmt_check ;;
  fmt)       do_fmt ;;
  *)
    echo "usage: tools/verify-all.sh [all|test|lint|fmt-check|fmt]" >&2
    exit 2
    ;;
esac

printf '\n\033[1m==> summary\033[0m\n'
for p in ${PASSED+"${PASSED[@]}"}; do printf '  \033[32mok\033[0m    %s\n' "$p"; done
for f in ${FAILED+"${FAILED[@]}"}; do printf '  \033[31mFAIL\033[0m  %s\n' "$f"; done

if [ ${#FAILED[@]} -gt 0 ]; then
  printf '\n%d check(s) failed.\n' "${#FAILED[@]}"
  exit 1
fi
printf '\nAll checks passed in both workspaces.\n'
