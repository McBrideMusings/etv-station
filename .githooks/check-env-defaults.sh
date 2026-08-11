#!/usr/bin/env bash
# Reads a list of repo-relative file paths (one per line) on stdin and
# rejects any of the scanned files that contain a ${VAR:-default} whose
# default looks like a hostname or filesystem path (contains '.' or '/')
# rather than a safe literal like a port number or an empty fallback.
#
# Background: a real private hostname was once baked in as an env-var
# default in a committed config (${ETV_DEV_DOMAIN:-mac.example.com}) —
# the whole point of an env var is to keep the value out of the committed
# file. This is a defensive net against repeating that, not a strict
# validator: false positives on numeric ports, container-internal paths,
# or other genuinely safe defaults are tolerable (issue #227). If a match
# is a reviewed false positive, bypass with `git commit --no-verify`.
#
# Scanned: Dockerfile, docker/**, deploy/**, docs/**, .github/workflows/**,
# and any *.yml/*.yaml/*.toml, wherever tracked. vendor/etv-next/ is
# skipped — it's vendored upstream source absorbed via `git subtree`, not
# this project's own config (see CLAUDE.md "Submodule rules").
#
# Usage:
#   git diff --cached --name-only --diff-filter=ACM | .githooks/check-env-defaults.sh
#   git ls-files | .githooks/check-env-defaults.sh   # full-repo scan (CI)

set -euo pipefail

pattern='\$\{[A-Za-z_][A-Za-z0-9_]*:-[^}]*[./][^}]*\}'
found=0

while IFS= read -r file; do
  [ -n "$file" ] || continue
  case "$file" in
    vendor/etv-next/*) continue ;;
    Dockerfile|docker/*|deploy/*|docs/*|.github/workflows/*|*.yml|*.yaml|*.toml) ;;
    *) continue ;;
  esac
  [ -f "$file" ] || continue

  if matches=$(grep -nE "$pattern" -- "$file"); then
    if [ "$found" -eq 0 ]; then
      echo "etv-station: possible private default committed (an env var with a"
      echo "hostname- or path-shaped fallback, \${VAR:-default}):"
      echo
    fi
    found=1
    echo "  $file"
    echo "$matches" | sed 's/^/    /'
    echo
  fi
done

if [ "$found" -eq 1 ]; then
  cat <<'EOF'
The whole point of an env var is to keep the value out of the committed
file. If a default above is a real hostname, domain, IP, or filesystem
path: move it to .env (gitignored) and reference the var with no
fallback, e.g. ${VAR} instead of ${VAR:-mac.example.com}.

If a default above is genuinely safe (a port number, a container-internal
path, a public literal), this is a known false positive (issue #227) —
rerun the commit with --no-verify.
EOF
  exit 1
fi

exit 0
