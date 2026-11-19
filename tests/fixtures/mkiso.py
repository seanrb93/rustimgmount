"""`rustimgmount create <folder>`: makes an ISO image from a folder and reads it back with
a small ISO 9660 / Joliet reader, checking that every file comes out as it went in.

Usage: mkiso.py <rustimgmount.exe> <work dir>"""
import os, shutil, struct, subprocess, sys

EXE, D = sys.argv[1], sys.argv[2]
shutil.rmtree(D, ignore_errors=True)
os.makedirs(D)
fails = 0
def report(name, err, ok="ok"):
    global fails
    print(("PASS" if not err else "FAIL"), "%-28s %s" % (name, err or ok))
    fails += bool(err)
def create(*args, env=None):
    r = subprocess.run([EXE, "create", *args], capture_output=True, text=True, env=env)
    lines = (r.stderr + r.stdout).strip().splitlines()
    return r.returncode, [l for l in lines if "%" not in l]
def write(path, data):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "wb") as f: f.write(data)
def pattern(n, seed):
    return bytes((i * 31 + seed + (i >> 8)) & 255 for i in range(n))

# ---- reader

def descriptors(img):
    out, lba = {}, 16
    while True:
        d = img[lba * 2048:(lba + 1) * 2048]
        assert d[1:6] == b"CD001", "no volume descriptor at sector %d" % lba
        if d[0] == 255: return out
        out["joliet" if d[0] == 2 and d[88:91] == b"%/E" else "plain" if d[0] == 1 else d[0]] = d
        lba += 1

def both32(b, at):
    le, be = struct.unpack_from("<I", b, at)[0], struct.unpack_from(">I", b, at + 4)[0]
    assert le == be, "the two byte orders of a number disagree"
    return le

