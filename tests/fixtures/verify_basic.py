"""Run `rustimgmount convert` on each fixture and compare the logical contents."""
import os, struct, subprocess, sys

EXE, D = sys.argv[1], sys.argv[2]
OUTD = os.path.join(D, "out")
os.makedirs(OUTD, exist_ok=True)
for f in os.listdir(OUTD):
    os.remove(os.path.join(OUTD, f))

def rd(n):
    with open(os.path.join(D, n), "rb") as f:
        return f.read()

def read_vhd(path):
    with open(path, "rb") as f:
        b = f.read()
    foot = b[-512:]
    assert foot[:8] == b"conectix", "no footer"
    assert b[:512] == foot, "footer copy mismatch"
    cs = struct.unpack(">I", foot[64:68])[0]
    z = bytearray(foot); z[64:68] = b"\0\0\0\0"
    assert (~sum(z)) & 0xFFFFFFFF == cs, "bad checksum"
    size = struct.unpack(">Q", foot[48:56])[0]
    dh = b[512:1536]
    assert dh[:8] == b"cxsparse"
    bat_off, = struct.unpack(">Q", dh[16:24])
    nent, bsz = struct.unpack(">II", dh[28:36])
    out = bytearray(size)
    for i in range(nent):
        e, = struct.unpack(">I", b[bat_off + 4 * i: bat_off + 4 * i + 4])
        if e == 0xFFFFFFFF:
            continue
        data = b[e * 512 + 512: e * 512 + 512 + bsz]
        n = min(bsz, size - i * bsz)
        out[i * bsz:i * bsz + n] = data[:n]
    return bytes(out), len(b)

iso = rd("test.iso")
disk = rd("disk.img")
floppy = rd("floppy.img")
fails = 0

def conv(name):
    out = os.path.join(OUTD, name + ".out")  # no known extension: rustimgmount picks ISO or VHD
    r = subprocess.run([EXE, "convert", os.path.join(D, name), out], capture_output=True, text=True)
    if r.returncode != 0:
        raise Exception(r.stderr.strip())
    return out

def check(name, kind, expect):
    global fails
    try:
        out = conv(name)
        if kind == "iso":
            with open(out, "rb") as f:
                got = f.read()
            ok = got[:len(expect)] == expect and not any(got[len(expect):])
            info = "%d bytes" % len(got)
        else:
            got, fsize = read_vhd(out)
            ok = got[:len(expect)] == expect and not any(got[len(expect):])
            info = "virtual %d, file %d" % (len(got), fsize)
        print(("PASS" if ok else "FAIL"), name, info)
        fails += not ok
    except Exception as e:
        print("FAIL", name, e)
        fails += 1

for n in ["test_iso_as.img", "mode1.cue", "mode1.bin", "mode2.cue", "mode2.bin", "mixed.cue", "bare.bin",
          "alcohol.mds", "alcohol.mdf", "clone.ccd", "clone.img", "nero.nrg"]:
    check(n, "iso", iso)

for n in ["disk.img", "disk.vdi", "disk.qcow2", "disk_sparse.vmdk", "disk_stream.vmdk", "disk_flat.vmdk", "split.vmdk"]:
    check(n, "vhd", disk)

# Superfloppy: expect a synthesized MBR + 1 MiB gap + volume
try:
    out = conv("floppy.img")
    got, _ = read_vhd(out)
    mbr = got[:512]
    ok = (mbr[510:512] == b"\x55\xAA" and mbr[446 + 4] == 0x01 and struct.unpack("<I", mbr[454:458])[0] == 2048
          and got[1 << 20:(1 << 20) + len(floppy)] == floppy and len(got) >= 3 << 20)
    print("PASS" if ok else "FAIL", "floppy.img (superfloppy wrap)", len(got))
    fails += not ok
except Exception as e:
    print("FAIL floppy.img", e); fails += 1

# `info` on natively mountable files should not convert
for n in ["test.iso"]:
    r = subprocess.run([EXE, "info", os.path.join(D, n)], capture_output=True, text=True)
    print("INFO", n, "->", " | ".join(l.strip() for l in r.stdout.splitlines() if l.startswith(("Format", "Mount"))))

print("failures:", fails)
sys.exit(1 if fails else 0)
