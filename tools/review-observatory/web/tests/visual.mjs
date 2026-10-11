import { fixtureRoute } from "./fixture.mjs";
import { chromium } from "@playwright/test";
import { mkdir, appendFile, readdir, readFile } from "node:fs/promises";
import path from "node:path";
import { createHash } from "node:crypto";
const hash = createHash("sha256");
async function fingerprint(directory) {
  for (const entry of (await readdir(directory, { withFileTypes: true })).sort(
    (a, b) => a.name.localeCompare(b.name),
  )) {
    const file = path.join(directory, entry.name);
    if (entry.isDirectory()) await fingerprint(file);
    else {
      hash.update(file);
      hash.update(await readFile(file));
    }
  }
}
await fingerprint("src");
await fingerprint("public");
for (const file of ["index.html", "package-lock.json", "vite.config.ts"])
  hash.update(await readFile(file));
const source_sha256 = hash.digest("hex");
const directory = path.resolve(
  process.cwd(),
  "../../../.anki-repo/observatory/visual-evidence",
);
await mkdir(directory, { recursive: true });
const browser = await chromium.launch({
  executablePath: process.env.CHROMIUM_PATH,
  headless: true,
});
const revision = process.env.VISUAL_REVISION || "shell-01";
for (const width of (process.env.VISUAL_WIDTHS || "1440,390")
  .split(",")
  .map(Number))
  for (const theme of (process.env.VISUAL_THEMES || "light,dark").split(",")) {
    const page = await browser.newPage({
      viewport: { width, height: 1000 },
      colorScheme: theme,
    });
    if (process.env.VISUAL_FIXTURE === "1")
      await page.route("**/api/v1/**", fixtureRoute);
    await page.goto(process.env.VISUAL_URL || "http://127.0.0.1:5173/", {
      waitUntil: "domcontentloaded",
    });
    await page.locator("h1").waitFor();
    await page.waitForTimeout(500);
    await page.evaluate((theme) => {
      localStorage.setItem("observatory-theme", theme);
      document.documentElement.classList.toggle("dark", theme === "dark");
    }, theme);
    await page.screenshot({
      path: path.join(directory, `${revision}-${width}-${theme}.png`),
      fullPage: true,
    });
    await appendFile(
      path.join(directory, "iterations.jsonl"),
      JSON.stringify({
        revision,
        source_sha256,
        width,
        theme,
        route: page.url(),
        png: `${revision}-${width}-${theme}.png`,
        inspection: "pending native view_image",
      }) + "\n",
    );
    await page.close();
  }
await browser.close();
