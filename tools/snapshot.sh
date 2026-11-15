#!/usr/bin/env bash
# Copies TCI_transcode from HDZ' vault into its git snapshot, curated (same rule as RadioHanze: the repository carries
# what someone outside needs, in English, and none of the vault's internal notes).
#
# Ends:       ~/repos/TCI_transcode holds code, examples, tools, Cargo files and README and CLAUDE.md (folder rule 4: every repo says what it is); no other vault notes.
# Ways:       rsync --delete from the vault folder, leaving out: .git, target, .gitignore, .DS_Store,
#             every note named "TCI_transcode - <Topic>.md" (executive summary,
#             reviews) and "Recherche - *.md" (German research notes); a find afterwards removes such files that an
#             older copy left behind (never inside .git). Prints git status afterwards. SNAPSHOT_DST overrides the target.
# Means:      rsync, git.
# Provides:   CLI: bash tools/snapshot.sh
# Expects:    ~/repos/TCI_transcode exists as a git repository.
# Ensures:    nothing is committed or pushed; .git survives (checked).
# Fails:      set -e: a missing snapshot folder ends the script.
# Tags:       #radio #build
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DST="${SNAPSHOT_DST:-$HOME/repos/TCI_transcode}"
[ -d "$DST/.git" ] || { echo "no git snapshot at $DST"; exit 1; }
# Pitfall (RadioHanze 2026-10-21): --delete-excluded removed .git; excluded files are left alone by rsync instead.
rsync -a --delete \
  --exclude .git --exclude target --exclude .gitignore --exclude .DS_Store \
  --exclude 'TCI_transcode - *.md' --exclude 'Recherche - *.md' \
  "$HERE/" "$DST/"
find "$DST" -path "$DST/.git" -prune -o \
  \( -name 'TCI_transcode - *.md' -o -name 'Recherche - *.md' -o -name .DS_Store \) \
  -print0 | xargs -0 rm -rf
[ -d "$DST/.git" ] || { echo "SNAPSHOT BROKE .git in $DST"; exit 2; }
git -C "$DST" status --short
