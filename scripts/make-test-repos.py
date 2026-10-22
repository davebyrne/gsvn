#!/usr/bin/env python3
"""Generate Subversion test repositories for gsvn.

The repositories are produced by writing an SVN dump stream (format version 2,
full texts, no deltas, no checksums) straight into ``svnadmin load`` through a
pipe. That is orders of magnitude faster than committing through a client.

Repositories:

  small    realistic tiny project (~50 revisions, branches, tag, binary file,
           >2 MB text file, renames, deletes, odd path names, properties)
  wide     directories with 20 000 and 100 000 entries
  deep     a path nested 200 levels deep
  history  many (default 200 000) small revisions, incl. one 50 000-path commit

Usage:
  scripts/make-test-repos.py [OUTDIR] [--force] [--only small,deep]
                             [--history-revs N]

Everything is deterministic (fixed RNG seeds and dates), so the same script
version always produces the same repositories (except for the repo UUID,
which is also fixed per repo).

Requires: Python 3.8+, svnadmin (1.10+ for --no-flush-to-disk).
"""

import argparse
import datetime as dt
import io
import os
import random
import shutil
import subprocess
import sys
import time

# --------------------------------------------------------------------------
# Dump stream writer
# --------------------------------------------------------------------------


def _b(s):
    return s if isinstance(s, bytes) else s.encode("utf-8")


def encode_props(props):
    """Serialize a property dict into the dump/hash format (bytes)."""
    out = io.BytesIO()
    for k, v in props.items():
        k, v = _b(k), _b(v)
        out.write(b"K %d\n%s\nV %d\n%s\n" % (len(k), k, len(v), v))
    out.write(b"PROPS-END\n")
    return out.getvalue()


class DumpWriter:
    """Writes an SVN dump stream (format 2, fulltexts) to a binary stream."""

    def __init__(self, out, uuid):
        self.out = out
        self.rev = -1
        w = out.write
        w(b"SVN-fs-dump-format-version: 2\n\n")
        w(b"UUID: %s\n\n" % _b(uuid))

    def revision(self, author, date, log):
        """Start a new revision. ``date`` is a datetime (UTC). Returns rev number."""
        self.rev += 1
        props = {}
        if log is not None:
            props["svn:log"] = log
        if author is not None:
            props["svn:author"] = author
        props["svn:date"] = date.strftime("%Y-%m-%dT%H:%M:%S.%fZ")
        p = encode_props(props)
        self.out.write(
            b"Revision-number: %d\nProp-content-length: %d\nContent-length: %d\n\n"
            % (self.rev, len(p), len(p))
        )
        self.out.write(p)
        self.out.write(b"\n")
        return self.rev

    def node(self, path, action, kind=None, props=None, text=None,
             copyfrom=None):
        """Emit one node record.

        action:   'add' | 'change' | 'delete' | 'replace'
        kind:     'file' | 'dir' (omit for delete)
        props:    dict -> full property set for the node (replaces existing);
                  None -> properties untouched
        text:     bytes -> full text of the file; None -> no text
        copyfrom: (path, rev) or None
        """
        h = [b"Node-path: " + _b(path)]
        if kind:
            h.append(b"Node-kind: " + _b(kind))
        h.append(b"Node-action: " + _b(action))
        if copyfrom:
            h.append(b"Node-copyfrom-rev: %d" % copyfrom[1])
            h.append(b"Node-copyfrom-path: " + _b(copyfrom[0]))
        p = encode_props(props) if props is not None else b""
        t = _b(text) if text is not None else b""
        if props is not None:
            h.append(b"Prop-content-length: %d" % len(p))
        if text is not None:
            h.append(b"Text-content-length: %d" % len(t))
        if props is not None or text is not None:
            h.append(b"Content-length: %d" % (len(p) + len(t)))
        w = self.out.write
        w(b"\n".join(h))
        w(b"\n\n")
        if p:
            w(p)
        if t:
            w(t)
        w(b"\n\n")

    # convenience helpers -------------------------------------------------
    def mkdir(self, path, props=None):
        self.node(path, "add", "dir", props=props if props is not None else {})

    def add_file(self, path, text, props=None):
        self.node(path, "add", "file",
                  props=props if props is not None else {}, text=text)

    def change_file(self, path, text, props=None):
        self.node(path, "change", "file", props=props, text=text)

    def delete(self, path):
        self.node(path, "delete")

    def copy(self, kind, path, src, srcrev):
        self.node(path, "add", kind, copyfrom=(src, srcrev))


