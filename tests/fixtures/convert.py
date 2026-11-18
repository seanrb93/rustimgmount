"""`rustimgmount convert --to <format>`: converts fixture images made by the other suites
between formats and checks that nothing is lost on the way.

Usage: convert.py <rustimgmount.exe> <fixture-data dir> <work dir>"""
import os, shutil, struct, subprocess, sys, zlib

EXE, DATA, D = sys.argv[1], sys.argv[2], sys.argv[3]
shutil.rmtree(D, ignore_errors=True)
os.makedirs(D)
fails = 0
def src(n): return os.path.join(DATA, *n.split("/"))
def out(n): return os.path.join(D, n)
def read(path):
    with open(path, "rb") as f: return f.read()
def convert(*args, env=None):
    r = subprocess.run([EXE, "convert", *args], capture_output=True, text=True, env=env)
    lines = (r.stderr + r.stdout).strip().splitlines()
    return r.returncode, [l for l in lines if "%" not in l]
def report(name, err, ok="ok"):
    global fails
    print(("PASS" if not err else "FAIL"), "%-28s %s" % (name, err or ok))
    fails += bool(err)
def chain(name, steps, expect, final):
    """Run conversions one after another; the last output must equal `expect`."""
    for args in steps:
        code, lines = convert(*args)
        if code:
            return report(name, "%s: %s" % (os.path.basename(args[1]), lines[-1] if lines else "failed"))
    report(name, None if read(final) == expect else "result differs from the original", "identical after the round trip")

# ---------------------------------------------------------------- discs
game_bin = read(src("more/audio/game.bin"))
chain("cue -> chd -> cue", [(src("more/audio/game.cue"), out("game.chd")), (out("game.chd"), out("back.cue"))], game_bin, out("back.bin"))
cue = read(out("back.cue")).decode() if os.path.exists(out("back.cue")) else ""
want = ['FILE "back.bin" BINARY', "TRACK 01 MODE1/2352", "TRACK 02 AUDIO", "TRACK 03 AUDIO", "INDEX 00 00:07:25", "INDEX 01 00:09:25"]
report("cue sheet from chd", None if all(x in cue for x in want) else "unexpected cue sheet: %r" % cue, "tracks, modes and pregap kept")

# The data track of the CHD is the same file system as the cue's.
convert(src("more/audio/game.cue"), out("from_cue.iso")); convert(out("game.chd"), out("from_chd.iso"))
ok = os.path.exists(out("from_chd.iso")) and read(out("from_cue.iso")) == read(out("from_chd.iso"))
report("chd -> iso", None if ok else "ISO from the CHD differs from the ISO from the cue")

iso = read(src("basic/test.iso"))
def uncso(path):
    b = read(path)
    if b[:4] != b"CISO": raise ValueError("not a CSO")
    total, block, ver, align = struct.unpack("<QIBB", b[8:22])
    n = (total + block - 1) // block
    idx = struct.unpack("<%dI" % (n + 1), b[24:24 + 4 * (n + 1)])
    o = bytearray()
    for i in range(n):
        a, e = (idx[i] & 0x7FFFFFFF) << align, (idx[i + 1] & 0x7FFFFFFF) << align
        o += b[a:a + block] if idx[i] & 0x80000000 else zlib.decompress(b[a:e], -15)
    return bytes(o[:total])
code, lines = convert(src("basic/test.iso"), "--to", "cso", out("t.cso"))
err = lines[-1] if code else None
if not err:
    try: err = None if uncso(out("t.cso")) == iso else "decoded CSO differs from the ISO"
    except Exception as e: err = "CSO not readable: %s" % e
report("iso -> cso", err, "decodes to the same ISO (%d -> %d bytes)" % (len(iso), os.path.getsize(out("t.cso")) if not err else 0))
chain("cso -> iso", [(out("t.cso"), out("t_cso.iso"))], iso, out("t_cso.iso"))
chain("iso -> chd -> iso", [(src("basic/test.iso"), out("t.chd")), (out("t.chd"), out("t_chd.iso"))], iso, out("t_chd.iso"))
chain("iso -> cue (bin)", [(src("basic/test.iso"), out("t.cue"))], iso, out("t.bin"))
chain("mode2 cue -> chd -> cue", [(src("basic/mode2.cue"), out("m2.chd")), (out("m2.chd"), out("m2.cue"))], read(src("basic/mode2.bin")), out("m2.bin"))
report("mode2 cue sheet", None if "MODE2/2352" in read(out("m2.cue")).decode() else "track mode lost")

