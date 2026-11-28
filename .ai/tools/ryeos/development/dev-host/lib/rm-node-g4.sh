#!/usr/bin/env bash
# ryeos:signed:2026-11-27T16:17:51Z:9d6be01a87d67a9dc1b9a1d1746fd37f030a32819b82ee9fae886b65a8044760:NB2y+CqC4nug3hJC0j5yKCrNW9/2Vdo/MgGbC3vNUutVLgaBsbTq5hUlfCF6tRVw9psDZK7cUoH1LwjURuZ9DQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
# Externally invoked host helper in the development bundle; lib/ is excluded
# from Tool resolution, so this is signed source, not an admitted Tool. It
# grants no RyeOS authority.
#
# rm-node-g4.sh: read one cell of one track's row in G-4's table
# (10-running-unattended.md), by column header, as data. Sourced by rm-node-up.sh
# and rm-node-down.sh.
#
#   g4_cell <table> <track> <column header>
#
# Prints the cell, trimmed, and returns 0; returns 3 when the table has no header
# row with that column, or no row whose Track name cell is exactly `<track>`
# (backticked). A word elsewhere in the row never counts: only the named cell is read.
g4_cell() {
    python3 -I -c '
import sys
table, track, column = sys.argv[1:4]
header = None
for line in open(table, encoding="utf-8"):
    s = line.strip()
    if not s.startswith("|"):
        continue
    cells = [c.strip() for c in s.strip("|").split("|")]
    if header is None:
        if "Track name" in cells and column in cells:
            header = cells
        continue
    if len(cells) == len(header) and cells[header.index("Track name")] == "`" + track + "`":
        print(cells[header.index(column)])
        sys.exit(0)
sys.exit(3)
' "$1" "$2" "$3"
}