class Clock:
    """Monotonically increasing commit timestamps."""

    def __init__(self, start, rng, min_s=60, max_s=6 * 3600):
        self.t = start
        self.rng = rng
        self.min_s, self.max_s = min_s, max_s

    def next(self):
        self.t += dt.timedelta(seconds=self.rng.randint(self.min_s, self.max_s),
                               microseconds=self.rng.randint(0, 999999))
        return self.t


EPOCH = dt.datetime(2024, 1, 1, 9, 0, 0)

# --------------------------------------------------------------------------
# Loading
# --------------------------------------------------------------------------


def load_repo(path, generator):
    """Create repo at ``path`` and stream ``generator(dump_writer_stream)`` into it."""
    subprocess.run(["svnadmin", "create", "--fs-type", "fsfs", path], check=True)
    # Rep-sharing (an SQLite insert per representation) buys nothing for
    # generated data and costs ~15% load time.
    with open(os.path.join(path, "db", "fsfs.conf"), "a") as f:
        f.write("\n[rep-sharing]\nenable-rep-sharing = false\n")
    cmd = ["svnadmin", "load", "--quiet", "--no-flush-to-disk",
           "--bypass-prop-validation", "-M", "512", path]
    proc = subprocess.Popen(cmd, stdin=subprocess.PIPE)
    out = io.BufferedWriter(proc.stdin, buffer_size=4 << 20)
    try:
        generator(out)
        out.flush()
    except BrokenPipeError:
        pass
    finally:
        try:
            out.close()
        except BrokenPipeError:
            pass
    if proc.wait() != 0:
        raise RuntimeError("svnadmin load failed for %s" % path)


def youngest(path):
    r = subprocess.run(["svnlook", "youngest", path], check=True,
                       capture_output=True, text=True)
    return int(r.stdout.strip())


def du(path):
    total = 0
    for root, _dirs, files in os.walk(path):
        for f in files:
            try:
                total += os.lstat(os.path.join(root, f)).st_size
            except OSError:
                pass
    return total


# --------------------------------------------------------------------------
# Repo: small
# --------------------------------------------------------------------------

MAIN_RS = """\
// $Id$
use std::env;

mod lib;

fn main() {
    let args: Vec<String> = env::args().collect();
    println!("hello from gsvn-demo, {} args", args.len());
}
"""

LIB_RS = """\
/// Adds two numbers.
pub fn add(a: i32, b: i32) -> i32 {
    a + b
}

#[cfg(test)]
mod tests {
    #[test]
    fn it_adds() {
        assert_eq!(super::add(2, 2), 4);
    }
}
"""

UTIL_C = """\
#include <stdio.h>
#include "util.h"

int util_square(int x)
{
    return x * x;
}
"""

UTIL_H = """\
#ifndef UTIL_H
#define UTIL_H

int util_square(int x);

#endif
"""

BUILD_PY = """\
#!/usr/bin/env python3
\"\"\"Build helper.\"\"\"
import subprocess


def main():
    subprocess.run(["cargo", "build"], check=True)


if __name__ == "__main__":
    main()
"""

MAKEFILE = """\
CC ?= cc
CFLAGS ?= -O2 -Wall

all: util.o
\tcargo build

util.o: src/util.c src/util.h
\t$(CC) $(CFLAGS) -c -o $@ $<

clean:
\trm -f *.o
\tcargo clean

.PHONY: all clean
"""

README = """\
# gsvn-demo

A tiny demo project used to exercise the **gsvn** repository browser.

## Building

    make

## Layout

* `src/` – Rust and C sources
* `scripts/` – helper scripts
* `docs/` – documentation
"""


