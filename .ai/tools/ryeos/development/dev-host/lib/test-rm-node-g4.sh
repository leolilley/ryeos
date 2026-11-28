#!/usr/bin/env bash
# ryeos:signed:2026-11-27T16:17:52Z:c55d919146b82e847b3d7347e93e9f33def52329b2c7c87f666d548344ab1559:bi8wSD7ubyWHahB9rJuhmgGf5CQgJpTOkkEVmn20PXzElq+5lBk0VR4V+3KJkj8pi+yaZd3T7YsSNJX+0QVqDg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
# Externally invoked host helper in the development bundle; lib/ is excluded
# from Tool resolution, so this is signed source, not an admitted Tool. It
# grants no RyeOS authority.
#
# test-rm-node-g4.sh: offline tests of the G-4 cell reader and of both helpers'
# refusals, against a fixture table whose notes say "not throwaway" and
# "not own-history-only" in cells other than the designated ones. Needs no node.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
. "$here/rm-node-g4.sh"
t="$(mktemp)"; trap 'rm -f "$t" "$t.m"' EXIT
cat > "$t" <<'T'
  | Node | Track name | Owning thread | Notes | Removal (rulings 1.37, 1.39) | Throwaway | Build profile |
  |---|---|---|---|---|---|---|
  | proof | `t-ok` | x | a throwaway node | own-history-only | yes | test |
  | standing | `t-word` | x | not throwaway, not own-history-only | export first | no | test |
  | wrong profile | `t-rel` | x | | own-history-only | yes | release |
T
fail=0
check() { [[ $2 == "$3" ]] || { echo "FAIL $1: got [$2] want [$3]"; fail=1; }; }
check "throwaway cell" "$(g4_cell "$t" t-ok Throwaway)" yes
check "word in the notes cell does not count" "$(g4_cell "$t" t-word Throwaway)" no
check "removal cell" "$(g4_cell "$t" t-word 'Removal (rulings 1.37, 1.39)')" "export first"
check "no row" "$(g4_cell "$t" t-none Throwaway || echo missing)" missing
check "no such column" "$(g4_cell "$t" t-ok Colour || echo missing)" missing
# rm-node-down refuses (exit 3, from the table) a row whose notes say the word but
# whose Removal cell does not, and a track with no row
for tr in t-word t-none; do
    rc=0; err="$(RM_NODE_G4_TABLE="$t" "$here/rm-node-down.sh" "$tr" --own-history-only 2>&1 >/dev/null)" || rc=$?
    check "down $tr exit" "$rc" 3
    [[ $err == *"G-4"* ]] || { echo "FAIL down $tr: refusal did not come from the table: $err"; fail=1; }
done
[[ $fail == 0 ]] && echo "test-rm-node-g4: ok" || exit 1
