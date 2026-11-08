//! Calendar sync and editing across sources: the local calendar, CalDAV
//! servers (including Google) and Microsoft Graph.

use std::sync::Mutex;

use crate::account::Credential;
use crate::caldav::CalDav;
use crate::error::{Error, Result};
use crate::graph;
use crate::ical::Event;
use crate::scan::{self, ScannerConfig, Verdict};
use crate::store::{CalSource, Store};
use crate::sync::lock;

/// Synced window around today.
pub const PAST: i64 = 60 * 86400;
pub const FUTURE: i64 = 400 * 86400;

pub fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Google's CalDAV endpoint for an account.
pub fn google_caldav_url(email: &str) -> String {
    format!("https://apidata.googleusercontent.com/caldav/v2/{}/user", crate::http::encode(email))
}

fn caldav(source: &CalSource, cred: &Credential) -> CalDav {
    CalDav { user: source.username.clone(), cred: cred.clone() }
}

fn bearer(cred: &Credential) -> Result<&str> {
    match cred {
        Credential::Bearer(t) => Ok(t),
        Credential::Password(_) => Err(Error::Other("Microsoft calendars need a Microsoft sign-in".into())),
    }
}

/// Lists the source's calendars and refreshes their events. Returns how many
/// events are now stored for it.
/// Rejects downloaded calendar data that the virus scanner flags. Data that
/// couldn't be scanned (engine still starting) is accepted: calendars hold
/// text only, and refusing them would empty the calendar.
async fn check(scanner: &ScannerConfig, data: &[u8], what: &str) -> Result<()> {
    if let Verdict::Infected { signature } = scan::scan(scanner, data).await {
        return Err(Error::Other(format!("{what} was blocked: malware detected ({signature})")));
    }
    Ok(())
}

pub async fn sync_source(store: &Mutex<Store>, source: &CalSource, cred: Option<&Credential>, scanner: &ScannerConfig) -> Result<usize> {
    let (from, to) = (now() - PAST, now() + FUTURE);
    match source.kind.as_str() {
        "local" => return Ok(0),
        "holidays" => return sync_holidays(store, source, from, to).await,
        "ics" => return sync_ics(store, source, from, to, scanner).await,
        _ => {}
    }
    let Some(cred) = cred else { return Err(Error::Other("missing credentials".into())) };
    let mut total = 0;
    match source.kind.as_str() {
        "caldav" | "google" => {
            let dav = caldav(source, cred);
            let cals = dav.discover(&source.url).await?;
            let ids: Vec<String> = cals.iter().map(|c| c.url.clone()).collect();
            for c in &cals {
                let id = lock(store)?.upsert_calendar(source.id, &c.url, &c.name, c.color.as_deref(), false)?;
                let (remote, raw) = dav.events_raw(&c.url, from, to).await?;
                check(scanner, &raw, &format!("Calendar \"{}\"", c.name)).await?;
                let events: Vec<_> = remote.into_iter().map(|e| (e.href, e.etag, e.event)).collect();
                total += events.len();
                lock(store)?.replace_events(id, from, to, &events)?;
            }
            lock(store)?.retain_calendars(source.id, &ids)?;
        }
        "microsoft" => {
            let token = bearer(cred)?;
            let cals = graph::calendars(token).await?;
            let ids: Vec<String> = cals.iter().map(|c| c.id.clone()).collect();
            for c in &cals {
                let id = lock(store)?.upsert_calendar(source.id, &c.id, &c.name, c.color.as_deref(), !c.can_edit)?;
                let remote = graph::events(token, &c.id, from, to).await?;
                let text: String = remote.iter().map(|e| format!("{}\n{}\n{}\n", e.event.title, e.event.location, e.event.description)).collect();
                check(scanner, text.as_bytes(), &format!("Calendar \"{}\"", c.name)).await?;
                let events: Vec<_> = remote.into_iter().map(|e| (e.id, None, e.event)).collect();
                total += events.len();
                lock(store)?.replace_events(id, from, to, &events)?;
            }
            lock(store)?.retain_calendars(source.id, &ids)?;
        }
        other => return Err(Error::Other(format!("unknown calendar source: {other}"))),
    }
    Ok(total)
}