# Dreamcast GD-ROM: gdi -> chd -> gdi keeps every track and its address.
os.makedirs(out("gd"))
code, lines = convert(src("emu/disc.gdi"), out("dc.chd"))
code = code or convert(out("dc.chd"), out("gd/dc.gdi"))[0]
err = "conversion failed: %s" % lines[-1] if code else None
if not err:
    for n, ext in ((1, "bin"), (2, "raw"), (3, "bin"), (4, "raw"), (5, "bin")):
        if read(out("gd/dc (Track %02d).%s" % (n, ext))) != read(src("emu/track%02d.%s" % (n, ext))): err = "track %d differs" % n
    gdi = read(out("gd/dc.gdi")).decode().split("\r\n")
    if not err and (gdi[0] != "5" or not gdi[3].startswith('3 45000 4 2352 "dc (Track 03).bin"')): err = "unexpected gdi: %r" % gdi
report("gdi -> chd -> gdi", err, "5 tracks identical, addresses kept")

# ---------------------------------------------------------------- disks
disk = read(src("basic/disk.img"))
for f in ("vmdk", "vdi", "qcow2", "vhd", "vhdx", "chd"):
    chain("img -> %s -> img" % f, [(src("basic/disk.img"), out("d." + f)), (out("d." + f), "--to", "img", out("d_%s.img" % f))], disk, out("d_%s.img" % f))
chain("vmdk -> qcow2 -> vdi -> img", [(out("d.vmdk"), out("e.qcow2")), (out("e.qcow2"), out("e.vdi")), (out("e.vdi"), "--to", "img", out("e.img"))], disk, out("e.img"))
# Non-Windows formats get the exact bytes: a floppy stays a floppy (no partition table added).
floppy = read(src("basic/floppy.img"))
chain("floppy img -> vdi -> img", [(src("basic/floppy.img"), out("f.vdi")), (out("f.vdi"), "--to", "img", out("f.img"))], floppy, out("f.img"))
sizes = {f: os.path.getsize(out("d." + f)) for f in ("vmdk", "vdi", "qcow2") if os.path.exists(out("d." + f))}
report("sparse output", None if sizes and max(sizes.values()) < len(disk) else "VM disks are not smaller than the raw disk: %s" % sizes, "empty space not stored")

# ---------------------------------------------------------------- more formats, options
chain("iso -> zso -> iso", [(src("basic/test.iso"), out("t.zso")), (out("t.zso"), out("t_zso.iso"))], iso, out("t_zso.iso"))
chain("cue -> chd-zstd -> cue", [(src("more/audio/game.cue"), "--to", "chd-zstd", out("zs.chd")), (out("zs.chd"), out("zs.cue"))], game_bin, out("zs.bin"))
for f, e in (("vhd-fixed", "vhd"), ("qcow2-compressed", "qcow2"), ("chd-zstd", "chd")):
    o = out("v_%s.%s" % (f, e))
    chain("img -> %s -> img" % f, [(src("basic/disk.img"), "--to", f, o), (o, "--to", "img", out("v_%s.img" % f))], disk, out("v_%s.img" % f))
report("fixed vhd size", None if os.path.getsize(out("v_vhd-fixed.vhd")) == len(disk) + 512 else "not the disk plus a footer")
report("compressed qcow2", None if os.path.getsize(out("v_qcow2-compressed.qcow2")) < os.path.getsize(out("d.qcow2")) else "not smaller than plain qcow2")

