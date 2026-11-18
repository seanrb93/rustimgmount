"""Render one file icon per image format into assets/formats/<ext>.ico, plus a preview sheet.

Each icon is a picture of what the format holds, with a coloured label carrying the
format's name: a disc for optical images (blue label) and emulator formats (purple), a
hard drive for virtual-machine disks (green), a plain file for raw and forensic images
(slate), and a floppy disk for floppy and retro-computer formats (teal). Shape and colour
tell the families apart at sizes too small for the text.

Needs Pillow (for the lettering). Usage: python assets/make_format_icons.py
The .ico files are committed; build.rs embeds them in rustimgmount.exe.
"""
import io, os, struct
from PIL import Image, ImageDraw, ImageFont
import make_icon

HERE = os.path.dirname(os.path.abspath(__file__))
OUT = os.path.join(HERE, "formats")
SIZES = [16, 20, 24, 32, 40, 48, 64, 256]

# family: (label colour, picture, extensions)
FAMILIES = {
    "optical": ("#2F6FD6", "disc", "iso bin cue nrg mdf mds mdx ccd cdi b5t b6t pdi isz"),
    "emulation": ("#8E44AD", "disc", "chd cso zso dax gdi ecm pbp"),
    "vm": ("#2E9E5B", "drive", "vhd vhdx avhd avhdx vmdk vdi qcow2 hdd"),
    "raw": ("#56667A", "file", "img ima raw dd 001 e01 ex01 dsk"),
    "retro": ("#12908A", "floppy", "hdi fdi nhd st msa d88 d77 d98 flp vfd"),
}


def font(px):
    for name in ("seguisb.ttf", "segoeuib.ttf", "arialbd.ttf"):
        try:
            return ImageFont.truetype(name, px)
        except OSError:
            pass
    return ImageFont.load_default()


def disc(n):
    """The disc alone, a little smaller and higher than in the app icon, as a PIL image."""
    img = make_icon.render(n, badge=False, center=(128, 104), radius=100)
    data = bytes(int(round(max(0.0, min(1.0, c)) * 255)) for px in img for c in px)
    return Image.frombytes("RGBA", (n, n), data)


def drawn(n, paint):
    """A picture painted in 256-unit design coordinates at 8x, reduced to n x n."""
    k = 8
    s = n * k / 256
    big = Image.new("RGBA", (n * k, n * k), (0, 0, 0, 0))
    d = ImageDraw.Draw(big)
    w = lambda units: max(1, int(round(units * s)))
    P = lambda pts: [(x * s, y * s) for x, y in pts]
    B = lambda x0, y0, x1, y1: [x0 * s, y0 * s, x1 * s, y1 * s]
    paint(d, P, B, w, s)
    return big.resize((n, n), Image.LANCZOS)


def drive(n):
    """A hard disk drive seen from above: casing, platter, spindle and the head arm."""
    def paint(d, P, B, w, s):
        d.rounded_rectangle(B(44, 12, 220, 240), radius=20 * s, fill=(0, 0, 0, 60))
        d.rounded_rectangle(B(38, 6, 214, 234), radius=20 * s, fill="#AEB7C1", outline="#5F6975", width=w(6))
        d.rounded_rectangle(B(52, 20, 200, 160), radius=12 * s, fill="#D3D9DF", outline="#8C96A2", width=w(3))
        d.ellipse(B(60, 26, 176, 142), fill="#EEF1F4", outline="#7D8792", width=w(4))
        d.ellipse(B(84, 50, 152, 118), outline="#C9D0D7", width=w(3))
        d.ellipse(B(102, 68, 134, 100), fill="#8F99A5", outline="#5F6975", width=w(3))
        d.ellipse(B(113, 79, 123, 89), fill="#3F4852")
        d.line(P([(184, 142), (140, 104)]), fill="#5F6975", width=w(13))
        d.line(P([(184, 142), (140, 104)]), fill="#9BA5B0", width=w(6))
        d.ellipse(B(170, 128, 198, 156), fill="#6C7682", outline="#3F4852", width=w(3))
        for x, y in ((52, 20), (200, 20)):
            d.ellipse(B(x - 7, y - 7, x + 7, y + 7), fill="#7D8792")
    return drawn(n, paint)


def plain_file(n):
    """A sheet of paper with a folded corner."""
    def paint(d, P, B, w, s):
        d.polygon(P([(58, 12), (170, 12), (218, 60), (218, 242), (58, 242)]), fill=(0, 0, 0, 55))
        d.polygon(P([(52, 6), (164, 6), (212, 54), (212, 236), (52, 236)]), fill="#FAFBFC", outline="#7D8792", width=w(6))
        d.polygon(P([(164, 6), (164, 54), (212, 54)]), fill="#CDD4DB", outline="#7D8792", width=w(6))
        for y in (78, 102, 126):
            d.rounded_rectangle(B(76, y, 188, y + 11), radius=5 * s, fill="#C3CAD2")
    return drawn(n, paint)