// ---- public holidays (Nager.Holidays, open data, no account needed) -------------

/// Nager.Holidays (formerly date.nager.at, which still answers but now
/// redirects its website here).
const HOLIDAYS_API: &str = "https://nagerholidays.com/api/v3";

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Country {
    pub code: String,
    pub name: String,
}

pub async fn holiday_countries() -> Result<Vec<Country>> {
    let v = crate::http::send(crate::http::Request::new("GET", &format!("{HOLIDAYS_API}/AvailableCountries"))).await?.ok()?.json()?;
    let mut out: Vec<Country> = v
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| Some(Country { code: c.get("countryCode")?.as_str()?.to_string(), name: c.get("name")?.as_str()?.to_string() }))
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

#[derive(Debug, Clone, PartialEq)]
pub struct Holiday {
    pub date: i64,
    pub name: String,
    /// None = nationwide; otherwise ISO 3166-2 regions, e.g. ["ES-CT"].
    pub regions: Option<Vec<String>>,
}

pub fn parse_holidays(v: &serde_json::Value) -> Vec<Holiday> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|h| {
            let date = crate::ical::parse_iso(h.get("date")?.as_str()?)?;
            let local = h.get("localName").and_then(|x| x.as_str()).unwrap_or("");
            let name = if local.is_empty() { h.get("name")?.as_str()? } else { local };
            let regions = h.get("counties").and_then(|c| c.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect());
            Some(Holiday { date, name: name.to_string(), regions })
        })
        .collect()
}

async fn holidays_for_year(country: &str, year: i64) -> Result<Vec<Holiday>> {
    if country.len() != 2 || !country.bytes().all(|b| b.is_ascii_alphabetic()) {
        return Err(Error::Other("invalid country code".into()));
    }
    let r = crate::http::send(crate::http::Request::new("GET", &format!("{HOLIDAYS_API}/PublicHolidays/{year}/{country}"))).await?;
    if r.status == 204 {
        return Ok(Vec::new());
    }
    Ok(parse_holidays(&r.ok()?.json()?))
}

/// Regions that have their own holidays this year (for the region picker).
pub async fn holiday_regions(country: &str) -> Result<Vec<String>> {
    let year = crate::ical::civil_from_days(now().div_euclid(86400)).0;
    let mut regions: Vec<String> = holidays_for_year(country, year).await?.into_iter().flat_map(|h| h.regions.unwrap_or_default()).collect();
    regions.sort();
    regions.dedup();
    Ok(regions)
}

/// Source url is "ES" or "ES/ES-CT" (country plus optional region).
async fn sync_holidays(store: &Mutex<Store>, source: &CalSource, from: i64, to: i64) -> Result<usize> {
    let (country, region) = match source.url.split_once('/') {
        Some((c, r)) => (c.to_string(), Some(r.to_string())),
        None => (source.url.clone(), None),
    };
    let (y0, y1) = (crate::ical::civil_from_days(from.div_euclid(86400)).0, crate::ical::civil_from_days(to.div_euclid(86400)).0);
    let mut events = Vec::new();
    for year in y0..=y1 {
        for h in holidays_for_year(&country, year).await? {
            let applies = match (&h.regions, &region) {
                (None, _) => true,
                (Some(rs), Some(r)) => rs.iter().any(|x| x == r),
                (Some(_), None) => false,
            };
            if applies {
                let e = Event { uid: format!("{}-{}@holidays", h.date, country), title: h.name, start: h.date, end: h.date + 86400, all_day: true, ..Default::default() };
                events.push((String::new(), None, e));
            }
        }
    }
    let cal = lock(store)?.upsert_calendar(source.id, "holidays", &source.name, Some("#107c10"), true)?;
    lock(store)?.replace_events(cal, from, to, &events)?;
    Ok(events.len())
}

// ---- subscribed calendars (ICS / webcal links) -----------------------------------

pub fn normalize_ics_url(url: &str) -> String {
    let url = url.trim();
    match url.strip_prefix("webcal://").or_else(|| url.strip_prefix("webcals://")) {
        Some(rest) => format!("https://{rest}"),
        None => url.to_string(),
    }
}

