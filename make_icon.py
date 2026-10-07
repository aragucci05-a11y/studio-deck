# Generates studiodeck.ico (16-256 px): graphite rounded square, 2x2 tile grid, one blue tile with a green dot.
#   python make_icon.py
from PIL import Image, ImageDraw

S = 1024  # draw big, downsample for smooth edges


def lerp(a, b, t):
    return tuple(int(a[i] + (b[i] - a[i]) * t) for i in range(3))


img = Image.new("RGBA", (S, S), (0, 0, 0, 0))
# vertical graphite -> near-black gradient, masked by a rounded square
grad = Image.new("RGBA", (S, S))
gd = ImageDraw.Draw(grad)
for y in range(S):
    gd.line([(0, y), (S, y)], fill=lerp((72, 72, 78), (16, 16, 18), y / S) + (255,))
mask = Image.new("L", (S, S), 0)
m = 40
ImageDraw.Draw(mask).rounded_rectangle([m, m, S - m, S - m], radius=230, fill=255)
img.paste(grad, (0, 0), mask)

d = ImageDraw.Draw(img)
pad, gap = 230, 56
tile = (S - 2 * pad - gap) // 2
for i in range(4):
    x = pad + (i % 2) * (tile + gap)
    y = pad + (i // 2) * (tile + gap)
    color = (10, 132, 255, 255) if i == 0 else (235, 235, 240, 70)
    d.rounded_rectangle([x, y, x + tile, y + tile], radius=60, fill=color)
    if i == 0:
        r = 46
        cx, cy = x + tile - 80, y + tile - 80
        d.ellipse([cx - r, cy - r, cx + r, cy + r], fill=(48, 209, 88, 255))

sizes = [16, 24, 32, 48, 64, 256]
img.resize((256, 256), Image.LANCZOS).save("studiodeck.ico", sizes=[(s, s) for s in sizes])
