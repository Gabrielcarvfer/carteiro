//! CalDAV client (RFC 4791): discovery, listing calendars, reading events in a
//! time range (server-expanded to UTC), and creating/deleting events.

use crate::account::Credential;
use crate::error::{Error, Result};
use crate::http::{self, Request, Response, Url};
use crate::ical::{self, Event};
use crate::xml;

#[derive(Debug, Clone, PartialEq)]
pub struct RemoteCalendar {
    pub url: String,
    pub name: String,
    pub color: Option<String>,
    pub ctag: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RemoteEvent {
    pub href: String,
    pub etag: Option<String>,
    pub event: Event,
}

pub struct CalDav {
    pub user: String,
    pub cred: Credential,
}

const PROPFIND_PRINCIPAL: &str = r#"<?xml version="1.0" encoding="utf-8"?><d:propfind xmlns:d="DAV:"><d:prop><d:current-user-principal/></d:prop></d:propfind>"#;
const PROPFIND_HOME: &str = r#"<?xml version="1.0" encoding="utf-8"?><d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:prop><c:calendar-home-set/></d:prop></d:propfind>"#;
const PROPFIND_CALENDARS: &str = r#"<?xml version="1.0" encoding="utf-8"?><d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav" xmlns:cs="http://calendarserver.org/ns/" xmlns:ic="http://apple.com/ns/ical/"><d:prop><d:displayname/><d:resourcetype/><c:supported-calendar-component-set/><cs:getctag/><ic:calendar-color/></d:prop></d:propfind>"#;

impl CalDav {
    fn request(&self, method: &str, url: &str) -> Request {
        let r = Request::new(method, url);
        match &self.cred {
            Credential::Password(p) => r.basic(&self.user, p),
            Credential::Bearer(t) => r.bearer(t),
        }
    }

    pub(crate) async fn dav(&self, method: &str, url: &str, depth: &str, body: &str) -> Result<(Response, xml::Node)> {
        let r = http::send(self.request(method, url).header("Depth", depth).body("application/xml; charset=utf-8", body))
            .await?
            .ok()?;
        let doc = xml::parse(&r.text())?;
        Ok((r, doc))
    }

    /// Finds the user's calendars starting from a server URL (any of: the
    /// server root, a principal URL, or a calendar home).
    pub async fn discover(&self, start: &str) -> Result<Vec<RemoteCalendar>> {
        let start_url = Url::parse(start)?;
        // 1. Principal: from the given URL, else from /.well-known/caldav.
        let mut principal = None;
        for candidate in [start.to_string(), start_url.join("/.well-known/caldav")?.to_string_full()] {
            if let Ok((resp, doc)) = self.dav("PROPFIND", &candidate, "0", PROPFIND_PRINCIPAL).await {
                if let Some(href) = doc.find("current-user-principal").and_then(|n| n.text_of("href")) {
                    principal = Some(Url::parse(&resp.url)?.join(&href)?);
                    break;
                }
            }
        }
        let principal = principal.unwrap_or(start_url);
        // 2. Calendar home.
        let (resp, doc) = self.dav("PROPFIND", &principal.to_string_full(), "0", PROPFIND_HOME).await?;
        let home = match doc.find("calendar-home-set").and_then(|n| n.text_of("href")) {
            Some(h) => Url::parse(&resp.url)?.join(&h)?,
            None => principal,
        };
        // 3. Calendars in the home.
        let (resp, doc) = self.dav("PROPFIND", &home.to_string_full(), "1", PROPFIND_CALENDARS).await?;
        let base = Url::parse(&resp.url)?;
        let mut out = Vec::new();
        for r in doc.all("response") {
            let Some(rt) = r.find("resourcetype") else { continue };
            if rt.child("calendar").is_none() {
                continue;
            }
            // Skip task-only lists (VTODO) when the server says so.
            if let Some(set) = r.find("supported-calendar-component-set") {
                let comps = set.all("comp");
                if !comps.is_empty() && !comps.iter().any(|c| c.attr("name").is_some_and(|n| n.eq_ignore_ascii_case("VEVENT"))) {
                    continue;
                }
            }
            let Some(href) = r.text_of("href") else { continue };
            let url = base.join(&href)?.to_string_full();
            out.push(RemoteCalendar {
                name: r.text_of("displayname").filter(|n| !n.is_empty()).unwrap_or_else(|| href.trim_end_matches('/').rsplit('/').next().unwrap_or("Calendar").to_string()),
                color: r.text_of("calendar-color").filter(|c| !c.is_empty()).map(|c| c.chars().take(7).collect()),
                ctag: r.text_of("getctag"),
                url,
            });
        }
        if out.is_empty() {
            return Err(Error::Other("no calendars found on this server".into()));
        }
        Ok(out)
    }

