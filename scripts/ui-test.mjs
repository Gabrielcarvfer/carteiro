// Interface tests: drives ui/ in headless Chromium against the mock backend
// (ui/js/mock.js) and checks the features end to end, on a desktop-sized and
// a phone-sized window. No packages needed: Node 22+ and a Chromium build.
//
//   node scripts/ui-test.mjs /path/to/chrome
//
// Serves ui/ itself on a free local port; exits non-zero on any failure.
import { spawn } from "node:child_process";
import { createServer } from "node:http";
import { readFile } from "node:fs/promises";
import { extname, join, normalize } from "node:path";

const chrome = process.argv[2] ?? process.env.CHROME;
if (!chrome) { console.error("usage: node scripts/ui-test.mjs /path/to/chrome"); process.exit(2); }
const root = join(import.meta.dirname, "..", "ui");
const types = { ".html": "text/html", ".js": "text/javascript", ".css": "text/css", ".svg": "image/svg+xml", ".json": "application/json" };
const server = createServer(async (req, res) => {
  let name = decodeURIComponent(new URL(req.url, "http://x").pathname);
  if (name.endsWith("/")) name += "index.html";
  const path = normalize(join(root, name));
  if (!path.startsWith(root)) return res.writeHead(403).end();
  try {
    const body = await readFile(path);
    res.writeHead(200, { "content-type": types[extname(path)] ?? "application/octet-stream" }).end(body);
  } catch { res.writeHead(404).end(); }
}).listen(0, "127.0.0.1");
await new Promise((r) => server.on("listening", r));
const url = `http://127.0.0.1:${server.address().port}/`;

const port = 9400 + Math.floor(Math.random() * 400);
const proc = spawn(chrome, ["--headless", "--disable-gpu", `--remote-debugging-port=${port}`, "--user-data-dir=" + join(process.env.TMPDIR ?? "/tmp", `carteiro-ui-test-${port}`), "about:blank"], { stdio: "ignore" });
let targets;
for (let i = 0; i < 50 && !targets; i++) {
  await new Promise((r) => setTimeout(r, 200));
  targets = await fetch(`http://127.0.0.1:${port}/json`).then((r) => r.json()).catch(() => null);
}
const ws = new WebSocket(targets.find((t) => t.type === "page").webSocketDebuggerUrl);
await new Promise((r) => (ws.onopen = r));
let id = 0; const pending = {}; const errors = [];
ws.onmessage = (m) => { const d = JSON.parse(m.data); if (d.id) pending[d.id]?.(d); if (d.method === "Runtime.exceptionThrown") errors.push(d.params.exceptionDetails.exception?.description ?? d.params.exceptionDetails.text); };
const send = (method, params = {}) => new Promise((r) => { const i = ++id; pending[i] = r; ws.send(JSON.stringify({ id: i, method, params })); });
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const js = async (e) => {
  const r = await send("Runtime.evaluate", { expression: e, awaitPromise: true, returnByValue: true });
  if (r.result.exceptionDetails) throw new Error(`${e}\n  ${r.result.exceptionDetails.exception?.description}`);
  return r.result.result.value;
};
const click = (sel) => js(`(() => { const el = document.querySelector(${JSON.stringify(sel)}); if (!el) throw new Error("missing " + ${JSON.stringify(sel)}); el.click(); })()`);
const clickText = (sel, text) => js(`(() => { const el = [...document.querySelectorAll(${JSON.stringify(sel)})].find((x) => x.textContent.includes(${JSON.stringify(text)})); if (!el) throw new Error("no " + ${JSON.stringify(sel)} + " with " + ${JSON.stringify(text)}); el.click(); })()`);
await send("Runtime.enable");

let failed = 0;
async function test(name, fn) {
  try { await fn(); console.log(`ok   ${name}`); }
  catch (e) { failed++; console.log(`FAIL ${name}\n     ${String(e.message ?? e).split("\n").join("\n     ")}`); }
}
function expect(cond, what) { if (!cond) throw new Error(what); }
async function page(width, height, mobile) {
  await send("Emulation.setDeviceMetricsOverride", { width, height, deviceScaleFactor: 1, mobile });
  await send("Page.navigate", { url });
  await sleep(1500);
}

