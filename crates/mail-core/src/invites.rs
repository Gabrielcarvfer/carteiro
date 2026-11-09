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
    let outcome = if parts.is_empty() { "no readable calendar" } else { outcomes.iter().copied().find(|o| *o != "answer").unwrap_or("answer") };
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
        let changed = if method == "CANCEL" && store.invite_exists(&key)? {
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
            let done = store.save_invite(&key, owner, sequence, &instances, by_hand, &organizer, account)?;
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
        assert_eq!(titles(&s), [("20261109".into(), "maybe".into()), ("20261116".into(), "".into())]);
        // A stale update (older sequence) changes nothing.
        import(&mut s, 3, "ana@example.org", &mail(&ics("REQUEST", SERIES), false), now).unwrap();
        assert_eq!(titles(&s).len(), 2);
        // One date cancelled, then the whole series: kept, marked cancelled.
        let one = "BEGIN:VEVENT\r\nUID:weekly-1\r\nSEQUENCE:2\r\nRECURRENCE-ID;TZID=Europe/Lisbon:20261116T100000\r\nDTSTART;TZID=Europe/Lisbon:20261116T100000\r\nSTATUS:CANCELLED\r\nEND:VEVENT\r\n";
        import(&mut s, 5, "ana@example.org", &mail(&ics("CANCEL", one), false), now).unwrap();
        assert_eq!(titles(&s), [("20261109".into(), "maybe".into()), ("20261116".into(), "cancelled".into())]);
        import(&mut s, 6, "ana@example.org", &mail(&ics("CANCEL", &update), false), now).unwrap();
        assert_eq!(titles(&s), [("20261109".into(), "cancelled".into()), ("20261116".into(), "cancelled".into())]);
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
        // …and their cancellation doesn't touch Ana's either.
        import(&mut s, 3, "mallory@evil.example", &mail(&ics("CANCEL", SERIES), false), now).unwrap();
        assert!(titles(&s).iter().filter(|t| t.1 == "cancelled").count() == 4, "only the copy is cancelled");
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
        // What happened to each message is counted.
        let stats: std::collections::HashMap<String, i64> = s.invite_stats().unwrap().into_iter().collect();
        assert_eq!(stats.get("answer"), Some(&1));
        assert_eq!(stats.get("cancelled"), Some(&2));
        assert!(stats.get("added").is_some_and(|n| *n >= 5));
    }
}
