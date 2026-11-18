"""Build emulation-format fixtures, convert each with rustimgmount, verify the result
by reading the ISO 9660 file system like an OS would."""
import gzip, lzma, os, shutil, struct, subprocess, sys, zipfile, zlib

EXE, MKCHD, D = sys.argv[1], sys.argv[2], sys.argv[3]
shutil.rmtree(D, ignore_errors=True)
os.makedirs(D)
OUT = os.path.join(D, "out")
os.makedirs(OUT)
S = 2048

def p(n): return os.path.join(D, n)
def w(n, data):
    with open(p(n), "wb") as f: f.write(data)

# ---------------------------------------------------------------- ISO builder
def both16(v): return struct.pack("<H", v) + struct.pack(">H", v)
def both32(v): return struct.pack("<I", v) + struct.pack(">I", v)
def dirrec(name, lba, size, flags):
    n = len(name)
    r = bytearray(33 + n + ((n + 1) % 2))
    r[0] = len(r); r[2:10] = both32(lba); r[10:18] = both32(size)
    r[18:25] = bytes([124, 1, 1, 0, 0, 0, 0]); r[25] = flags; r[28:32] = both16(1); r[32] = n; r[33:33 + n] = name
    return bytes(r)

HELLO = b"Hello from an emulation image!\r\n" * 10

