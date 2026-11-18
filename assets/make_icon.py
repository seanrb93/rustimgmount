"""Render the rustimgmount icon (assets/rustimgmount.ico) and a preview PNG.

Pure Python, no dependencies: shapes are signed-distance functions in a 256-unit design
space, rendered natively at every icon size with analytic anti-aliasing.
Usage: python assets/make_icon.py
"""
import math, os, struct, zlib

HERE = os.path.dirname(os.path.abspath(__file__))
SIZES = [16, 20, 24, 32, 40, 48, 64, 96, 128, 256]


def hexc(h, a=1.0):
    h = h.lstrip("#")
    return [int(h[i:i + 2], 16) / 255 for i in (0, 2, 4)] + [a]


def lerp(a, b, t):
    return [x + (y - x) * t for x, y in zip(a, b)]


def clamp(v, lo=0.0, hi=1.0):
    return lo if v < lo else hi if v > hi else v


# ---------------------------------------------------------------- shapes (SDF, design units)
def d_circle(x, y, cx, cy, r):
    return math.hypot(x - cx, y - cy) - r


def d_ring(x, y, cx, cy, r_in, r_out):
    d = math.hypot(x - cx, y - cy)
    return max(r_in - d, d - r_out)


def d_rrect(x, y, cx, cy, hw, hh, r):
    qx, qy = abs(x - cx) - hw + r, abs(y - cy) - hh + r
    return math.hypot(max(qx, 0), max(qy, 0)) + min(max(qx, qy), 0) - r


def d_poly(x, y, pts):
    """Convex polygon (clockwise in screen coords) as an intersection of half-planes."""
    d = -1e9
    n = len(pts)
    for i in range(n):
        (x1, y1), (x2, y2) = pts[i], pts[(i + 1) % n]
        ex, ey = x2 - x1, y2 - y1
        l = math.hypot(ex, ey)
        d = max(d, ((x - x1) * ey - (y - y1) * ex) / l)
    return d


# ---------------------------------------------------------------- the icon
DISC_C, DISC_R = (112, 112), 104
BADGE = dict(cx=188, cy=188, hw=58, hh=48, r=14)


def render(n):
    px = 256 / n  # design units per pixel
    small = n <= 24
    img = [[0.0, 0.0, 0.0, 0.0] for _ in range(n * n)]

    def cov(d):
        return clamp(0.5 - d / px)

    def over(i, col, a):
        if a <= 0:
            return
        dst = img[i]
        sa = col[3] * a
        oa = sa + dst[3] * (1 - sa)
        if oa <= 0:
            return
        for k in range(3):
            dst[k] = (col[k] * sa + dst[k] * dst[3] * (1 - sa)) / oa
        dst[3] = oa

    cx, cy = DISC_C
    # Small sizes get a bigger drive badge so the "mount" arrow stays readable.
    b = dict(cx=176, cy=178, hw=78, hh=66, r=18) if small else BADGE
    for j in range(n):
        for i in range(n):
            x, y = (i + 0.5) * px, (j + 0.5) * px
            k = j * n + i
            dist = math.hypot(x - cx, y - cy)
            ang = math.atan2(y - cy, x - cx)

            # Disc body: silver with two soft reflection wedges and a faint iridescent tint.
            a = cov(dist - DISC_R)
            if a > 0:
                sheen = 0.5 + 0.5 * math.cos(2 * (ang + 0.75))
                base = lerp(hexc("#AEB7C1"), hexc("#F3F6F9"), 0.25 + 0.75 * sheen ** 1.6)
                tint = lerp(hexc("#C9D8FF"), hexc("#FFD9CC"), 0.5 + 0.5 * math.sin(ang * 3 + 1))
                col = lerp(base, tint, 0.18 * (dist / DISC_R))
                col[3] = 1
                over(k, col, a)
                # darker outer rim
                rim = max(1.6, 0.55 * px)  # at least ~1 px wide at small sizes
                over(k, hexc("#7D8792"), cov(abs(dist - (DISC_R - rim)) - rim) * a)
            # Rust-copper label ring with a light inner and outer edge.
            a = cov(d_ring(x, y, cx, cy, 24, 56))
            if a > 0:
                t = clamp(((x - cx) + (y - cy) + 56) / 112)
                over(k, lerp(hexc("#D9692A"), hexc("#8A3410"), t), a)
                over(k, hexc("#F29A5C", 0.9), cov(abs(dist - 55) - 1.4))
                over(k, hexc("#5E2208", 0.6), cov(abs(dist - 25) - 1.2))
            # Clear plastic hub around the hole.
            over(k, hexc("#E9EEF2", 0.85), cov(d_ring(x, y, cx, cy, 13, 24)))

            # Drive badge: soft shadow, dark body with outline.
            over(k, [0, 0, 0, 0.30], cov(d_rrect(x, y, b["cx"] + 3, b["cy"] + 4, b["hw"] + 2, b["hh"] + 2, b["r"] + 2)))
            d = d_rrect(x, y, b["cx"], b["cy"], b["hw"], b["hh"], b["r"])
            a = cov(d)
            if a > 0:
                t = clamp((y - (b["cy"] - b["hh"])) / (2 * b["hh"]))
                over(k, lerp(hexc("#46525F"), hexc("#1E252D"), t), a)
                over(k, hexc("#11161B"), cov(abs(d + 1.2) - 1.4))
                # top highlight line
                over(k, hexc("#FFFFFF", 0.18), cov(d_rrect(x, y, b["cx"], b["cy"] - b["hh"] + 6, b["hw"] - 12, 1.5, 1.5)))
            # Mount arrow (orange, pointing down into the drive).
            ax, ay = b["cx"] - (0 if small else 10), b["cy"] - (2 if small else 6)
            s = 1.55 if small else 1.0
            arrow = min(
                d_rrect(x, y, ax, ay - 14 * s, 7 * s * (1.25 if small else 1), 14 * s, 2),
                d_poly(x, y, [(ax - 26 * s, ay - 2 * s), (ax + 26 * s, ay - 2 * s), (ax, ay + 26 * s)]),
            )
            over(k, hexc("#F28C38"), cov(arrow))
            over(k, hexc("#FFD2A8", 0.55), cov(abs(arrow + 1) - 0.9) * cov(arrow))
            if not small:
                # Drive slot and green activity LED.
                over(k, hexc("#0B0F13"), cov(d_rrect(x, y, b["cx"] - 10, b["cy"] + 30, 34, 4, 3)))
                led = d_circle(x, y, b["cx"] + 38, b["cy"] + 30, 6)
                over(k, hexc("#5BE37A", 0.45), cov(led - 5))
                over(k, hexc("#6CF08A"), cov(led))

            # Punch the centre hole through everything (badge doesn't reach it).
            hole = cov(dist - 13)
            if hole > 0:
                img[k][3] *= 1 - hole
    return img