# CloneCD output: raw sectors in .img, a .sub with position data, and a .ccd sheet.
code, lines = convert(src("more/audio/game.cue"), "--to", "ccd", out("g.ccd"), "--verify")
err = lines[-1] if code else None
if not err:
    sub = read(out("g.sub")); ccd = read(out("g.ccd")).decode()
    def crc16(b):
        c = 0
        for x in b:
            c ^= x << 8
            for _ in range(8): c = ((c << 1) ^ 0x1021 if c & 0x8000 else c << 1) & 0xFFFF
        return c ^ 0xFFFF
    q = sub[96 * 700 + 12:96 * 700 + 24]  # sector 700: first sector of track 3 proper (after its pregap)
    if read(out("g.img")) != game_bin: err = ".img differs from the cue's .bin"
    elif len(sub) != len(game_bin) // 2352 * 96: err = ".sub has the wrong size"
    elif q[:3] != bytes([0x01, 0x03, 0x01]) or q[3:6] != b"\0\0\0" or q[7:10] != bytes([0x00, 0x11, 0x25]): err = "Q channel of sector 700 is wrong: %s" % q.hex()
    elif struct.unpack(">H", q[10:12])[0] != crc16(q[:10]): err = "Q channel checksum is wrong"
    elif sub[96 * 600:96 * 600 + 12] != b"\xff" * 12 or sub[96 * 600 + 14] != 0: err = "pregap of track 3 is not flagged"
    elif "[TRACK 3]" not in ccd or "INDEX 0=550" not in ccd or "INDEX 1=700" not in ccd: err = "unexpected .ccd"
report("cue -> ccd/img/sub", err, "sectors identical, subchannel position data valid")
# Subchannel data survives ccd -> chd -> ccd unchanged (it is what LibCrypt-protected discs need).
if not err:
    marked = bytearray(read(out("g.sub"))); marked[96 * 123 + 20] ^= 0xFF; marked[96 * 500 + 40] = 0xA5  # "protection": odd values
    os.makedirs(out("sub")); os.makedirs(out("sub2"))
    for e in ("ccd", "img"): shutil.copy(out("g." + e), out("sub/g." + e))
    open(out("sub/g.sub"), "wb").write(marked); open(out("sub/g.sbi"), "wb").write(b"SBI\0test")
    code = convert(out("sub/g.ccd"), "--to", "chd", "--verify")[0] or convert(out("sub/g.chd"), "--to", "ccd", out("sub2/g.ccd"), "--verify")[0]
    err = "conversion failed" if code else None
    if not err and read(out("sub2/g.sub")) != bytes(marked): err = "subchannel data changed"
    elif not err and read(out("sub2/g.img")) != game_bin: err = "sectors changed"
    elif not err and not os.path.exists(out("sub/g.sbi")): err = "sbi vanished"
    report("ccd -> chd -> ccd", err, "subchannel data identical")
    chain("ccd -> cue (bin)", [(out("sub/g.ccd"), out("sub2/c.cue"))], game_bin, out("sub2/c.bin"))
    shutil.copy(out("sub/g.sbi"), out("sub/k.sbi")); shutil.copy(out("sub/g.ccd"), out("sub/k.ccd")); shutil.copy(out("sub/g.img"), out("sub/k.img"))
    convert(out("sub/k.ccd"), "--to", "chd")
    report(".sbi follows the image", None if os.path.exists(out("sub/k.sbi")) and not os.path.exists(out("sub2/k.sbi")) and os.path.exists(out("sub/k.chd")) else "missing")
    convert(out("sub/k.ccd"), "--to", "cue", "--out-dir", out("sub3"))
    report(".sbi copied to --out-dir", None if os.path.exists(out("sub3/k.sbi")) and os.path.exists(out("sub3/k.cue")) else "k.sbi / k.cue not in the folder")

# --verify notices a damaged result and removes it (the hook flips one byte after writing).
bad = dict(os.environ, RUSTIMGMOUNT_TEST_DAMAGE="1")
for f, args in (("chd", (src("more/audio/game.cue"), out("bad.chd"))), ("vdi", (src("basic/disk.img"), out("bad.vdi"))), ("cue", (out("game.chd"), out("bad.cue"))), ("cso", (src("basic/test.iso"), out("bad.cso")))):
    code, lines = convert(*args, "--verify", env=bad)
    left = [n for n in os.listdir(D) if n.startswith("bad.")]
    err = None if code and "did not read back the same" in " ".join(lines) and not left else "exit %d, left %s, said %r" % (code, left, lines[-1:] )
    report("verify catches damage (%s)" % f, err, "reported and deleted")
