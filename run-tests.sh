#!/usr/bin/env bash
# Every test_*.py, run as a plain script, fully offline.
#
#   RUNGBOT_OFFLINE=1  makes the ticker layer refuse every network call, so a test that
#                      forgets to inject prices fails loudly instead of hitting a venue.
#   HOME               an empty temp dir: no config or state on this machine can leak in.
#
# Exit 0 when every test passed. One line per test.
set -u
cd "$(dirname "$0")"
export RUNGBOT_OFFLINE=1
export HOME="$(mktemp -d)"
export XDG_CONFIG_HOME="$HOME/.config"
export XDG_STATE_HOME="$HOME/.local/state"
fail=0; n=0
for t in tests/test_*.py; do
  n=$((n + 1))
  if out="$(timeout 120 python3 "$t" 2>&1)"; then
    printf 'ok    %-22s %s\n' "$(basename "$t")" "$(printf '%s\n' "$out" | tail -n 1 | cut -c1-90)"
  else
    fail=$((fail + 1))
    printf 'FAIL  %-22s\n' "$(basename "$t")"
    printf '%s\n' "$out" | tail -n 25 | sed 's/^/      /'
  fi
done
printf '%d tests, %d failed\n' "$n" "$fail"
[ "$fail" -eq 0 ]