def make_iso(total, base=0, label=b"TESTISO", big=None):
    """ISO whose sector numbers start at `base` (absolute addressing, like a 2nd session
    or GD-ROM high-density area). `big` = (lba, bytes) adds BIG.DAT outside this image."""
    img = bytearray(S * total)
    pvd = bytearray(S)
    pvd[0] = 1; pvd[1:6] = b"CD001"; pvd[6] = 1
    pvd[8:40] = b" " * 32; pvd[40:72] = label.ljust(32)
    pvd[80:88] = both32(base + total + (len(big[1]) // S + 1 if big else 0))
    pvd[120:124] = both16(1); pvd[124:128] = both16(1); pvd[128:132] = both16(S); pvd[132:140] = both32(10)
    pvd[140:144] = struct.pack("<I", base + 18); pvd[148:152] = struct.pack(">I", base + 19)
    pvd[156:190] = dirrec(b"\x00", base + 20, S, 2)
    for o, l in ((190, 128), (318, 128), (446, 128), (574, 128), (702, 37), (739, 37), (776, 37)):
        pvd[o:o + l] = b" " * l
    for o in (813, 830): pvd[o:o + 17] = b"2024010100000000\x00"
    for o in (847, 864): pvd[o:o + 17] = b"0000000000000000\x00"
    pvd[881] = 1
    img[16 * S:17 * S] = pvd
    t = bytearray(S); t[0] = 255; t[1:6] = b"CD001"; t[6] = 1
    img[17 * S:18 * S] = t
    img[18 * S:18 * S + 10] = bytes([1, 0]) + struct.pack("<I", base + 20) + struct.pack("<H", 1) + b"\0\0"
    img[19 * S:19 * S + 10] = bytes([1, 0]) + struct.pack(">I", base + 20) + struct.pack(">H", 1) + b"\0\0"
    root = dirrec(b"\x00", base + 20, S, 2) + dirrec(b"\x01", base + 20, S, 2) + dirrec(b"HELLO.TXT;1", base + 21, len(HELLO), 0)
    if big:
        root += dirrec(b"BIG.DAT;1", big[0], len(big[1]), 0)
    img[20 * S:20 * S + len(root)] = root
    img[21 * S:21 * S + len(HELLO)] = HELLO
    for i in range(22, total):
        img[i * S:i * S + 16] = struct.pack("<QQ", base + i, 0xC0FFEE)
    return bytes(img)

def bcd(n): return ((n // 10) << 4) | (n % 10)
def raw(iso, mode=1, lba0=0):
    out = bytearray()
    for i in range(len(iso) // S):
        a = lba0 + i + 150
        hdr = b"\x00" + b"\xff" * 10 + b"\x00" + bytes([bcd(a // 4500), bcd((a // 75) % 60), bcd(a % 75), mode])
        d = iso[i * S:(i + 1) * S]
        out += hdr + d + b"\0" * 288 if mode == 1 else hdr + b"\0\0\x08\0\0\0\x08\0" + d + b"\0" * 280
    return bytes(out)

iso = make_iso(400)
w("test.iso", iso)
mode1 = raw(iso, 1)
mode2 = raw(iso, 2)
w("mode1.bin", mode1)
w("mode2.bin", mode2)

# ---------------------------------------------------------------- ISO reader (verification)
def check_iso(data, want_big=None):
    pvd = data[16 * S:17 * S]
    if pvd[1:6] != b"CD001" or pvd[0] != 1:
        return "no PVD at sector 16"
    root = struct.unpack("<I", pvd[158:162])[0]
    d = data[root * S:(root + 1) * S]
    found = {}
    o = 0
    while o < S and d[o]:
        l = d[o]; n = d[o + 32]; name = d[o + 33:o + 33 + n]
        found[name] = struct.unpack("<II", d[o + 2:o + 6] + d[o + 10:o + 14])
        o += l
    if b"HELLO.TXT;1" not in found:
        return "HELLO.TXT missing (root at %d, entries %s)" % (root, list(found))
    lba, size = found[b"HELLO.TXT;1"]
    if data[lba * S:lba * S + size] != HELLO:
        return "HELLO.TXT content wrong (lba %d)" % lba
    if want_big is not None:
        if b"BIG.DAT;1" not in found:
            return "BIG.DAT missing"
        lba, size = found[b"BIG.DAT;1"]
        if data[lba * S:lba * S + size] != want_big:
            return "BIG.DAT content wrong (lba %d)" % lba
    return None

# ---------------------------------------------------------------- LZ4 (tiny encoder: literals + run-length matches)
def lz4_block(data):
    out = bytearray(); i = 0; lit_start = 0; n = len(data)
    def emit(lits, mlen):
        tok_l = min(len(lits), 15); tok_m = 0 if mlen is None else min(mlen - 4, 15)
        out.append((tok_l << 4) | tok_m)
        if tok_l == 15:
            r = len(lits) - 15
            while r >= 255: out.append(255); r -= 255
            out.append(r)
        out.extend(lits)
        if mlen is not None:
            out.extend(struct.pack("<H", 1))
            if tok_m == 15:
                r = mlen - 4 - 15
                while r >= 255: out.append(255); r -= 255
                out.append(r)
    while i < n - 12:
        j = i + 1
        while j < n - 5 and data[j] == data[i]: j += 1
        run = j - (i + 1)
        if run >= 8:
            emit(data[lit_start:i + 1], run)
            i = j; lit_start = i
        else:
            i += 1
    emit(data[lit_start:], None)
    return bytes(out)

def ciso(data, magic, ver, block=2048, align=0, mode="deflate"):
    n = (len(data) + block - 1) // block
    hdr = magic + struct.pack("<IQIBBH", 24, len(data), block, ver, align, 0)
    body = bytearray(); idx = []
    pos0 = 24 + (n + 1) * 4
    for b in range(n):
        chunk = data[b * block:(b + 1) * block].ljust(block, b"\0")
        pos = pos0 + len(body)
        pad = (-pos) % (1 << align); body += b"\0" * pad; pos += pad
        if mode == "lz4":
            c = lz4_block(chunk)
        else:
            co = zlib.compressobj(9, zlib.DEFLATED, -15); c = co.compress(chunk) + co.flush()
        flag = 0
        if magic == b"ZISO" or (magic == b"CISO" and ver == 1):
            if len(c) >= block: c = chunk; flag = 0x80000000
        else:  # CSO v2: raw if size >= block, bit31 = LZ4
            if len(c) >= block: c = chunk
            elif mode == "lz4": flag = 0x80000000
        idx.append((pos >> align) | flag)
        body += c
    end = pos0 + len(body); end += (-end) % (1 << align); body += b"\0" * ((-(pos0 + len(body))) % (1 << align))
    idx.append(end >> align)
    return hdr + b"".join(struct.pack("<I", x) for x in idx) + bytes(body)

w("game.cso", ciso(iso, b"CISO", 1))
w("game_align.cso", ciso(iso, b"CISO", 1, align=2))
w("game_v2.cso", ciso(iso, b"CISO", 2, mode="lz4"))
w("game.zso", ciso(iso, b"ZISO", 1, mode="lz4"))

def dax(data, nc=None):
    B = 8192; n = (len(data) + B - 1) // B
    comps = []
    for b in range(n):
        chunk = data[b * B:(b + 1) * B].ljust(B, b"\0")
        comps.append(chunk if nc and nc[0] <= b < nc[0] + nc[1] else zlib.compress(chunk))
    hdr = b"DAX\0" + struct.pack("<III", len(data), 1 if nc else 0, 1 if nc else 0) + b"\0" * 16
    pos = 32 + n * 6 + (8 if nc else 0)
    offs = []
    for c in comps: offs.append(pos); pos += len(c)
    t = b"".join(struct.pack("<I", o) for o in offs) + b"".join(struct.pack("<H", len(c) & 0xFFFF) for c in comps)
    if nc: t += struct.pack("<II", *nc)
    return hdr + t + b"".join(comps)

w("game.dax", dax(iso))
w("game_nc.dax", dax(iso, nc=(2, 3)))

# ---------------------------------------------------------------- Dreamcast GDI / GD-ROM CHD
low = make_iso(300, label=b"LOWDENS")
hd_total = 600
big = bytes(range(256)) * 40  # 10 KiB, lives in track 5
t3_lba = 45000
t4_lba = t3_lba + hd_total
t5_lba = t4_lba + 300
hd = make_iso(hd_total, base=t3_lba, label=b"GDROMHD", big=(t5_lba + 2, big))
t5 = bytearray(S * 50); t5[2 * S:2 * S + len(big)] = big
w("track01.bin", raw(low, 1, 0))
w("track02.raw", b"\x22" * 2352 * 300)
w("track03.bin", raw(hd, 1, t3_lba))
w("track04.raw", b"\x33" * 2352 * 300)
w("track05.bin", raw(bytes(t5), 1, t5_lba))
w("disc.gdi", ("5\n1 0 4 2352 track01.bin 0\n2 450 0 2352 track02.raw 0\n3 %d 4 2352 track03.bin 0\n"
                "4 %d 0 2352 track04.raw 0\n5 %d 4 2352 \"track05.bin\" 0\n" % (t3_lba, t4_lba, t5_lba)).encode())

# ---------------------------------------------------------------- CDI-like: audio session + data session with absolute LBAs
cdi_lba = 11702
cdi_iso = make_iso(400, base=cdi_lba, label=b"DCSESS2")
w("dreamcast.cdi", b"\x44" * 2352 * 300 + raw(cdi_iso, 1, cdi_lba) + b"\0" * 1000 + b"CDIFOOTER" * 10)

# ---------------------------------------------------------------- MDS
def mds(track_ss, mode_byte, lba=0, offset=0):
    m = bytearray(0x58 + 24 + 80)
    m[0:16] = b"MEDIA DESCRIPTOR"; m[0x10:0x12] = b"\x01\x05"; struct.pack_into("<H", m, 0x14, 1)
    struct.pack_into("<I", m, 0x50, 0x58)
    so = 0x58
    struct.pack_into("<iiHBBHHII", m, so, -150, 400, 1, 1, 0, 1, 1, 0, so + 24)
    t = so + 24
    m[t] = mode_byte; m[t + 1] = 8 if track_ss == 2448 else 0; m[t + 2] = 0x14; m[t + 4] = 1
    struct.pack_into("<H", m, t + 0x10, track_ss)
    struct.pack_into("<I", m, t + 0x24, lba)
    struct.pack_into("<Q", m, t + 0x28, offset)
    struct.pack_into("<I", m, t + 0x30, 1)
    return bytes(m)

w("alc.mdf", b"\x55" * 4096 + mode1)   # data does not start at offset 0
w("alc.mds", mds(2352, 0xAA, offset=4096))

# ---------------------------------------------------------------- ECM
def ecm_num(ty, num):
    n = num - 1
    out = bytearray([ty | ((n & 0x1F) << 2) | (0x80 if n >> 5 else 0)])
    n >>= 5
    while n:
        out.append((n & 0x7F) | (0x80 if n >> 7 else 0)); n >>= 7
    return bytes(out)

def ecm_mode1(raw_bin):
    o = bytearray(b"ECM\0")
    secs = len(raw_bin) // 2352
    o += ecm_num(1, secs)
    for i in range(secs):
        s = raw_bin[i * 2352:(i + 1) * 2352]
        o += s[12:15] + s[16:16 + 2048]
    o += bytes([0xFC, 0xFF, 0xFF, 0xFF, 0x3F]) + b"\0\0\0\0"
    return bytes(o)

def ecm_mode2(raw_bin):
    o = bytearray(b"ECM\0")
    for i in range(len(raw_bin) // 2352):
        s = raw_bin[i * 2352:(i + 1) * 2352]
        o += ecm_num(0, 16) + s[:16] + ecm_num(2, 1) + s[20:24] + s[24:24 + 2048]
    o += bytes([0xFC, 0xFF, 0xFF, 0xFF, 0x3F]) + b"\0\0\0\0"
    return bytes(o)

w("mode1.bin.ecm", ecm_mode1(mode1))
w("mode2.bin.ecm", ecm_mode2(mode2))
w("ecmgame.bin.ecm", ecm_mode1(mode1))
w("ecmgame.cue", b'FILE "ecmgame.bin" BINARY\n  TRACK 01 MODE1/2352\n    INDEX 01 00:00:00\n')

# ---------------------------------------------------------------- gzip / xz / zip
with open(p("named.gz"), "wb") as fo, gzip.GzipFile(filename="inner.iso", mode="wb", fileobj=fo) as g:
    g.write(iso)
w("game.iso.gz", gzip.compress(iso))
w("mode1.bin.gz", gzip.compress(mode1))
w("game.iso.xz", lzma.compress(iso))
with zipfile.ZipFile(p("game_cue.zip"), "w", zipfile.ZIP_DEFLATED) as z:
    z.writestr("Game (USA)/Game (USA).cue", 'FILE "Game (USA).bin" BINARY\n  TRACK 01 MODE1/2352\n    INDEX 01 00:00:00\n')
    z.writestr("Game (USA)/Game (USA).bin", mode1)
with zipfile.ZipFile(p("game_iso.zip"), "w", zipfile.ZIP_DEFLATED) as z:
    z.writestr("readme.txt", "hi")
    z.writestr("game.iso", iso)
disk = open(sys.argv[4], "rb").read()
w("disk.img.gz", gzip.compress(disk, 1))

# ---------------------------------------------------------------- CHD (via the chd crate's writer)
def mk(*args):
    r = subprocess.run([MKCHD] + list(args), capture_output=True, text=True)
    if r.returncode: raise SystemExit("mkchd failed: %s %s" % (args, r.stderr))
for codec in ["default", "zl", "lz", "fl", "zs", "none"]:
    mk("cd", p("cd_%s.chd" % codec), codec, "MODE1_RAW:" + p("mode1.bin"))
mk("cd", p("cd_cooked.chd"), "default", "MODE1:" + p("test.iso"))
mk("cd", p("cd_mode2.chd"), "default", "MODE2_RAW:" + p("mode2.bin"))
w("audio.raw", b"\x66" * 2352 * 37)
mk("cd", p("cd_audiofirst.chd"), "default", "AUDIO:" + p("audio.raw"), "MODE1_RAW:" + p("mode1.bin"))
mk("gd", p("gdrom.chd"), "default", "MODE1_RAW:" + p("track01.bin"), "AUDIO:" + p("track02.raw"),
   "MODE1_RAW:" + p("track03.bin"), "AUDIO:" + p("track04.raw"), "MODE1_RAW:" + p("track05.bin"))
mk("dvd", p("dvd.chd"), p("test.iso"))
mk("hd", p("hdd.chd"), sys.argv[4])

# ---------------------------------------------------------------- run + verify
def read_vhd(path):
    b = open(path, "rb").read()
    foot = b[-512:]; assert foot[:8] == b"conectix"
    size = struct.unpack(">Q", foot[48:56])[0]
    bat_off = struct.unpack(">Q", b[512 + 16:512 + 24])[0]
    nent, bsz = struct.unpack(">II", b[512 + 28:512 + 36])
    out = bytearray(size)
    for i in range(nent):
        e, = struct.unpack(">I", b[bat_off + 4 * i:bat_off + 4 * i + 4])
        if e != 0xFFFFFFFF:
            n = min(bsz, size - i * bsz)
            out[i * bsz:i * bsz + n] = b[e * 512 + 512:e * 512 + 512 + n]
    return bytes(out)

fails = 0
def run(name, kind="iso", big=None, expect_lba=None):
    global fails
    out = os.path.join(OUT, name.replace(".", "_") + "." + kind)
    r = subprocess.run([EXE, "convert", p(name), out], capture_output=True, text=True)
    if r.returncode:
        print("FAIL %-22s %s" % (name, (r.stderr or r.stdout).strip().splitlines()[-1])); fails += 1; return
    data = open(out, "rb").read()
    if kind == "vhd":
        got = read_vhd(out)
        err = None if got[:len(disk)] == disk and not any(got[len(disk):]) else "disk contents differ"
    else:
        err = check_iso(data, big)
        if not err and expect_lba is not None and data[(expect_lba + 16) * S + 1:(expect_lba + 16) * S + 6] != b"CD001":
            err = "file system not placed at LBA %d" % expect_lba
    print(("PASS" if not err else "FAIL"), "%-22s %s" % (name, err or "%d bytes" % len(data)))
    fails += bool(err)

for n in ["game.cso", "game_align.cso", "game_v2.cso", "game.zso", "game.dax", "game_nc.dax",
          "alc.mds", "alc.mdf", "mode1.bin.ecm", "mode2.bin.ecm", "ecmgame.cue",
          "named.gz", "game.iso.gz", "mode1.bin.gz", "game.iso.xz", "game_cue.zip", "game_iso.zip",
          "cd_default.chd", "cd_zl.chd", "cd_lz.chd", "cd_fl.chd", "cd_zs.chd", "cd_none.chd",
          "cd_cooked.chd", "cd_mode2.chd", "cd_audiofirst.chd", "dvd.chd"]:
    run(n)
run("dreamcast.cdi", expect_lba=cdi_lba)
run("disc.gdi", big=big, expect_lba=t3_lba)
run("gdrom.chd", big=big, expect_lba=t3_lba)
run("hdd.chd", "vhd")
run("disk.img.gz", "vhd")

r = subprocess.run([EXE, "info", p("disc.gdi")], capture_output=True, text=True)
print(r.stdout)
print("failures:", fails)
sys.exit(1 if fails else 0)