def make_big_text(rng, min_bytes):
    words = ("lorem ipsum dolor sit amet consectetur adipiscing elit sed do "
             "eiusmod tempor incididunt ut labore et dolore magna aliqua "
             "subversion revision branch tag trunk merge blame diff").split()
    lines = []
    size = 0
    n = 0
    while size < min_bytes:
        n += 1
        line = "%07d %s\n" % (n, " ".join(rng.choice(words)
                                          for _ in range(rng.randint(4, 14))))
        lines.append(line)
        size += len(line)
    return "".join(lines).encode()


def make_binary(rng, n):
    # PNG-ish header with NUL bytes, then pseudo-random bytes (lots of NULs).
    head = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR\x00\x00\x00\x10\x00\x00\x00\x10"
    body = bytes(rng.choice((0, 0, 0, rng.randint(0, 255))) for _ in range(n))
    return head + body


class Tree:
    """Minimal in-memory model of the repo tree (for copies & prop sets)."""

    def __init__(self):
        self.nodes = {}  # path -> dict(kind, text, props)

    def add(self, path, kind, text=None, props=None):
        self.nodes[path] = {"kind": kind, "text": text, "props": dict(props or {})}

    def copy(self, src, dst):
        for p, n in list(self.nodes.items()):
            if p == src or p.startswith(src + "/"):
                self.nodes[dst + p[len(src):]] = {
                    "kind": n["kind"], "text": n["text"], "props": dict(n["props"])}

    def delete(self, path):
        for p in list(self.nodes):
            if p == path or p.startswith(path + "/"):
                del self.nodes[p]