async fn sync_ics(store: &Mutex<Store>, source: &CalSource, from: i64, to: i64, scanner: &ScannerConfig) -> Result<usize> {
    let r = crate::http::send(crate::http::Request::new("GET", &normalize_ics_url(&source.url))).await?.ok()?;
    check(scanner, &r.body, &format!("Calendar feed \"{}\"", source.name)).await?;
    // Feeds aren't expanded by a server: series are expanded here.
    let events: Vec<_> = crate::recur::expand_all(crate::ical::parse_events(&r.text()), from, to, 1000)
        .into_iter()
        .map(|e| (String::new(), None, e))
        .collect();
    let cal = lock(store)?.upsert_calendar(source.id, "feed", &source.name, Some("#8764b8"), true)?;
    lock(store)?.replace_events(cal, from, to, &events)?;
    Ok(events.len())
}

fn new_uid() -> String {
    let mut b = [0u8; 16];
    let _ = ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut b);
    format!("{}@carteiro", crate::b64::encode_with(b, crate::b64::URL_SAFE, false))
}

/// Creates (`event_id` None) or updates an event. Returns the local id.
pub async fn save_event(
    store: &Mutex<Store>,
    source: &CalSource,
    cred: Option<&Credential>,
    calendar_id: i64,
    event_id: Option<i64>,
    mut event: Event,
) -> Result<i64> {
    let (cal, existing) = {
        let st = lock(store)?;
        let cal = st.calendar(calendar_id)?.ok_or_else(|| Error::Other("calendar not found".into()))?;
        let existing = match event_id {
            Some(id) => Some(st.event(id)?.ok_or_else(|| Error::Other("event not found".into()))?),
            None => None,
        };
        (cal, existing)
    };
    if cal.read_only {
        return Err(Error::Other("This calendar is read-only".into()));
    }
    if existing.as_ref().is_some_and(|e| e.event.recurring) && source.kind != "local" {
        return Err(Error::Other("Repeating events can't be edited here yet; change them in the web calendar".into()));
    }
    if event.end < event.start {
        event.end = event.start;
    }
    event.uid = existing.as_ref().map(|e| e.event.uid.clone()).filter(|u| !u.is_empty()).unwrap_or_else(new_uid);

    let (remote_id, etag) = match (source.kind.as_str(), cred) {
        // On this device: an event keeps its link to its series (invitations).
        ("local", _) | (_, None) => (existing.as_ref().map(|e| e.remote_id.clone()).unwrap_or_default(), None),
        ("microsoft", Some(c)) => {
            let token = bearer(c)?;
            match &existing {
                Some(e) => {
                    graph::update(token, &e.remote_id, &event).await?;
                    (e.remote_id.clone(), None)
                }
                None => (graph::create(token, &cal.remote_id, &event).await?, None),
            }
        }
        (_, Some(c)) => {
            let dav = caldav(source, c);
            let href = existing.as_ref().map(|e| e.remote_id.as_str()).filter(|h| !h.is_empty());
            let etag = existing.as_ref().and_then(|e| e.etag.as_deref());
            (dav.put(&cal.remote_id, &event, href, etag, now()).await?, None)
        }
    };
    let st = lock(store)?;
    match existing {
        Some(e) => {
            st.update_event(e.id, &remote_id, etag, &event)?;
            Ok(e.id)
        }
        None => st.insert_event(calendar_id, &remote_id, etag, &event),
    }
}

