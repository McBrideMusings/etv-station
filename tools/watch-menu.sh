#!/usr/bin/env bash
# Interactive cascade behind `admin watch`: environment -> channel -> mode,
# then hand off to tools/overlay-watch.sh or tools/watch-live.sh.
#
# The channel list is built by walking deploy/appdata/channels/*/ (prod) or
# globbing examples/channels & examples/samples (dev), so a new channel just
# shows up next run — nothing here names one by hand. This used to be an
# inline kind="python" action using admin_lib's pick_target; the tool retired
# that kind, so the cascade lives here and the action is interactive-shell
# (the kind whose job is reading the user's keystrokes).
#
# dev "live" opens the whole lineup (examples/ has no cheap way to learn one
# channel's number without a full, env-complete config load) rather than one
# channel's stream, which prod's directory-numbered layout allows.
set -uo pipefail

# pick <prompt> <name...> — numbered menu on stderr, chosen name on stdout.
pick() {
  local prompt=$1
  shift
  local -a opts=("$@")
  [[ ${#opts[@]} -gt 0 ]] || return 1
  echo "$prompt" >&2
  local i
  for i in "${!opts[@]}"; do
    printf '  %2d) %s\n' "$((i + 1))" "${opts[$i]}" >&2
  done
  local choice
  read -rp "> " choice || return 1
  [[ $choice =~ ^[0-9]+$ ]] && ((choice >= 1 && choice <= ${#opts[@]})) || return 1
  printf '%s\n' "${opts[$((choice - 1))]}"
}

env_choice=$(pick "Watch which environment?" \
  "prod — deploy/appdata, the real deployed config" \
  "dev — examples/, local dev stack") || exit 0
env_choice=${env_choice%% *}

declare -a names=()
declare -A dev_paths=()
if [[ $env_choice == prod ]]; then
  while IFS= read -r d; do names+=("$(basename "$d")"); done \
    < <(find deploy/appdata/channels -mindepth 1 -maxdepth 1 -type d 2>/dev/null | sort)
else
  for p in examples/channels/*.yaml examples/samples/*.yaml; do
    [[ -e $p ]] || continue
    n=$(basename "$p" .yaml)
    names+=("$n")
    dev_paths[$n]=$p
  done
fi
if [[ ${#names[@]} -eq 0 ]]; then
  echo "watch: no channels found for $env_choice" >&2
  exit 1
fi

channel=$(pick "Watch which channel? ($env_choice)" "${names[@]}") || exit 0

mode=$(pick "Watch how?" \
  "overlay — isolated preview, background fixture, hot-reloads on save" \
  "live — the real stream") || exit 0
mode=${mode%% *}

if [[ $mode == overlay ]]; then
  if [[ $env_choice == prod ]]; then
    exec ./tools/overlay-watch.sh "$channel"
  fi
  exec ./tools/overlay-watch.sh "${dev_paths[$channel]}"
elif [[ $env_choice == prod ]]; then
  : "${PROD_URL:?PROD_URL is unset — it names the deployed station and lives in .env}"
  n=$((10#${channel%%-*}))
  exec ./tools/watch-live.sh "$PROD_URL/channel/$n.m3u8"
else
  exec ./tools/watch-live.sh "http://127.0.0.1:${ETV_PORT:-8409}/channels.m3u"
fi