def gen_small(out):
    rng = random.Random(1001)
    d = DumpWriter(out, "6a1c7e2b-0000-4000-8000-00000000a001")
    clock = Clock(EPOCH, rng, 30 * 60, 30 * 3600)
    t = Tree()
    authors = ["alice", "bob", "carol", "dave", "jürgen"]
    revs = {}

    def rev(log, author=None):
        return d.revision(author or rng.choice(authors), clock.next(), log)

    def addf(path, text, props=None):
        text = _b(text)
        t.add(path, "file", text, props)
        d.add_file(path, text, props)

    def mkdir(path, props=None):
        t.add(path, "dir", None, props)
        d.mkdir(path, props)

    def edit(path, text):
        text = _b(text)
        t.nodes[path]["text"] = text
        d.change_file(path, text)

    def setprops(path, props):
        n = t.nodes[path]
        n["props"].update(props)
        n["props"] = {k: v for k, v in n["props"].items() if v is not None}
        d.node(path, "change", n["kind"], props=n["props"])

    def append(path, line):
        edit(path, t.nodes[path]["text"].decode("utf-8") + line)

    def cp(src, dst, srcrev):
        kind = t.nodes[src]["kind"]
        t.copy(src, dst)
        d.copy(kind, dst, src, srcrev)

    def rm(path):
        t.delete(path)
        d.delete(path)

    d.revision(None, EPOCH, None)  # r0 with an early date

    # r1
    rev("Initial repository layout.", "alice")
    for p in ("trunk", "branches", "tags"):
        mkdir(p)

    # r2
    rev("Import gsvn-demo skeleton.\n\nRust binary + a small C helper library,\n"
        "a Makefile and a Python build helper.", "alice")
    addf("trunk/README.md", README)
    addf("trunk/Makefile", MAKEFILE)
    mkdir("trunk/src")
    addf("trunk/src/main.rs", MAIN_RS)
    addf("trunk/src/lib.rs", LIB_RS)
    addf("trunk/src/util.c", UTIL_C)
    addf("trunk/src/util.h", UTIL_H)
    mkdir("trunk/scripts")
    addf("trunk/scripts/build.py", BUILD_PY, {"svn:executable": "*"})
    addf("trunk/scripts/old_deploy.py", "# obsolete deploy script\nprint('deploy')\n")
    mkdir("trunk/docs")

    # r3 binary file
    rev("Add logo (binary).", "bob")
    mkdir("trunk/assets")
    addf("trunk/assets/logo.png", make_binary(rng, 3000),
         {"svn:mime-type": "application/octet-stream"})

    # r4 large text file (> 2 MB)
    rev("BUG-101: add sample data set used by the parser benchmarks.", "carol")
    mkdir("trunk/data")
    addf("trunk/data/big.txt", make_big_text(rng, 2_400_000),
         {"svn:eol-style": "native"})

    # r5 odd path names
    rev("Docs: user guide and retina icon.\n\nNote the space and the '@' in the "
        "file names - peg revision parsing must cope with them.", "dave")
    addf("trunk/docs/user guide.md", "# User Guide\n\nOpen a repository URL.\n")
    addf("trunk/assets/icon@2x.txt", "pretend this is a 2x icon\n")

    # r6 properties
    rev("Set svn:ignore on trunk and svn:keywords on main.rs.", "alice")
    setprops("trunk", {"svn:ignore": "target\n*.o\n"})
    setprops("trunk/src/main.rs", {"svn:keywords": "Id Rev Author Date"})

    # r7 .. r15 trunk edits
    msgs = [
        "BUG-123: fix off-by-one in argument count.",
        "Refactor util_square to avoid overflow.\n\nSee BUG-130 for details.",
        "Typo fixes in README.",
        "BUG-124 BUG-125: handle empty input and trailing newline.",
        "Add unit test for add().",
        "Update Makefile flags.",
        "Small cleanups.\n\n- remove unused import\n- reformat\n- comments",
        "BUG-200: make build.py return non-zero on failure.",
        "Bump docs.",
    ]
    targets = ["trunk/src/main.rs", "trunk/src/util.c", "trunk/README.md",
               "trunk/src/lib.rs", "trunk/src/lib.rs", "trunk/Makefile",
               "trunk/src/main.rs", "trunk/scripts/build.py",
               "trunk/docs/user guide.md"]
    for i, (m, f) in enumerate(zip(msgs, targets)):
        rev(m)
        comment = "#" if f.endswith((".py", "Makefile", ".md")) else "//"
        if f.endswith(".c"):
            comment = "/*"
        line = ("%s edit %d */\n" % (comment, i)) if comment == "/*" else \
            ("%s edit %d\n" % (comment, i))
        append(f, line)
        if i == 3:  # a multi-file commit
            append("trunk/src/util.h", "/* BUG-125 */\n")
    revs["trunk_before_branch"] = d.rev  # r15

    # r16 branch by copy
    rev("Create feature-x branch from trunk.", "bob")
    cp("trunk", "branches/feature-x", revs["trunk_before_branch"])
    revs["branch_feature_x"] = d.rev

    # r17 .. r20 branch work + trunk work interleaved
    rev("feature-x: start new parser (BUG-300).", "bob")
    addf("branches/feature-x/src/parser.rs",
         "pub fn parse(s: &str) -> Vec<&str> {\n    s.split(',').collect()\n}\n")
    rev("trunk: README wording.", "carol")
    append("trunk/README.md", "\nMore words.\n")
    rev("feature-x: parser handles whitespace.\n\nBUG-300", "bob")
    edit("branches/feature-x/src/parser.rs",
         "pub fn parse(s: &str) -> Vec<&str> {\n"
         "    s.split(',').map(|x| x.trim()).collect()\n}\n")
    rev("trunk: tweak util.h guard.", "dave")
    append("trunk/src/util.h", "/* guard tweak */\n")
    revs["trunk_for_tag"] = d.rev  # r20

    # r21 tag
    rev("Tag release 0.1.", "alice")
    cp("trunk", "tags/v0.1", revs["trunk_for_tag"])
    revs["tag_v0_1"] = d.rev

    # r22 rename (copy + delete) util.c -> helpers.c
    rev("Rename util.c to helpers.c.", "carol")
    cp("trunk/src/util.c", "trunk/src/helpers.c", d.rev - 1)
    rm("trunk/src/util.c")
    revs["rename_util_c"] = d.rev
    # r23 fix Makefile after rename
    rev("Makefile: follow util.c rename.", "carol")
    edit("trunk/Makefile",
         t.nodes["trunk/Makefile"]["text"].decode().replace("util.c", "helpers.c"))

    # r24 delete a file
    rev("Remove obsolete deploy script.", "dave")
    rm("trunk/scripts/old_deploy.py")
    revs["delete_old_deploy"] = d.rev

    # r25 merge feature-x into trunk (mergeinfo)
    rev("Merge feature-x into trunk (BUG-300).", "alice")
    addf("trunk/src/parser.rs", t.nodes["branches/feature-x/src/parser.rs"]["text"])
    setprops("trunk", {"svn:mergeinfo": "/branches/feature-x:%d-%d"
                       % (revs["branch_feature_x"] + 1, revs["branch_feature_x"] + 3)})
    revs["merge_feature_x"] = d.rev

    # r26 release branch copied from the tag
    rev("Create release-0.1 maintenance branch from tags/v0.1.", "alice")
    cp("tags/v0.1", "branches/release-0.1", revs["tag_v0_1"])
    revs["branch_release_0_1"] = d.rev

    # r27 hotfix on the release branch
    rev("BUG-410: hotfix overflow on release branch.", "bob")
    append("branches/release-0.1/src/util.c", "/* BUG-410 hotfix */\n")

    # r28 property change on file (eol-style + keywords), and remove svn:executable
    rev("Normalise properties.", "jürgen")
    setprops("trunk/src/lib.rs", {"svn:eol-style": "native"})
    setprops("trunk/scripts/build.py", {"svn:executable": None,
                                         "svn:eol-style": "LF"})

    # r29 binary file modification
    rev("Update logo.", "bob")
    edit("trunk/assets/logo.png", make_binary(rng, 3500))

    # r30 edit big text a little
    rev("BUG-101: append extra benchmark rows.", "carol")
    edit("trunk/data/big.txt", t.nodes["trunk/data/big.txt"]["text"]
         + b"9999999 extra row one\n9999999 extra row two\n")

    # r31 a branch of a branch
    rev("Branch feature-x-sub from feature-x.", "dave")
    cp("branches/feature-x", "branches/feature-x-sub", d.rev - 1)
    revs["branch_feature_x_sub"] = d.rev

    # r32 .. r50 assorted trunk work
    filler = [
        ("trunk/src/main.rs", "// iteration %d\n"),
        ("trunk/src/lib.rs", "// iteration %d\n"),
        ("trunk/src/helpers.c", "/* iteration %d */\n"),
        ("trunk/README.md", "Change %d.\n"),
        ("trunk/scripts/build.py", "# iteration %d\n"),
        ("trunk/src/parser.rs", "// iteration %d\n"),
    ]
    logs = [
        "Iteration %d.",
        "BUG-%d: misc fix.",
        "Work in progress (%d).\n\nLonger explanation on the second paragraph\n"
        "spanning\nseveral lines.",
        "Refactor step %d.",
        "Ünïcödé commit message %d — with “quotes”.",
    ]
    i = 0
    while d.rev < 50:
        i += 1
        lg = rng.choice(logs)
        rev(lg % (500 + i) if "BUG" in lg else lg % i)
        f, fmt = filler[i % len(filler)]
        append(f, fmt % i)
        if i % 5 == 0:
            f2, fmt2 = filler[(i + 2) % len(filler)]
            append(f2, fmt2 % (i * 100))
        if i == 7:
            mkdir("trunk/src/empty-dir")
        if i == 9:
            # rename a directory: docs -> doc
            cp("trunk/docs", "trunk/doc", d.rev - 1)
            rm("trunk/docs")
            revs["rename_docs_dir"] = d.rev
    revs["last"] = d.rev
    return revs


