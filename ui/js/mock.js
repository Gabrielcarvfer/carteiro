// Fake backend for developing the UI in a browser without a mail server.

const now = Math.floor(Date.now() / 1000);
const account2 = {
  id: 2, name: "Demo Work", email: "demo@work.example", username: "demo", incoming_protocol: "imap",
  incoming: { host: "imap.work.example", port: 993, security: "tls" },
  smtp: { host: "smtp.work.example", port: 587, security: "starttls" }, signature: "",
};
const account = {
  id: 1, name: "Demo User", email: "demo@example.com", username: "demo", incoming_protocol: "imap",
  incoming: { host: "imap.example.com", port: 993, security: "tls" },
  smtp: { host: "smtp.example.com", port: 465, security: "tls" }, signature: "",
};
const folders = [
  { name: "INBOX", raw_name: "INBOX", delimiter: "/", special: "inbox", unread: 4, total: 12 },
  { name: "Drafts", raw_name: "Drafts", delimiter: "/", special: "drafts", unread: 0, total: 0 },
  { name: "Sent", raw_name: "Sent", delimiter: "/", special: "sent", unread: 0, total: 1 },
  { name: "Archive", raw_name: "Archive", delimiter: "/", special: "archive", unread: 0, total: 0 },
  { name: "Trash", raw_name: "Trash", delimiter: "/", special: "trash", unread: 0, total: 0 },
  { name: "Notifications", raw_name: "Notifications", delimiter: "/", special: null, unread: 0, total: 0 },
];
const msg = (id, key, [name, email], subject, text, ago, seen = true, html = null) => ({
  id, account_id: 1, folder: "INBOX", uid: id, message_id: `${id}@x`, thread_key: key, subject,
  from: { name, email }, to: [{ name: "Demo User", email: "demo@example.com" }], cc: [],
  date: now - ago, seen, flagged: false, has_attachments: false, snippet: text.slice(0, 120),
  body_text: text, quoted_text: "", body_html: html, attachments: [], attachments_blocked: false, warnings: [], verdict: { status: "clean" },
});
const threads = {
  t1: [
    msg(1, "t1", ["Ana Souza", "ana@example.com"], "Q3 planning", "Hi all,\n\nCan we meet Thursday to go over the Q3 plan?\n\nAna", 7200),
    msg(2, "t1", ["Bruno Lima", "bruno@example.com"], "RE: Q3 planning", "Thursday works for me. 2pm?", 3600),
    { ...msg(3, "t1", ["Ana Souza", "ana@example.com"], "RE: Q3 planning", "2pm it is, I'll send an invite.", 600, false),
      has_attachments: true, attachments: [{ name: "agenda.pdf", mime: "application/pdf", size: 81234, danger: null }, { name: "invite.ics", mime: "text/calendar", size: 1200, danger: null }],
      invites: [{ id: 8, calendar_id: 3, remote_id: "q3-series", title: "Q3 planning", start: Math.floor(now / 86400) * 86400 + 2 * 86400 + 14 * 3600, end: Math.floor(now / 86400) * 86400 + 2 * 86400 + 15 * 3600, all_day: false, floating: false, recurring: true, tzid: "", location: "Room 4", description: "", rsvp: "" }] },
  ],
  t3: [{ ...msg(6, "t3", ["Billing", "billing@mybank-secure.example"], "Your account is suspended", "Please verify your account.", 900, false,
    "<p>Please <a href='https://mybank-secure.example.ru/login'>www.mybank.example</a> verify.</p>"),
    attachments: [{ name: "invoice.pdf.exe", mime: "application/octet-stream", size: 48213, danger: "disguised program: looks like .pdf but is .exe" }],
    warnings: ["A link shows \"www.mybank.example\" but actually opens mybank-secure.example.ru.", "Blocked attachment \"invoice.pdf.exe\": disguised program: looks like .pdf but is .exe."],
    has_attachments: true }],
  t2: [msg(4, "t2", ["Build server", "ci@corp.example"], "Build failed on main", "Build #4411 failed.", 1800, false,
    "<h3 style='color:#c50f1f'>Build #4411 failed</h3><p>See the <a href='https://example.com'>logs</a>.</p>")],
  t4: [{ ...msg(7, "t4", ["Marta Ribeiro", "marta@studio.example"], "Brand refresh: first drafts", "Hi! Attached are the first logo drafts. I like option B the most, curious what you think.\n\nMarta", 5400, false),
    has_attachments: true, attachments: [{ name: "logo-drafts.pdf", mime: "application/pdf", size: 2412000, danger: null }] }],
  t5: [msg(8, "t5", ["Lisbon Air", "bookings@lisbonair.example"], "Your trip to Porto is confirmed", "Booking ref K7Q2LP. Departure Friday 08:40 from gate B12.", 14400),
    msg(9, "t5", ["Lisbon Air", "bookings@lisbonair.example"], "Check-in is now open", "Check in now and pick your seat.", 10800)],
  t6: [msg(10, "t6", ["Mum", "mum@family.example"], "Sunday lunch?", "Are you coming on Sunday? Your aunt is bringing the cake.", 26000),
    msg(11, "t6", ["Demo User", "demo@example.com"], "RE: Sunday lunch?", "Yes! I'll bring wine.", 24000)],
  t7: [msg(12, "t7", ["Cloud Storage", "no-reply@storage.example"], "You're using 80% of your storage", "Free up space or upgrade your plan.", 90000)],
  t8: [{ ...msg(13, "t8", ["Pedro Alves", "pedro@supplier.example"], "Invoice 2026-114", "Hello, please find the November invoice attached. Payment within 30 days.", 100000),
    flagged: true, has_attachments: true, attachments: [{ name: "invoice-2026-114.pdf", mime: "application/pdf", size: 98000, danger: null }] }],
};
let rules = [];
const day0 = Math.floor(now / 86400) * 86400;
const calendars = [
  { id: 1, source_id: 1, source_kind: "local", source_name: "On this device", remote_id: "local", name: "My calendar", color: "#0f6cbd", read_only: false, visible: true },
  { id: 3, source_id: 1, source_kind: "local", source_name: "On this device", remote_id: "invitations", name: "Invitations", color: "#ca5010", read_only: false, visible: true },
  { id: 2, source_id: 2, source_kind: "microsoft", source_name: "demo@work.example", remote_id: "x", name: "Work", color: "#c239b3", read_only: false, visible: true },
];
let events = [
  // 10:00-11:00 New York wall-clock time (today), as the engine stores zoned events.
  { id: 90, calendar_id: 2, title: "Call with NY office", start: day0 + 10 * 3600, end: day0 + 11 * 3600, all_day: false, floating: true, tzid: "America/New_York", recurring: false, location: "", description: "" },
  { id: 1, calendar_id: 2, title: "Team standup", start: day0 + 9 * 3600, end: day0 + 9.25 * 3600, all_day: false, floating: false, recurring: true, location: "Video call", description: "" },
  { id: 2, calendar_id: 2, title: "Project review", start: day0 + 11 * 3600, end: day0 + 12.5 * 3600, all_day: false, floating: false, recurring: false, location: "Room 2", description: "Q3 numbers" },
  { id: 3, calendar_id: 1, title: "Dentist", start: day0 + 86400 + 16 * 3600, end: day0 + 86400 + 17 * 3600, all_day: false, floating: false, recurring: false, location: "", description: "" },
  { id: 4, calendar_id: 1, title: "Public holiday", start: day0 + 3 * 86400, end: day0 + 4 * 86400, all_day: true, floating: false, recurring: false, location: "", description: "" },
  { id: 6, calendar_id: 1, title: "Gym", start: day0 - 86400 + 18 * 3600, end: day0 - 86400 + 19 * 3600, all_day: false, floating: false, recurring: false, location: "", description: "" },
  { id: 7, calendar_id: 2, title: "1:1 with Ana", start: day0 + 2 * 86400 + 14 * 3600, end: day0 + 2 * 86400 + 14.5 * 3600, all_day: false, floating: false, recurring: false, location: "", description: "" },
  { id: 8, calendar_id: 2, title: "Q3 planning", start: day0 + 2 * 86400 + 10 * 3600, end: day0 + 2 * 86400 + 11.5 * 3600, all_day: false, floating: false, recurring: false, location: "Room 4", description: "" },
  { id: 9, calendar_id: 1, title: "Flight to Porto", start: day0 + 4 * 86400 + 8.6 * 3600, end: day0 + 4 * 86400 + 9.6 * 3600, all_day: false, floating: false, recurring: false, location: "Gate B12", description: "" },
  { id: 10, calendar_id: 2, title: "Sprint demo", start: day0 - 2 * 86400 + 15 * 3600, end: day0 - 2 * 86400 + 16 * 3600, all_day: false, floating: false, recurring: false, location: "", description: "" },
  ...[0, 7, 14, 21].map((d, i) => ({ id: 200 + i, calendar_id: 3, remote_id: "weekly-1", title: "Weekly design review", start: day0 + (d + 1) * 86400 + 10 * 3600, end: day0 + (d + 1) * 86400 + 11 * 3600, all_day: false, floating: false, recurring: true, location: "Room 3", description: "", rsvp: "" })),
  { id: 5, calendar_id: 2, title: "Design sync", start: day0 + 11.5 * 3600, end: day0 + 13 * 3600, all_day: false, floating: false, recurring: false, location: "", description: "" },
];

