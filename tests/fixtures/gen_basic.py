"""Generate test images in many formats from a tiny ISO and tiny FAT disks."""
import os, struct, sys, zlib

OUT = sys.argv[1]
os.makedirs(OUT, exist_ok=True)

def w(name, data):
    with open(os.path.join(OUT, name), "wb") as f:
        f.write(data)

# ---------------------------------------------------------------- ISO 9660
def both16(v): return struct.pack("<H", v) + struct.pack(">H", v)
def both32(v): return struct.pack("<I", v) + struct.pack(">I", v)

def dirrec(name, lba, size, flags):
    n = len(name)
    rec = bytearray(33 + n + ((n + 1) % 2))
    rec[0] = len(rec)
    rec[2:10] = both32(lba)
    rec[10:18] = both32(size)
    rec[18:25] = bytes([124, 1, 1, 0, 0, 0, 0])
    rec[25] = flags
    rec[28:32] = both16(1)
    rec[32] = n
    rec[33:33 + n] = name
    return bytes(rec)

def make_iso(total=400):
    S = 2048
    img = bytearray(S * total)
    content = b"Hello from the test ISO!\r\n" * 20
    pvd = bytearray(S)
    pvd[0] = 1; pvd[1:6] = b"CD001"; pvd[6] = 1
    pvd[8:40] = b" " * 32
    pvd[40:72] = b"TESTISO".ljust(32)
    pvd[80:88] = both32(total)
    pvd[120:124] = both16(1); pvd[124:128] = both16(1); pvd[128:132] = both16(S)
    pvd[132:140] = both32(10)
    pvd[140:144] = struct.pack("<I", 18); pvd[148:152] = struct.pack(">I", 19)
    pvd[156:190] = dirrec(b"\x00", 20, S, 2)
    for o, l in ((190, 128), (318, 128), (446, 128), (574, 128), (702, 37), (739, 37), (776, 37)):
        pvd[o:o + l] = b" " * l
    for o in (813, 830):
        pvd[o:o + 17] = b"2024010100000000\x00"
    for o in (847, 864):
        pvd[o:o + 17] = b"0000000000000000\x00"
    pvd[881] = 1
    img[16 * S:17 * S] = pvd
    term = bytearray(S); term[0] = 255; term[1:6] = b"CD001"; term[6] = 1
    img[17 * S:18 * S] = term
    img[18 * S:18 * S + 10] = bytes([1, 0]) + struct.pack("<I", 20) + struct.pack("<H", 1) + b"\x00\x00"
    img[19 * S:19 * S + 10] = bytes([1, 0]) + struct.pack(">I", 20) + struct.pack(">H", 1) + b"\x00\x00"
    root = dirrec(b"\x00", 20, S, 2) + dirrec(b"\x01", 20, S, 2) + dirrec(b"HELLO.TXT;1", 21, len(content), 0)
    img[20 * S:20 * S + len(root)] = root
    img[21 * S:21 * S + len(content)] = content
    # Some recognisable data spread over the disc
    for i in range(22, total):
        img[i * S:i * S + 16] = struct.pack("<QQ", i, 0xDEADBEEF)
    return bytes(img)

iso = make_iso()
w("test.iso", iso)
w("test_iso_as.img", iso)