def floppy(n):
    """A 3.5-inch floppy disk: dark shell with a cut corner, metal shutter, paper label."""
    def paint(d, P, B, w, s):
        d.polygon(P([(42, 16), (202, 16), (228, 42), (228, 242), (42, 242)]), fill=(0, 0, 0, 60))
        d.polygon(P([(36, 10), (196, 10), (222, 36), (222, 236), (36, 236)]), fill="#39424F", outline="#1B2027", width=w(6))
        d.rectangle(B(82, 10, 182, 88), fill="#C9D0D7", outline="#6C7682", width=w(4))
        d.rectangle(B(146, 22, 168, 76), fill="#39424F")
        d.rectangle(B(58, 118, 200, 236), fill="#F2F4F6", outline="#9BA5B0", width=w(3))
        d.rectangle(B(44, 208, 56, 224), fill="#1B2027")
    return drawn(n, paint)


def label(n, text, colour):
    """The coloured label (and, from 32 px up, the format name), drawn at 8x and reduced."""
    k = 8
    s = n * k / 256  # pixels per design unit in the oversized drawing
    big = Image.new("RGBA", (n * k, n * k), (0, 0, 0, 0))
    d = ImageDraw.Draw(big)
    # Smaller icons get a taller band: it is all there is to tell formats apart.
    top = 150 if n >= 32 else 138
    box = [6 * s, top * s, 250 * s, 246 * s]
    d.rounded_rectangle([box[0] + 3 * s, box[1] + 5 * s, box[2] + 3 * s, box[3] + 5 * s], radius=22 * s, fill=(0, 0, 0, 70))
    d.rounded_rectangle(box, radius=22 * s, fill=colour, outline=(255, 255, 255, 235), width=max(1, int(5 * s)))
    if n >= 32:
        size = 86
        while size > 20:
            f = font(int(size * s))
            l, t, r, b = d.textbbox((0, 0), text, font=f)
            if r - l <= 216 * s:
                break
            size -= 2
        x = (box[0] + box[2]) / 2 - (l + r) / 2
        y = (box[1] + box[3]) / 2 - (t + b) / 2
        d.text((x, y), text, font=f, fill=(255, 255, 255, 255))
    return big.resize((n, n), Image.LANCZOS)


def ico(images):
    head = struct.pack("<HHH", 0, 1, len(images))
    entries, data = b"", b""
    off = 6 + 16 * len(images)
    for n, blob in images:
        entries += struct.pack("<BBBBHHII", n % 256, n % 256, 0, 0, 1, 32, len(blob), off + len(data))
        data += blob
    return head + entries + data


def main():
    os.makedirs(OUT, exist_ok=True)
    pictures = {"disc": disc, "drive": drive, "file": plain_file, "floppy": floppy}
    bases = {(name, n): make(n) for name, make in pictures.items() for n in SIZES}
    made = {}
    for colour, picture, exts in FAMILIES.values():
        for ext in exts.split():
            blobs = []
            for n in SIZES:
                img = Image.alpha_composite(bases[(picture, n)], label(n, ext.upper(), colour))
                buf = io.BytesIO()
                img.save(buf, "PNG", optimize=True)
                blobs.append((n, buf.getvalue()))
                made.setdefault(ext, {})[n] = img
            with open(os.path.join(OUT, ext + ".ico"), "wb") as f:
                f.write(ico(blobs))
    # Preview: every icon at 64, 32 and 16 px on light and dark backgrounds.
    exts = list(made)
    cols, cell = 12, 96
    rows = (len(exts) + cols - 1) // cols
    sheet = Image.new("RGBA", (cols * cell, rows * 150 * 2), (245, 245, 247, 255))
    for half, bg in enumerate([(245, 245, 247, 255), (32, 33, 36, 255)]):
        ImageDraw.Draw(sheet).rectangle([0, half * rows * 150, cols * cell, (half + 1) * rows * 150], fill=bg)
        for i, ext in enumerate(exts):
            x, y = (i % cols) * cell, half * rows * 150 + (i // cols) * 150
            sheet.alpha_composite(made[ext][64], (x + 16, y + 10))
            sheet.alpha_composite(made[ext][32], (x + 12, y + 86))
            sheet.alpha_composite(made[ext][16], (x + 56, y + 94))
    sheet.save(os.path.join(HERE, "format_icons_preview.png"))
    print("wrote %d icons to assets/formats and assets/format_icons_preview.png" % len(exts))


if __name__ == "__main__":
    main()