const summary = (key) => {
  const m = threads[key], last = m[m.length - 1];
  return {
    thread_key: key, subject: m[0].subject, participants: [...new Set(m.map((x) => x.from.name))],
    snippet: last.snippet, last_date: last.date, count: m.length, unread: m.filter((x) => !x.seen).length,
    flagged: m.some((x) => x.flagged), has_attachments: false, latest_id: last.id,
  };
};

let contacts = [
  { id: 1, source_id: 1, read_only: false, uid: "a", name: "Ana Souza", emails: ["ana@example.com"], phones: ["+55 11 91234-5678"], organization: "Acme", notes: "" },
  { id: 2, source_id: 2, read_only: true, uid: "b", name: "Bob Stone", emails: ["bob@work.example"], phones: [], organization: "", notes: "Met at the conference." },
  { id: 4, source_id: 1, read_only: false, uid: "d", name: "Marta Ribeiro", emails: ["marta@studio.example"], phones: ["+351 912 345 678"], organization: "Ribeiro Studio", notes: "" },
  { id: 5, source_id: 2, read_only: true, uid: "e", name: "Pedro Alves", emails: ["pedro@supplier.example"], phones: [], organization: "Supplier Ltd", notes: "" },
  { id: 6, source_id: 1, read_only: false, uid: "f", name: "Mum", emails: ["mum@family.example"], phones: ["+351 213 000 000"], organization: "", notes: "" },
  { id: 7, source_id: 2, read_only: true, uid: "g", name: "Bruno Lima", emails: ["bruno@example.com"], phones: [], organization: "Acme", notes: "" },
  { id: 8, source_id: 1, read_only: false, uid: "h", name: "Zoe Martins", emails: ["zoe@example.org"], phones: [], organization: "", notes: "" },
  { id: 3, source_id: 1, read_only: false, uid: "c", name: "Carol Lima", emails: ["carol@example.org", "c.lima@work.example"], phones: [], organization: "", notes: "" },
];