def records(img, lba, length):
    """(name bytes, lba, length, flags) of a folder's listing, without '.' and '..'."""
    data, pos, out = img[lba * 2048:lba * 2048 + length], 0, []
    assert length % 2048 == 0 and len(data) == length, "a listing is not whole sectors"
    while pos < length:
        l = data[pos]
        if l == 0:
            pos = (pos // 2048 + 1) * 2048
            continue
        assert pos % 2048 + l <= 2048, "a record crosses a sector boundary"
        n = data[pos + 32]
        out.append((data[pos + 33:pos + 33 + n], both32(data, pos + 2), both32(data, pos + 10), data[pos + 25]))
        pos += l
    assert [r[0] for r in out[:2]] == [b"\0", b"\1"], "a listing does not start with '.' and '..'"
    return out[2:]

def tree(img, d, joliet):
    """{path: bytes or None for a folder} of one tree, plus the names in recorded order."""
    files, order = {}, []
    def walk(lba, length, prefix):
        pending, names = None, []
        for name, l, n, flags in records(img, lba, length):
            text = name.decode("utf-16-be") if joliet else name.decode("ascii")
            if flags & 2:
                names.append(name)
                files[prefix + text] = None
                walk(l, n, prefix + text + "/")
                continue
            assert text.endswith(";1"), "a file name has no version: %r" % text
            path = prefix + text[:-2]
            data = img[l * 2048:l * 2048 + n]
            assert len(data) == n, "a file runs past the end of the image"
            if pending is None: names.append(name)
            pending = (pending or b"") + data
            if flags & 0x80: continue          # more extents of the same file follow
            files[path] = pending
            pending = None
        assert pending is None, "the last extent of a file is missing"
        order.append(names)
    walk(both32(d, 158), both32(d, 166), "")
    return files, order

def path_table(img, d, joliet):
    """[(name, lba, parent number)] from both copies of a path table, which must agree."""
    size = both32(d, 132)
    out = []
    for fmt, at in (("<", struct.unpack_from("<I", d, 140)[0]), (">", struct.unpack_from(">I", d, 148)[0])):
        t, pos, rows = img[at * 2048:at * 2048 + size], 0, []
        while pos < size:
            n = t[pos]
            lba, parent = struct.unpack_from(fmt + "IH", t, pos + 2)
            rows.append((t[pos + 8:pos + 8 + n], lba, parent))
            pos += 8 + n + n % 2
        out.append(rows)
    assert out[0] == out[1], "the two copies of the path table disagree"
    return out[0]

def check(iso, expect, hidden=()):
    """Compare both trees of `iso` with {path: bytes or None}."""
    with open(iso, "rb") as f: img = f.read()
    try:
        d = descriptors(img)
        if len(img) % 2048 or both32(d["plain"], 80) * 2048 != len(img): return "the image's size is not what it says"
        got, order = tree(img, d["joliet"], True)
        if set(got) != set(expect): return "names differ: %s" % sorted(set(got) ^ set(expect))[:4]
        bad = [p for p in expect if got[p] != expect[p]]
        if bad: return "contents differ: %s" % bad[:4]
        plain, plain_order = tree(img, d["plain"], False)
        if sorted(v for v in plain.values() if v is not None) != sorted(v for v in expect.values() if v is not None):
            return "the plain ISO 9660 tree holds different files"
        if len(plain) != len(expect): return "the plain tree has %d entries, not %d" % (len(plain), len(expect))
        for p in plain:
            for part in p.split("/"):
                stem, _, ext = part.partition(".")
                if not (0 < len(stem) <= 8 and len(ext) <= 3 and all(c.isupper() or c.isdigit() or c == "_" for c in stem + ext)):
                    return "not an 8.3 name: %s" % p
        for names in order + plain_order:
            if names != sorted(names): return "a listing is not in order"
        for key, joliet in (("plain", False), ("joliet", True)):
            rows = path_table(img, d[key], joliet)
            folders = 1 + sum(v is None for v in expect.values())
            if len(rows) != folders: return "the path table lists %d folders, not %d" % (len(rows), folders)
            if rows[0][0] != b"\0" or rows[0][1] != both32(d[key], 158): return "the path table does not start at the root"
            if any(not 1 <= parent <= i + 1 for i, (_, _, parent) in enumerate(rows)): return "a folder comes before its parent in the path table"
        if hidden:
            flags = {n.decode("utf-16-be"): f for n, _, _, f in records(img, both32(d["joliet"], 158), both32(d["joliet"], 166))}
            if any(not flags[h + ";1"] & 1 for h in hidden): return "a hidden file is not marked hidden"
    except AssertionError as e:
        return str(e)
    return None

# ---- a folder with a bit of everything

src = os.path.join(D, "Holiday photos")
expect = {
    "readme.txt": b"hello\r\n",
    "empty.bin": b"",
    "exactly one sector.dat": pattern(2048, 1),
    "big.bin": pattern(300_000, 2),
    "a long name with spaces and several.dots.in.it.tar.gz": pattern(5000, 3),
    "日本語のファイル.txt": "こんにちは".encode("utf-8"),
    "emoji \U0001F600.txt": b"smile",
    "docs": None,
    "docs/notes.txt": pattern(70, 4),
    "docs/deep": None,
    "docs/deep/deeper": None,
    "docs/deep/deeper/deepest": None,
    "docs/deep/deeper/deepest/file.txt": pattern(4097, 5),
    "empty folder": None,
    "many": None,
    "x" * 103: b"longest name allowed",
}
# Enough files with the same first eight letters to fill several sectors of a listing
# and to need many different short names.
for i in range(150):
    expect["many/screenshot %03d of the garden.png" % i] = pattern(10 + i, i)
for path, data in expect.items():
    full = os.path.join(src, *path.split("/"))
    if data is None: os.makedirs(full, exist_ok=True)
    else: write(full, data)
subprocess.run(["attrib", "+h", os.path.join(src, "readme.txt")], check=True)

iso = os.path.join(D, "Holiday photos.iso")
code, lines = create(src)
err = "failed: %s" % lines if code else None if os.path.exists(iso) else "no %s" % iso
report("folder to ISO", err or check(iso, expect, hidden=["readme.txt"]), "%d entries, both trees read back" % len(expect))

with open(iso, "rb") as f:
    f.seek(17 * 2048 + 40)
    label = f.read(32).decode("utf-16-be").rstrip()
    f.seek(16 * 2048 + 40)
    plain_label = f.read(32).decode("ascii").rstrip()
report("disc name", None if (label, plain_label) == ("Holiday photos", "HOLIDAY_PHOTOS") else "got %r and %r" % (label, plain_label), "taken from the folder")

# The result is an ordinary data disc to the rest of the program.
r = subprocess.run([EXE, "info", iso], capture_output=True, text=True)
report("info on the result", None if r.returncode == 0 and "iso" in (r.stdout + r.stderr).lower() else "info failed: %s" % (r.stdout + r.stderr).strip()[-200:])

# ---- names for the output

code, lines = create(src)
second = os.path.join(D, "Holiday photos (2).iso")
report("existing ISO is kept", None if code == 0 and os.path.exists(second) else "no 'Holiday photos (2).iso': %s" % lines, "the new one gets a number")
named = os.path.join(D, "named.iso")
code, lines = create(src, named, "--label", "Summer 2024")
with open(named, "rb") as f:
    f.seek(17 * 2048 + 40)
    label = f.read(32).decode("utf-16-be").rstrip()
report("--label and output name", None if code == 0 and label == "Summer 2024" else "label %r: %s" % (label, lines))
before = os.path.getmtime(named)
code, lines = create(src, named)
report("refuses to overwrite", None if code != 0 and any("already exists" in l for l in lines) and os.path.getmtime(named) == before else "exit %d: %s" % (code, lines))
code, lines = create(src, named, "--overwrite")
report("--overwrite", None if code == 0 else "failed: %s" % lines)
code, lines = create(iso)
report("refuses a file", None if code != 0 and any("not a folder" in l for l in lines) else "exit %d: %s" % (code, lines))

# An image written into the folder it is made from must not end up inside itself.
inside = os.path.join(src, "self.iso")
code, lines = create(src, inside)
err = "failed: %s" % lines if code else check(inside, expect, hidden=["readme.txt"])
report("output inside the folder", err, "left out of its own contents")
os.remove(inside)

# ---- files too big for one extent

# (The real limit is 4 GiB; the test hook lowers it to two sectors.)
env = dict(os.environ, RUSTIMGMOUNT_TEST_EXTENT="2")
multi = os.path.join(D, "multi.iso")
code, lines = create(src, multi, env=env)
report("files in several extents", ("failed: %s" % lines) if code else check(multi, expect, hidden=["readme.txt"]), "read back whole")

# ---- names an ISO can't hold

odd = os.path.join(D, "odd")
write(os.path.join(odd, "a;b.txt"), b"semicolon")
write(os.path.join(odd, "ok.txt"), b"fine")
code, lines = create(odd)
err = "failed: %s" % lines if code else check(os.path.join(D, "odd.iso"), {"a_b.txt": b"semicolon", "ok.txt": b"fine"})
report("';' in a name", err or (None if any("';'" in l for l in lines) else "no note about the changed name"), "written as '_', with a note")
write("\\\\?\\" + os.path.abspath(os.path.join(odd, "y" * 104 + ".txt")), b"too long")
code, lines = create(odd, os.path.join(D, "odd2.iso"))
left = [f for f in os.listdir(D) if f.startswith("odd2")]
report("name too long", None if code != 0 and any("longer than 103" in l for l in lines) and not left else "exit %d, files %s: %s" % (code, left, lines), "refused, nothing left behind")

empty = os.path.join(D, "nothing here")
os.makedirs(empty)
code, lines = create(empty)
report("empty folder", ("failed: %s" % lines) if code else check(os.path.join(D, "nothing here.iso"), {}))

print("failures:", fails)
sys.exit(1 if fails else 0)