# ---------------------------------------------------------------- encoders
def png(img, n):
    raw = bytearray()
    for j in range(n):
        raw.append(0)
        for i in range(n):
            r, g, b_, a = img[j * n + i]
            raw += bytes(int(round(clamp(c) * 255)) for c in (r, g, b_, a))

    def chunk(t, d):
        return struct.pack(">I", len(d)) + t + d + struct.pack(">I", zlib.crc32(t + d) & 0xFFFFFFFF)

    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", n, n, 8, 6, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(bytes(raw), 9)) + chunk(b"IEND", b""))


def ico(images):
    head = struct.pack("<HHH", 0, 1, len(images))
    entries, data = b"", b""
    off = 6 + 16 * len(images)
    for n, blob in images:
        entries += struct.pack("<BBBBHHII", n % 256, n % 256, 0, 0, 1, 32, len(blob), off + len(data))
        data += blob
    return head + entries + data


def main():
    pngs = [(n, png(render(n), n)) for n in SIZES]
    with open(os.path.join(HERE, "rustimgmount.ico"), "wb") as f:
        f.write(ico(pngs))
    # Preview: the 256 px icon at 2x, plus every size at 1:1 on light and dark strips.
    big = render(512)
    W = 512 + 40 + sum(SIZES) + 10 * len(SIZES)
    H = max(512, 2 * max(SIZES) + 60)
    canvas = [[0.96, 0.96, 0.97, 1.0] for _ in range(W * H)]

    def blit(img, n, ox, oy, bg):
        for j in range(n):
            for i in range(n):
                s = img[j * n + i]
                d = canvas[(oy + j) * W + ox + i]
                a = s[3]
                for c in range(3):
                    d[c] = s[c] * a + bg[c] * (1 - a)

    blit(big, 512, 0, 0, [0.96, 0.96, 0.97])
    x = 552
    for n in SIZES:
        img = render(n)
        for row, bg in ((10, [0.96, 0.96, 0.97]), (max(SIZES) + 40, [0.12, 0.13, 0.15])):
            for j in range(-6, n + 6):
                for i in range(-4, n + 6):
                    canvas[(row + j) * W + x + i] = bg + [1.0]
            blit(img, n, x, row, bg)
        x += n + 10
    with open(os.path.join(HERE, "icon_preview.png"), "wb") as f:
        f.write(png_rect(canvas, W, H))
    # 8x nearest-neighbour zoom of the small sizes, for checking pixels.
    small = [16, 20, 24, 32]
    zw = sum(n * 8 + 16 for n in small)
    zh = 32 * 8 * 2 + 48
    zoom = [[0.5, 0.5, 0.5, 1.0] for _ in range(zw * zh)]
    x = 8
    for n in small:
        img = render(n)
        for oy, bg in ((8, [0.96, 0.96, 0.97]), (32 * 8 + 32, [0.12, 0.13, 0.15])):
            for j in range(n * 8):
                for i in range(n * 8):
                    s = img[(j // 8) * n + i // 8]
                    zoom[(oy + j) * zw + x + i] = [s[c] * s[3] + bg[c] * (1 - s[3]) for c in range(3)] + [1.0]
        x += n * 8 + 16
    with open(os.path.join(HERE, "icon_zoom.png"), "wb") as f:
        f.write(png_rect(zoom, zw, zh))
    print("wrote assets/rustimgmount.ico, assets/icon_preview.png, assets/icon_zoom.png")


def png_rect(img, w, h):
    raw = bytearray()
    for j in range(h):
        raw.append(0)
        for i in range(w):
            raw += bytes(int(round(clamp(c) * 255)) for c in img[j * w + i])

    def chunk(t, d):
        return struct.pack(">I", len(d)) + t + d + struct.pack(">I", zlib.crc32(t + d) & 0xFFFFFFFF)

    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 6, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(bytes(raw), 9)) + chunk(b"IEND", b""))


if __name__ == "__main__":
    main()