# --------------------------------------------------------------------------
# Repo: wide
# --------------------------------------------------------------------------


def gen_wide(out):
    rng = random.Random(2002)
    d = DumpWriter(out, "6a1c7e2b-0000-4000-8000-00000000a002")
    clock = Clock(EPOCH, rng)
    d.revision(None, EPOCH, None)

    d.revision("alice", clock.next(), "Layout.")
    d.mkdir("trunk")
    d.mkdir("trunk/wide20k")
    d.mkdir("trunk/wide100k")

    def fill(dirpath, start, count, subdir_every):
        for i in range(start, start + count):
            if subdir_every and i % subdir_every == 0:
                d.mkdir("%s/sub%06d" % (dirpath, i))
                d.add_file("%s/sub%06d/inner.txt" % (dirpath, i), b"inner %d\n" % i)
            else:
                d.add_file("%s/f%06d.txt" % (dirpath, i), b"file %d\n" % i)

    # 20 000 entries in a single revision (every 200th entry is a subdir)
    d.revision("bob", clock.next(), "Add 20 000 entries to trunk/wide20k.")
    fill("trunk/wide20k", 0, 20000, 200)

    # 100 000 entries in five revisions of 20 000 (every 1000th a subdir)
    for chunk in range(5):
        d.revision("carol", clock.next(),
                   "Add entries %d..%d to trunk/wide100k."
                   % (chunk * 20000, chunk * 20000 + 19999))
        fill("trunk/wide100k", chunk * 20000, 20000, 1000)

    d.revision("dave", clock.next(), "Touch a few files in the wide dirs.")
    d.change_file("trunk/wide20k/f000001.txt", b"file 1 changed\n")
    d.change_file("trunk/wide100k/f099999.txt", b"file 99999 changed\n")
    d.add_file("trunk/README.txt",
               b"trunk/wide20k: 20000 entries (100 subdirs)\n"
               b"trunk/wide100k: 100000 entries (100 subdirs)\n")
    return {"last": d.rev}


