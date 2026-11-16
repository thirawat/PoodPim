// Renders assets/demo/scene.html frame by frame with headless Chrome and writes PNG frames
// to the folder given; scripts/make-demo.sh runs this and turns the frames into assets/demo.gif.
//
//   npm i puppeteer-core@24.10.0     (in any folder; run this from there)
//   node /path/to/scripts/make-demo.mjs <frames-dir> [fps]
import { createRequire } from "node:module";
import { mkdirSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

// puppeteer-core is installed where this is run from, not next to the script.
const require = createRequire(resolve(process.cwd(), "noop.js"));
const puppeteer = require("puppeteer-core");

const [outDir, fpsArg] = process.argv.slice(2);
if (!outDir) throw new Error("usage: node make-demo.mjs <frames-dir> [fps]");
const fps = Number(fpsArg || 15);
const scene = resolve(dirname(fileURLToPath(import.meta.url)), "../assets/demo/scene.html");
const chrome = process.env.CHROME || "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";

mkdirSync(outDir, { recursive: true });
const browser = await puppeteer.launch({ executablePath: chrome, headless: true });
const page = await browser.newPage();
await page.setViewport({ width: 960, height: 540, deviceScaleFactor: 1 });
await page.goto(pathToFileURL(scene).href, { waitUntil: "networkidle0" });
await page.evaluate(() => document.fonts.ready);
const duration = await page.evaluate(() => window.DURATION);
const frames = Math.round(duration * fps);
for (let i = 0; i < frames; i++) {
  await page.evaluate(t => window.render(t), i / fps);
  await page.screenshot({ path: `${outDir}/f${String(i).padStart(4, "0")}.png` });
}
await browser.close();
console.log(`${frames} frames at ${fps} fps → ${outDir}`);
