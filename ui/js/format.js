import { html, raw } from "./dom.js";

export function shortDate(ts) {
  if (!ts) return "";
  const d = new Date(ts * 1000), now = new Date();
  if (d.toDateString() === now.toDateString()) return d.toLocaleTimeString([], { hour: "numeric", minute: "2-digit" });
  if ((now - d) / 86400000 < 6) return d.toLocaleDateString([], { weekday: "short", hour: "numeric", minute: "2-digit" });
  if (d.getFullYear() === now.getFullYear()) return d.toLocaleDateString([], { month: "short", day: "numeric" });
  return d.toLocaleDateString();
}

export const longDate = (ts) => (ts ? new Date(ts * 1000).toLocaleString() : "");

export function initials(s) {
  const p = String(s).replace(/[<>"@.]/g, " ").trim().split(/\s+/);
  return ((p[0]?.[0] ?? "?") + (p.length > 1 ? p[p.length - 1][0] : "")).toUpperCase();
}

export function avatar(name, size = 32) {
  let h = 0;
  for (const c of String(name)) h = (h * 31 + c.charCodeAt(0)) | 0;
  return html`<div class="avatar" style="width:${size}px;height:${size}px;background:hsl(${Math.abs(h) % 360} 55% 42%)">${initials(name)}</div>`;
}

export function bytes(n) {
  if (n < 1024) return `${n} B`;
  if (n < 1048576) return `${(n / 1024).toFixed(0)} KB`;
  return `${(n / 1048576).toFixed(1)} MB`;
}

const LABELS = { inbox: "Inbox", sent: "Sent Items", drafts: "Drafts", trash: "Deleted Items", junk: "Junk Email", archive: "Archive" };
export const folderLabel = (f) => (f.special && LABELS[f.special]) || (f.delimiter ? f.name.split(f.delimiter).pop() : f.name);

export const addrList = (list) => list.map((a) => (a.name ? `${a.name} <${a.email}>` : a.email)).join(", ");

// Inline 24px icon set, so nothing is fetched at runtime.
const PATHS = {
  menu: "M3 6h18M3 12h18M3 18h18",
  back: "M15 18l-6-6 6-6",
  compose: "M4 20h4L19 9l-4-4L4 16v4zM14 6l4 4",
  sync: "M20 11a8 8 0 0 0-14.9-3M4 4v4h4M4 13a8 8 0 0 0 14.9 3M20 20v-4h-4",
  search: "M11 18a7 7 0 1 0 0-14 7 7 0 0 0 0 14zM21 21l-5-5",
  inbox: "M3 13h5l2 3h4l2-3h5M5 5h14l2 8v6H3v-6z",
  sent: "M4 12l16-8-6 16-3-7z",
  drafts: "M4 20h4L19 9l-4-4L4 16z",
  trash: "M4 7h16M10 11v6M14 11v6M6 7l1 13h10l1-13M9 7V4h6v3",
  junk: "M12 3l9 16H3zM12 10v4M12 17v.5",
  archive: "M3 5h18v4H3zM5 9v10h14V9M10 13h4",
  folder: "M3 6h6l2 2h10v11H3z",
  flag: "M5 21V4h11l-2 4 2 4H5",
  read: "M3 8l9 6 9-6M3 8v10h18V8l-9-5z",
  reply: "M10 8L4 13l6 5M4 13h11a5 5 0 0 1 5 5",
  replyall: "M12 8l-6 5 6 5M8 8l-6 5 6 5M8 13h7a5 5 0 0 1 5 5",
  forward: "M14 8l6 5-6 5M20 13H9a5 5 0 0 0-5 5",
  rules: "M4 6h10M4 12h16M4 18h7M17 4v4M14 16v4",
  more: "M5 12h.5M12 12h.5M19 12h.5",
  close: "M6 6l12 12M18 6L6 18",
  clip: "M8 12l6-6a3 3 0 0 1 4 4l-8 8a5 5 0 0 1-7-7l7-7",
  shield: "M12 3l8 3v6c0 5-3.5 8-8 9-4.5-1-8-4-8-9V6z",
  settings: "M12 15a3 3 0 1 0 0-6 3 3 0 0 0 0 6zM19 12a7 7 0 0 0-.1-1.2l2-1.6-2-3.4-2.4 1a7 7 0 0 0-2-1.2L14 3h-4l-.5 2.6a7 7 0 0 0-2 1.2l-2.4-1-2 3.4 2 1.6A7 7 0 0 0 5 12c0 .4 0 .8.1 1.2l-2 1.6 2 3.4 2.4-1a7 7 0 0 0 2 1.2L10 21h4l.5-2.6a7 7 0 0 0 2-1.2l2.4 1 2-3.4-2-1.6c.1-.4.1-.8.1-1.2z",
  plus: "M12 5v14M5 12h14",
  up: "M12 19V5M6 11l6-6 6 6",
  down: "M12 5v14M6 13l6 6 6-6",
  person: "M12 12a4 4 0 1 0 0-8 4 4 0 0 0 0 8zM4 21a8 8 0 0 1 16 0",
  calendar: "M4 5h16v15H4zM4 9h16M8 3v4M16 3v4",
  chevron: "M9 6l6 6-6 6",
  moon: "M20 14.5A8 8 0 1 1 9.5 4a6.5 6.5 0 0 0 10.5 10.5z",
  sun: "M12 17a5 5 0 1 0 0-10 5 5 0 0 0 0 10zM12 1v2M12 21v2M4.2 4.2l1.4 1.4M18.4 18.4l1.4 1.4M1 12h2M21 12h2M4.2 19.8l1.4-1.4M18.4 5.6l1.4-1.4",
};

export const icon = (name, size = 18) =>
  raw(`<svg width="${size}" height="${size}" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="${PATHS[name] ?? PATHS.folder}"/></svg>`);