for (const [label, w, h, mobile] of [["desktop", 1280, 860, false], ["phone", 390, 844, true]]) {
  console.log(`# ${label}`);

  await page(w, h, mobile);
  await test("flag in an open conversation changes at once", async () => {
    await clickText(".item", "Q3 planning"); await sleep(800);
    await click("[data-click=conv][data-kind=flag]"); await sleep(100);
    expect(await js(`!!document.querySelector("[data-click=conv][data-kind=unflag]")`), "flag button didn't switch");
  });
  await test("reply buttons have space below them", async () => {
    expect(await js(`parseFloat(getComputedStyle(document.querySelector(".quick-reply")).marginBottom)`) >= 12, "no spacing");
  });
  await test("invitation card: answer, then send it", async () => {
    expect(await js(`!!document.querySelector(".invite")`), "no invitation card");
    expect(await js(`document.querySelector(".invite [data-click=send-rsvp]").disabled`), "send enabled before an answer");
    await click(".invite [data-v=maybe]"); await sleep(300);
    expect(await js(`document.querySelector(".invite .rsvp .on")?.textContent`) === "Maybe", "answer not shown");
    await click(".invite [data-click=send-rsvp]"); await sleep(400);
    expect(/sent to the organizer/.test(await js(`document.querySelector(".toast")?.textContent ?? ""`)), "no confirmation");
  });
  await test("tapping an .ics attachment asks before doing anything", async () => {
    await clickText(".att", "invite.ics"); await sleep(500);
    const opts = await js(`[...document.querySelectorAll(".ask [data-ask]")].map((b) => b.textContent)`);
    expect(opts.includes("Add to calendar") && opts.includes("Save the file to Downloads"), `options: ${opts}`);
    expect(await js(`!!document.querySelector(".ask .ics-list")`), "event not described");
    await click(".ask [data-ask=add]"); await sleep(900);
    expect(await js(`!document.querySelector("#calendar").hidden`), "calendar not shown after adding");
  });

  await page(w, h, mobile);
  const unreadOf = (text) => js(`[...document.querySelectorAll("#threads .item")].find((x) => x.textContent.includes(${JSON.stringify(text)}))?.classList.contains("unread")`);
  const rect = (text) => js(`(() => { const r = [...document.querySelectorAll("#threads .item")].find((x) => x.textContent.includes(${JSON.stringify(text)})).getBoundingClientRect(); return [r.left + r.width / 2, r.top + r.height / 2, r.width]; })()`);
  const touch = (type, x, y) => send("Input.dispatchTouchEvent", { type, touchPoints: type === "touchEnd" ? [] : [{ x, y }] });
  // A finger tap on the element matching `sel` (and containing `text`).
  const tap = async (sel, text = "", index = 0) => {
    const [x, y] = await js(`(() => { const el = [...document.querySelectorAll(${JSON.stringify(sel)})].filter((x) => x.textContent.includes(${JSON.stringify(text)}))[${index}]; if (!el) throw new Error("nothing to tap: ${sel.replace(/"/g, "'")} ${text}"); const r = el.getBoundingClientRect(); return [r.left + r.width / 2, r.top + r.height / 2]; })()`);
    await touch("touchStart", x, y); await sleep(40); await touch("touchEnd"); await sleep(250);
  };
  await test("read and unread show at once, in the list and the conversation", async () => {
    expect(await unreadOf("Build failed"), "test message should start unread");
    await js(`[...document.querySelectorAll("#threads .item")].find((x) => x.textContent.includes("Build failed")).querySelector("[data-kind=read]").click()`);
    expect(!(await unreadOf("Build failed")), "list didn't update at once");
    await clickText(".item", "Brand refresh"); await sleep(600);
    await click("[data-click=conv][data-kind=unread]"); await sleep(50);
    expect(await js(`!!document.querySelector("[data-click=conv][data-kind=read]")`), "conversation button didn't switch");
    expect(await unreadOf("Brand refresh"), "list not marked unread at once");
  });
  if (mobile) {
    await page(w, h, mobile);
    await test("press and hold picks; taps add more; delete them together", async () => {
      const before = await js(`document.querySelectorAll("#threads .item").length`);
      const [x, y] = await rect("Cloud Storage");
      await touch("touchStart", x, y); await sleep(700);
      const during = await js(`document.querySelectorAll("#threads .item.picked").length`);
      await touch("touchEnd"); await sleep(400);
      expect(await js(`document.querySelectorAll("#threads .item.picked").length`) === 1, `hold didn't pick (while held: ${during})`);
      expect(await js(`!document.querySelector("#pick-bar").hidden`), "no selection bar");
      await tap("#threads .item", "Pedro Alves");
      await tap("#threads .item", "Lisbon Air");
      expect(await js(`document.querySelector(".pick-count").textContent`) === "3 selected", "taps didn't add");
      await tap("#threads .item", "Lisbon Air");
      expect(await js(`document.querySelector(".pick-count").textContent`) === "2 selected", "tap didn't remove");
      await tap("#pick-bar [data-kind=delete]"); await sleep(400);
      expect(await js(`document.querySelectorAll("#threads .item").length`) === before - 2, "not deleted together");
      expect(await js(`document.querySelector("#pick-bar").hidden`), "bar stayed");
    });
    const swipe = async (x, y, dx, dy = 0) => {
      await touch("touchStart", x, y);
      for (let i = 1; i <= 8; i++) { await touch("touchMove", x + (dx * i) / 8, y + (dy * i) / 8); await sleep(16); }
      await touch("touchEnd"); await sleep(350);
    };
    await test("swipe left opens a tray of actions; a button acts", async () => {
      await page(w, h, mobile);
      const [x, y, width] = await rect("Mum");
      expect(!(await unreadOf("Mum")), "test message should start read");
      const scrollBefore = await js(`document.querySelector("#threads").scrollTop`);
      await swipe(x, y, -width * 0.5, 30);
      expect(await js(`document.querySelector("#threads").scrollTop`) === scrollBefore, "the list scrolled during a sideways swipe");
      const tray = await js(`[...document.querySelectorAll("#threads .item.tray-open .tray-btn")].map((b) => b.textContent.trim())`);
      expect(tray.join("|") === "Unread|Flag|Archive|Delete", `tray: ${tray}`);
      expect(/translateX\(-/.test(await js(`document.querySelector("#threads .item.tray-open .item-body").style.transform`)), "row didn't slide sideways");
      expect(await js(`!document.querySelector("#reader .pane h1")`), "a swipe opened the conversation");
      await tap("#threads .item.tray-open .tray-btn[data-kind=unread]");
      expect(await unreadOf("Mum"), "Unread button didn't act");
      expect(await js(`!document.querySelector("#threads .item.tray-open")`), "tray stayed open");
    });
    await test("a tap elsewhere closes the tray without opening anything", async () => {
      const [x, y, width] = await rect("Mum");
      await swipe(x, y, -width * 0.5);
      await tap("#threads .item", "Mum");
      expect(await js(`!document.querySelector("#threads .item.tray-open")`), "tray stayed open");
      expect(await js(`!document.querySelector("#reader .pane h1")`), "the tap opened the conversation");
    });
    await test("swipe right picks; check boxes show; taps add more", async () => {
      const [x, y, width] = await rect("Pedro Alves");
      await swipe(x, y, width * 0.4);
      expect(await js(`document.querySelectorAll("#threads .item.picked").length`) === 1, "swipe right didn't pick");
      expect(await js(`document.querySelectorAll("#threads .pick-box").length`) === await js(`document.querySelectorAll("#threads .item").length`), "no check box on every row");
      await tap("#threads .item", "Mum");
      expect(await js(`document.querySelector(".pick-count").textContent`) === "2 selected", "tap didn't add");
      await tap("[data-click=pick-clear]");
      expect(await js(`!document.querySelector("#threads .pick-box")`), "check boxes stayed");
    });
    await test("picking a conversation ticks each of its messages; untick one", async () => {
      await page(w, h, mobile);
      const [x, y] = await rect("Q3 planning");
      await touch("touchStart", x, y); await sleep(700); await touch("touchEnd"); await sleep(600);
      const boxes = () => js(`[...document.querySelectorAll("#threads .child .pick-box")].map((b) => b.getAttribute("aria-checked"))`);
      expect((await boxes()).join() === "true,true,true", `children: ${await boxes()}`);
      await tap("#threads .child", "", 1);
      expect((await boxes()).join() === "true,false,true", `after untick: ${await boxes()}`);
      expect(await js(`document.querySelector("#threads .item.sel, #threads .item [aria-checked=mixed]") !== null`), "conversation not shown as partly picked");
      expect(await js(`document.querySelector(".pick-count").textContent`) === "2 messages", await js(`document.querySelector(".pick-count").textContent`));
      await tap("#threads .child", "", 1);
      expect(await js(`document.querySelector(".pick-count").textContent`) === "1 selected", "re-ticking didn't make it whole again");
    });
  } else {
    await test("Ctrl-click picks several; flag them together", async () => {
      await page(w, h, mobile);
      for (const t of ["Cloud Storage", "Lisbon Air"]) await js(`[...document.querySelectorAll("#threads .item")].find((x) => x.textContent.includes(${JSON.stringify(t)})).dispatchEvent(new MouseEvent("click", { bubbles: true, ctrlKey: true }))`);
      await sleep(100);
      expect(await js(`document.querySelector(".pick-count").textContent`) === "2 selected", "ctrl-click didn't pick");
      await click("#pick-bar [data-kind=flag]"); await sleep(300);
      expect(await js(`[...document.querySelectorAll("#threads .item")].filter((x) => /Cloud Storage|Lisbon Air/.test(x.textContent) && x.querySelector(".flagged")).length`) === 2, "not flagged together");
    });
  }

  await page(w, h, mobile);
  await test("opening a search result shows the message that matched", async () => {
    await js(`(() => { const i = [...document.querySelectorAll("[data-input=search]")].find((x) => x.offsetParent); i.value = "has:pdf 2pm"; i.dispatchEvent(new Event("input", { bubbles: true })); })()`);
    await sleep(500);
    await clickText(".item", "Q3 planning"); await sleep(800);
    const hits = await js(`[...document.querySelectorAll("#reader .card.hit")].map((c) => c.id)`);
    expect(hits.length === 1, `hits: ${hits}`);
    expect(await js(`!!document.querySelector("#reader .card.hit .hit-tag")`), "no marker");
    expect(await js(`[...document.querySelectorAll("#reader .card.hit .att.hit")].map((x) => x.textContent).join()`).then((t) => t.includes("agenda.pdf")), "matching attachment not highlighted");
    expect(await js(`document.querySelectorAll("#reader .card.collapsed").length`) === 2, "other messages should stay closed");
  });

  await page(w, h, mobile);
  await test("saving an attachment says so at once, once; a second save asks", async () => {
    await clickText(".item", "Q3 planning"); await sleep(800);
    const chip = `[...document.querySelectorAll("#reader .att")].find((x) => x.textContent.includes("agenda.pdf"))`;
    await js(`${chip}.click()`); await sleep(50);
    expect(await js(`${chip}.classList.contains("busy") && ${chip}.textContent.includes("checking and saving")`), "no feedback while saving");
    await js(`${chip}.click()`); await sleep(320);
    expect(/agenda\.pdf/.test(await js(`document.querySelector("#activity:not([hidden])")?.textContent ?? ""`)), "background task not shown while saving");
    await sleep(350);
    expect(await js(`document.querySelector("#activity").hidden`), "task stayed after it ended");
    expect(await js(`window.__saves`) === 1, `saved ${await js("window.__saves")} times`);
    expect(await js(`${chip}.textContent.includes("saved")`), "not shown as saved");
    await js(`${chip}.click()`); await sleep(200);
    expect(/Already saved to/.test(await js(`document.querySelector(".ask p")?.textContent ?? ""`)), "no question before saving again");
    await click(".ask [data-ask='']"); await sleep(100);
    expect(await js(`window.__saves`) === 1, "saved again after Cancel");
  });

  await page(w, h, mobile);
  await test("scrolling to the end asks for older mail", async () => {
    await js(`(() => { const el = document.querySelector("#threads"); el.style.height = "200px"; el.scrollTop = el.scrollHeight; el.dispatchEvent(new Event("scroll")); })()`);
    await sleep(500);
    expect(await js(`!document.querySelector("#threads .older-row")`), "loading row stayed");
    expect(await js(`document.querySelectorAll("#threads .item").length`) > 0, "list emptied");
  });

  await page(w, h, mobile);
  await test("small month marks days with events", async () => {
    await click("[data-mode=calendar]"); await sleep(1000);
    expect(await js(`document.querySelectorAll(".mini-grid .busy").length`) > 0, "no marked days");
  });
  await test("remove one date of a series, then the whole series", async () => {
    await clickText("button", "Month"); await sleep(700);
    const count = () => js(`[...document.querySelectorAll("[data-click=cal-open]")].filter((b) => b.textContent.includes("Weekly design review")).length`);
    const before = await count();
    expect(before >= 2, `series not shown (${before})`);
    await clickText("[data-click=cal-open]", "Weekly design review"); await sleep(400);
    await click("[data-click=ev-delete]"); await sleep(300);
    await click(".ask [data-ask=one]"); await sleep(700);
    expect(await count() === before - 1, "one date not removed");
    await clickText("[data-click=cal-open]", "Weekly design review"); await sleep(400);
    await click("[data-click=ev-delete]"); await sleep(300);
    await click(".ask [data-ask=all]"); await sleep(700);
    expect(await count() === 0, "series not removed");
  });

  await page(w, h, mobile);
  await test("contacts: letter jump and deleting any contact", async () => {
    await click("[data-mode=contacts]"); await sleep(800);
    await click(".ct-letter"); await sleep(300);
    expect(await js(`document.querySelectorAll(".ct-jumper .on").length`) > 0, "no letters to jump to");
    await click(".ct-jumper .on[data-letter=B]"); await sleep(300);
    expect(await js(`!document.querySelector(".ct-jumper")`), "jumper didn't close");
    const n = await js(`document.querySelectorAll(".ct-item").length`);
    await clickText(".ct-item", "Bob Stone"); await sleep(300); // a synced, read-only contact
    await click("[data-click=ct-delete]"); await sleep(300);
    expect(/won't come back/.test(await js(`document.querySelector(".ask p").textContent`)), "synced deletion not explained");
    await click(".ask [data-ask=yes]"); await sleep(500);
    expect(await js(`document.querySelectorAll(".ct-item").length`) === n - 1, "not deleted");
  });

  await page(w, h, mobile);
  await test("settings: four sections, swiped and tapped", async () => {
    await js(`document.querySelector("[data-click=settings]").click()`); await sleep(1000);
    const titles = await js(`[...document.querySelectorAll("#modal .pivot-titles button")].map((b) => b.textContent)`);
    expect(titles.join("|") === "general|security & privacy|your data|about", `titles: ${titles}`);
    await js(`(() => { const p = document.querySelector("#modal .pivots"); p.scrollTo({ left: p.clientWidth, behavior: "instant" }); })()`); await sleep(400);
    expect(await js(`document.querySelector("#modal .pivot-titles .on").textContent`) === "security & privacy", "swipe didn't change section");
    await click("[data-click=pivot][data-k=about]"); await sleep(800);
    expect(await js(`document.querySelector("#modal .pivot-titles .on").textContent`) === "about", "tap didn't change section");
    expect(await js(`!document.querySelector("#modal [name=g-id]")`), "OAuth fields still there");
    expect(await js(`document.querySelectorAll("#modal [name=work-day]:checked").length`) === 5, "working days not Mon-Fri");
  });
  await test("sign-in buttons hidden when the build has no app IDs", async () => {
    expect(await js(`document.body.classList.contains("no-google") && document.body.classList.contains("no-microsoft")`), "not hidden");
  });
}

if (errors.length) { failed++; console.log(`FAIL page errors:\n     ${errors.join("\n     ")}`); }
proc.kill();
server.close();
console.log(failed ? `${failed} failed` : "all passed");
process.exit(failed ? 1 : 0);
