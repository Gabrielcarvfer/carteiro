//! Calendar invitations that arrive by mail (an `.ics` attachment or a
//! text/calendar part), put straight into the "Invitations" calendar. Only
//! for messages ClamAV found clean. Nothing is ever sent back: whether you
//! go is your own note, and the organizer and other guests aren't told.

use crate::ical::{calendar_method, parse_events, Event};
use crate::store::Store;
use crate::Result;
use mail_parser::{MessageParser, MimeHeaders};
use std::collections::BTreeMap;

/// Invitations bigger than this aren't invitations.
const MAX_ICS: usize = 1 << 20;
/// How far around today a series is laid out.
const PAST: i64 = 90 * 86400;
const FUTURE: i64 = 2 * 365 * 86400;
const MAX_INSTANCES: usize = 800;

/// The calendar objects a message carries.
pub fn calendar_parts(raw: &[u8]) -> Vec<String> {
    let Some(msg) = MessageParser::default().parse(raw) else { return Vec::new() };
    msg.parts
        .iter()
        .filter(|p| {
            let ct = p.content_type().map(|c| format!("{}/{}", c.ctype(), c.subtype().unwrap_or("")).to_ascii_lowercase()).unwrap_or_default();
            let name = p.attachment_name().unwrap_or("").to_ascii_lowercase();
            ct == "text/calendar" || ct == "application/ics" || name.ends_with(".ics")
        })
        .map(|p| String::from_utf8_lossy(p.contents()).into_owned())
        .filter(|t| t.len() <= MAX_ICS && t.contains("BEGIN:VCALENDAR") && t.contains("BEGIN:VEVENT"))
        .collect()
}

/// The shape of a message's calendar-looking parts, without their contents:
/// (content type, file name extension, size, first bytes as hex, has
/// VCALENDAR, has VEVENT, method), case-insensitive. For diagnosis.
pub fn calendar_part_shapes(raw: &[u8]) -> Vec<(String, String, usize, String, bool, bool, String)> {
    let Some(msg) = MessageParser::default().parse(raw) else { return Vec::new() };
    msg.parts
        .iter()
        .filter(|p| {
            let ct = p.content_type().map(|c| format!("{}/{}", c.ctype(), c.subtype().unwrap_or("")).to_ascii_lowercase()).unwrap_or_default();
            let name = p.attachment_name().unwrap_or("").to_ascii_lowercase();
            ct.contains("calendar") || ct.contains("ics") || name.ends_with(".ics") || name.ends_with(".vcs")
        })
        .map(|p| {
            let ct = p.content_type().map(|c| format!("{}/{}", c.ctype(), c.subtype().unwrap_or(""))).unwrap_or_default();
            let ext = p.attachment_name().and_then(|n| n.rsplit_once('.').map(|(_, e)| e.to_string())).unwrap_or_default();
            let b = p.contents();
            let up = String::from_utf8_lossy(b).to_ascii_uppercase();
            let method = up.lines().find_map(|l| l.strip_prefix("METHOD:")).unwrap_or("").trim().to_string();
            (ct, ext, b.len(), b.iter().take(6).map(|x| format!("{x:02x}")).collect(), up.contains("BEGIN:VCALENDAR"), up.contains("BEGIN:VEVENT"), method)
        })
        .collect()
}

/// Reads the invitations in a clean message from `sender` into the
/// calendar, once per message. Returns the series it touched.
pub fn import(store: &mut Store, msg: i64, sender: &str, raw: &[u8], now: i64) -> Result<Vec<String>> {
    if store.invites_checked(msg)? {
        return Ok(Vec::new());
    }
    let account = store.message(msg)?.map(|m| m.account_id);
    let mut touched = Vec::new();
    let mut outcomes: Vec<&'static str> = Vec::new();
    let parts = calendar_parts(raw);
    for ics in &parts {
        let (uids, outcome) = apply(store, sender, ics, now, false, account)?;
        outcomes.push(outcome);
        for uid in uids {
            // The same invitation often comes twice (inline and as invite.ics).
            if !touched.contains(&uid) {
                touched.push(uid);
            }
        }
    }
    let outcome = if parts.is_empty() { "no calendar" } else { outcomes.iter().copied().find(|o| *o != "answer").unwrap_or("answer") };
    store.mark_invites(msg, &touched, outcome)?;
    Ok(touched)
}

