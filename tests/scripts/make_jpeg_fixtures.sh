#!/bin/sh
# Regenerates tests/fixtures/jpeg: small JPEGs written by libjpeg-turbo's cjpeg
# and the djpeg (islow IDCT, fancy upsampling) decode of each, which the
# decoder tests in src/jpeg.rs use as their libjpeg reference.
set -eu
cd "$(dirname "$0")/../fixtures/jpeg"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

python3 - "$work/source.ppm" <<'EOF'
import sys
width, height = 37, 29
pixels = bytearray()
for y in range(height):
    for x in range(width):
        edge = 255 if (x // 6 + y // 5) % 2 else 0
        pixels += bytes(((x * 7) % 256, (y * 9) % 256, (edge + x * y) % 256))
with open(sys.argv[1], "wb") as out:
    out.write(b"P6\n%d %d\n255\n" % (width, height) + pixels)
EOF

printf '0;\n1;\n2;\n' > "$work/scans.txt"

encode() {
    name=$1
    shift
    cjpeg "$@" -outfile "$name.jpg" "$work/source.ppm"
}

encode s444 -quality 90 -sample 1x1
encode s422 -quality 90 -sample 2x1
encode s420_restart -quality 85 -sample 2x2 -restart 2B
encode s440 -quality 90 -sample 1x2
encode s31 -quality 90 -sample 3x1
encode s32_odd -quality 90 -sample 3x2,1x1,1x2
encode gray -quality 90 -grayscale -restart 1
encode adobe_rgb -quality 90 -rgb
encode multiscan -quality 90 -sample 2x2 -scans "$work/scans.txt"
encode sof1_q16 -quality 2
encode progressive -quality 90 -progressive
encode arithmetic -quality 90 -arithmetic

for name in s444 s422 s420_restart s440 s31 s32_odd adobe_rgb multiscan sof1_q16; do
    djpeg -dct int -ppm -outfile "$name.ppm" "$name.jpg"
done
djpeg -dct int -pnm -outfile gray.pgm gray.jpg