# --------------------------------------------------------------------------
# Repo: deep
# --------------------------------------------------------------------------


def gen_deep(out):
    rng = random.Random(3003)
    d = DumpWriter(out, "6a1c7e2b-0000-4000-8000-00000000a003")
    clock = Clock(EPOCH, rng)
    d.revision(None, EPOCH, None)

    d.revision("alice", clock.next(), "Create a 200-level deep directory chain.")
    d.mkdir("trunk")
    path = "trunk"
    levels = []
    for i in range(200):
        path = "%s/d%03d" % (path, i)
        d.mkdir(path)
        levels.append(path)
        if i % 25 == 0 or i == 199:
            d.add_file("%s/sibling-%03d.txt" % (path, i), b"sibling at level %d\n" % i)
        if i % 50 == 10:
            d.mkdir("%s/side%03d" % (path, i))
            d.add_file("%s/side%03d/x.txt" % (path, i), b"side branch\n")
    leaf = path + "/leaf.txt"
    d.add_file(leaf, b"you found the leaf\n")

    d.revision("bob", clock.next(), "Edit the leaf.")
    d.change_file(leaf, b"you found the leaf\nedited\n")

    d.revision("carol", clock.next(), "Add another file half way down.")
    d.add_file(levels[99] + "/halfway.txt", b"halfway\n")
    return {"last": d.rev, "leaf": leaf}


# --------------------------------------------------------------------------
# Repo: history
# --------------------------------------------------------------------------