pub async fn delete_event(store: &Mutex<Store>, source: &CalSource, cred: Option<&Credential>, event_id: i64) -> Result<()> {
    let e = lock(store)?.event(event_id)?.ok_or_else(|| Error::Other("event not found".into()))?;
    if e.event.recurring && source.kind != "local" {
        return Err(Error::Other("Repeating events can't be deleted here yet; change them in the web calendar".into()));
    }
    match (source.kind.as_str(), cred) {
        ("local", _) | (_, None) => {}
        ("microsoft", Some(c)) => graph::delete(bearer(c)?, &e.remote_id).await?,
        (_, Some(c)) => caldav(source, c).delete(&e.remote_id, e.etag.as_deref()).await?,
    }
    lock(store)?.delete_event(event_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nager_holidays() {
        let v: serde_json::Value = serde_json::from_str(
            r#"[{"date":"2026-09-11","localName":"Diada Nacional de Catalunya","name":"National Day of Catalonia","countryCode":"ES","global":false,"counties":["ES-CT"]},
                {"date":"2026-12-25","localName":"Navidad","name":"Christmas Day","countryCode":"ES","global":true,"counties":null}]"#,
        )
        .unwrap();
        let h = parse_holidays(&v);
        assert_eq!(h[0].name, "Diada Nacional de Catalunya");
        assert_eq!(h[0].regions.as_deref(), Some(&["ES-CT".to_string()][..]));
        assert_eq!(h[1].regions, None);
        assert_eq!(crate::ical::fmt_date(h[1].date), "20261225");
        assert_eq!(normalize_ics_url("webcal://example.com/a.ics"), "https://example.com/a.ics");
    }

    #[tokio::test]
    async fn infected_calendar_data_is_rejected() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 1 << 16];
            // Read until the zero-length terminator chunk, then answer.
            let mut got = Vec::new();
            while !got.ends_with(&[0, 0, 0, 0]) {
                let n = s.read(&mut buf).await.unwrap();
                if n == 0 { break; }
                got.extend_from_slice(&buf[..n]);
            }
            s.write_all(b"stream: Eicar-Test-Signature FOUND\0").await.unwrap();
        });
        let cfg = ScannerConfig { clamd_addr: addr, ..Default::default() };
        let err = check(&cfg, b"BEGIN:VCALENDAR...", "Calendar feed \"x\"").await.unwrap_err();
        assert!(err.to_string().contains("Eicar-Test-Signature"));
        // Engine not ready: accepted.
        assert!(check(&ScannerConfig::default(), b"x", "y").await.is_ok());
    }

    #[tokio::test]
    async fn local_calendar_crud() {
        let store = Mutex::new(Store::open_in_memory().unwrap());
        let cal = store.lock().unwrap().ensure_local_calendar().unwrap();
        let source = store.lock().unwrap().cal_sources().unwrap().remove(0);
        let ev = Event { title: "Dentist".into(), start: 1_000_000, end: 1_003_600, ..Default::default() };
        let id = save_event(&store, &source, None, cal, None, ev.clone()).await.unwrap();
        let saved = store.lock().unwrap().event(id).unwrap().unwrap();
        assert!(saved.event.uid.ends_with("@carteiro"));
        let moved = Event { start: 2_000_000, end: 2_000_000 - 5, ..ev };
        save_event(&store, &source, None, cal, Some(id), moved).await.unwrap();
        let saved = store.lock().unwrap().event(id).unwrap().unwrap();
        assert_eq!((saved.event.start, saved.event.end), (2_000_000, 2_000_000)); // end clamped
        delete_event(&store, &source, None, id).await.unwrap();
        assert!(store.lock().unwrap().event(id).unwrap().is_none());
    }

    #[tokio::test]
    async fn editing_one_date_keeps_it_in_its_series() {
        let mut st = Store::open_in_memory().unwrap();
        let t = |d: &str| crate::ical::parse_datetime(d).unwrap().0;
        let ev = |start: i64| crate::ical::Event { title: "Sync".into(), start, end: start + 3600, recurring: true, ..Default::default() };
        st.save_invite("s-1", "ana@x.org", 0, &[ev(t("20261109T100000Z")), ev(t("20261116T100000Z"))], false, "ana@x.org", None).unwrap();
        let cal = st.invitations_calendar().unwrap();
        let first = st.events_between(0, i64::MAX / 2).unwrap().into_iter().find(|e| e.calendar_id == cal).unwrap();
        let src = st.cal_sources().unwrap().into_iter().find(|s| s.kind == "local").unwrap();
        let store = Mutex::new(st);
        let mut edited = first.event.clone();
        edited.title = "Sync (room changed)".into();
        save_event(&store, &src, None, cal, Some(first.id), edited).await.unwrap();
        let st = store.lock().unwrap();
        let after = st.event(first.id).unwrap().unwrap();
        assert_eq!((after.remote_id.as_str(), after.event.title.as_str()), ("s-1", "Sync (room changed)"));
        assert_eq!(st.remove_series_range(first.id, None, None).unwrap(), 2, "still removed with its series");
    }
}
