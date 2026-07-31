#!/usr/bin/env bash
set -euo pipefail

BIN="${EASYDL_BIN:-target/release/easydl}"
MODE="${EASYDL_SMOKE_MODE:-inspect}"
KEEP="${EASYDL_SMOKE_KEEP:-0}"

case "$MODE" in
  inspect|plan|download) ;;
  *) echo "EASYDL_SMOKE_MODE must be inspect, plan, or download" >&2; exit 2 ;;
esac

if [[ ! -x "$BIN" ]]; then
  echo "Smoke binary not found; building locked release binary" >&2
  cargo build --release --locked
fi

workspace="$(mktemp -d)"
cleanup() {
  if [[ "$KEEP" = "1" ]]; then
    echo "Smoke workspace retained: $workspace" >&2
  else
    rm -rf "$workspace"
  fi
}
trap cleanup EXIT

common=()
if [[ -n "${EASYDL_SMOKE_COOKIES:-}" ]]; then
  common+=(--cookies "$EASYDL_SMOKE_COOKIES")
fi

ran=0
run_platform() {
  local platform="$1"
  local input="$2"
  [[ -n "$input" ]] || return 0
  ran=$((ran + 1))
  echo "[$platform] inspect" >&2
  "$BIN" "${common[@]}" inspect "$input" --json > "$workspace/$platform-inspect.json"
  python3 - "$workspace/$platform-inspect.json" "$platform" <<'PY'
import json, sys
path, expected = sys.argv[1:]
value = json.load(open(path))
assert value.get("schemaVersion") == 2, value
assert value.get("platform") == expected, value
assert "media" in value and "choices" in value, value
PY

  if [[ "$MODE" = "plan" ]]; then
    echo "[$platform] plan" >&2
    "$BIN" "${common[@]}" plan "$input" > "$workspace/$platform-plan.json"
    python3 - "$workspace/$platform-plan.json" <<'PY'
import json, sys
value = json.load(open(sys.argv[1]))
assert value.get("schemaVersion") == 2, value
assert value.get("tasks"), value
text = json.dumps(value).lower()
for forbidden in ['"cookie"', 'sessdata', 'authorization']:
    assert forbidden not in text, forbidden
PY
  elif [[ "$MODE" = "download" ]]; then
    echo "[$platform] download" >&2
    mkdir -p "$workspace/$platform-output"
    "$BIN" "${common[@]}" download "$input" \
      --output-dir "$workspace/$platform-output" \
      --json > "$workspace/$platform-summary.json"
    python3 - "$workspace/$platform-summary.json" <<'PY'
import json, sys
value = json.load(open(sys.argv[1]))
assert value.get("schemaVersion") == 2, value
assert value.get("success") is True, value
assert value.get("artifacts") or value.get("skipped"), value
PY
  fi
}

run_platform kuaishou "${EASYDL_SMOKE_KUAISHOU:-}"
run_platform xhs "${EASYDL_SMOKE_XHS:-}"
run_platform douyin "${EASYDL_SMOKE_DOUYIN:-}"
run_platform bilibili "${EASYDL_SMOKE_BILIBILI:-}"

if [[ "$ran" = "0" ]]; then
  cat >&2 <<'EOF'
No real smoke inputs configured.
Set one or more of:
  EASYDL_SMOKE_KUAISHOU
  EASYDL_SMOKE_XHS
  EASYDL_SMOKE_DOUYIN
  EASYDL_SMOKE_BILIBILI
Optional:
  EASYDL_SMOKE_MODE=inspect|plan|download
  EASYDL_SMOKE_COOKIES=/secure/path/cookies.txt
  EASYDL_SMOKE_KEEP=1
  EASYDL_BIN=/path/to/easydl
EOF
  exit 2
fi

echo "Real-link smoke passed for $ran platform input(s) in $MODE mode" >&2