code, lines = convert(src("basic/test.iso"), out("t.cso"), "--overwrite", "--verify")
report("--overwrite", None if code == 0 and "Verified" in " ".join(lines) else "failed: %r" % lines[-1:])

# The DVD kind of CHD for a small image (what emulators want for PSP and PS2 discs).
code, lines = convert(src("basic/test.iso"), "--to", "chd-dvd", out("dvd.chd"), "--verify")
err = lines[-1] if code else None
if not err:
    convert(out("dvd.chd"), out("dvd.iso"))
    err = None if b"DVD " in read(out("dvd.chd")) and b"CHT2" not in read(out("dvd.chd")) else "not a DVD-type CHD"
    err = err or (None if read(out("dvd.iso")) == iso else "contents changed")
report("iso -> chd-dvd -> iso", err, "DVD-type CHD, identical after the round trip")
code, lines = convert(src("more/audio/game.cue"), "--to", "chd-dvd", out("no.chd"))
report("multi-track -> chd-dvd", None if code and "DVD type" in " ".join(lines) and not os.path.exists(out("no.chd")) else "expected a refusal, got %r" % lines[-1:], "refused: it has several tracks")

# What the Convert dialog offers.
def formats(*names):
    r = subprocess.run([EXE, "formats", *[src(n) for n in names]], capture_output=True, text=True)
    import json
    return [f["id"] for f in json.loads(r.stdout)["formats"]]
checks = [
    (("more/audio/game.cue",), ["iso", "cso", "zso", "chd", "chd-zstd", "chd-dvd", "ccd"]),
    (("basic/disk.img",), ["vhd", "vhd-fixed", "vhdx", "vmdk", "vdi", "qcow2", "qcow2-compressed"]),
    (("emu/disc.gdi",), ["iso", "cso", "zso", "chd", "chd-zstd"]),
    (("emu/gdrom.chd",), ["iso", "cso", "zso", "chd", "chd-zstd", "gdi"]),
    (("basic/test.iso", "basic/disk.img"), []),
    (("basic/test.iso", "more/audio/game.cue"), ["cso", "zso", "chd", "chd-zstd", "chd-dvd", "ccd"]),
]
bad_lists = ["%s -> %s" % (n, formats(*n)) for n, want in checks if formats(*n) != want]
report("formats offered", "; ".join(bad_lists) or None, "discs, disks, GD-ROMs and mixed selections")

# ---------------------------------------------------------------- refusals and clean-up
def refuses(name, args, words):
    code, lines = convert(*args)
    msg = lines[-1] if lines else ""
    report(name, None if code and all(w in msg for w in words) else "expected an error mentioning %s, got: %r" % (words, msg), "refused: " + msg[7:70])
