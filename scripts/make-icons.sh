#!/bin/sh
# Rebuilds assets/icon.svg and the icon files in assets/icons from
# assets/logo.svg. Run after changing the logo.
# Needs: python3, ImageMagick (convert), and png2icns (icnsutils).
set -eu
cd "$(dirname "$0")/.."

python3 - <<'PY'
import re
logo = open("assets/logo.svg").read()
inner = logo[logo.index(">", logo.index("<svg")) + 1 : logo.rindex("</svg>")]
inner = re.sub(r"<!--.*?-->", "", inner, flags=re.S).strip()
open("assets/icon.svg", "w").write(f'''<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1024 1024" role="img" aria-label="Captain Zevo">
  <!-- App icon: the logo from logo.svg on a rounded square, sized to the
       macOS icon grid. Regenerate with scripts/make-icons.sh after changing
       logo.svg. -->
  <rect x="100" y="100" width="824" height="824" rx="185" fill="#161c15"/>
  <rect x="100" y="100" width="824" height="824" rx="185" fill="none" stroke="#2f3b2c" stroke-width="6"/>
  <g transform="translate(160 160) scale(11)">
    {inner}
  </g>
</svg>
''')
PY

for size in 16 32 128 256 512 1024; do
    convert -background none -density 384 assets/icon.svg -resize "${size}x${size}" \
        "assets/icons/icon-${size}.png"
done
png2icns assets/icons/icon.icns assets/icons/icon-16.png assets/icons/icon-32.png \
    assets/icons/icon-128.png assets/icons/icon-256.png assets/icons/icon-512.png \
    assets/icons/icon-1024.png >/dev/null
echo "Wrote assets/icon.svg and assets/icons/"
