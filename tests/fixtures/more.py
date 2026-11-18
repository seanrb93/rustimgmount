"""Fixtures for the third batch of formats (7z, bz2, split raw, split MDF, E01, PBP, M3U,
ISZ, PC-98 HDI/FDI/NHD, Atari ST/MSA, Alcohol MDX / MDS v2). Converts each with rustimgmount
and verifies the result by reading its file system the way an OS would (ISO 9660 for
discs, FAT12/16 for disks)."""
import bz2, hashlib, os, random, shutil, struct, subprocess, sys, zlib

EXE, MKFIX, D = sys.argv[1], sys.argv[2], sys.argv[3]
shutil.rmtree(D, ignore_errors=True)
os.makedirs(D)
OUT = os.path.join(D, "out")
os.makedirs(OUT)
S = 2048
rnd = random.Random(1234)
def p(n): return os.path.join(D, n)
def w(n, data):
    os.makedirs(os.path.dirname(p(n)), exist_ok=True)
    with open(p(n), "wb") as f: f.write(data)

# ---------------------------------------------------------------- ISO 9660 builder / reader
def both16(v): return struct.pack("<H", v) + struct.pack(">H", v)
def both32(v): return struct.pack("<I", v) + struct.pack(">I", v)
def dirrec(name, lba, size, flags):
    n = len(name); r = bytearray(33 + n + ((n + 1) % 2))
    r[0] = len(r); r[2:10] = both32(lba); r[10:18] = both32(size)
    r[18:25] = bytes([124, 1, 1, 0, 0, 0, 0]); r[25] = flags; r[28:32] = both16(1); r[32] = n; r[33:33 + n] = name
    return bytes(r)
def hello(tag): return (b"Hello from %s!\r\n" % tag) * 8

def make_iso(total, label=b"TESTISO", text=None):
    text = text or hello(label)
    img = bytearray(S * total)
    pvd = bytearray(S); pvd[0] = 1; pvd[1:6] = b"CD001"; pvd[6] = 1
    pvd[8:40] = b" " * 32; pvd[40:72] = label.ljust(32); pvd[80:88] = both32(total)
    pvd[120:124] = both16(1); pvd[124:128] = both16(1); pvd[128:132] = both16(S); pvd[132:140] = both32(10)
    pvd[140:144] = struct.pack("<I", 18); pvd[148:152] = struct.pack(">I", 19)
    pvd[156:190] = dirrec(b"\x00", 20, S, 2)
    for o, l in ((190, 128), (318, 128), (446, 128), (574, 128), (702, 37), (739, 37), (776, 37)): pvd[o:o + l] = b" " * l
    for o in (813, 830): pvd[o:o + 17] = b"2024010100000000\x00"
    for o in (847, 864): pvd[o:o + 17] = b"0000000000000000\x00"
    pvd[881] = 1; img[16 * S:17 * S] = pvd
    t = bytearray(S); t[0] = 255; t[1:6] = b"CD001"; t[6] = 1; img[17 * S:18 * S] = t
    img[18 * S:18 * S + 10] = bytes([1, 0]) + struct.pack("<I", 20) + struct.pack("<H", 1) + b"\0\0"
    img[19 * S:19 * S + 10] = bytes([1, 0]) + struct.pack(">I", 20) + struct.pack(">H", 1) + b"\0\0"
    root = dirrec(b"\x00", 20, S, 2) + dirrec(b"\x01", 20, S, 2) + dirrec(b"HELLO.TXT;1", 21, len(text), 0)
    img[20 * S:20 * S + len(root)] = root; img[21 * S:21 * S + len(text)] = text
    for i in range(22, total):  # incompressible-ish noise in some sectors
        img[i * S:i * S + 64] = bytes(rnd.getrandbits(8) for _ in range(64)) if i % 7 == 0 else struct.pack("<Q", i) * 8
    return bytes(img)

def check_iso(data, text):
    pvd = data[16 * S:17 * S]
    if pvd[1:6] != b"CD001": return "no PVD"
    root = struct.unpack("<I", pvd[158:162])[0]; d = data[root * S:(root + 1) * S]; o = 0; found = {}
    while o < S and d[o]:
        n = d[o + 32]; found[d[o + 33:o + 33 + n]] = struct.unpack("<II", d[o + 2:o + 6] + d[o + 10:o + 14]); o += d[o]
    if b"HELLO.TXT;1" not in found: return "HELLO.TXT missing"
    lba, size = found[b"HELLO.TXT;1"]
    return None if data[lba * S:lba * S + size] == text else "HELLO.TXT content wrong"

