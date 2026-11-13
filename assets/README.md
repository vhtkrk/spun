# Tray icon

The tray badge is the right side of the wordmark: the U and N that spin into
each other. It is not stored as an image file. `build.rs` rasterizes
`spun-wordmark.svg` at build time into two 8-bit coverage masks, one per
glyph, and `src/icon.rs` draws them at runtime in the configured colours and
rotation. Keeping the native pixel representation avoids a runtime image
decoder or reliance on icon themes. Building needs `rsvg-convert`
(librsvg2-bin).

The masks are square and 128 px wide, and the art spans about 116 px of that.
A panel fits the badge into a square slot, so a square pixmap uses the whole
slot; the earlier 239×128 canvas was scaled down by width and ended up about
half the height of the slot. Building the rotation into the render means the
art can turn to any angle without clipping, so only the corners of the square
are spare.

The pixels are ARGB32 in network byte order, as StatusNotifierItem requires,
and transparent pixels have zero alpha. The two glyphs are drawn separately,
so the U can be one colour and the N another; see the `[looks]` section in the
README.

The image was created for spun and uses the project's license.

# Logo

`spun-wordmark.svg` is the wordmark, `spun-wordmark.png` its raster. Each
letter is one ribbon with a groove down the middle. The colour is the teal of
the tray badge, `#2FB0AC`. The SVG draws the groove with a mask, so set the
colour with the `fill` of the masked `rect`. Inside the mask, each letter is a
group with an id: `spun-s`, `spun-p`, `spun-u` and `spun-n`. `build.rs` hides
all groups except one to cut a single glyph out of the wordmark.

# App icon

`packaging/foo.xn__nda.spun.png` is the icon for launchers and app stores: the
badge in the brand teal, 256 px square. It is the square of the wordmark that
`build.rs` uses, with the S and the P hidden. It is a PNG, because the
wordmark draws its fades with a luminance mask, which not every icon renderer
supports. After a change to the wordmark, render it again:

```sh
sed 's|<svg[^>]*>|<svg xmlns="http://www.w3.org/2000/svg" width="256" height="256" viewBox="126.3 -28.35 152 152"><style>#spun-s,#spun-p{display:none}</style>|' \
  assets/spun-wordmark.svg | rsvg-convert -w 256 -h 256 -o packaging/foo.xn__nda.spun.png
```