def gen_history(out, nrevs):
    rng = random.Random(4004)
    d = DumpWriter(out, "6a1c7e2b-0000-4000-8000-00000000a004")
    clock = Clock(EPOCH, rng, 1, 600)
    d.revision(None, EPOCH, None)
    authors = ["alice", "bob", "carol", "dave", "erin", "frank", "grace",
               "heidi", "ivan", "jürgen"]
    files = ["trunk/README.txt",
             "trunk/src/core/engine.c", "trunk/src/core/engine.h",
             "trunk/src/ui/window.rs", "trunk/src/ui/model.rs",
             "trunk/docs/changelog.txt", "trunk/tools/gen.py"]
    nlines = 40
    content = {f: ["%s line %d\n" % (f.rsplit("/", 1)[-1], i) for i in range(nlines)]
               for f in files}

    def text(f):
        return "".join(content[f]).encode()

    d.revision("alice", clock.next(), "Initial import.")
    for p in ("trunk", "trunk/src", "trunk/src/core", "trunk/src/ui",
              "trunk/docs", "trunk/tools", "branches", "tags"):
        d.mkdir(p)
    for f in files:
        d.add_file(f, text(f))

    big_rev = nrevs // 2
    info = {}
    msgs = ["Tweak %s.", "BUG-%d: fix in %s.", "Refactor %s.",
            "Update %s\n\nMore detail on a second paragraph.", "%s: typo."]
    rnd = rng.random
    choice = rng.choice
    randrange = rng.randrange
    while d.rev < nrevs:
        r = d.rev + 1
        if r == big_rev:
            d.revision("bulkbot", clock.next(),
                       "Generate 50 000 files under trunk/generated (bulk commit).")
            d.mkdir("trunk/generated")
            for g in range(50):
                dp = "trunk/generated/g%02d" % g
                d.mkdir(dp)
                for k in range(1000 - 1):
                    d.add_file("%s/f%03d.txt" % (dp, k), b"generated\n")
            info["bulk_rev"] = r
            continue
        if r % 20000 == 0:
            d.revision("alice", clock.next(), "Tag snapshot r%d." % (r - 1))
            d.copy("dir", "tags/snap-%06d" % r, "trunk", r - 1)
            continue
        nfiles = 1 if rnd() < 0.85 else randrange(2, 4)
        touched = rng.sample(files, nfiles)
        m = choice(msgs)
        name = touched[0].rsplit("/", 1)[-1]
        log = m % (randrange(1, 5000), name) if "BUG" in m else m % name
        d.revision(choice(authors), clock.next(), log)
        for f in touched:
            ln = randrange(nlines)
            content[f][ln] = "%s line %d (r%d)\n" % (f.rsplit("/", 1)[-1], ln, r)
            d.change_file(f, text(f))
    info["last"] = d.rev
    return info


# --------------------------------------------------------------------------
# main
# --------------------------------------------------------------------------


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("outdir", nargs="?", default="target/test-repos")
    ap.add_argument("--force", action="store_true",
                    help="delete and regenerate existing repositories")
    ap.add_argument("--only", default="small,wide,deep,history",
                    help="comma-separated subset (default: all)")
    ap.add_argument("--history-revs", type=int, default=200_000,
                    help="number of revisions in the 'history' repo (default 200000)")
    ap.add_argument("--no-pack", action="store_true",
                    help="skip 'svnadmin pack' after loading")
    args = ap.parse_args()

    if shutil.which("svnadmin") is None:
        sys.exit("svnadmin not found in PATH")

    gens = {
        "small": gen_small,
        "wide": gen_wide,
        "deep": gen_deep,
        "history": lambda out: gen_history(out, args.history_revs),
    }
    wanted = [n.strip() for n in args.only.split(",") if n.strip()]
    for n in wanted:
        if n not in gens:
            sys.exit("unknown repo %r (choose from %s)" % (n, ", ".join(gens)))

    os.makedirs(args.outdir, exist_ok=True)
    for name in wanted:
        path = os.path.join(args.outdir, name)
        if os.path.exists(path):
            if not args.force:
                print("%-8s exists, skipping (use --force to regenerate)" % name)
                continue
            shutil.rmtree(path)
        tmp = path + ".tmp"
        if os.path.exists(tmp):
            shutil.rmtree(tmp)
        t0 = time.monotonic()
        print("%-8s generating ..." % name, flush=True)
        info = {}

        def run(out, _g=gens[name]):
            info.update(_g(out) or {})

        load_repo(tmp, run)
        if not args.no_pack:
            subprocess.run(["svnadmin", "pack", "--quiet", tmp], check=True)
        # Make sure a server running as another uid can read the repo.
        for root, dirs, files in os.walk(tmp):
            for x in dirs:
                os.chmod(os.path.join(root, x), 0o755)
            for x in files:
                p = os.path.join(root, x)
                os.chmod(p, os.stat(p).st_mode | 0o444)
        os.rename(tmp, path)
        el = time.monotonic() - t0
        print("%-8s done: r%d, %.1f MB, %.1fs" % (
            name, youngest(path), du(path) / 1e6, el), flush=True)
        for k, v in info.items():
            if k != "last":
                print("           %s = %s" % (k, v))


if __name__ == "__main__":
    main()
