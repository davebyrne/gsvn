#!/usr/bin/env bash
# Regenerate the parser fixtures in this directory.
#
#   crates/svn-backend/tests/fixtures/regen.sh            # local repo only
#   crates/svn-backend/tests/fixtures/regen.sh --remote   # also svn.apache.org
#
# Local fixtures (local_*) come from a throw-away file:// repository built in
# a temp dir. UUID and svn:date revprops are pinned so the output is
# reproducible (only the temp-dir path inside <url>/<root> changes; it is
# rewritten to file:///tmp/gsvn-fixture/repo below).
#
# Remote fixtures (asf_*) are captured from https://svn.apache.org/repos/asf
# at pinned revisions; requests are kept tiny.
#
# Never touches ~/.subversion: every svn call uses --config-dir in the temp dir.
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
export LC_ALL=C.UTF-8
CFG="$WORK/config"
mkdir -p "$CFG"
SVN=(svn --config-dir "$CFG" --non-interactive)
REPO="$WORK/repo"
URL="file://$REPO"
FAKE="file:///tmp/gsvn-fixture/repo"

svnadmin create "$REPO"
svnadmin setuuid "$REPO" 01234567-89ab-cdef-0123-456789abcdef

# Pin the date of revision $1 to $2.
setdate() { svnadmin setrevprop "$REPO" -r "$1" svn:date <(printf '%s' "$2"); }
setdate 0 "2026-01-01T00:00:00.000000Z"

"${SVN[@]}" mkdir -m "Initial layout" --username alice \
    "$URL/trunk" "$URL/branches" "$URL/tags" >/dev/null
setdate 1 "2026-01-02T10:00:00.000001Z"

WC="$WORK/wc"
"${SVN[@]}" checkout -q "$URL/trunk" "$WC"
cd "$WC"

# --- r2: lots of awkward files -------------------------------------------
seq 1 30 | sed 's/^/line /' > hello.txt
printf 'spaces\n' > 'a b.txt'
printf 'at\n' > 'at@sign.txt'
printf 'amp\n' > 'amp&co.txt'
printf 'unicode\n' > 'unicodé-日本.txt'
printf 'first\nlast without newline' > noeol.txt
printf 'one\r\ntwo\r\nthree\r\n' > crlf.txt
printf 'caf\xe9\nna\xefve\n' > latin1.txt
printf 'keep\n-- old dashes\nmiddle\n++ old plus\nIndex: not a header\n@@ -1 +1 @@ not a hunk\nend\n' > dashes.txt
printf '\x00\x01\x02\x03binary\xff\xfe' > binary.bin
: > empty.txt
mkdir 'sub dir'
printf 'nested\n' > 'sub dir/nested.txt'
"${SVN[@]}" add -q --force .
"${SVN[@]}" propset -q svn:mime-type application/octet-stream binary.bin
# Message with leading/trailing newlines, markup and non-ASCII.
printf '\n  Add files <b>&amp; "stuff"</b> '"'"'q'"'"' & more\n\nUnicode: 日本語 — ü\n\n' > "$WORK/msg2"
"${SVN[@]}" commit -q --username bob -F "$WORK/msg2"
setdate 2 "2026-02-03T04:05:06.789012Z"

# --- r3: modifications (empty message) -----------------------------------
sed -i -e 's/^line 3$/line three/' -e 's/^line 25$/line twenty-five/' -e '/^line 27$/d' hello.txt
printf 'first\nlast changed, still no newline' > noeol.txt
printf 'one\r\nTWO\r\nthree\r\n' > crlf.txt
printf 'caf\xe9\nna\xefve!\n' > latin1.txt
printf 'keep\n--- new dashes\nmiddle\n+++ new plus\nIndex: still not a header\n@@ -1 +1 @@ not a hunk\nend\n' > dashes.txt
printf '\x00\x01\x02\x03binary changed\xff\xfe' > binary.bin
"${SVN[@]}" commit -q --username carol -m ""
setdate 3 "2026-03-04T05:06:07.000000Z"

# --- r4: property-only change; author removed afterwards ------------------
"${SVN[@]}" update -q
"${SVN[@]}" propset -q my:prop 'single value' hello.txt
printf 'line one\nline two\n' > "$WORK/multi"
"${SVN[@]}" propset -q my:multi -F "$WORK/multi" hello.txt
"${SVN[@]}" propset -q svn:ignore "$(printf '*.o\n*.tmp\n')" .
printf 'bin\x01ary\x02' > "$WORK/binprop"
"${SVN[@]}" propset -q my:binary -F "$WORK/binprop" hello.txt
"${SVN[@]}" commit -q --username dave -m "Props only"
setdate 4 "2026-04-05T06:07:08.000000Z"
svnadmin delrevprop "$REPO" -r 4 svn:author

