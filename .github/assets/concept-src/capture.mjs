// Renders concept.html frame-by-frame with Playwright and writes PNGs to ./frames.
// The animation is deterministic: concept.html exposes window.seek(t) so every
// frame is reproducible. Assemble the GIF with the ffmpeg command in README.md.
//
// Usage: node capture.mjs [path-to-node_modules-containing-playwright]
import { mkdirSync } from 'node:fs';
import { pathToFileURL } from 'node:url';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const pwRoot = process.argv[2];
const { chromium } = await import(
  pwRoot ? pathToFileURL(join(pwRoot, 'playwright', 'index.mjs')).href : 'playwright'
);

const OUT = join(HERE, 'frames');
mkdirSync(OUT, { recursive: true });

const FPS = 10;
const DURATION = 13.0;
const FRAMES = Math.round(FPS * DURATION);

const browser = await chromium.launch();
const page = await browser.newPage({
  viewport: { width: 960, height: 540 },
  deviceScaleFactor: 2,
});
await page.goto(pathToFileURL(join(HERE, 'concept.html')).href);
await page.waitForFunction('typeof window.seek === "function"');

for (let i = 0; i < FRAMES; i++) {
  await page.evaluate((t) => window.seek(t), i / FPS);
  await page.screenshot({ path: join(OUT, `f_${String(i).padStart(3, '0')}.png`) });
}
await browser.close();
console.log(`captured ${FRAMES} frames -> ${OUT}`);
