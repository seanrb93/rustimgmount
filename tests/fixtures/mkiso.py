"""`rustimgmount create <folder>`: makes an ISO image from a folder and reads it back with
a small ISO 9660 / Joliet reader, checking that every file comes out as it went in.

Usage: mkiso.py <rustimgmount.exe> <work dir>"""
import ctypes, functools, os, shutil, struct, subprocess, sys

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

def by_standard(a, b, joliet):
    """ISO 9660's order: the name, then the extension, each as if padded with spaces."""
    def parts(raw):
        units = list(struct.unpack(">%dH" % (len(raw) // 2), raw)) if joliet else list(raw)
        if units[-2:] != [0x3B, 0x31]: return units, []           # a folder: no version, no extension
        units = units[:-2]
        dot = units.index(0x2E) if 0x2E in units else len(units)
        return units[:dot], units[dot + 1:]
    for x, y in zip(parts(a), parts(b)):
        n = max(len(x), len(y))
        x, y = x + [0x20] * (n - len(x)), y + [0x20] * (n - len(y))
        if x != y: return -1 if x < y else 1
    return 0

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
            if names != sorted(names, key=functools.cmp_to_key(lambda a, b: by_standard(a, b, names is not None and names in order))): return "a listing is not in order"
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

# ---- UDF reader

def crc16(data):
    c = 0
    for b in data:
        c ^= b << 8
        for _ in range(8):
            c = ((c << 1) ^ 0x1021) & 0xFFFF if c & 0x8000 else (c << 1) & 0xFFFF
    return c

def tagged(img, sector, want, at=None):
    """The descriptor at `sector` (or at byte offset `at`), after checking its tag."""
    off = sector * 2048 if at is None else at
    ident, version, checksum, _, _, crc, crc_len, location = struct.unpack_from("<HHBBHHHI", img, off)
    assert ident == want, "UDF: expected descriptor %d, found %d at sector %d" % (want, ident, sector)
    assert version == 2, "UDF: descriptor version %d" % version
    head = img[off:off + 16]
    assert sum(head[:4] + head[5:]) & 255 == checksum, "UDF: wrong tag checksum in descriptor %d" % want
    body = img[off + 16:off + 16 + crc_len]
    assert crc16(body) == crc, "UDF: wrong CRC in descriptor %d" % want
    return img[off:off + 16 + crc_len], location

def d_chars(b):
    if not b: return ""
    assert b[0] in (8, 16), "UDF: unknown character width %d" % b[0]
    return b[1:].decode("latin-1") if b[0] == 8 else b[1:].decode("utf-16-be")

def udf_tree(img):
    """{path: bytes or None for a folder} of the UDF file system, plus its volume name."""
    assert img[19 * 2048 + 1:19 * 2048 + 6] == b"BEA01" and img[20 * 2048 + 1:20 * 2048 + 6] == b"NSR02" and img[21 * 2048 + 1:21 * 2048 + 6] == b"TEA01", "UDF: no recognition sequence"
    last = len(img) // 2048 - 1
    anchor, loc = tagged(img, 256, 2)
    assert loc == 256 and tagged(img, last, 2)[1] == last, "UDF: anchor in the wrong place"
    length, start = struct.unpack_from("<II", anchor, 16)
    part = fsd_at = label = None
    for copy in (start, struct.unpack_from("<I", anchor, 28)[0]):
        seen = []
        for s in range(copy, copy + length // 2048):
            ident = struct.unpack_from("<H", img, s * 2048)[0]
            d, loc = tagged(img, s, ident)
            assert loc == s, "UDF: a descriptor says it is at %d but is at %d" % (loc, s)
            seen.append(ident)
            if ident == 5: part = struct.unpack_from("<II", d, 188)
            if ident == 6:
                fsd_at = struct.unpack_from("<I", d, 252)[0]
                label = d_chars(d[84:84 + d[211]])
                assert struct.unpack_from("<I", d, 212)[0] == 2048, "UDF: block size"
            if ident == 8: break
        assert seen == [1, 4, 5, 6, 7, 8], "UDF: volume descriptors %s" % seen
    lvid, _ = tagged(img, 64, 9)
    assert struct.unpack_from("<I", lvid, 28)[0] == 1, "UDF: volume not marked closed"
    p0, plen = part
    assert p0 + plen == last, "UDF: the partition does not end before the last sector"
    fsd, loc = tagged(img, p0 + fsd_at, 256)
    assert loc == fsd_at
    root = struct.unpack_from("<I", fsd, 404)[0]
    files = {}
    counts = [0, 0]
    def entry(lbn):
        fe, loc = tagged(img, p0 + lbn, 261)
        assert loc == lbn, "UDF: a file entry says it is at block %d but is at %d" % (loc, lbn)
        size = struct.unpack_from("<Q", fe, 56)[0]
        l_ea, l_ad = struct.unpack_from("<II", fe, 168)
        data = b""
        for i in range(l_ad // 8):
            n, pos = struct.unpack_from("<II", fe, 176 + l_ea + i * 8)
            assert n >> 30 == 0, "UDF: an extent that is not recorded data"
            data += img[(p0 + pos) * 2048:(p0 + pos) * 2048 + n]
        assert len(data) == size, "UDF: a file's extents add up to %d bytes, not %d" % (len(data), size)
        return fe[27], data, struct.unpack_from("<Q", fe, 160)[0]
    ids = set()
    def walk(lbn, parent, prefix):
        kind, data, unique = entry(lbn)
        assert kind == 4, "UDF: a folder's entry is not a folder"
        counts[1] += 1
        first_block = struct.unpack_from("<I", tagged(img, p0 + lbn, 261)[0], 180)[0]
        pos, first = 0, True
        while pos < len(data):
            n_id, l_iu = data[pos + 19], struct.unpack_from("<H", data, pos + 36)[0]
            size = (38 + l_iu + n_id + 3) & ~3
            fid, loc = tagged(img, 0, 257, at=(p0 + first_block) * 2048 + pos)
            assert loc == first_block + pos // 2048, "UDF: a directory record carries the wrong block number"
            flags, target = data[pos + 18], struct.unpack_from("<I", data, pos + 24)[0]
            name = d_chars(data[pos + 38 + l_iu:pos + 38 + l_iu + n_id])
            if first:
                assert flags & 8 and target == parent and not name, "UDF: a folder does not start with its parent"
            elif flags & 2:
                files[prefix + name] = None
                walk(target, lbn, prefix + name + "/")
            else:
                kind, content, unique = entry(target)
                assert kind == 5, "UDF: a file's entry is not a file"
                assert unique not in ids, "UDF: two files share an id"
                ids.add(unique)
                files[prefix + name] = content
                counts[0] += 1
            first = False
            pos += size
    walk(root, root, "")
    n_files, n_dirs = struct.unpack_from("<II", lvid, 120)
    assert (n_files, n_dirs) == tuple(counts), "UDF: the volume says %d files and %d folders, the tree has %s" % (n_files, n_dirs, counts)
    return files, label

def check_udf(iso, expect, label=None):
    with open(iso, "rb") as f: img = f.read()
    try:
        got, name = udf_tree(img)
    except AssertionError as e:
        return str(e)
    if set(got) != set(expect): return "UDF names differ: %s" % [n[:40] for n in sorted(set(got) ^ set(expect))[:4]]
    bad = [p for p in expect if got[p] != expect[p]]
    if bad: return "UDF contents differ: %s" % bad[:4]
    if label is not None and name != label: return "UDF volume name %r, not %r" % (name, label)
    return None

def has_udf(iso):
    with open(iso, "rb") as f:
        f.seek(20 * 2048 + 1)
        return f.read(5) == b"NSR02"

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

# Names that only clash once they are changed or folded: both files are kept.
clash = os.path.join(D, "clash")
clash_expect = {"a;b.txt": b"semicolon", "a_b.txt": b"underscore", "k.txt": b"small k", "\u212a.txt": b"kelvin sign",
                "a.txt": b"plain", "a-b.txt": b"dash", "a b.txt": b"space", "a!.txt": b"bang", "ab.txt": b"ab"}
for name, data in clash_expect.items(): write(os.path.join(clash, name), data)
code, lines = create(clash)
if code: err = "failed: %s" % lines
else:
    with open(os.path.join(D, "clash.iso"), "rb") as f: img = f.read()
    got, _ = tree(img, descriptors(img)["joliet"], True)
    want = dict(clash_expect); want["a_b~1.txt"] = want.pop("a;b.txt")
    err = None if got == want else "got %s" % sorted(got)
    err = err or check(os.path.join(D, "clash.iso"), want)
report("names that would clash", err, "both kept, in the standard's order")

# ---- reading the image back

checked = os.path.join(D, "checked.iso")
code, lines = create(src, checked, "--verify")
report("--verify", None if code == 0 and any("Verified" in l for l in lines) else "exit %d: %s" % (code, lines[-2:]))
code, lines = create(src, checked, "--verify", "--overwrite", env=env)
report("--verify, files in several extents", None if code == 0 else "exit %d: %s" % (code, lines[-2:]))
code, lines = create(longs_early := os.path.join(D, "Holiday photos"), os.path.join(D, "checked udf.iso"), "--verify", "--udf")
report("--verify with UDF", None if code == 0 else "exit %d: %s" % (code, lines[-2:]))
damaged = os.path.join(D, "damaged.iso")
code, lines = create(src, damaged, "--verify", env=dict(os.environ, RUSTIMGMOUNT_TEST_DAMAGE="1"))
left = [f for f in os.listdir(D) if f.startswith("damaged")]
report("--verify notices damage", None if code != 0 and any("big.bin differs" in l for l in lines) and not left else "exit %d, files %s: %s" % (code, left, lines[-2:]), "and the image is deleted")
code, lines = create(src, os.path.join(D, "checked.chd"), "--verify")
report("--verify on a packed image", None if code == 0 and any("Verified" in l for l in lines) else "exit %d: %s" % (code, lines[-2:]))

# A folder made for the result inside the folder being imaged is not part of the image.
inner = os.path.join(src, "made here", "deeper")
code, lines = create(src, "--out-dir", inner)
err = "failed: %s" % lines if code else check(os.path.join(inner, "Holiday photos.iso"), expect, hidden=["readme.txt"])
report("--out-dir inside the folder", err, "left out of the image")
shutil.rmtree(os.path.join(src, "made here"))
# ---- names too long for Joliet: the image gets the UDF file system as well

def long_path(*parts):
    return "\\\\?\\" + os.path.abspath(os.path.join(*parts))
longs = os.path.join(D, "long names")
long_expect = {
    "short.txt": b"short",
    "L" * 150 + ".txt": b"long latin",
    "L" * 150 + ".dat": b"long latin, same start",
    "\u65e5" * 110 + ".txt": b"long cjk",
    "semi;colon.txt": b"semicolon",
    "sub": None,
    "sub/" + "d" * 200: None,
    "sub/" + "d" * 200 + "/inside.bin": pattern(5000, 9),
}
for path, data in long_expect.items():
    full = long_path(longs, *path.split("/"))
    if data is None: os.makedirs(full, exist_ok=True)
    else:
        os.makedirs(os.path.dirname(full), exist_ok=True)
        with open(full, "wb") as f: f.write(data)
long_iso = os.path.join(D, "long names.iso")
code, lines = create(longs)
err = "failed: %s" % lines if code else check_udf(long_iso, long_expect, "long names")
if not err:
    # The older trees carry the same files under shortened names.
    with open(long_iso, "rb") as f: img = f.read()
    joliet, _ = tree(img, descriptors(img)["joliet"], True)
    if sorted(v for v in joliet.values() if v is not None) != sorted(v for v in long_expect.values() if v is not None): err = "the Joliet tree holds different files"
    elif max(len(p.split("/")[-1].encode("utf-16-le")) // 2 for p in joliet) > 103: err = "a Joliet name is longer than 103 characters"
    elif not any("UDF" in l for l in lines): err = "no note about UDF"
report("names over 103 characters", err, "kept whole in a UDF tree")

report("no UDF unless needed", "the ordinary image has UDF" if has_udf(iso) else None)
forced = os.path.join(D, "forced udf.iso")
code, lines = create(src, forced, "--udf")
report("--udf on the big folder", ("failed: %s" % lines) if code else check_udf(forced, expect, "Holiday photos") or check(forced, expect, hidden=["readme.txt"]), "all three trees read back")
code, lines = create(src, forced, "--udf", "--overwrite", env=env)
report("UDF files in several extents", ("failed: %s" % lines) if code else check_udf(forced, expect))

for name, what in (("y" * 251 + ".txt", "255 Latin characters"), ("\u65e5" * 124 + ".txt", "128 other characters")):
    too = os.path.join(D, "too long")
    shutil.rmtree(long_path(too), ignore_errors=True)
    os.makedirs(too)
    with open(long_path(too, name), "wb") as f: f.write(b"too long")
    code, lines = create(too)
    left = [f for f in os.listdir(D) if f.startswith("too long.")]
    report("refuses " + what, None if code != 0 and any("too long" in l for l in lines) and not left else "exit %d, files %s: %s" % (code, left, lines[-2:]), "nothing left behind")

# ---- files that can't be read

locked = os.path.join(D, "locked")
write(os.path.join(locked, "fine.txt"), b"fine")
write(os.path.join(locked, "in use.dat"), b"held open by another program")
k32 = ctypes.WinDLL("kernel32", use_last_error=True)
k32.CreateFileW.restype = ctypes.c_void_p
k32.CreateFileW.argtypes = [ctypes.c_wchar_p, ctypes.c_uint32, ctypes.c_uint32, ctypes.c_void_p, ctypes.c_uint32, ctypes.c_uint32, ctypes.c_void_p]
k32.CloseHandle.argtypes = [ctypes.c_void_p]
# Opened for writing with no sharing, as a program working on the file would.
handle = k32.CreateFileW(os.path.join(locked, "in use.dat"), 0xC0000000, 0, None, 3, 0, None)
try:
    code, lines = create(locked)
    left = [f for f in os.listdir(D) if f.startswith("locked.")]
    ok = code != 0 and any("in use.dat" in l for l in lines) and any("--skip-unreadable" in l for l in lines) and not left
    report("unreadable file", None if ok else "exit %d, files %s: %s" % (code, left, lines), "refused before writing, and named")
    code, lines = create(locked, "--skip-unreadable")
    err = "failed: %s" % lines if code else check(os.path.join(D, "locked.iso"), {"fine.txt": b"fine"})
    report("--skip-unreadable", err or (None if any("in use.dat" in l for l in lines) else "the file left out is not named"), "left out, and named")
finally:
    k32.CloseHandle(handle)

# ---- packed formats, and where the result goes

def convert_back(packed, name):
    back = os.path.join(D, name)
    r = subprocess.run([EXE, "convert", packed, back], capture_output=True, text=True)
    return back if r.returncode == 0 else None
for fmt, ext in (("cso", "cso"), ("zso", "zso"), ("chd", "chd"), ("chd-zstd", "chd")):
    packed = os.path.join(D, "packed %s.%s" % (fmt, ext))
    code, lines = create(src, packed, "--to", fmt, "--label", "Packed")
    back = None if code else convert_back(packed, "packed %s back.iso" % fmt)
    err = "failed: %s" % lines if code else "could not be converted back" if not back else check(back, expect, hidden=["readme.txt"])
    if not err:
        with open(back, "rb") as f:
            f.seek(17 * 2048 + 40)
            if f.read(32).decode("utf-16-be").rstrip() != "Packed": err = "the disc name was lost"
    stray = [f for f in os.listdir(D) if "temp" in f or f.endswith(".partial")]
    report("folder to " + fmt, err or ("left behind: %s" % stray if stray else None), "converts back to the same files")
code, lines = create(src, "--to", "cso")
report("--to names the result", None if code == 0 and os.path.exists(os.path.join(D, "Holiday photos.cso")) else "no 'Holiday photos.cso': %s" % lines)
code, lines = create(src, os.path.join(D, "by extension.cso"))
with open(os.path.join(D, "by extension.cso"), "rb") as f: magic = f.read(4)
report("format from the output name", None if code == 0 and magic == b"CISO" else "exit %d, starts with %r" % (code, magic))
code, lines = create(src, "--to", "vhd")
report("refuses a disk format", None if code != 0 and any("iso, cso" in l for l in lines) else "exit %d: %s" % (code, lines))
elsewhere = os.path.join(D, "elsewhere", "deeper")
code, lines = create(src, "--out-dir", elsewhere)
report("--out-dir", None if code == 0 and os.path.exists(os.path.join(elsewhere, "Holiday photos.iso")) else "exit %d: %s" % (code, lines))
# Explorer passes a drive as "D:\" and the backslash swallows the closing quote.
code, lines = create(src + '"', os.path.join(D, "quote.iso"))
report("stray quote from Explorer", None if code == 0 else "exit %d: %s" % (code, lines))

# ---- a packed image cancelled half-way, and two runs after the same file

import time
slow = os.path.join(D, "slow")
os.makedirs(slow)
for i in range(4):
    with open(os.path.join(slow, "noise %d.bin" % i), "wb") as f: f.write(os.urandom(48 << 20))
slow_out = os.path.join(D, "slow.chd")
temp_iso = slow_out + ".rustimgmount-temp.iso.partial"
first = subprocess.Popen([EXE, "create", slow, slow_out], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
deadline = time.time() + 60
while not os.path.exists(slow_out + ".partial") and first.poll() is None and time.time() < deadline: time.sleep(0.005)
if first.poll() is not None or not os.path.exists(slow_out + ".partial"):
    report("cancel while packing", "the first run ended before it could be cancelled")
else:
    # A second run for the same file must not write into the first one's files.
    code, lines = create(slow, slow_out)
    report("second run for the same file", None if code != 0 and any("already being written" in l for l in lines) else "exit %d: %s" % (code, lines[-2:]), "refused")
    still = os.path.exists(temp_iso) and first.poll() is None
    first.kill(); first.wait()
    subprocess.run([EXE, "list"], capture_output=True)      # any command tidies up
    left = [f for f in os.listdir(D) if f.startswith("slow.")]
    err = "the first run's files were gone before it was cancelled" if not still else ("left behind: %s" % left if left else None)
    report("cancel while packing", err, "the temporary ISO and the half-made CHD are both removed")
shutil.rmtree(slow)

empty = os.path.join(D, "nothing here")
os.makedirs(empty)
code, lines = create(empty)
report("empty folder", ("failed: %s" % lines) if code else check(os.path.join(D, "nothing here.iso"), {}))

print("failures:", fails)
sys.exit(1 if fails else 0)