# --- r5: branch (copy with copyfrom) --------------------------------------
"${SVN[@]}" copy -q -m "Branch feature-1" --username alice \
    "$URL/trunk@4" "$URL/branches/feature-1"
setdate 5 "2026-05-06T07:08:09.100000Z"
"${SVN[@]}" update -q

# --- r6: delete, replace, empty add, prop modify/delete ------------------
"${SVN[@]}" rm -q empty.txt 'a b.txt'
"${SVN[@]}" rm -q 'amp&co.txt'
printf 'replaced\n' > 'amp&co.txt'
"${SVN[@]}" add -q 'amp&co.txt'
: > new-empty.txt
"${SVN[@]}" add -q new-empty.txt
"${SVN[@]}" propset -q my:prop 'changed value' hello.txt
"${SVN[@]}" propdel -q my:multi hello.txt
"${SVN[@]}" commit -q --username bob -m "Delete, replace & add"
setdate 6 "2026-06-07T08:09:10.000000Z"

# --- r7: change on the branch; r8: merge it to trunk (svn:mergeinfo) -------
"${SVN[@]}" checkout -q "$URL/branches/feature-1" "$WORK/wcb"
printf 'branch line\n' >> "$WORK/wcb/a b.txt"
"${SVN[@]}" commit -q --username alice -m "Change on branch" "$WORK/wcb"
setdate 7 "2026-07-01T00:00:00.000000Z"
"${SVN[@]}" update -q
# Record the merge only (a b.txt is deleted on trunk): mergeinfo change.
"${SVN[@]}" merge -q --record-only -c 7 "$URL/branches/feature-1" .
"${SVN[@]}" commit -q --username bob -m "Record merge of r7"
setdate 8 "2026-08-01T00:00:00.000000Z"
cd "$HERE"

# Run svn and rewrite the temp repo path to a stable fake one.
cap() { local out=$1; shift; "${SVN[@]}" "$@" | sed "s#file://$REPO#$FAKE#g" > "$HERE/$out"; }

# Most captures are pinned to r6 (the tests were written against r6).
cap local_info_root.xml   info --xml "$URL@6"
cap local_info_multi.xml  info --xml "$URL/trunk/a b.txt@5" "$URL/trunk/unicodé-日本.txt@6" "$URL/trunk/at@sign.txt@6" "$URL/branches@6"
cap local_list_trunk.xml  list --xml "$URL/trunk@6"
cap local_list_multi.xml  list --xml "$URL/branches@6" "$URL/tags@6" "$URL/trunk/sub%20dir@6"
cap local_log.xml         log --xml "$URL@6"
cap local_log_v.xml       log -v --xml "$URL@6"
cap local_blame.xml       blame --xml "$URL/trunk/hello.txt@6"
cap local_blame_range.xml blame --xml -r 3:6 "$URL/trunk/hello.txt@6"
cap local_proplist_file.xml proplist -v --xml "$URL/trunk/hello.txt@4"
cap local_proplist_dir.xml  proplist -v --xml "$URL/trunk@6"
cap local_proplist_none.xml proplist -v --xml "$URL/trunk/crlf.txt@6"
for r in 2 3 4 5 6 8; do
    cap "local_diff_r$r.diff" diff --internal-diff -c "$r" "$URL@$r"
done
cap local_diff_r1_6.diff  diff --internal-diff -r 1:6 "$URL/trunk@6"
cap local_diff_empty.diff diff --internal-diff -c 3 "$URL/trunk/sub%20dir@3"

if [[ "${1:-}" == "--remote" ]]; then
    ASF=https://svn.apache.org/repos/asf
    PEG=1939142
    "${SVN[@]}" info --xml "$ASF/subversion/trunk@$PEG" > asf_info.xml
    "${SVN[@]}" list --xml "$ASF/subversion/trunk/doc@$PEG" > asf_list.xml
    "${SVN[@]}" log -v --xml -l 20 "$ASF/subversion/trunk@$PEG" > asf_log_v.xml
    "${SVN[@]}" diff --internal-diff -c 1938822 "$ASF/subversion/trunk@1938822" > asf_diff.diff
    "${SVN[@]}" blame --xml "$ASF/subversion/trunk/doc/README@$PEG" > asf_blame.xml
fi
echo "fixtures written to $HERE"