/// A calendar file you chose to add from a message: like a mailed
/// invitation, except that dates you removed from the series come back.
/// Returns how many series it added or changed.
pub fn import_ics(store: &mut Store, sender: &str, ics: &str, now: i64, account: Option<i64>) -> Result<usize> {
    let (changed, _) = apply(store, sender, ics, now, true, account)?;
    if changed.is_empty() {
        return Err(crate::Error::Other("Nothing was added: this event belongs to an invitation from someone else".into()));
    }
    Ok(changed.len())
}

/// Applies one calendar object; returns the series it changed and what
/// happened ("added", "cancelled", "answer", "older update", "nothing").
///
/// Every clean calendar file counts: invitations and updates add or replace
/// their meeting; cancellations mark it cancelled (kept, with its history; a
/// cancellation for a meeting not here yet adds it as cancelled); replies
/// (people answering a meeting) add it when it isn't here, else they're
/// just answers. Only the first sender or the organizer may change a
/// meeting: anyone else's file is added as a separate copy instead.
fn apply(store: &mut Store, sender: &str, ics: &str, now: i64, by_hand: bool, account: Option<i64>) -> Result<(Vec<String>, &'static str)> {
    let method = calendar_method(ics);
    let answer = matches!(method.as_str(), "REPLY" | "COUNTER" | "DECLINECOUNTER" | "REFRESH");
    let mut series: BTreeMap<String, Vec<Event>> = BTreeMap::new();
    for e in parse_events(ics) {
        if !e.uid.is_empty() && e.uid.len() <= 512 && e.start != 0 {
            series.entry(e.uid.clone()).or_default().push(e);
        }
    }
    let mut touched = Vec::new();
    let mut outcome = "nothing";
    for (uid, mut events) in series {
        let organizer = events.iter().map(|e| e.organizer.clone()).find(|o| !o.is_empty()).unwrap_or_default();
        if answer && store.invite_exists(&uid)? {
            outcome = "answer";
            continue;
        }
        // Someone who may not change the meeting: theirs is kept apart.
        let key = if store.may_change_invite(&uid, sender, &organizer)? { uid.clone() } else { format!("{uid}#{}", sender.to_lowercase()) };
        // Only changed dates (no series in the file) of a meeting we have:
        // those dates move.
        let only_dates = !events.is_empty() && events.iter().all(|e| e.recurrence_id.is_some());
        let changed = if method != "CANCEL" && !answer && only_dates && store.invite_exists(&key)? {
            let mut any = false;
            for e in &events {
                any |= store.update_invite_instance(&key, sender, &organizer, e.recurrence_id.unwrap_or(e.start), e)?;
            }
            outcome = if any { "date changed" } else { "nothing" };
            any
        } else if method == "CANCEL" && store.invite_exists(&key)? {
            let one = events.iter().find(|e| e.recurrence_id.is_some()).filter(|_| !events.iter().any(|e| e.recurrence_id.is_none()));
            let done = store.cancel_invite(&key, sender, &organizer, one.and_then(|e| e.recurrence_id))?;
            if done {
                outcome = "cancelled";
            }
            done
        } else {
            if method == "CANCEL" {
                for e in &mut events {
                    e.cancelled = true;
                }
            }
            let sequence = events.iter().map(|e| e.sequence).max().unwrap_or(0);
            // Laid out from the series' start (past meetings included, so old
            // invitations show up too) to two years after the later of today
            // and that start, at most MAX_INSTANCES dates.
            let first = events.iter().map(|e| e.start).min().unwrap_or(now);
            let instances = crate::recur::expand_all_with(events, first.min(now - PAST), now.max(first) + FUTURE, MAX_INSTANCES, true);
            let owner = if answer && !organizer.is_empty() { organizer.as_str() } else { sender };
            let done = store.save_invite(&key, owner, sequence, &instances, by_hand, &organizer, account, now)?;
            outcome = if done { if method == "CANCEL" { "cancelled" } else { "added" } } else { "older update" };
            done
        };
        if changed {
            touched.push(key);
        }
    }
    Ok((touched, outcome))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::Addr;

    fn mail(ics: &str, attach: bool) -> Vec<u8> {
        let part = if attach {
            format!("Content-Type: application/octet-stream; name=\"invite.ics\"\r\nContent-Disposition: attachment; filename=\"invite.ics\"\r\n\r\n{ics}")
        } else {
            format!("Content-Type: text/calendar; method=REQUEST; charset=utf-8\r\n\r\n{ics}")
        };
        format!("From: Ana <ana@example.org>\r\nSubject: Invitation\r\nContent-Type: multipart/mixed; boundary=b\r\n\r\n--b\r\nContent-Type: text/plain\r\n\r\nYou're invited\r\n--b\r\n{part}\r\n--b--\r\n").into_bytes()
    }

    fn ics(method: &str, body: &str) -> String {
        format!("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nMETHOD:{method}\r\n{body}END:VCALENDAR\r\n")
    }

    const SERIES: &str = "BEGIN:VEVENT\r\nUID:weekly-1\r\nSEQUENCE:0\r\nSUMMARY:Team sync\r\nDTSTART;TZID=Europe/Lisbon:20261109T100000\r\nDTEND;TZID=Europe/Lisbon:20261109T103000\r\nRRULE:FREQ=WEEKLY;COUNT=4\r\nORGANIZER:mailto:ana@example.org\r\nATTENDEE;RSVP=TRUE:mailto:me@x.org\r\nEND:VEVENT\r\n";

    /// (date, answer) of each meeting in the Invitations calendar; a
    /// cancelled one's answer reads "cancelled".
    fn titles(s: &Store) -> Vec<(String, String)> {
        let cal = s.invitations_calendar().unwrap();
        s.events_between(0, i64::MAX / 2)
            .unwrap()
            .into_iter()
            .filter(|e| e.calendar_id == cal)
            .map(|e| (crate::ical::fmt_date(e.event.start), if e.event.cancelled { "cancelled".to_string() } else { e.rsvp }))
            .collect()
    }

    fn store_with_messages(n: u32) -> (Store, i64) {
        let s = Store::open_in_memory().unwrap();
        let ep = crate::Endpoint { host: "h".into(), port: 993, security: crate::Security::Tls, allow_insecure: false };
        let mut a = crate::Account {
            id: 0, name: "Me".into(), email: "me@x.org".into(), username: "me".into(), incoming_protocol: crate::Protocol::Imap,
            incoming: ep.clone(), smtp: ep, smtp_username: None, signature: String::new(), auth: crate::AuthMethod::Password,
        };
        s.add_account(&mut a).unwrap();
        for uid in 1..=n {
            let meta = crate::message::MessageMeta { message_id: format!("m{uid}"), from: Addr { name: String::new(), email: "x@y.org".into() }, ..Default::default() };
            s.insert_message(&crate::store::NewMessage { account_id: a.id, folder: "INBOX".into(), uid: Some(uid), pop_uidl: None, meta, seen: false, flagged: false, size: 1, raw: None }).unwrap();
        }
        (s, a.id)
    }

    #[test]
    fn invitations_series_answers_updates_and_cancels() {
        let (mut s, account) = store_with_messages(12);
        let now = crate::ical::parse_datetime("20261101T000000Z").unwrap().0;
        assert_eq!(import(&mut s, 1, "ana@example.org", &mail(&ics("REQUEST", SERIES), true), now).unwrap(), ["weekly-1"]);
        assert_eq!(titles(&s).len(), 4, "the whole series");
        assert_eq!(s.invite_reply_info("weekly-1").unwrap(), Some(("ana@example.org".into(), 0, Some(account))), "who to answer, from which account");
        assert!(import(&mut s, 1, "ana@example.org", &mail(&ics("REQUEST", SERIES), true), now).unwrap().is_empty(), "once per message");
        // Your answer is per date, and survives an update.
        let first = s.invites_of(1, now).unwrap().remove(0);
        assert_eq!(first.event.title, "Team sync");
        s.set_rsvp(first.id, "maybe").unwrap();
        let update = SERIES.replace("SEQUENCE:0", "SEQUENCE:1").replace("COUNT=4", "COUNT=2");
        import(&mut s, 2, "ana@example.org", &mail(&ics("REQUEST", &update), false), now).unwrap();
        // The dates it no longer has stay, cancelled.
        let c = || "cancelled".to_string();
        assert_eq!(titles(&s), [("20261109".into(), "maybe".into()), ("20261116".into(), "".into()), ("20261123".into(), c()), ("20261130".into(), c())]);
        // A stale update (older sequence) changes nothing.
        import(&mut s, 3, "ana@example.org", &mail(&ics("REQUEST", SERIES), false), now).unwrap();
        assert_eq!(titles(&s).len(), 4);
        // One date cancelled, then the whole series: kept, marked cancelled.
        let one = "BEGIN:VEVENT\r\nUID:weekly-1\r\nSEQUENCE:2\r\nRECURRENCE-ID;TZID=Europe/Lisbon:20261116T100000\r\nDTSTART;TZID=Europe/Lisbon:20261116T100000\r\nSTATUS:CANCELLED\r\nEND:VEVENT\r\n";
        import(&mut s, 5, "ana@example.org", &mail(&ics("CANCEL", one), false), now).unwrap();
        assert_eq!(titles(&s), [("20261109".into(), "maybe".into()), ("20261116".into(), c()), ("20261123".into(), c()), ("20261130".into(), c())]);
        import(&mut s, 6, "ana@example.org", &mail(&ics("CANCEL", &update), false), now).unwrap();
        assert!(titles(&s).iter().all(|t| t.1 == "cancelled"));
        // A new invitation brings it back (newer sequence).
        let up3 = SERIES.replace("SEQUENCE:0", "SEQUENCE:3");
        import(&mut s, 8, "ana@example.org", &mail(&ics("REQUEST", &up3), false), now).unwrap();
        assert_eq!(titles(&s).len(), 4);
        assert!(titles(&s).iter().all(|t| t.1 != "cancelled"));
        // Removing dates yourself: they stay removed when the organizer updates.
        let cal = s.invitations_calendar().unwrap();
        let evs: Vec<_> = s.events_between(0, i64::MAX / 2).unwrap().into_iter().filter(|e| e.calendar_id == cal).collect();
        assert_eq!(s.remove_series_range(evs[1].id, Some(evs[1].event.start), Some(evs[1].event.start + 1)).unwrap(), 1, "one date");
        assert_eq!(s.remove_series_range(evs[0].id, Some(evs[3].event.start), None).unwrap(), 1, "from a date on");
        let up4 = SERIES.replace("SEQUENCE:0", "SEQUENCE:4");
        import(&mut s, 9, "ana@example.org", &mail(&ics("REQUEST", &up4), false), now).unwrap();
        assert_eq!(titles(&s).iter().map(|t| t.0.as_str()).collect::<Vec<_>>(), ["20261109", "20261123"]);
        s.remove_series_range(evs[0].id, None, None).unwrap();
        assert!(titles(&s).is_empty(), "the whole series");
        // Adding the file by hand brings it back, removed dates included.
        assert_eq!(import_ics(&mut s, "ana@example.org", &ics("REQUEST", &up4), now, None).unwrap(), 1);
        assert_eq!(titles(&s).len(), 4);
    }

    // The calendar's sync button reads every clean invitation again: what's
    // missing comes back, nothing is doubled, newer wins, removals stick.
    #[test]
    fn rescan_adds_missing_meetings_without_duplicates() {
        let s = Store::open_in_memory().unwrap();
        let ep = crate::Endpoint { host: "h".into(), port: 993, security: crate::Security::Tls, allow_insecure: false };
        let mut a = crate::Account {
            id: 0, name: "Me".into(), email: "me@x.org".into(), username: "me".into(), incoming_protocol: crate::Protocol::Imap,
            incoming: ep.clone(), smtp: ep, smtp_username: None, signature: String::new(), auth: crate::AuthMethod::Password,
        };
        s.add_account(&mut a).unwrap();
        // Arrives (clean) in this order: the series, an update to 2 dates,
        // another meeting, and a mail with no calendar.
        let other = SERIES.replace("weekly-1", "other-1").replace("COUNT=4", "COUNT=1").replace("Team sync", "Lunch");
        let update = SERIES.replace("SEQUENCE:0", "SEQUENCE:1").replace("COUNT=4", "COUNT=2");
        let mails: Vec<Vec<u8>> = vec![
            mail(&ics("REQUEST", SERIES), true),
            mail(&ics("REQUEST", &update), false),
            mail(&ics("REQUEST", &other), true),
            b"From: Bo <bo@example.org>\r\nSubject: hi\r\n\r\nno calendar here\r\n".to_vec(),
        ];
        let mut ids = Vec::new();
        for (n, raw) in mails.iter().enumerate() {
            let mut meta = crate::message::parse_meta(raw);
            meta.message_id = format!("m{n}");
            meta.date = 1_000 + n as i64;
            let id = s.insert_message(&crate::store::NewMessage { account_id: a.id, folder: "INBOX".into(), uid: Some(n as u32 + 1), pop_uidl: None, meta, seen: false, flagged: false, size: 1, raw: Some(raw.clone()) }).unwrap();
            s.set_text(id, true, "").unwrap();
            ids.push(id);
        }
        // Only the first mail was read when it came (say a crash or an older
        // version missed the rest).
        let now = crate::ical::parse_datetime("20261101T000000Z").unwrap().0;
        let mut st = s;
        import(&mut st, ids[0], "ana@example.org", &mails[0], now).unwrap();
        assert_eq!(titles(&st).len(), 4);
        let store = std::sync::Mutex::new(st);

        assert_eq!(crate::ops::rescan_invites_at(&store, now).unwrap(), 3, "the three mails with a calendar file");
        let after = titles(&store.lock().unwrap());
        assert_eq!(after.iter().filter(|t| t.1 != "cancelled").count(), 3, "2 dates of the updated series + the other meeting: {after:?}");
        assert_eq!(after.iter().filter(|t| t.1 == "cancelled").count(), 2, "the dates the update dropped stay, cancelled: {after:?}");
        assert!(crate::ops::take_invites_changed());

        // Again, and again: nothing doubles.
        crate::ops::rescan_invites_at(&store, now).unwrap();
        crate::ops::rescan_invites_at(&store, now).unwrap();
        assert_eq!(titles(&store.lock().unwrap()), after, "rescanning doesn't add copies");

        // A date you removed stays removed; your answer stays.
        {
            let st = store.lock().unwrap();
            let cal = st.invitations_calendar().unwrap();
            let evs: Vec<_> = st.events_between(0, i64::MAX / 2).unwrap().into_iter().filter(|e| e.calendar_id == cal).collect();
            let weekly: Vec<_> = evs.iter().filter(|e| e.event.uid == "weekly-1").collect();
            st.set_rsvp(weekly[0].id, "yes").unwrap();
            st.remove_series_range(weekly[1].id, Some(weekly[1].event.start), Some(weekly[1].event.start + 1)).unwrap();
            let other: Vec<_> = evs.iter().filter(|e| e.event.uid == "other-1").collect();
            st.remove_series_range(other[0].id, None, None).unwrap();
        }
        let kept = titles(&store.lock().unwrap());
        assert_eq!(kept.len(), 3, "the answered date and the two cancelled ones: {kept:?}");
        crate::ops::rescan_invites_at(&store, now).unwrap();
        assert_eq!(titles(&store.lock().unwrap()), kept, "removed dates and series don't come back; answers stay");
        assert_eq!(kept[0].1, "yes");
    }

    // An organizer moving one date of a series (Outlook sends just that
    // date, often with the series' own sequence number) moves it here too.
    #[test]
    fn one_changed_date_moves() {
        let (mut s, _) = store_with_messages(4);
        let now = crate::ical::parse_datetime("20261101T000000Z").unwrap().0;
        let series = SERIES.replace("SEQUENCE:0", "SEQUENCE:3");
        import(&mut s, 1, "ana@example.org", &mail(&ics("REQUEST", &series), true), now).unwrap();
        let cal = s.invitations_calendar().unwrap();
        let second = s.events_between(0, i64::MAX / 2).unwrap().into_iter().filter(|e| e.calendar_id == cal).nth(1).unwrap();
        s.set_rsvp(second.id, "yes").unwrap();
        // 16 Nov 10:00 moves to 17 Nov 11:00, in another room; same sequence.
        let moved = "BEGIN:VEVENT\r\nUID:weekly-1\r\nSEQUENCE:3\r\nRECURRENCE-ID;TZID=Europe/Lisbon:20261116T100000\r\nDTSTART;TZID=Europe/Lisbon:20261117T110000\r\nDTEND;TZID=Europe/Lisbon:20261117T113000\r\nLOCATION:Room 9\r\nORGANIZER:mailto:ana@example.org\r\nEND:VEVENT\r\n";
        let touched = import(&mut s, 2, "ana@example.org", &mail(&ics("REQUEST", moved), false), now).unwrap();
        let outcome = s.invite_outcome(2).unwrap();
        assert_eq!(touched, ["weekly-1"]);
        assert_eq!(outcome.as_deref(), Some("date changed"));
        let evs: Vec<_> = s.events_between(0, i64::MAX / 2).unwrap().into_iter().filter(|e| e.calendar_id == cal).collect();
        assert_eq!(evs.iter().map(|e| crate::ical::fmt_date(e.event.start)).collect::<Vec<_>>(), ["20261109", "20261117", "20261123", "20261130"], "moved, not copied");
        let m = evs.iter().find(|e| crate::ical::fmt_date(e.event.start) == "20261117").unwrap();
        assert_eq!(m.event.location, "Room 9");
        assert_eq!(m.event.title, "Team sync", "the title it had");
        assert_eq!(m.rsvp, "yes", "your answer stays");
        // Someone else can't move it.
        import(&mut s, 3, "mallory@evil.example", &mail(&ics("REQUEST", &moved.replace("20261117T11", "20261118T11").replace("RECURRENCE-ID;TZID=Europe/Lisbon:20261116", "RECURRENCE-ID;TZID=Europe/Lisbon:20261117")), false), now).unwrap();
        assert!(s.events_between(0, i64::MAX / 2).unwrap().iter().any(|e| e.calendar_id == cal && crate::ical::fmt_date(e.event.start) == "20261117"));
    }

    // Two invitations for one meeting (or a copy kept apart from another
    // sender): shown once, the one you answered kept; different meetings at
    // the same time, or with different times, stay apart.
    #[test]
    fn the_same_meeting_shows_once() {
        let (mut s, _) = store_with_messages(8);
        let now = crate::ical::parse_datetime("20261101T000000Z").unwrap().0;
        let one = |uid: &str, title: &str, start: &str, end: &str| {
            format!("BEGIN:VEVENT\r\nUID:{uid}\r\nSUMMARY:{title}\r\nDTSTART:{start}\r\nDTEND:{end}\r\nORGANIZER:mailto:ana@example.org\r\nEND:VEVENT\r\n")
        };
        // Two invitations in one email for the same meeting (different uids).
        let both = format!("{}{}", one("a-1", "Partner sync", "20261119T150000Z", "20261119T160000Z"), one("a-2", " partner sync", "20261119T150000Z", "20261119T160000Z"));
        import(&mut s, 1, "ana@example.org", &mail(&ics("REQUEST", &both), false), now).unwrap();
        // The same again from someone else: kept apart, and identical.
        import(&mut s, 2, "bo@example.org", &mail(&ics("REQUEST", &one("a-1", "Partner sync", "20261119T150000Z", "20261119T160000Z")), false), now).unwrap();
        // A different meeting at the same time, and the same title later.
        import(&mut s, 3, "ana@example.org", &mail(&ics("REQUEST", &one("b-1", "Budget", "20261119T150000Z", "20261119T160000Z")), false), now).unwrap();
        import(&mut s, 4, "ana@example.org", &mail(&ics("REQUEST", &one("a-3", "Partner sync", "20261126T150000Z", "20261126T160000Z")), false), now).unwrap();
        let cal = s.invitations_calendar().unwrap();
        let stored: i64 = s.conn().query_row("SELECT COUNT(*) FROM events WHERE calendar_id = ?1", [cal], |r| r.get(0)).unwrap();
        assert_eq!(stored, 5, "nothing deleted");
        let shown = |s: &Store| s.events_between(0, i64::MAX / 2).unwrap().into_iter().filter(|e| e.calendar_id == cal).map(|e| (e.event.title.trim().to_lowercase(), crate::ical::fmt_date(e.event.start), e.remote_id, e.rsvp)).collect::<Vec<_>>();
        let v = shown(&s);
        assert_eq!(v.iter().map(|e| (e.0.as_str(), e.1.as_str())).collect::<Vec<_>>(), [("partner sync", "20261119"), ("budget", "20261119"), ("partner sync", "20261126")], "{v:?}");
        assert_eq!(v[0].2, "a-1", "the first sender's, oldest");
        // The copy you answered is the one shown.
        let hidden: i64 = s.conn().query_row("SELECT id FROM events WHERE remote_id = 'a-2'", [], |r| r.get(0)).unwrap();
        s.set_rsvp(hidden, "yes").unwrap();
        let v = shown(&s);
        assert_eq!((v[0].2.as_str(), v[0].3.as_str()), ("a-2", "yes"));
        assert_eq!(v.len(), 3);
        // Search shows it once too.
        assert_eq!(s.search_events("apple", 50).unwrap().len(), 2);
    }

    // Monday's meeting: a two-weekly series, then an update sent right after
    // one of its dates ends the series earlier (Outlook's "this and
    // following" change). That date took place: it stays as it was; the
    // coming ones stay too, cancelled. A date the update moves isn't left
    // behind as a cancelled copy.
    #[test]
    fn an_update_never_erases_dates() {
        let (mut s, _) = store_with_messages(4);
        let at = |d: &str| crate::ical::parse_datetime(d).unwrap().0;
        let base = "BEGIN:VEVENT\r\nUID:proj-1\r\nSEQUENCE:3\r\nSUMMARY:Project sync\r\nDTSTART:20261026T140000Z\r\nDTEND:20261026T150000Z\r\nRRULE:FREQ=WEEKLY;INTERVAL=2;COUNT=4\r\nORGANIZER:mailto:ana@example.org\r\nEND:VEVENT\r\n";
        import(&mut s, 1, "ana@example.org", &mail(&ics("REQUEST", base), false), at("20261020T000000Z")).unwrap();
        assert_eq!(titles(&s).iter().map(|t| t.0.as_str()).collect::<Vec<_>>(), ["20261026", "20261109", "20261123", "20261207"]);
        // Sent on 9 Nov after that day's meeting: the series now ends on 26
        // Oct, so only that date remains in it.
        let shorter = base.replace("SEQUENCE:3", "SEQUENCE:4").replace("COUNT=4", "UNTIL=20261026T150000Z");
        let monday_evening = at("20261109T163000Z");
        import(&mut s, 2, "ana@example.org", &mail(&ics("REQUEST", &shorter), false), monday_evening).unwrap();
        let c = || "cancelled".to_string();
        assert_eq!(
            titles(&s),
            [("20261026".into(), "".into()), ("20261109".into(), "".into()), ("20261123".into(), c()), ("20261207".into(), c())],
            "9 Nov took place: kept as it was; the coming dates kept, cancelled"
        );
        // Reading the same email again changes nothing.
        s.forget_invites(2).unwrap();
        import(&mut s, 2, "ana@example.org", &mail(&ics("REQUEST", &shorter), false), monday_evening).unwrap();
        assert_eq!(titles(&s).len(), 4);
        // A full update that moves 23 Nov to the 24th: only the 24th shows.
        let moved = format!(
            "{}BEGIN:VEVENT\r\nUID:proj-1\r\nSEQUENCE:5\r\nSUMMARY:Project sync\r\nRECURRENCE-ID:20261123T140000Z\r\nDTSTART:20261124T140000Z\r\nDTEND:20261124T150000Z\r\nORGANIZER:mailto:ana@example.org\r\nEND:VEVENT\r\n",
            base.replace("SEQUENCE:3", "SEQUENCE:5")
        );
        import(&mut s, 3, "ana@example.org", &mail(&ics("REQUEST", &moved), false), monday_evening).unwrap();
        let v = titles(&s);
        assert!(v.iter().any(|t| t.0 == "20261124" && t.1.is_empty()), "{v:?}");
        assert!(!v.iter().any(|t| t.0 == "20261123"), "the moved date isn't left behind: {v:?}");
    }

    #[test]
    fn every_calendar_file_counts() {
        let (mut s, _) = store_with_messages(12);
        let now = crate::ical::parse_datetime("20261101T000000Z").unwrap().0;
        import(&mut s, 1, "ana@example.org", &mail(&ics("REQUEST", SERIES), true), now).unwrap();
        // Someone else can't change Ana's meeting: their file is a separate copy.
        let forged = SERIES.replace("SEQUENCE:0", "SEQUENCE:9").replace("Team sync", "Moved!").replace("ORGANIZER:mailto:ana@example.org", "ORGANIZER:mailto:mallory@evil.example");
        assert_eq!(import(&mut s, 2, "mallory@evil.example", &mail(&ics("REQUEST", &forged), false), now).unwrap(), ["weekly-1#mallory@evil.example"]);
        fn by_series(s: &Store, uid: &str) -> Vec<String> {
            let cal = s.invitations_calendar().unwrap();
            s.events_between(0, i64::MAX / 2).unwrap().into_iter().filter(|e| e.calendar_id == cal && e.remote_id == uid).map(|e| e.event.title).collect()
        }
        assert_eq!(by_series(&s, "weekly-1"), ["Team sync"; 4], "Ana's meeting untouched");
        assert_eq!(by_series(&s, "weekly-1#mallory@evil.example").len(), 4);
        // …and their cancellation doesn't touch Ana's either, even naming Ana as organizer.
        import(&mut s, 3, "mallory@evil.example", &mail(&ics("CANCEL", SERIES), false), now).unwrap();
        fn cancelled(s: &Store, uid: &str) -> usize {
            s.events_between(0, i64::MAX / 2).unwrap().iter().filter(|e| e.remote_id == uid && e.event.cancelled).count()
        }
        assert_eq!(cancelled(&s, "weekly-1"), 0, "Ana's meeting isn't cancelled by a stranger");
        assert_eq!(cancelled(&s, "weekly-1#mallory@evil.example"), 4, "only the stranger's copy is");
        // The organizer may update, even when forwarded by someone else first.
        let fwd = SERIES.replace("weekly-1", "fwd-1");
        import(&mut s, 4, "assistant@example.org", &mail(&ics("REQUEST", &fwd), false), now).unwrap();
        let fwd2 = fwd.replace("SEQUENCE:0", "SEQUENCE:1").replace("Team sync", "Team sync (room 2)");
        assert_eq!(import(&mut s, 5, "ana@example.org", &mail(&ics("REQUEST", &fwd2), false), now).unwrap(), ["fwd-1"]);
        assert_eq!(by_series(&s, "fwd-1")[0], "Team sync (room 2)");
        // A cancellation for a meeting not here yet: added, as cancelled.
        let gone = "BEGIN:VEVENT\r\nUID:gone-1\r\nSUMMARY:Offsite\r\nDTSTART:20261201T090000Z\r\nDTEND:20261201T170000Z\r\nSTATUS:CANCELLED\r\nEND:VEVENT\r\n";
        assert_eq!(import(&mut s, 6, "ana@example.org", &mail(&ics("CANCEL", gone), false), now).unwrap(), ["gone-1"]);
        assert!(s.events_between(0, i64::MAX / 2).unwrap().iter().any(|e| e.remote_id == "gone-1" && e.event.cancelled));
        // A reply to a meeting not here (yours, organized elsewhere) adds it;
        // a reply to one that's here is just an answer.
        let mine = SERIES.replace("weekly-1", "mine-1").replace("ORGANIZER:mailto:ana@example.org", "ORGANIZER:mailto:me@x.org");
        assert_eq!(import(&mut s, 7, "bob@example.org", &mail(&ics("REPLY", &mine), false), now).unwrap(), ["mine-1"]);
        assert!(import(&mut s, 8, "carol@example.org", &mail(&ics("REPLY", SERIES), false), now).unwrap().is_empty());
        assert_eq!(by_series(&s, "weekly-1"), ["Team sync"; 4], "an answer changes nothing");
        // An invitation to a meeting long past still goes in (history).
        let old = "BEGIN:VEVENT\r\nUID:old-1\r\nSUMMARY:Kickoff\r\nDTSTART:20251001T090000Z\r\nDTEND:20251001T100000Z\r\nEND:VEVENT\r\n";
        assert_eq!(import(&mut s, 10, "carol@example.org", &mail(&ics("REQUEST", old), true), now).unwrap(), ["old-1"]);
        assert_eq!(s.invites_of(10, now).unwrap()[0].event.title, "Kickoff", "a year-old meeting is kept");
        // Each meeting knows the mail it came from (and what each did).
        let src: Vec<(i64, String)> = s.invite_sources("gone-1").unwrap();
        assert_eq!(src.len(), 1);
        assert_eq!(src[0].1, "cancelled");
        assert_eq!(s.invite_sources("weekly-1").unwrap().iter().map(|x| x.1.as_str()).collect::<Vec<_>>(), ["added"]);
        // What happened to each message is counted.
        let stats: std::collections::HashMap<String, i64> = s.invite_stats().unwrap().into_iter().collect();
        assert_eq!(stats.get("answer"), Some(&1));
        assert_eq!(stats.get("cancelled"), Some(&2));
        assert!(stats.get("added").is_some_and(|n| *n >= 5));
    }
}