export async function mock(cmd, a) {
  await new Promise((r) => setTimeout(r, 50));
  switch (cmd) {
    case "list_accounts": return [account, account2];
    case "update_account": return a.account;
    case "list_unified": return (await mock("list_threads", { folder: "INBOX", query: a.query }))
      .map((t) => ({ ...t, account_id: 1, account_email: account.email, folder: "INBOX" }));
    case "test_scanner": throw `Couldn't reach clamd at ${a.addr}: connection refused`;
    case "download_offline_now": return { listed: 120, downloaded: 118, freed: 40 };
    case "save_attachment": return `/home/demo/Downloads/attachment-${a.index}`;
    case "export_mail": return ["/home/demo/Downloads/Carteiro mail 2026-11-08 0130", [{ path: "/home/demo/Downloads/Carteiro mail 2026-11-08 0130/demo@example.com", folders: 5, messages: 1234, skipped: 0 }]];
    case "export_settings": return "/home/demo/Downloads/Carteiro settings 2026-11-08 0130.json";
    case "import_settings": return { report: { accounts_added: ["demo@new.example"], accounts_existing: ["demo@example.com"], needs_password: ["demo@new.example"], needs_sign_in: [], rules_added: 3, calendars_added: 1, calendars_to_readd: ["Work CalDAV"] }, ui: { week_start: "1", theme: "dark" } };
    case "get_settings": return { offline_days: 30, close_to_tray: true, notify: true, sync_minutes: 15, work_days: [0, 1, 2, 3, 4], work_start: 480, work_end: 1080, off_hours_minutes: 60, schedule_version: 1, auto_update: true, google: { client_id: "", client_secret: null }, microsoft: { client_id: "", client_secret: null } };
    case "send_message": return "sent";
    case "guess_settings": return { incoming_protocol: "imap", incoming: account.incoming, smtp: account.smtp, note: null };
    case "add_account": return { ...a.account, id: 2 };
    case "sync_account": return { new_messages: 0, moved_by_rules: 0, folders: folders.length, errors: [] };
    case "list_folders": return folders;
    case "load_older": return 0;
    case "search_hits": {
      const words = a.query.toLowerCase().split(/\s+/).filter((w) => w && !w.includes(":"));
      const exts = [...a.query.matchAll(/has:(?:attachment:)?(\w+)/gi)].map((x) => x[1].toLowerCase()).filter((x) => x !== "attachment");
      const ms = (threads[a.threadKey] ?? []).filter((m) => words.every((w) => (m.subject + " " + m.body_text + " " + m.attachments.map((f) => f.name).join(" ")).toLowerCase().includes(w))
        && exts.every((e) => m.attachments.some((f) => f.name.toLowerCase().endsWith("." + e))));
      return { message_ids: ms.map((m) => m.message_id).reverse(), extensions: exts };
    }
    case "thread_ids": return (threads[a.threadKey] ?? []).map((m) => m.id);
    case "list_threads": {
      if (a.folder !== "INBOX" || a.offset > 0) return [];
      // A small subset of the engine's search syntax.
      const words = (a.query ?? "").toLowerCase().split(/\s+/).filter(Boolean);
      const q = words.filter((w) => !w.includes(":")).join(" ");
      return Object.keys(threads).map(summary)
        .filter((t) => !q || (t.subject + t.snippet + t.participants.join()).toLowerCase().includes(q))
        .filter((t) => !words.includes("is:unread") || t.unread)
        .filter((t) => !words.includes("is:flagged") || t.flagged)
        .filter((t) => !words.includes("has:attachment") || t.has_attachments)
        .sort((x, y) => y.last_date - x.last_date);
    }
    case "open_thread": return structuredClone(threads[a.threadKey] ?? []);
    case "delete_messages": case "move_messages":
      for (const [k, t] of Object.entries(threads)) { const left = t.filter((m) => !a.ids.includes(m.id)); if (left.length) threads[k] = left; else delete threads[k]; }
      return null;
    case "set_flag":
      for (const t of Object.values(threads)) for (const m of t) if (a.ids.includes(m.id)) m[a.flag] = a.on;
      return null;
    case "list_rules": return rules;
    case "save_rule": {
      const r = { ...a.rule, id: a.rule.id ?? Date.now() };
      rules = rules.some((x) => x.id === r.id) ? rules.map((x) => (x.id === r.id ? r : x)) : [...rules, r];
      return r;
    }
    case "delete_rule": rules = rules.filter((r) => r.id !== a.id); return null;
    case "reorder_rules": rules = a.ids.map((id) => rules.find((r) => r.id === id)); return null;
    case "run_rules": return 0;
    case "get_scanner": return { enabled: true, builtin: true, clamd_addr: "", block_when_unavailable: true };
    case "scanner_status": return { state: "ready", detail: "", version: "ClamAV 1.5.4/28176/Sat Nov  7 08:21:01 2026" };
    case "scanner_update_now": return { state: "ready", detail: "", version: "ClamAV 1.5.4/28177/Sun Nov  8 08:21:01 2026" };
    case "storage_usage": return [0, 0];
    case "update_status": case "check_update": return { enabled: true, current: "0.1.0", available: "0.2.0", ready: false, message: "Carteiro 0.2.0 is available." };
    case "install_update": return null;
    case "tracker_status": case "update_trackers": return { builtin: 119175, downloaded: 308642, updated: Math.floor(Date.now() / 1000) - 3600 };
    case "list_contacts": { const q = (a.query ?? "").toLowerCase(); return contacts.filter((c) => (c.name + " " + c.emails.join(" ")).toLowerCase().includes(q)).sort((p, r) => p.name.localeCompare(r.name)); }
    case "list_contact_sources": return [{ id: 1, kind: "local", account_id: null, url: "", username: "", name: "On this device" }, { id: 2, kind: "carddav", account_id: null, url: "https://dav.example.com/", username: "me", name: "Work contacts" }];
    case "save_contact": { const id = a.id ?? Date.now(); contacts = [...contacts.filter((c) => c.id !== id), { ...a.contact, id, source_id: 1, read_only: false }].sort((p, q) => p.name.localeCompare(q.name)); return id; }
    case "delete_contact": contacts = contacts.filter((c) => c.id !== a.id); return null;
    case "import_contacts": return { added: 2, merged: 1 };
    case "export_contacts": return "~/Downloads/Carteiro/Carteiro contacts.vcf";
    case "sync_contacts": return [];
    case "suggest_addresses": return contacts.flatMap((c) => c.emails.map((e) => [c.name, e])).filter(([n, e]) => (n + " " + e).toLowerCase().includes(a.prefix.toLowerCase())).slice(0, 8);
    case "set_rsvp": for (const t of Object.values(threads)) for (const m of t) for (const e of m.invites ?? []) if (e.id === a.eventId) e.rsvp = a.rsvp;
      for (const e of events) if (e.id === a.eventId) e.rsvp = a.rsvp;
      return null;
    case "preview_ics": return { method: "REQUEST", in_calendar: false, events: [{ title: "Q3 planning", start: Math.floor(now / 86400) * 86400 + 2 * 86400 + 14 * 3600, end: Math.floor(now / 86400) * 86400 + 2 * 86400 + 15 * 3600, all_day: false, floating: false, recurring: true, tzid: "", location: "Room 4" }] };
    case "add_ics": return 1;
    case "send_rsvp": return "sent";
    case "remove_series": { const ev = events.find((e) => e.id === a.eventId); const before = events.length;
      events = events.filter((e) => !(e.title === ev.title && e.calendar_id === ev.calendar_id && e.start >= (a.from ?? -Infinity) && e.start < (a.to ?? Infinity))); return before - events.length; }
    case "set_utc_offset": case "sync_phone_contacts": return null;
    case "list_calendars": return calendars;
    case "list_cal_sources": return [
      { id: 1, kind: "local", account_id: null, url: "", username: "", name: "On this device" },
      { id: 2, kind: "microsoft", account_id: null, url: "", username: "gabriel@work.example", name: "gabriel@work.example" }];
    case "calendar_events": return events.filter((e) => e.end > a.from && e.start < a.to && calendars.find((c) => c.id === e.calendar_id).visible);
    case "search_events": { const w = a.query.toLowerCase().split(/\s+/).filter(Boolean); return events.filter((e) => w.every((x) => (e.title + " " + (e.location ?? "") + " " + (e.description ?? "")).toLowerCase().includes(x))).sort((p, q) => p.start - q.start); }
    case "set_calendar_visible": calendars.find((c) => c.id === a.calendarId).visible = a.visible; return null;
    case "sync_calendars": return [];
    case "save_event": {
      const e = { ...a.event, calendar_id: a.calendarId, id: a.eventId ?? Date.now() };
      events = [...events.filter((x) => x.id !== e.id), e];
      return e.id;
    }
    case "holiday_countries": return [{ code: "BR", name: "Brazil" }, { code: "ES", name: "Spain" }, { code: "PT", name: "Portugal" }];
    case "holiday_regions": return a.country === "ES" ? ["ES-AN", "ES-CT", "ES-MD", "ES-VC"] : [];
    case "add_holiday_calendar": {
      const id = 10 + calendars.length;
      calendars.push({ id, source_id: id, source_kind: "holidays", source_name: a.name, remote_id: "holidays", name: a.name, color: "#107c10", read_only: true, visible: true });
      events.push({ id: id * 100, calendar_id: id, title: "Diada Nacional de Catalunya", start: day0 + 2 * 86400, end: day0 + 3 * 86400, all_day: true, floating: false, recurring: false, location: "", description: "" });
      return id;
    }
    case "add_local_calendar": calendars.push({ id: 50 + calendars.length, source_id: 1, source_kind: "local", source_name: "On this device", remote_id: "local-x", name: a.name, color: a.color, read_only: false, visible: true }); return 1;
    case "remove_local_calendar": calendars.splice(calendars.findIndex((c) => c.id === a.calendarId), 1); return null;
    case "remove_cal_source": calendars.splice(calendars.findIndex((c) => c.source_id === a.sourceId), 1); return null;
    case "delete_event": events = events.filter((e) => e.id !== a.eventId); return null;
    case "oauth_sign_in": throw "No OAuth client ID configured for this provider (Settings → Sign-in apps).";
    default: return null;
  }
}