refuses("disc -> vmdk", (src("basic/test.iso"), "--to", "vmdk", out("x.vmdk")), ["CD/DVD", "iso"])
refuses("disk -> iso", (src("basic/disk.img"), "--to", "iso", out("x.iso")), ["hard disk", "vhd"])
refuses("cue -> gdi", (src("basic/mixed.cue"), "--to", "gdi", out("x.gdi")), ["GD-ROM"])
refuses("existing output", (src("basic/test.iso"), out("t.cso")), ["already exists"])
refuses("same format", (out("t.cso"), "--to", "cso", out("y.cso")), ["already"])
refuses("unknown format", (src("basic/test.iso"), "--to", "zip"), ["unknown format"])
# A bare .bin of raw sectors only lacks its cue sheet: that is all that gets written.
shutil.copy(src("basic/mode2.bin"), out("lone.bin"))
code, lines = convert(out("lone.bin"), "--to", "cue", "--verify")
sheet = read(out("lone.cue")).decode() if os.path.exists(out("lone.cue")) else ""
err = (lines[-1] if code else None) or (None if 'FILE "lone.bin" BINARY' in sheet and "TRACK 01 MODE2/2352" in sheet else "unexpected cue: %r" % sheet)
err = err or (None if read(out("lone.bin")) == read(src("basic/mode2.bin")) else "the .bin was changed")
report("lone bin -> cue", err, "wrote only the cue sheet")
chain("lone bin + new cue -> iso", [(out("lone.cue"), out("lone.iso"))], iso, out("lone.iso"))
# The same for a .bin that is a plain 2048-byte image.
shutil.copy(src("basic/test.iso"), out("flat.bin"))
code, lines = convert(out("flat.bin"), "--to", "cue")
sheet = read(out("flat.cue")).decode() if os.path.exists(out("flat.cue")) else ""
err = (lines[-1] if code else None) or (None if 'FILE "flat.bin" BINARY' in sheet and "MODE1/2048" in sheet and read(out("flat.bin")) == iso else "unexpected: %r" % sheet)
report("plain bin -> cue", err, "wrote only the cue sheet")
chain("plain bin + new cue -> iso", [(out("flat.cue"), out("flat.iso"))], iso, out("flat.iso"))
# A conversion never writes over the image it is reading, even with --overwrite: a CloneCD
# image named by its .img would have that very .img rewritten.
for e in ("ccd", "img", "sub"): shutil.copy(out("g." + e), out("own." + e))
before = read(out("own.img"))
refuses("never overwrites its source", (out("own.img"), "--to", "ccd", "--overwrite"), ["part of the image being converted"])
report("source untouched", None if read(out("own.img")) == before else "the source file was changed")

# Recompressing: a flavour of the same format gets its own name; the plain format is refused.
for name, to, made, back in (("zs.chd", "chd-zstd", "zs (zstd).chd", None), ("d.vhd", "vhd-fixed", "d (fixed).vhd", disk), ("d.qcow2", "qcow2-compressed", "d (compressed).qcow2", disk)):
    code, lines = convert(out(name), "--to", to, "--verify")
    err = lines[-1] if code else None if os.path.exists(out(made)) else "%s was not written" % made
    if not err and back is not None:
        convert(out(made), "--to", "img", out(made + ".img"))
        err = None if read(out(made + ".img")) == back else "contents changed"
    report("%s -> %s" % (name.split(".")[-1], to), err, "wrote " + made)
chain("recompressed chd -> cue", [(out("zs (zstd).chd"), out("rz.cue"))], game_bin, out("rz.bin"))
# ...and back to the standard flavour.
for name, to, made in (("zs (zstd).chd", "chd", "zs (zstd) (lzma).chd"), ("d (fixed).vhd", "vhd", "d (fixed) (dynamic).vhd"), ("d (compressed).qcow2", "qcow2", "d (compressed) (uncompressed).qcow2")):
    code, lines = convert(out(name), "--to", to, "--verify")
    report("%s -> %s" % (name, to), lines[-1] if code else None if os.path.exists(out(made)) else "%s was not written" % made, "wrote " + made)
report("back to dynamic vhd", None if os.path.getsize(out("d (fixed) (dynamic).vhd")) < os.path.getsize(out("d (fixed).vhd")) else "not smaller than the fixed one")
left = [f for r, _, fs in os.walk(D) for f in fs if f.endswith(".partial")]
report("no partial files", "left behind: %s" % left if left else None, "failed conversions leave nothing")

# A conversion killed half-way (Cancel in the progress dialog) is tidied up by the next command.
data = out("appdata"); os.makedirs(os.path.join(data, "converting"))
partial = out("killed.chd.partial"); open(partial, "wb").write(b"x" * 100)
keep = out("keep.txt"); open(keep, "w").write("not a partial file")
open(os.path.join(data, "converting", "4000000000.txt"), "w").write(partial + "\n" + keep + "\n")
env = dict(os.environ, RUSTIMGMOUNT_DATA=data, RUSTIMGMOUNT_CACHE=os.path.join(data, "cache"))
subprocess.run([EXE, "info", src("basic/test.iso")], capture_output=True, env=env)
err = "partial file still there" if os.path.exists(partial) else "deleted a file that isn't a partial" if not os.path.exists(keep) else None
report("cancelled conversion", err, "its .partial file is removed by the next command")

print("failures:", fails)
sys.exit(1 if fails else 0)
