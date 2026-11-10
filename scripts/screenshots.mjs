// The screenshots in README.md and docs/: ui/ in headless Chromium with the
// mock data (ui/js/mock.js), desktop and phone sizes, written to
// docs/screenshots. Phones are taken at 2x and scaled down with ImageMagick.
//
//   node scripts/screenshots.mjs /path/to/chrome [step,step...]
import { spawn, execFileSync } from "node:child_process";
import { createServer } from "node:http";
import { readFile, writeFile } from "node:fs/promises";
import { extname, join, normalize } from "node:path";

const [chrome, only] = process.argv.slice(2);
if (!chrome) { console.error("usage: node scripts/screenshots.mjs /path/to/chrome [steps]"); process.exit(2); }
const root = join(import.meta.dirname, "..", "ui");
const outDir = join(import.meta.dirname, "..", "docs", "screenshots");
const types = { ".html": "text/html", ".js": "text/javascript", ".css": "text/css", ".svg": "image/svg+xml", ".json": "application/json" };
const server = createServer(async (req, res) => {
  let name = decodeURIComponent(new URL(req.url, "http://x").pathname);
  if (name.endsWith("/")) name += "index.html";
  const path = normalize(join(root, name));
  if (!path.startsWith(root)) return res.writeHead(403).end();
  let body;
  try { body = await readFile(path); } catch { return res.writeHead(404).end(); }
  res.writeHead(200, { "content-type": types[extname(path)] ?? "application/octet-stream" }).end(body);
}).listen(0, "127.0.0.1");
await new Promise((r) => server.on("listening", r));
const url = `http://127.0.0.1:${server.address().port}/`;

const port = 9400 + Math.floor(Math.random() * 400);
const proc = spawn(chrome, ["--headless", "--disable-gpu", "--hide-scrollbars", `--remote-debugging-port=${port}`,
  "--user-data-dir=" + join(process.env.TMPDIR ?? "/tmp", `carteiro-shots-${port}`), "about:blank"], { stdio: "ignore" });
let targets;
for (let i = 0; i < 50 && !targets; i++) { await new Promise((r) => setTimeout(r, 200)); targets = await fetch(`http://127.0.0.1:${port}/json`).then((r) => r.json()).catch(() => null); }
const ws = new WebSocket(targets.find((t) => t.type === "page").webSocketDebuggerUrl);
await new Promise((r) => (ws.onopen = r));
let id = 0; const pending = {}; const errors = [];
ws.onmessage = (m) => { const d = JSON.parse(m.data); if (d.id) pending[d.id]?.(d); if (d.method === "Runtime.exceptionThrown") errors.push(d.params.exceptionDetails.exception?.description); };
const send = (method, params = {}) => new Promise((r) => { const i = ++id; pending[i] = r; ws.send(JSON.stringify({ id: i, method, params })); });
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const js = async (e) => (await send("Runtime.evaluate", { expression: e, awaitPromise: true, returnByValue: true })).result.result?.value;
await send("Runtime.enable"); await send("Page.enable");

async function page(w, h, mobile, theme) {
  await send("Emulation.setDeviceMetricsOverride", { width: w, height: h, deviceScaleFactor: mobile ? 2 : 1, mobile });
  await send("Emulation.setEmulatedMedia", { features: [{ name: "prefers-color-scheme", value: theme }] });
  await send("Page.navigate", { url }); await sleep(1800);
}
async function shot(name, phone) {
  const r = await send("Page.captureScreenshot", { format: "png" });
  const file = join(outDir, `${name}.png`);
  await writeFile(file, Buffer.from(r.result.data, "base64"));
  execFileSync("magick", [file, "-strip", ...(phone ? ["-resize", "50%"] : []), file]);
  console.log(`docs/screenshots/${name}.png`);
}
const click = (sel) => js(`document.querySelector(${JSON.stringify(sel)})?.click()`);
const clickText = (sel, text) => js(`[...document.querySelectorAll(${JSON.stringify(sel)})].find((x) => x.textContent.includes(${JSON.stringify(text)}))?.click()`);
const steps = {
  async mailDesktop() { await page(1440, 900, false, "light"); await js(`document.querySelectorAll('.item')[0]?.click()`); await sleep(1200); await shot("desktop-mail"); },
  async mailDark() { await page(1440, 900, false, "dark"); await js(`document.querySelectorAll('.item')[1]?.click()`); await sleep(1200); await shot("desktop-mail-dark"); },
  async calDesktop() { await page(1440, 900, false, "light"); await click("[data-mode=calendar]"); await sleep(1200); await shot("desktop-calendar"); },
  // A contact with their conversations.
  async contactsDesktop() { await page(1440, 900, false, "light"); await click("[data-mode=contacts]"); await sleep(800); await clickText(".ct-item [data-click=ct-open], .ct-item", "Ana Souza"); await sleep(900); await shot("desktop-contacts"); },
  async phoneMail() { await page(390, 844, true, "light"); await shot("phone-mail", true); },
  async phoneRead() { await page(390, 844, true, "light"); await js(`document.querySelectorAll('.item')[0]?.click()`); await sleep(1200); await shot("phone-reading", true); },
  async phoneCal() { await page(390, 844, true, "light"); await click("[data-mode=calendar]"); await sleep(1200); await shot("phone-calendar", true); },
  // Not in the README: for checking the settings layout.
  async settingsAbout() { await page(1440, 900, false, "light"); await click("[data-click=settings]"); await sleep(1000); await click("[data-click=pivot][data-k=about]"); await sleep(900); await shot("settings-about"); },
  async settingsAboutPhone() { await page(390, 844, true, "light"); await js(`document.querySelector("[data-click=settings]").click()`); await sleep(1000); await click("[data-click=pivot][data-k=about]"); await sleep(900); await shot("settings-about-phone", true); },
  async phoneJump() { await page(390, 844, true, "dark"); await click("[data-mode=contacts]"); await sleep(800); await click(".ct-letter"); await sleep(400); await shot("phone-contacts", true); },
};
for (const [k, f] of Object.entries(steps)) if (!only || only.split(",").includes(k)) await f();
if (errors.length) console.log("page errors:", errors);
proc.kill(); server.close(); process.exit(errors.length ? 1 : 0);
