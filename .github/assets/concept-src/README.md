# difflore-concept.gif source

`concept.html` is the whole animation: a deterministic `window.seek(t)`
renders the state for any timestamp, so the GIF is fully reproducible and
editable — change the HTML, re-run the two commands below.

Storyboard (13s): team decisions in two places (PR review history + live
session correction) → difflore drafts rule candidates with provenance →
human approves the real ones and rejects noise → agent recalls the approved
rules over MCP before editing → review gate: no repeat comments → tagline.

## Regenerate

Requires ffmpeg and any `node_modules` containing playwright (with chromium
installed), e.g. the difflore-cloud checkout:

```bash
node capture.mjs ../../../..//difflore-cloud/node_modules   # or omit arg if playwright resolves
ffmpeg -y -framerate 10 -i frames/f_%03d.png \
  -vf "scale=960:540:flags=lanczos,split[a][b];[a]palettegen=max_colors=128[p];[b][p]paletteuse=dither=bayer:bayer_scale=5" \
  ../difflore-concept.gif
rm -rf frames
```
