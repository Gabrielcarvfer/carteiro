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
    for ics in calendar_parts(raw) {
        for uid in apply(store, sender, &ics, now, false, account)? {
            // The same invitation often comes twice (inline and as invite.ics).
            if !touched.contains(&uid) {
                touched.push(uid);
            }
        }
    }
    store.mark_invites(msg, &touched)?;
    Ok(touched)
}

/// A calendar file you chose to add from a message: like a mailed
/// invitation, except that dates you removed from the series come back.
/// Returns how many series it added or changed.
pub fn import_ics(store: &mut Store, sender: &str, ics: &str, now: i64, account: Option<i64>) -> Result<usize> {
    let changed = apply(store, sender, ics, now, true, account)?;
    if changed.is_empty() {
        return Err(crate::Error::Other("Nothing was added: this event belongs to an invitation from someone else".into()));
    }
    Ok(changed.len())
}

/// Applies one calendar object; returns the series it changed.
fn apply(store: &mut Store, sender: &str, ics: &str, now: i64, by_hand: bool, account: Option<i64>) -> Result<Vec<String>> {
    let method = calendar_method(ics);
    let mut touched = Vec::new();
    // Replies and counter-proposals are other people's answers, not events.
    if !matches!(method.as_str(), "" | "REQUEST" | "PUBLISH" | "CANCEL") {
        return Ok(touched);
    }
    let mut series: BTreeMap<String, Vec<Event>> = BTreeMap::new();
    for e in parse_events(ics) {
        if !e.uid.is_empty() && e.uid.len() <= 512 && e.start != 0 {
            series.entry(e.uid.clone()).or_default().push(e);
        }
    }
    for (uid, events) in series {
        let changed = if method == "CANCEL" {
            match events.iter().find(|e| e.recurrence_id.is_some()) {
                Some(one) if !events.iter().any(|e| e.recurrence_id.is_none()) => store.cancel_invite(&uid, sender, one.recurrence_id)?,
                _ => store.cancel_invite(&uid, sender, None)?,
            }
        } else {
            let sequence = events.iter().map(|e| e.sequence).max().unwrap_or(0);
            let organizer = events.iter().map(|e| e.organizer.clone()).find(|o| !o.is_empty()).unwrap_or_default();
            // Laid out from the series' start (past meetings included, so old
            // invitations show up too) to two years after the later of today
            // and that start, at most MAX_INSTANCES dates.
            let first = events.iter().map(|e| e.start).min().unwrap_or(now);
            let instances = crate::recur::expand_all(events, first.min(now - PAST), now.max(first) + FUTURE, MAX_INSTANCES);
            store.save_invite(&uid, sender, sequence, &instances, by_hand, &organizer, account)?
        };
        if changed {
            touched.push(uid);
        }
    }
    Ok(touched)
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

    fn titles(s: &Store) -> Vec<(String, String)> {
        let cal = s.invitations_calendar().unwrap();
        s.events_between(0, i64::MAX / 2).unwrap().into_iter().filter(|e| e.calendar_id == cal).map(|e| (crate::ical::fmt_date(e.event.start), e.rsvp)).collect()
    }

    #[test]
    fn invitations_series_answers_updates_and_cancels() {
        let mut s = Store::open_in_memory().unwrap();
        let now = crate::ical::parse_datetime("20261101T000000Z").unwrap().0;
        let ep = crate::Endpoint { host: "h".into(), port: 993, security: crate::Security::Tls, allow_insecure: false };
        let mut a = crate::Account {
            id: 0, name: "Me".into(), email: "me@x.org".into(), username: "me".into(), incoming_protocol: crate::Protocol::Imap,
            incoming: ep.clone(), smtp: ep, smtp_username: None, signature: String::new(), auth: crate::AuthMethod::Password,
        };
        s.add_account(&mut a).unwrap();
        for uid in 1..=10 {
            let meta = crate::message::MessageMeta { message_id: format!("m{uid}"), from: Addr { name: String::new(), email: "x@y.org".into() }, ..Default::default() };
            s.insert_message(&crate::store::NewMessage { account_id: a.id, folder: "INBOX".into(), uid: Some(uid), pop_uidl: None, meta, seen: false, flagged: false, size: 1, raw: None }).unwrap();
        }
        assert_eq!(import(&mut s, 1, "ana@example.org", &mail(&ics("REQUEST", SERIES), true), now).unwrap(), ["weekly-1"]);
        assert_eq!(titles(&s).len(), 4, "the whole series");
        assert_eq!(s.invite_reply_info("weekly-1").unwrap(), Some(("ana@example.org".into(), 0, Some(a.id))), "who to answer, from which account");
        assert!(import(&mut s, 1, "ana@example.org", &mail(&ics("REQUEST", SERIES), true), now).unwrap().is_empty(), "once per message");
        // Your answer is per date, and survives an update.
        let first = s.invites_of(1, now).unwrap().remove(0);
        assert_eq!(first.event.title, "Team sync");
        s.set_rsvp(first.id, "maybe").unwrap();
        let update = SERIES.replace("SEQUENCE:0", "SEQUENCE:1").replace("COUNT=4", "COUNT=2");
        import(&mut s, 2, "ana@example.org", &mail(&ics("REQUEST", &update), false), now).unwrap();
        assert_eq!(titles(&s), [("20261109".into(), "maybe".into()), ("20261116".into(), "".into())]);
        // A stale update (older sequence) and a stranger's changes are ignored.
        import(&mut s, 3, "ana@example.org", &mail(&ics("REQUEST", SERIES), false), now).unwrap();
        import(&mut s, 4, "mallory@evil.example", &mail(&ics("CANCEL", &update), false), now).unwrap();
        assert_eq!(titles(&s).len(), 2);
        // One instance cancelled, then the whole series.
        let one = "BEGIN:VEVENT\r\nUID:weekly-1\r\nSEQUENCE:2\r\nRECURRENCE-ID;TZID=Europe/Lisbon:20261116T100000\r\nDTSTART;TZID=Europe/Lisbon:20261116T100000\r\nSTATUS:CANCELLED\r\nEND:VEVENT\r\n";
        import(&mut s, 5, "ana@example.org", &mail(&ics("CANCEL", one), false), now).unwrap();
        assert_eq!(titles(&s), [("20261109".into(), "maybe".into())]);
        import(&mut s, 6, "ana@example.org", &mail(&ics("CANCEL", &update), false), now).unwrap();
        assert!(titles(&s).is_empty());
        // Removing dates yourself: they stay removed when the organizer updates.
        let up3 = SERIES.replace("SEQUENCE:0", "SEQUENCE:3");
        import(&mut s, 8, "ana@example.org", &mail(&ics("REQUEST", &up3), false), now).unwrap();
        assert_eq!(titles(&s).len(), 4);
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
        assert!(import_ics(&mut s, "mallory@evil.example", &ics("REQUEST", &up4), now, None).is_err(), "not someone else's series");
        // Other people's replies are not events.
        let reply = SERIES.replace("weekly-1", "weekly-2");
        import(&mut s, 7, "bob@example.org", &mail(&ics("REPLY", &reply), false), now).unwrap();
        assert_eq!(titles(&s).len(), 4);
        // An invitation to a meeting long past still goes in (history).
        let old = "BEGIN:VEVENT\r\nUID:old-1\r\nSUMMARY:Kickoff\r\nDTSTART:20251001T090000Z\r\nDTEND:20251001T100000Z\r\nEND:VEVENT\r\n";
        assert_eq!(import(&mut s, 10, "carol@example.org", &mail(&ics("REQUEST", old), true), now).unwrap(), ["old-1"]);
        assert_eq!(s.invites_of(10, now).unwrap()[0].event.title, "Kickoff", "a year-old meeting is kept");
    }
}