    /// Events overlapping [from, to), with recurrences expanded by the server.
    pub async fn events(&self, calendar_url: &str, from: i64, to: i64) -> Result<Vec<RemoteEvent>> {
        Ok(self.events_raw(calendar_url, from, to).await?.0)
    }

    /// Like `events`, plus the raw response (for malware scanning).
    pub async fn events_raw(&self, calendar_url: &str, from: i64, to: i64) -> Result<(Vec<RemoteEvent>, Vec<u8>)> {
        let (s, e) = (ical::fmt_utc(from), ical::fmt_utc(to));
        let body = format!(
            r#"<?xml version="1.0" encoding="utf-8"?><c:calendar-query xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:prop><d:getetag/><c:calendar-data><c:expand start="{s}" end="{e}"/></c:calendar-data></d:prop><c:filter><c:comp-filter name="VCALENDAR"><c:comp-filter name="VEVENT"><c:time-range start="{s}" end="{e}"/></c:comp-filter></c:comp-filter></c:filter></c:calendar-query>"#
        );
        let (resp, doc) = self.dav("REPORT", calendar_url, "1", &body).await?;
        let base = Url::parse(&resp.url)?;
        Ok((parse_report(&doc, &base), resp.body))
    }

    /// Creates (etag None) or replaces an event. Returns the resource URL.
    pub async fn put(&self, calendar_url: &str, event: &Event, href: Option<&str>, etag: Option<&str>, now: i64) -> Result<String> {
        let url = match href {
            Some(h) => h.to_string(),
            None => {
                let safe: String = event.uid.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect();
                Url::parse(calendar_url)?.join(&format!("{safe}.ics"))?.to_string_full()
            }
        };
        let mut req = self.request("PUT", &url).body("text/calendar; charset=utf-8", ical::build_event(event, now));
        req = match etag {
            Some(t) => req.header("If-Match", t),
            None if href.is_none() => req.header("If-None-Match", "*"),
            None => req,
        };
        http::send(req).await?.ok()?;
        Ok(url)
    }

    pub async fn delete(&self, href: &str, etag: Option<&str>) -> Result<()> {
        let mut req = self.request("DELETE", href);
        if let Some(t) = etag {
            req = req.header("If-Match", t);
        }
        let r = http::send(req).await?;
        if r.status == 404 {
            return Ok(()); // already gone
        }
        r.ok().map(|_| ())
    }
}

fn parse_report(doc: &xml::Node, base: &Url) -> Vec<RemoteEvent> {
    let mut out = Vec::new();
    for r in doc.all("response") {
        let (Some(href), Some(data)) = (r.text_of("href"), r.find("calendar-data")) else { continue };
        let href = base.join(&href).map(|u| u.to_string_full()).unwrap_or(href);
        let etag = r.text_of("getetag");
        for event in ical::parse_events(&data.text) {
            out.push(RemoteEvent { href: href.clone(), etag: etag.clone(), event });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_report_with_expanded_instances() {
        let doc = xml::parse(r#"<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
          <d:response><d:href>/cal/work/abc.ics</d:href><d:propstat><d:prop>
            <d:getetag>"1"</d:getetag>
            <c:calendar-data>BEGIN:VCALENDAR
BEGIN:VEVENT
UID:abc
SUMMARY:Standup
DTSTART:20261109T090000Z
DTEND:20261109T091500Z
RECURRENCE-ID:20261109T090000Z
END:VEVENT
BEGIN:VEVENT
UID:abc
SUMMARY:Standup
DTSTART:20261110T090000Z
DTEND:20261110T091500Z
RECURRENCE-ID:20261110T090000Z
END:VEVENT
END:VCALENDAR
</c:calendar-data></d:prop></d:propstat></d:response></d:multistatus>"#)
        .unwrap();
        let ev = parse_report(&doc, &Url::parse("https://dav.example.com/cal/work/").unwrap());
        assert_eq!(ev.len(), 2);
        assert_eq!(ev[0].href, "https://dav.example.com/cal/work/abc.ics");
        assert_eq!(ev[0].etag.as_deref(), Some("\"1\""));
        assert_eq!(ev[1].event.start - ev[0].event.start, 86400);
        assert!(ev[0].event.recurring);
    }
}