def bcd(n): return ((n // 10) << 4) | (n % 10)

def raw_sector(lba, data, mode):
    a = lba + 150
    msf = bytes([bcd(a // 4500), bcd((a // 75) % 60), bcd(a % 75)])
    sync = b"\x00" + b"\xff" * 10 + b"\x00"
    if mode == 1:
        return sync + msf + b"\x01" + data + b"\x00" * 288
    # mode 2 form 1: subheader 8 bytes, data, edc/ecc 280
    return sync + msf + b"\x02" + b"\x00\x00\x08\x00\x00\x00\x08\x00" + data + b"\x00" * 280

def to_raw(iso, mode, sub=False):
    out = bytearray()
    for i in range(len(iso) // 2048):
        s = raw_sector(i, iso[i * 2048:(i + 1) * 2048], mode)
        assert len(s) == 2352
        out += s
        if sub:
            out += b"\x00" * 96
    return bytes(out)

mode1 = to_raw(iso, 1)
w("mode1.bin", mode1)
w("mode1.cue", b'FILE "mode1.bin" BINARY\r\n  TRACK 01 MODE1/2352\r\n    INDEX 01 00:00:00\r\n')
mode2 = to_raw(iso, 2)
w("mode2.bin", mode2)
w("mode2.cue", b'FILE "mode2.bin" BINARY\n  TRACK 01 MODE2/2352\n    INDEX 01 00:00:00\n')
# Audio track first, data track second, in one file
audio = b"\x11" * 2352 * 300
w("mixed.bin", audio + mode1)
w("mixed.cue", b'FILE "mixed.bin" BINARY\n  TRACK 01 AUDIO\n    INDEX 01 00:00:00\n  TRACK 02 MODE1/2352\n    INDEX 00 00:04:00\n    INDEX 01 00:04:00\n')
# Mode1 raw without a cue sheet (bare .bin)
w("bare.bin", mode1)
# Alcohol: 2448-byte sectors with subchannel
w("alcohol.mdf", to_raw(iso, 1, sub=True))
w("alcohol.mds", b"MEDIA DESCRIPTOR" + b"\x00" * 64)
# CloneCD
w("clone.img", mode1)
w("clone.ccd", b"[CloneCD]\nVersion=3\n")
w("clone.sub", b"")

# Nero NRG v2 with a DAOX chunk; track begins after a 150-sector pregap
pregap = 150 * 2048
start = pregap
end = start + len(iso)
dao = bytearray()
dao += struct.pack(">I", 0) + b"\x00" * 13 + b"\x00" + b"\x00" + b"\x00" + bytes([1, 1])
dao += b"\x00" * 12 + struct.pack(">H", 2048) + bytes([0, 0, 0, 0]) + struct.pack(">QQQ", 0, start, end)
chunks_off = end
chunks = b"DAOX" + struct.pack(">I", len(dao)) + bytes(dao) + b"END!" + struct.pack(">I", 0)
w("nero.nrg", b"\x00" * pregap + iso + chunks + b"NER5" + struct.pack(">Q", chunks_off))

# ---------------------------------------------------------------- FAT
def fat_volume(total_sectors, fat_bits, label):
    bps = 512
    spc = 1 if fat_bits == 12 else 4
    reserved = 1
    nfats = 2
    root_entries = 224 if fat_bits == 12 else 512
    root_secs = root_entries * 32 // bps
    clusters_guess = total_sectors // spc
    fat_size = ((clusters_guess + 2) * fat_bits // 8 + bps - 1) // bps + 1
    vol = bytearray(total_sectors * bps)
    bs = bytearray(512)
    bs[0:3] = b"\xEB\x3C\x90"
    bs[3:11] = b"MSDOS5.0"
    struct.pack_into("<HBHBHHBHHHII", bs, 11, bps, spc, reserved, nfats, root_entries,
                     total_sectors if total_sectors < 65536 else 0, 0xF0 if fat_bits == 12 else 0xF8,
                     fat_size, 18, 2, 0, total_sectors if total_sectors >= 65536 else 0)
    bs[36] = 0 if fat_bits == 12 else 0x80
    bs[38] = 0x29
    bs[39:43] = b"\x12\x34\x56\x78"
    bs[43:54] = label.ljust(11)
    bs[54:62] = (b"FAT12   " if fat_bits == 12 else b"FAT16   ")
    bs[510:512] = b"\x55\xAA"
    vol[0:512] = bs
    content = b"Hello from a FAT%d disk image!\r\n" % fat_bits
    for f in range(nfats):
        fo = (reserved + f * fat_size) * bps
        if fat_bits == 12:
            vol[fo:fo + 5] = bytes([bs[21], 0xFF, 0xFF, 0xFF, 0x0F])
        else:
            vol[fo:fo + 6] = struct.pack("<HHH", 0xFF00 | bs[21], 0xFFFF, 0xFFFF)
    root_off = (reserved + nfats * fat_size) * bps
    ent = bytearray(32)
    ent[0:11] = label.ljust(11); ent[11] = 0x08
    vol[root_off:root_off + 32] = ent
    ent = bytearray(32)
    ent[0:11] = b"HELLO   TXT"; ent[11] = 0x20
    struct.pack_into("<HHHI", ent, 22, 0, 0x5821, 2, len(content))
    vol[root_off + 32:root_off + 64] = ent
    data_off = root_off + root_secs * bps
    vol[data_off:data_off + len(content)] = content
    return bytes(vol)

floppy = fat_volume(2880, 12, b"FLOPPY")
w("floppy.img", floppy)
w("floppy.vfd", floppy)

def mbr_disk(size_mb=20):
    total = size_mb * 2048
    start = 2048
    vol = fat_volume(total - start, 16, b"HARDDISK")
    disk = bytearray(total * 512)
    mbr = bytearray(512)
    mbr[440:444] = b"\xAB\xCD\xEF\x01"
    pe = struct.pack("<B3sB3sII", 0, b"\xFE\xFF\xFF", 0x06, b"\xFE\xFF\xFF", start, total - start)
    mbr[446:462] = pe
    mbr[510:512] = b"\x55\xAA"
    disk[0:512] = mbr
    disk[start * 512:start * 512 + len(vol)] = vol
    # Scatter recognisable data in the free space of the volume to make sparse formats work
    for k in range(5):
        o = (start + 4000 + k * 3000) * 512
        disk[o:o + 512] = bytes([0x40 + k]) * 512
    return bytes(disk)

disk = mbr_disk()
w("disk.img", disk)

# ---------------------------------------------------------------- VDI
def vdi(raw, block=1 << 20):
    blocks = (len(raw) + block - 1) // block
    hdr = bytearray(512)
    hdr[0:40] = b"<<< Oracle VM VirtualBox Disk Image >>>\n"
    struct.pack_into("<I", hdr, 0x40, 0xBEDA107F)
    struct.pack_into("<I", hdr, 0x44, 0x00010001)
    struct.pack_into("<I", hdr, 0x48, 0x190)
    struct.pack_into("<I", hdr, 0x4C, 1)
    map_off = 512
    data_off = 512 + ((blocks * 4 + 511) // 512) * 512
    struct.pack_into("<II", hdr, 0x154, map_off, data_off)
    struct.pack_into("<Q", hdr, 0x170, len(raw))
    struct.pack_into("<III", hdr, 0x178, block, 0, blocks)
    bmap = []
    data = bytearray()
    idx = 0
    for b in range(blocks):
        chunk = raw[b * block:(b + 1) * block]
        if chunk.count(0) == len(chunk):
            bmap.append(0xFFFFFFFF)
        else:
            bmap.append(idx); idx += 1
            data += chunk.ljust(block, b"\x00")
    struct.pack_into("<I", hdr, 0x184, idx)
    m = b"".join(struct.pack("<I", x) for x in bmap)
    return bytes(hdr) + m.ljust(data_off - 512, b"\x00") + bytes(data)

w("disk.vdi", vdi(disk))

# ---------------------------------------------------------------- QCOW2 v3
def qcow2(raw, cbits=16, compress_every=3):
    cs = 1 << cbits
    nclusters = (len(raw) + cs - 1) // cs
    l2_entries = cs // 8
    l1_size = (nclusters + l2_entries - 1) // l2_entries
    hdr = bytearray(cs)
    # layout: cluster0 header, cluster1 L1, cluster 2.. L2 tables, then data
    l1_off = cs
    l2_base = 2 * cs
    data_pos = l2_base + l1_size * cs
    out = bytearray()
    l1 = [0] * l1_size
    l2 = [[0] * l2_entries for _ in range(l1_size)]
    data = bytearray()
    for c in range(nclusters):
        chunk = raw[c * cs:(c + 1) * cs].ljust(cs, b"\x00")
        if chunk.count(0) == cs:
            continue
        host = data_pos + len(data)
        if c % compress_every == 0:
            co = zlib.compressobj(9, zlib.DEFLATED, -15)
            comp = co.compress(chunk) + co.flush()
            x = 62 - (cbits - 8)
            nsect = (len(comp) + (host & 511) + 511) // 512
            entry = (1 << 62) | ((nsect - 1) << x) | host
            data += comp
            pad = (-len(data)) % 512
            data += b"\x00" * pad
        else:
            pad = (-(data_pos + len(data))) % cs
            data += b"\x00" * pad
            host = data_pos + len(data)
            entry = (1 << 63) | host
            data += chunk
        l2[c // l2_entries][c % l2_entries] = entry
    for i in range(l1_size):
        l1[i] = (1 << 63) | (l2_base + i * cs)
    struct.pack_into(">4sIQIIQIIQQIIQ", hdr, 0, b"QFI\xfb", 3, 0, 0, cbits, len(raw), 0, l1_size, l1_off, 0, 0, 0, 0)
    struct.pack_into(">QQQII", hdr, 72, 0, 0, 0, 4, 104)
    out += hdr
    out += b"".join(struct.pack(">Q", x) for x in l1).ljust(cs, b"\x00")
    for t in l2:
        out += b"".join(struct.pack(">Q", x) for x in t)
    out += data
    return bytes(out)

w("disk.qcow2", qcow2(disk))

# ---------------------------------------------------------------- VMDK
def vmdk_sparse(raw, grain=128, stream=False):
    gbytes = grain * 512
    cap = len(raw) // 512
    grains = (cap + grain - 1) // grain
    gtes = 512
    gts = (grains + gtes - 1) // gtes
    desc = ('# Disk DescriptorFile\nversion=1\nCID=12345678\nparentCID=ffffffff\ncreateType="%s"\n'
            'RW %d SPARSE "x.vmdk"\n' % ("streamOptimized" if stream else "monolithicSparse", cap)).encode()
    desc_off = 1
    desc_sz = 20
    gd_off = desc_off + desc_sz
    gt_off = gd_off + ((gts * 4 + 511) // 512)
    data_off = gt_off + gts * 4  # each GT = 512*4 bytes = 4 sectors
    overhead = data_off
    body = bytearray()
    gt = [0] * (gts * gtes)
    for g in range(grains):
        chunk = raw[g * gbytes:(g + 1) * gbytes].ljust(gbytes, b"\x00")
        if chunk.count(0) == gbytes:
            continue
        sector = data_off + len(body) // 512
        if stream:
            comp = zlib.compress(chunk)
            rec = struct.pack("<QI", g * grain, len(comp)) + comp
            rec += b"\x00" * ((-len(rec)) % 512)
            body += rec
        else:
            body += chunk
        gt[g] = sector
    flags = 1 | 2 | (1 << 16 | 1 << 17 if stream else 0)
    def header(gdo):
        h = bytearray(512)
        struct.pack_into("<4sIIQQQQIQQQB4sH", h, 0, b"KDMV", 3 if stream else 1, flags, cap, grain,
                         desc_off, desc_sz, gtes, 0, gdo, overhead, 0, b"\n \r\n", 1 if stream else 0)
        return h
    out = bytearray()
    out += header(0xFFFFFFFFFFFFFFFF if stream else gd_off)
    out += desc.ljust(desc_sz * 512, b"\x00")
    gd = b"".join(struct.pack("<I", gt_off + i * 4) for i in range(gts))
    out += gd.ljust((gt_off - gd_off) * 512, b"\x00")
    out += b"".join(struct.pack("<I", x) for x in gt)
    assert len(out) == data_off * 512, (len(out), data_off * 512)
    out += body
    if stream:
        # footer marker + footer header + EOS
        out += struct.pack("<QII", 1, 0, 3) + b"\x00" * 496
        out += header(gd_off)
        out += b"\x00" * 512
    return bytes(out)

w("disk_sparse.vmdk", vmdk_sparse(disk))
w("disk_stream.vmdk", vmdk_sparse(disk, stream=True))
w("disk-flat.vmdk", disk)
w("disk_flat.vmdk", ('# Disk DescriptorFile\nversion=1\nCID=fffffffe\nparentCID=ffffffff\ncreateType="monolithicFlat"\n\n'
                    'RW %d FLAT "disk-flat.vmdk" 0\n' % (len(disk) // 512)).encode())
# Split into two flat extents
half = len(disk) // 2
w("split-f001.vmdk", disk[:half]); w("split-f002.vmdk", disk[half:])
w("split.vmdk", ('# Disk DescriptorFile\nversion=1\nparentCID=ffffffff\ncreateType="twoGbMaxExtentFlat"\n'
                 'RW %d FLAT "split-f001.vmdk" 0\nRW %d FLAT "split-f002.vmdk" 0\n' % (half // 512, half // 512)).encode())
print("ok")