def bcd(n): return ((n // 10) << 4) | (n % 10)
def raw(iso, mode=1):
    out = bytearray()
    for i in range(len(iso) // S):
        a = i + 150; hdr = b"\x00" + b"\xff" * 10 + b"\x00" + bytes([bcd(a // 4500), bcd((a // 75) % 60), bcd(a % 75), mode])
        dd = iso[i * S:(i + 1) * S]
        out += hdr + dd + b"\0" * 288 if mode == 1 else hdr + b"\0\0\x08\0\0\0\x08\0" + dd + b"\0" * 280
    return bytes(out)

# ---------------------------------------------------------------- FAT builder / reader
def fat_volume(total, bps, spc, bits, root, media, label, text, jump=b"\xEB\x3C\x90"):
    reserved, nfats = 1, 2
    clusters = total // spc
    fatsz = ((clusters + 2) * bits // 8 + bps - 1) // bps + 1
    vol = bytearray(total * bps)
    bs = bytearray(bps)
    bs[0:3] = jump; bs[3:11] = b"TESTOEM "
    struct.pack_into("<HBHBHHBHHHII", bs, 11, bps, spc, reserved, nfats, root, total if total < 65536 else 0, media, fatsz, 8, 2, 0, total if total >= 65536 else 0)
    bs[38] = 0x29; bs[39:43] = b"\x11\x22\x33\x44"; bs[43:54] = label.ljust(11); bs[54:62] = b"FAT%d   " % bits
    bs[510:512] = b"\x55\xAA"; vol[:bps] = bs
    for f in range(nfats):
        fo = (reserved + f * fatsz) * bps
        if bits == 12: vol[fo:fo + 5] = bytes([media, 0xFF, 0xFF, 0xFF, 0x0F])
        else: vol[fo:fo + 6] = struct.pack("<HHH", 0xFF00 | media, 0xFFFF, 0xFFFF)
    root_off = (reserved + nfats * fatsz) * bps
    e = bytearray(32); e[0:11] = b"HELLO   TXT"; e[11] = 0x20; struct.pack_into("<HHHI", e, 22, 0, 0x5821, 2, len(text))
    vol[root_off:root_off + 32] = e
    data_off = root_off + ((root * 32 + bps - 1) // bps) * bps
    vol[data_off:data_off + len(text)] = text
    return bytes(vol)

def read_vhd(path):
    b = open(path, "rb").read(); foot = b[-512:]; assert foot[:8] == b"conectix"
    size = struct.unpack(">Q", foot[48:56])[0]; bat = struct.unpack(">Q", b[528:536])[0]
    n, bs = struct.unpack(">II", b[540:548]); out = bytearray(size)
    for i in range(n):
        e, = struct.unpack(">I", b[bat + 4 * i:bat + 4 * i + 4])
        if e != 0xFFFFFFFF:
            k = min(bs, size - i * bs); out[i * bs:i * bs + k] = b[e * 512 + 512:e * 512 + 512 + k]
    return bytes(out)

def check_fat_disk(disk, text, part=0):
    """Read like Windows: MBR -> partition -> BPB (must have x86 jump, 512-byte sectors) -> root -> file."""
    if disk[510:512] != b"\x55\xAA": return "no MBR"
    pe = disk[446 + 16 * part:462 + 16 * part]
    start = struct.unpack("<I", pe[8:12])[0] * 512
    v = disk[start:]
    if v[0] not in (0xEB, 0xE9): return "boot sector has no x86 jump (Windows would reject it)"
    bps, spc, res, nf, root, tot16, media, fatsz = struct.unpack("<HBHBHHBH", v[11:24])
    if bps != 512: return "BPB still has %d-byte sectors" % bps
    if media != 0xF0 and media < 0xF8: return "invalid media byte %02x" % media
    total = tot16 or struct.unpack("<I", v[32:36])[0]
    root_off = (res + nf * fatsz) * bps; root_secs = (root * 32 + bps - 1) // bps
    data = root_off + root_secs * bps
    clusters = (total - res - nf * fatsz - root_secs) // spc
    bits = 12 if clusters < 4085 else 16
    for i in range(root):
        e = v[root_off + i * 32:root_off + i * 32 + 32]
        if e[0] == 0: break
        if e[0:11] == b"HELLO   TXT":
            c, size = struct.unpack("<HI", e[26:32]); got = bytearray(); fat = v[res * bps:]
            while 2 <= c < (0xFF8 if bits == 12 else 0xFFF8) and len(got) < size:
                got += v[data + (c - 2) * spc * bps:data + (c - 1) * spc * bps]
                if bits == 12:
                    x = struct.unpack("<H", fat[c * 3 // 2:c * 3 // 2 + 2])[0]; c = (x >> 4) if c & 1 else (x & 0xFFF)
                else:
                    c = struct.unpack("<H", fat[c * 2:c * 2 + 2])[0]
            return None if bytes(got[:size]) == text else "file content wrong"
    return "HELLO.TXT not found in root"

# ---------------------------------------------------------------- base images
iso = make_iso(400); T_ISO = hello(b"TESTISO")
w("game.iso", iso)
mode1 = raw(iso, 1); mode2 = raw(iso, 2)
disk_text = b"Hello from the raw disk!\r\n" * 5
def mbr_disk(mb=12):
    total = mb * 2048; start = 2048
    vol = fat_volume(total - start, 512, 4, 16, 512, 0xF8, b"RAWDISK", disk_text)
    d = bytearray(total * 512); m = bytearray(512)
    m[446:462] = struct.pack("<B3sB3sII", 0, b"\xFE\xFF\xFF", 0x06, b"\xFE\xFF\xFF", start, total - start)
    m[510:512] = b"\x55\xAA"; d[:512] = m; d[start * 512:start * 512 + len(vol)] = vol
    return bytes(d)
disk = mbr_disk(); w("disk.img", disk)

# ---------------------------------------------------------------- bz2, 7z, split raw, split MDF
w("game.iso.bz2", bz2.compress(iso))
os.makedirs(p("7zsrc/Game (Europe)"))
w("7zsrc/Game (Europe)/Game (Europe).cue", b'FILE "Game (Europe).bin" BINARY\r\n  TRACK 01 MODE1/2352\r\n    INDEX 01 00:00:00\r\n')
w("7zsrc/Game (Europe)/Game (Europe).bin", mode1)
def run_tool(*a):
    r = subprocess.run(list(a), capture_output=True, text=True)
    if r.returncode: raise SystemExit("tool failed: %s\n%s" % (a, r.stderr))
    return r.stdout
run_tool(MKFIX, "7z", p("7zsrc"), p("game.7z"))
for i, k in enumerate(range(0, len(disk), 4 << 20)): w("split.img.%03d" % (i + 1), disk[k:k + (4 << 20)])
for i, k in enumerate(range(0, len(iso), 300000)): w("splitiso.%03d" % (i + 1), iso[k:k + 300000])
third = (len(mode1) // 3) // 2352 * 2352
w("alc2.mdf", mode1[:third]); w("alc2.md0", mode1[third:2 * third]); w("alc2.md1", mode1[2 * third:])
w("plain2.mdf", mode1[:third]); w("plain2.md1", mode1[third:])

# ---------------------------------------------------------------- E01 (ewf-image writer)
os.makedirs(p("e01a")); os.makedirs(p("e01b")); os.makedirs(p("e01c"))
run_tool(MKFIX, "e01", p("disk.img"), p("e01a/disk.E01"))
run_tool(MKFIX, "e01", p("game.iso"), p("e01b/game.E01"))
segs = run_tool(MKFIX, "e01", p("disk.img"), p("e01c/multi.E01"), str(2 << 20)).split()
print("multi-segment E01:", len(segs), "segments")

# ---------------------------------------------------------------- PBP (popstation layout)
def psisoimg(raw_disc):
    BLOCK = 0x9300; hdr = bytearray(0x100000); hdr[0:12] = b"PSISOIMG0000"
    data = bytearray(); n = (len(raw_disc) + BLOCK - 1) // BLOCK
    for i in range(n):
        blk = raw_disc[i * BLOCK:(i + 1) * BLOCK].ljust(BLOCK, b"\0")
        co = zlib.compressobj(9, zlib.DEFLATED, -15); c = co.compress(blk) + co.flush()
        if i == 3 or len(c) >= BLOCK: c = blk  # one stored block
        struct.pack_into("<IH", hdr, 0x4000 + i * 32, len(data), len(c)); data += c
    return bytes(hdr) + bytes(data)
def pbp(discs):
    psar_off = 0x1000
    if len(discs) == 1:
        psar = psisoimg(discs[0])
    else:
        psar = bytearray(0x8000); psar[0:16] = b"PSTITLEIMG000000"; parts = []
        pos = len(psar)
        for i, dd in enumerate(discs):
            img = psisoimg(dd); struct.pack_into("<I", psar, 0x200 + 4 * i, pos); parts.append(img); pos += len(img)
        psar = bytes(psar) + b"".join(parts)
    h = bytearray(psar_off); h[0:4] = b"\0PBP"; struct.pack_into("<I", h, 4, 0x10000)
    for i in range(7): struct.pack_into("<I", h, 8 + 4 * i, 0x28)
    struct.pack_into("<I", h, 0x24, psar_off)
    return bytes(h) + psar
iso2 = make_iso(300, b"DISCTWO"); T_ISO2 = hello(b"DISCTWO")
w("ps1.pbp", pbp([raw(iso, 2)]))
w("multi.pbp", pbp([raw(iso, 2), raw(iso2, 2)]))
w("disc2.iso", iso2)
w("game.cue", b'FILE "game.bin" BINARY\n  TRACK 01 MODE1/2352\n    INDEX 01 00:00:00\n'); w("game.bin", mode1)
w("set.m3u", b"# multi-disc set\r\ngame.cue\r\ndisc2.iso\r\n")

# ---------------------------------------------------------------- ISZ
def isz(data, chunk=0x10000):
    n = (len(data) + chunk - 1) // chunk; ptrs = []; body = bytearray()
    for i in range(n):
        c = data[i * chunk:(i + 1) * chunk]
        if not any(c): ptrs.append((0, 0)); continue
        if i == 1: payload, t = c, 1
        elif i == 2: payload, t = b"BZh" + bz2.compress(c)[3:], 3
        else: payload, t = zlib.compress(c), 2
        ptrs.append((t, len(payload))); body += payload
    tab = bytearray()
    for t, size in ptrs: tab += ((t << 22) | size).to_bytes(3, "little")
    key = [0xB6, 0x8C, 0xA5, 0xDE]; tab = bytes(b ^ key[i & 3] for i, b in enumerate(tab))
    h = bytearray(64); h[0:4] = b"IsZ!"; h[4] = 64; h[5] = 1
    struct.pack_into("<H", h, 10, 2048); struct.pack_into("<I", h, 12, len(data) // 2048)
    struct.pack_into("<I", h, 25, n); struct.pack_into("<I", h, 29, chunk); h[33] = 3
    struct.pack_into("<I", h, 35, 64); struct.pack_into("<I", h, 43, 64 + len(tab))
    return bytes(h) + tab + bytes(body)
z = bytearray(iso); z[0x20000:0x30000] = b"\0" * 0x10000; z = bytes(z)  # one all-zero chunk
w("game.isz", isz(z))

# ---------------------------------------------------------------- PC-98
T_FDI = b"Hello from a PC-98 floppy!\r\n" * 4
T_HDI = b"Hello from a PC-98 hard disk!\r\n" * 4
fdvol = fat_volume(1232, 1024, 1, 12, 192, 0xFE, b"PC98FD", T_FDI)
fh = bytearray(4096); struct.pack_into("<IIIIIIII", fh, 0, 0, 0x90, 4096, len(fdvol), 1024, 8, 2, 77)
w("pc98.fdi", bytes(fh) + fdvol)
def pc98_disk(heads=8, spt=33, cyls=80, ss=512):
    d = bytearray(heads * spt * cyls * ss); d[0] = 0xCB  # IPL: not a boot sector
    start_c = 1; start = start_c * heads * spt * ss
    vol = fat_volume(5000, 2048, 1, 16, 112, 0xF8, b"PC98HD", T_HDI)  # 2048-byte sectors, padded root
    d[start:start + len(vol)] = vol
    e = bytearray(32); e[0] = 0xA0; e[1] = 0xA1; e[8] = 0; e[9] = 0; struct.pack_into("<H", e, 10, start_c)
    e[12] = spt - 1; e[13] = heads - 1; struct.pack_into("<H", e, 14, start_c + 9); e[16:32] = b"MS-DOS 6.20     "
    d[ss:ss + 32] = e
    return bytes(d), heads, spt, cyls, ss
hd, heads, spt, cyls, ss = pc98_disk()
hh = bytearray(4096); struct.pack_into("<IIIIIIII", hh, 0, 0, 0, 4096, len(hd), ss, spt, heads, cyls)
w("pc98.hdi", bytes(hh) + hd)
nh = bytearray(512); nh[0:15] = b"T98HDDIMAGE.R0\0"; struct.pack_into("<IIHHH", nh, 0x110, 512, cyls, heads, spt, ss)
w("pc98.nhd", bytes(nh) + hd)

# ---------------------------------------------------------------- Atari ST
T_ST = b"Hello from an Atari ST disk!\r\n" * 4
st = fat_volume(1440, 512, 2, 12, 112, 0xF7, b"ATARIST", T_ST, jump=b"\x60\x38\x00")
w("atari.st", st)
def msa(img, spt=9, sides=2):
    tracks = len(img) // (spt * 512 * sides); out = bytearray(struct.pack(">HHHHH", 0x0E0F, spt, sides - 1, 0, tracks - 1))
    for t in range(tracks):
        for s in range(sides):
            tr = img[(t * sides + s) * spt * 512:(t * sides + s + 1) * spt * 512]; enc = bytearray(); i = 0
            while i < len(tr):
                j = i
                while j < len(tr) and tr[j] == tr[i] and j - i < 65535: j += 1
                if j - i >= 5 or tr[i] == 0xE5: enc += bytes([0xE5, tr[i]]) + struct.pack(">H", j - i); i = j
                else: enc.append(tr[i]); i += 1
            if len(enc) >= len(tr): enc = tr
            out += struct.pack(">H", len(enc)) + enc
    return bytes(out)
w("atari.msa", msa(st))

# ---------------------------------------------------------------- Alcohol MDX / MDS v2
# AES-256 (FIPS-197), encryption only; checked against the standard test vector below.
SBOX = [0] * 256
def _init_sbox():
    p = q = 1
    while True:
        p = p ^ ((p << 1) & 0xFF) ^ (0x1B if p & 0x80 else 0)
        q ^= q << 1; q ^= q << 2; q ^= q << 4; q &= 0xFF
        if q & 0x80: q ^= 0x09
        x = q ^ ((q << 1) | (q >> 7)) ^ ((q << 2) | (q >> 6)) ^ ((q << 3) | (q >> 5)) ^ ((q << 4) | (q >> 4))
        SBOX[p] = (x ^ 0x63) & 0xFF
        if p == 1: break
    SBOX[0] = 0x63
_init_sbox()
def xt(a): return ((a << 1) ^ 0x1B) & 0xFF if a & 0x80 else a << 1
def key_expand(key):
    w_ = [list(key[i:i + 4]) for i in range(0, 32, 4)]; rcon = 1
    for i in range(8, 60):
        t = list(w_[i - 1])
        if i % 8 == 0: t = [SBOX[b] for b in t[1:] + t[:1]]; t[0] ^= rcon; rcon = xt(rcon)
        elif i % 8 == 4: t = [SBOX[b] for b in t]
        w_.append([a ^ b for a, b in zip(w_[i - 8], t)])
    return w_
def aes_enc(rk, blk):
    s = [b ^ rk[c][r] for c in range(4) for r, b in enumerate(blk[c * 4:c * 4 + 4])]
    for rnd_ in range(1, 15):
        s = [SBOX[b] for b in s]
        s = [s[((c + r) % 4) * 4 + r] for c in range(4) for r in range(4)]
        if rnd_ != 14:
            n = []
            for c in range(4):
                a = s[c * 4:c * 4 + 4]; t = a[0] ^ a[1] ^ a[2] ^ a[3]
                n += [a[i] ^ t ^ xt(a[i] ^ a[(i + 1) % 4]) for i in range(4)]
            s = n
        s = [b ^ rk[rnd_ * 4 + c][r] for c in range(4) for r, b in enumerate(s[c * 4:c * 4 + 4])]
    return bytes(s)
_k = bytes(range(32)); _rk = key_expand(_k)
assert aes_enc(_rk, bytes.fromhex("00112233445566778899aabbccddeeff")).hex() == "8ea2b7ca516745bfeafc49904b496089", "AES self-test failed"

def enc_cbc(key, iv, data):
    rk = key_expand(key); iv1 = iv[8:16]; chain = iv[:16]; out = bytearray()
    for i in range(0, len(data), 16):
        x = aes_enc(rk, bytes(a ^ b for a, b in zip(data[i:i + 16], chain)))
        out += bytes(a ^ b for a, b in zip(x, iv1 + iv1)); chain = x
    return bytes(out)
def edc_crc(b):
    c = 0
    for x in b:
        c ^= x
        for _ in range(8): c = (c >> 1) ^ 0xD8018001 if c & 1 else c >> 1
    return c
def salt_pw(salt):
    m = edc_crc(salt) ^ 0x567372FF; out = bytearray()
    for i in range(16):
        word = struct.unpack("<I", salt[i * 4:i * 4 + 4])[0]
        m = (m * 0x35E85A6D + 0x1548DCE9) & 0xFFFFFFFF; word ^= m ^ 0xEC564717; out += struct.pack("<I", word)
    return bytes(b if b else 0x5F for b in out)
def enc_header(key_data, csize, dsize):
    salt = bytes(rnd.getrandbits(8) for _ in range(64))
    dk = hashlib.pbkdf2_hmac("ripemd160", salt_pw(salt), salt, 2000, 152)
    plain = struct.pack("<IIHHI", zlib.crc32(key_data), 0x54525545, 1, 0x100, 0) + key_data + struct.pack("<II", csize, dsize)
    plain = plain.ljust(448, b"\0")
    return salt + enc_cbc(dk[32:64], dk[0:16], plain)
def descriptor(start_off, length, ss, compressed, group, table_off):
    d = bytearray(0xF0); struct.pack_into("<H", d, 0x14, 1); struct.pack_into("<I", d, 0x50, 0x60)
    struct.pack_into("<I", d, 0x60 + 0x14, 0x80); d[0x60 + 0x0A] = 1; struct.pack_into("<HH", d, 0x60 + 0x0C, 1, 1)
    t = 0x80; d[t] = 0x02 | 0x80 | 0x20 | 0x08; d[t + 2] = 0x14; d[t + 4] = 1
    struct.pack_into("<H", d, t + 0x10, ss); struct.pack_into("<I", d, t + 0x24, 0); struct.pack_into("<Q", d, t + 0x28, start_off)
    struct.pack_into("<II", d, t + 0x30, 1, 0xD0); struct.pack_into("<Q", d, t + 0x40, length)
    f = 0xD0; d[f + 4] = 1 if compressed else 0; struct.pack_into("<I", d, f + 12, group)
    struct.pack_into("<QQ", d, f + 16, length, table_off)
    return bytes(d[18:])
def protect(desc_body):
    key_data = bytes(rnd.getrandbits(8) for _ in range(256))
    comp = zlib.compress(desc_body); padded = comp.ljust((len(comp) + 15) // 16 * 16, b"\0")
    enc = b"".join(enc_cbc(key_data[32:64], key_data[0:16], padded[i:i + 512]) for i in range(0, len(padded), 512))
    return enc, enc_header(key_data, len(comp), len(desc_body))
def file_header(enc_off):
    return b"MEDIA DESCRIPTOR" + bytes([2, 1]) + b"(C) 2000-2015 Disc Soft Ltd." [:26] + struct.pack("<I", enc_off)

# MDX: compressed track data in groups of 8 sectors.
GROUP = 8; ss = 2352; secs = len(mode1) // ss
groups = bytearray(); table = []
for g in range((secs + GROUP - 1) // GROUP):
    chunk = mode1[g * GROUP * ss:(g + 1) * GROUP * ss]
    if not any(chunk): table.append(0x8000); continue
    if g == 2: table.append(0); groups += chunk; continue
    co = zlib.compressobj(9, zlib.DEFLATED, -15); c = co.compress(chunk) + co.flush()
    table.append(len(c)); groups += c
start_off = 0x40
tab = zlib.compress(b"".join(struct.pack("<H", x) for x in table))
table_off = len(groups)
body = bytes(groups) + tab
enc_desc, hdr = protect(descriptor(start_off, secs, ss, True, GROUP, table_off))
footer_off = start_off + len(body)
mdx = file_header(0xFFFFFFFF) + struct.pack("<QQ", footer_off, len(enc_desc) + 64)
mdx = mdx.ljust(start_off, b"\0") + body + enc_desc + hdr
w("alcohol.mdx", mdx)
# MDS v2 + plain .mdf
enc_desc, hdr = protect(descriptor(0, secs, ss, False, 0, 0))
w("alc_v2.mds", file_header(0x30 + len(enc_desc)) + enc_desc + hdr)
w("alc_v2.mdf", mode1)

# ---------------------------------------------------------------- twin disks (identical disk IDs)
def guid_bytes(s):
    h = bytes.fromhex(s.replace("-", ""))
    return h[3::-1] + h[5:3:-1] + h[7:5:-1] + h[8:]
T_GPT = b"Hello from a GPT disk!\r\n" * 4
def gpt_disk(disk_guid, part_guid, mb=12):
    total = mb * 2048; start = 2048; last = total - 34
    d = bytearray(total * 512)
    pm = bytearray(512); pm[446:462] = struct.pack("<B3sB3sII", 0, b"\0\x02\0", 0xEE, b"\xFF\xFF\xFF", 1, total - 1); pm[510:512] = b"\x55\xAA"
    d[:512] = pm
    vol = fat_volume(last - start + 1, 512, 4, 16, 512, 0xF8, b"GPTDISK", T_GPT)
    d[start * 512:start * 512 + len(vol)] = vol
    entries = bytearray(128 * 128)
    entries[0:16] = guid_bytes("EBD0A0A2-B9E5-4433-87C0-68B6B72699C7"); entries[16:32] = part_guid
    struct.pack_into("<QQQ", entries, 32, start, last, 0); entries[56:56 + 14] = "TestPart".encode("utf-16-le")[:14]
    def header(cur, alt, ents):
        h = bytearray(92)
        struct.pack_into("<8sIIIIQQQQ16sQIII", h, 0, b"EFI PART", 0x10000, 92, 0, 0, cur, alt, 34, total - 34, disk_guid, ents, 128, 128, zlib.crc32(entries))
        struct.pack_into("<I", h, 16, zlib.crc32(h)); return bytes(h).ljust(512, b"\0")
    d[512:1024] = header(1, total - 1, 2); d[1024:1024 + len(entries)] = entries
    d[(total - 33) * 512:(total - 1) * 512] = entries; d[(total - 1) * 512:] = header(total - 1, 1, total - 33)
    return bytes(d)
GG, PG = bytes(rnd.getrandbits(8) for _ in range(16)), bytes(rnd.getrandbits(8) for _ in range(16))
w("twins/gptA.img", gpt_disk(GG, PG)); w("twins/gptB.img", gpt_disk(GG, PG))
mbr_twin = bytearray(disk); mbr_twin[440:444] = b"\x78\x56\x34\x12"
w("twins/mbrA.img", bytes(mbr_twin)); w("twins/mbrB.img", bytes(mbr_twin))

# ---------------------------------------------------------------- large disk (> 2040 GiB) as sparse QCOW2
def sparse_qcow2(data, virtual_size, cbits=16):
    """QCOW2 of `virtual_size` bytes whose only allocated clusters hold `data` at offset 0."""
    cs = 1 << cbits; per_l2 = cs // 8
    l1_size = (virtual_size + cs * per_l2 - 1) // (cs * per_l2)
    nclusters = (len(data) + cs - 1) // cs
    n_l2 = (nclusters + per_l2 - 1) // per_l2
    l1_off = cs; l1_bytes = (l1_size * 8 + cs - 1) // cs * cs
    l2_off = l1_off + l1_bytes; data_off = l2_off + n_l2 * cs
    l1 = bytearray(l1_size * 8); l2 = bytearray(n_l2 * cs); body = bytearray()
    for i in range(n_l2): struct.pack_into(">Q", l1, i * 8, (1 << 63) | (l2_off + i * cs))
    for c in range(nclusters):
        chunk = data[c * cs:(c + 1) * cs].ljust(cs, b"\0")
        if not any(chunk): continue
        struct.pack_into(">Q", l2, c * 8, (1 << 63) | (data_off + len(body))); body += chunk
    h = bytearray(cs)
    struct.pack_into(">4sIQIIQIIQQIIQ", h, 0, b"QFI\xfb", 3, 0, 0, cbits, virtual_size, 0, l1_size, l1_off, 0, 0, 0, 0)
    struct.pack_into(">QQQII", h, 72, 0, 0, 0, 4, 104)
    return bytes(h) + bytes(l1).ljust(l1_bytes, b"\0") + bytes(l2) + bytes(body)
BIG = (5 << 40) // 2  # 2.5 TiB
w("huge.qcow2", sparse_qcow2(disk, BIG))

def crc32c(data):
    c = 0xFFFFFFFF
    for b in data:
        c ^= b
        for _ in range(8): c = (c >> 1) ^ 0x82F63B78 if c & 1 else c >> 1
    return c ^ 0xFFFFFFFF
assert crc32c(b"123456789") == 0xE3069283

def guid_bytes(s):
    h = bytes.fromhex(s.replace("-", ""))
    return h[3::-1] + h[5:3:-1] + h[7:5:-1] + h[8:]

def read_vhdx(path, limit):
    """Independent VHDX reader: checks signatures and CRC-32C checksums, then returns
    (virtual size, first `limit` bytes of the disk)."""
    with open(path, "rb") as f: b = f.read()
    assert b[:8] == b"vhdxfile", "no vhdxfile signature"
    for off in (64 << 10, 128 << 10):
        h = bytearray(b[off:off + 4096]); assert h[:4] == b"head", "bad header"
        crc = struct.unpack("<I", h[4:8])[0]; h[4:8] = b"\0\0\0\0"; assert crc32c(h) == crc, "header checksum"
    regions = {}
    for off in (192 << 10, 256 << 10):
        rt = bytearray(b[off:off + (64 << 10)]); assert rt[:4] == b"regi", "bad region table"
        crc = struct.unpack("<I", rt[4:8])[0]; rt[4:8] = b"\0\0\0\0"; assert crc32c(rt) == crc, "region table checksum"
        for i in range(struct.unpack("<I", rt[8:12])[0]):
            e = rt[16 + i * 32:48 + i * 32]; regions[bytes(e[:16])] = struct.unpack("<QI", e[16:28])
    bat_off, _ = regions[guid_bytes("2DC27766-F623-4200-9D64-115E9BFD4A08")]
    md_off, _ = regions[guid_bytes("8B7CA206-4790-4B9A-B8FE-575F050F886E")]
    md = b[md_off:md_off + (1 << 20)]; assert md[:8] == b"metadata"
    items = {}
    for i in range(struct.unpack("<H", md[10:12])[0]):
        e = md[32 + i * 32:64 + i * 32]; o, l = struct.unpack("<II", e[16:24]); items[bytes(e[:16])] = md[o:o + l]
    block = struct.unpack("<I", items[guid_bytes("CAA16737-FA36-4D43-B3B6-33F0AA44E76B")][:4])[0]
    size = struct.unpack("<Q", items[guid_bytes("2FA54224-CD1B-4876-B211-5DBED83BF4B8")])[0]
    lss = struct.unpack("<I", items[guid_bytes("8141BF1D-A96F-4709-BA47-F233A8FAAB5F")])[0]
    chunk = (1 << 23) * lss // block
    out = bytearray()
    for blk in range((limit + block - 1) // block):
        idx = blk + blk // chunk; e = struct.unpack("<Q", b[bat_off + idx * 8:bat_off + idx * 8 + 8])[0]
        if e & 7 == 6: fo = (e >> 20) << 20; out += b[fo:fo + block]
        else: out += b"\0" * block
    return size, bytes(out[:limit])

# ---------------------------------------------------------------- CD audio (1 kHz / 440 Hz sine tones)
import math
def tone(freq, seconds):
    frames = int(seconds * 75); n = frames * 588  # 588 stereo samples per CD frame
    pcm = bytearray()
    for i in range(n):
        v = int(12000 * math.sin(2 * math.pi * freq * i / 44100))
        pcm += struct.pack("<hh", v, -v)
    return bytes(pcm)
A1, A2 = tone(1000, 2), tone(440, 1)
data_raw = raw(iso, 1)
# CUE: data track, two audio tracks in one file, the second with a 2-second pregap.
pregap = b"\0" * (150 * 2352)
w("audio/game.bin", data_raw + A1 + pregap + A2)
d_frames, a1_frames = len(data_raw) // 2352, len(A1) // 2352
def msf(f): return "%02d:%02d:%02d" % (f // 4500, (f // 75) % 60, f % 75)
w("audio/game.cue", ('FILE "game.bin" BINARY\n  TRACK 01 MODE1/2352\n    INDEX 01 00:00:00\n'
                     '  TRACK 02 AUDIO\n    INDEX 01 %s\n  TRACK 03 AUDIO\n    INDEX 00 %s\n    INDEX 01 %s\n'
                     % (msf(d_frames), msf(d_frames + a1_frames), msf(d_frames + a1_frames + 150))).encode())
# Same disc as CHD (audio stored big-endian, like chdman), GDI, MDS/MDF and NRG.
w("audio/t1.bin", data_raw); w("audio/t2.raw", A1); w("audio/t3.raw", A2)
subprocess.run([MKFIX.replace("mkfixtures", "mkchd"), "cd", p("audio/game.chd"), "zl", "MODE1_RAW:" + p("audio/t1.bin"), "AUDIO:" + p("audio/t2.raw"), "AUDIO:" + p("audio/t3.raw")], check=True)
w("audio/dc.gdi", b"3\n1 0 4 2352 t1.bin 0\n2 600 0 2352 t2.raw 0\n3 900 0 2352 \"t3.raw\" 0\n")
mdf = data_raw + A1 + A2
def mds3():
    m = bytearray(0x58 + 24 + 80 * 3); m[0:16] = b"MEDIA DESCRIPTOR"; m[16:18] = b"\x01\x05"; struct.pack_into("<H", m, 0x14, 1); struct.pack_into("<I", m, 0x50, 0x58)
    struct.pack_into("<iiHBBHHII", m, 0x58, -150, 0, 1, 3, 0, 1, 3, 0, 0x58 + 24)
    for i, (mode, off) in enumerate([(0xAA, 0), (0xA9, len(data_raw)), (0xA9, len(data_raw) + len(A1))]):
        t = 0x58 + 24 + i * 80; m[t] = mode; m[t + 4] = i + 1; struct.pack_into("<H", m, t + 0x10, 2352); struct.pack_into("<Q", m, t + 0x28, off)
    return bytes(m)
w("audio/alc.mdf", mdf); w("audio/alc.mds", mds3())
def nrg3():
    body = bytearray(struct.pack(">I", 0) + b"\0" * 13 + b"\0\0\0" + bytes([1, 3]))
    for mode, (s, e) in [(0, (0, len(data_raw))), (7, (len(data_raw), len(data_raw) + len(A1))), (7, (len(data_raw) + len(A1), len(mdf)))]:
        body += b"\0" * 12 + struct.pack(">H", 2352) + bytes([mode, 0, 0, 0]) + struct.pack(">QQQ", s, s, e)
    chunks = b"DAOX" + struct.pack(">I", len(body)) + bytes(body) + b"END!" + struct.pack(">I", 0)
    return mdf + chunks + b"NER5" + struct.pack(">Q", len(mdf))
w("audio/nero.nrg", nrg3())

# ---------------------------------------------------------------- remaining formats
# RAR4 archive (stored entries) holding a CUE/BIN pair in a folder.
def rar4(entries):
    def hcrc(b): return struct.pack("<H", zlib.crc32(b) & 0xFFFF)
    out = bytearray(b"Rar!\x1a\x07\x00")
    main = struct.pack("<BHHHI", 0x73, 0, 13, 0, 0); out += hcrc(main) + main
    for name, data in entries:
        nb = name.encode()
        h = struct.pack("<BHHIIBIIBBHI", 0x74, 0x8000, 32 + len(nb), len(data), len(data), 2, zlib.crc32(data), 0x5821_0000, 20, 0x30, len(nb), 0x20) + nb
        out += hcrc(h) + h + data
    end = struct.pack("<BHH", 0x7B, 0x4000, 7); out += hcrc(end) + end
    return bytes(out)
w("game.rar", rar4([("Game (Japan)\\Game (Japan).cue", b'FILE "Game (Japan).bin" BINARY\n  TRACK 01 MODE1/2352\n    INDEX 01 00:00:00\n'),
                    ("Game (Japan)\\Game (Japan).bin", raw(iso, 1))]))
crypted = os.path.join(os.environ.get("USERPROFILE", ""), r".cargo\registry\src\index.crates.io-1949cf8c6b5b557f\unrar-0.5.8\data\crypted.rar")
if os.path.exists(crypted): shutil.copy(crypted, p("locked.rar"))

# zstd-compressed QCOW2 (QEMU's compression type 1), every other cluster compressed.
try:
    from compression import zstd as _zstd
except ImportError:
    _zstd = None
def qcow2_zstd(raw_disk, cbits=16):
    cs = 1 << cbits; n = (len(raw_disk) + cs - 1) // cs; per = cs // 8; l1n = (n + per - 1) // per
    l1_off = cs; l2_off = 2 * cs; data_off = l2_off + l1n * cs
    l1 = bytearray(l1n * 8); l2 = bytearray(l1n * cs); body = bytearray()
    for i in range(l1n): struct.pack_into(">Q", l1, i * 8, (1 << 63) | (l2_off + i * cs))
    for c in range(n):
        chunk = raw_disk[c * cs:(c + 1) * cs].ljust(cs, b"\0")
        if not any(chunk): continue
        host = data_off + len(body)
        if c % 2 == 0:
            comp = _zstd.compress(chunk); x = 62 - (cbits - 8)
            nsect = (len(comp) + (host & 511) + 511) // 512
            struct.pack_into(">Q", l2, c * 8, (1 << 62) | ((nsect - 1) << x) | host)
            body += comp + b"\0" * ((-len(comp)) % 512)
        else:
            body += b"\0" * ((-(data_off + len(body))) % cs); host = data_off + len(body)
            struct.pack_into(">Q", l2, c * 8, (1 << 63) | host); body += chunk
    h = bytearray(cs)
    struct.pack_into(">4sIQIIQIIQQIIQ", h, 0, b"QFI\xfb", 3, 0, 0, cbits, len(raw_disk), 0, l1n, l1_off, 0, 0, 0, 0)
    struct.pack_into(">QQQII", h, 72, 8, 0, 0, 4, 112); h[104] = 1  # compression type: zstd
    return bytes(h) + bytes(l1).ljust(cs, b"\0") + bytes(l2) + bytes(body)
if _zstd: w("zstd.qcow2", qcow2_zstd(disk))

# D88 floppy: PC-98 2HD (77 cylinders x 2 heads x 8 sectors of 1024 bytes), sectors stored
# out of order inside each track, plus one missing track.
T_D88 = b"Hello from a D88 floppy!\r\n" * 4
d88vol = fat_volume(1232, 1024, 1, 12, 192, 0xFE, b"D88DISK", T_D88)
def d88(vol):
    hdr = bytearray(0x2B0); hdr[0:8] = b"TESTD88\0"; hdr[0x1B] = 0x20
    body = bytearray(); pos = 0x2B0
    for t in range(154):
        if t == 150: continue  # a missing (unformatted) track
        struct.pack_into("<I", hdr, 0x20 + t * 4, pos + len(body))
        for r in [5, 1, 8, 2, 7, 3, 6, 4]:  # shuffled sector order
            data = vol[(t * 8 + r - 1) * 1024:(t * 8 + r) * 1024]
            body += struct.pack("<BBBBHBBB5sH", t // 2, t % 2, r, 3, 8, 0, 0, 0, b"\0" * 5, 1024) + data
    struct.pack_into("<I", hdr, 0x1C, 0x2B0 + len(body))
    return bytes(hdr) + bytes(body)
w("pc98.d88", d88(d88vol))

# CHD parent/child: the child holds only changes; its parent must sit next to it.
iso_v2 = bytearray(iso); iso_v2[21 * S:21 * S + 64] = b"Hello from a CHILD CHD!\r\n".ljust(64, b" "); iso_v2 = bytes(iso_v2)
w("chdfam/v1.bin", raw(iso, 1)); w("chdfam/v2.bin", raw(iso_v2, 1))
MKCHD = MKFIX.replace("mkfixtures", "mkchd")
subprocess.run([MKCHD, "cd", p("chdfam/parent.chd"), "zl", "MODE1_RAW:" + p("chdfam/v1.bin")], check=True)
subprocess.run([MKCHD, "cdchild", p("chdfam/child.chd"), p("chdfam/parent.chd"), "MODE1_RAW:" + p("chdfam/v2.bin")], check=True)
os.makedirs(p("orphan")); shutil.copy(p("chdfam/child.chd"), p("orphan/child.chd"))

def wav_ok(folder, expect):
    files = sorted(f for f in os.listdir(folder) if f.endswith(".wav"))
    if len(files) != len(expect): return "expected %d WAV files, got %s" % (len(expect), files)
    for f, pcm in zip(files, expect):
        b = open(os.path.join(folder, f), "rb").read()
        if b[:4] != b"RIFF" or b[8:16] != b"WAVEfmt " or struct.unpack("<HHI", b[20:28]) != (1, 2, 44100): return f + ": bad WAV header"
        if b[44:] != pcm: return f + ": audio differs from the original (wrong position or byte order)"
    return None

# ---------------------------------------------------------------- run + verify
fails = 0
def run(name, kind, check, env=None):
    global fails
    out = os.path.join(OUT, name.replace(".", "_").replace("#", "_").replace("/", "_") + "." + kind)
    r = subprocess.run([EXE, "convert", p(name), out], capture_output=True, text=True, env=env)
    if r.returncode:
        print("FAIL %-20s %s" % (name, (r.stderr or r.stdout).strip().splitlines()[-1])); fails += 1; return
    err = check(out)
    print(("PASS" if not err else "FAIL"), "%-20s %s" % (name, err or "ok"))
    fails += bool(err)
iso_ok = lambda text: (lambda out: check_iso(open(out, "rb").read(), text))
def same_disk(out):
    got = read_vhd(out)
    # rustimgmount gives a zero-signature MBR disk a signature (Windows needs one); nothing else may change.
    if got[440:444] == b"\0\0\0\0": return "MBR disk signature still zero"
    return None if got[:440] + got[444:len(disk)] == disk[:440] + disk[444:] else "disk contents differ"
fat_ok = lambda text: (lambda out: check_fat_disk(read_vhd(out), text))

for n in ["game.iso.bz2", "game.7z", "splitiso.001", "alc2.mdf", "plain2.mdf", "e01b/game.E01", "ps1.pbp",
          "multi.pbp#1", "game.isz", "alcohol.mdx", "alc_v2.mds"]:
    run(n, "iso", iso_ok(T_ISO))
run("multi.pbp#2", "iso", iso_ok(T_ISO2))
run("split.img.001", "vhd", same_disk)
run("e01a/disk.E01", "vhd", same_disk)
run("e01c/multi.E01", "vhd", same_disk)
run("pc98.fdi", "vhd", fat_ok(T_FDI))
run("pc98.hdi", "vhd", fat_ok(T_HDI))
run("pc98.nhd", "vhd", fat_ok(T_HDI))
run("atari.st", "vhd", fat_ok(T_ST))
run("atari.msa", "vhd", fat_ok(T_ST))

# VHDX: a 2.5 TiB sparse disk (too big for VHD) and a small disk forced to VHDX.
import time
def vhdx_ok(expect_size):
    def check(out):
        size, got = read_vhdx(out, len(disk))
        if size != expect_size: return "virtual size %d, expected %d" % (size, expect_size)
        if got[:440] + got[444:] != disk[:440] + disk[444:]: return "disk contents differ"
        return check_fat_disk(got, disk_text)
    return check
t0 = time.time()
run("huge.qcow2", "vhdx", vhdx_ok(BIG))
print("     (2.5 TiB sparse disk converted in %.1f s)" % (time.time() - t0))
run("disk.img", "vhdx", vhdx_ok(len(disk)), env=dict(os.environ, RUSTIMGMOUNT_FORCE_VHDX="1"))

# GPT image converts intact (no clash with any disk in this PC, so IDs must be unchanged).
def gpt_ok(out):
    got = read_vhd(out); src = open(p("twins/gptA.img"), "rb").read()
    if got[:len(src)] != src: return "GPT disk changed although its IDs clash with nothing"
    return None
run("twins/gptA.img", "vhd", gpt_ok)

# Remaining formats.
run("game.rar", "iso", iso_ok(T_ISO))
if _zstd: run("zstd.qcow2", "vhd", same_disk)
else: print("SKIP zstd.qcow2           (this Python has no compression.zstd)")
run("pc98.d88", "vhd", fat_ok(T_D88))
def child_ok(out):
    data = open(out, "rb").read()
    return None if data[21 * S:21 * S + 25] == b"Hello from a CHILD CHD!\r\n" and data[16 * S + 1:16 * S + 6] == b"CD001" else "child CHD content wrong"
run("chdfam/child.chd", "iso", child_ok)
def expect_error(name, words):
    global fails
    r = subprocess.run([EXE, "info", p(name)], capture_output=True, text=True)
    ok = r.returncode == 1 and words in r.stderr
    print(("PASS" if ok else "FAIL"), "%-20s %s" % (name, "clear error: " + r.stderr.strip()[:90] if ok else "unexpected: " + (r.stderr or r.stdout).strip()[:120]))
    fails += not ok
expect_error("orphan/child.chd", "parent")
if os.path.exists(p("locked.rar")): expect_error("locked.rar", "password")

# CD audio export: every container must give back exactly the original tones.
for n in ["audio/game.cue", "audio/game.chd", "audio/dc.gdi", "audio/alc.mds", "audio/nero.nrg"]:
    folder = os.path.join(OUT, n.replace("/", "_") + "_audio")
    r = subprocess.run([EXE, "audio", p(n), folder], capture_output=True, text=True)
    err = (r.stderr or r.stdout).strip().splitlines()[-1] if r.returncode else wav_ok(folder, [A1, A2])
    print(("PASS" if not err else "FAIL"), "%-20s %s" % (n + " audio", err or "2 tracks, samples identical"))
    fails += bool(err)

# Audio companion disc: an ISO with TRACKnn.WAV + ALBUM.CUE, read back like an OS would.
def iso_files(data):
    pvd = data[16 * S:17 * S]
    if pvd[1:6] != b"CD001": return None
    root, size = struct.unpack("<I", pvd[158:162])[0], struct.unpack("<I", pvd[166:170])[0]
    d = data[root * S:root * S + size]; o = 0; out = {}
    while o < len(d):
        if d[o] == 0:  # records don't cross sectors: skip to the next one
            o = (o // S + 1) * S; continue
        n = d[o + 32]; name = d[o + 33:o + 33 + n]
        if name not in (b"\x00", b"\x01"):
            lba, sz = struct.unpack("<I", d[o + 2:o + 6])[0], struct.unpack("<I", d[o + 10:o + 14])[0]
            out[name.decode()] = data[lba * S:lba * S + sz]
        o += d[o]
    return out
for n in ["audio/game.cue", "audio/game.chd"]:
    out = os.path.join(OUT, n.replace("/", "_") + "_audio.iso")
    r = subprocess.run([EXE, "audio", p(n), out], capture_output=True, text=True)
    err = None
    if r.returncode: err = (r.stderr or r.stdout).strip().splitlines()[-1]
    else:
        files = iso_files(open(out, "rb").read())
        cue = (files or {}).get("ALBUM.CUE;1", b"").decode()
        if files is None: err = "not an ISO"
        elif sorted(files) != ["ALBUM.CUE;1", "TRACK02.WAV;1", "TRACK03.WAV;1"]: err = "unexpected files: %s" % sorted(files)
        elif files["TRACK02.WAV;1"][44:] != A1 or files["TRACK03.WAV;1"][44:] != A2: err = "audio differs from the original"
        elif files["TRACK02.WAV;1"][:4] != b"RIFF": err = "bad WAV header"
        elif 'FILE "TRACK02.WAV" WAVE' not in cue or "TRACK 03 AUDIO" not in cue: err = "ALBUM.CUE is wrong: %r" % cue
    print(("PASS" if not err else "FAIL"), "%-20s %s" % (n + " disc", err or "audio disc: 2 WAVs + ALBUM.CUE, samples identical"))
    fails += bool(err)

for n in ["set.m3u", "multi.pbp"]:
    r = subprocess.run([EXE, "info", p(n)], capture_output=True, text=True)
    ok = r.returncode == 0 and "Discs:   2" in r.stdout
    print(("PASS" if ok else "FAIL"), "%-20s info lists 2 discs" % n); fails += not ok
print("failures:", fails)
sys.exit(1 if fails else 0)
