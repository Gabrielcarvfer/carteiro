//! Microsoft Graph calendar (Outlook.com / Microsoft 365).

use serde_json::{json, Value};

use crate::error::{Error, Result};
use crate::http::{self, encode, Request};
use crate::ical::{self, Event};

const BASE: &str = "https://graph.microsoft.com/v1.0";

#[derive(Debug, Clone, PartialEq)]
pub struct GraphCalendar {
    pub id: String,
    pub name: String,
    pub color: Option<String>,
    pub can_edit: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GraphEvent {
    pub id: String,
    pub event: Event,
}

async fn get(token: &str, url: &str) -> Result<Value> {
    // Times come back in UTC so no timezone database is needed.
    http::send(Request::new("GET", url).bearer(token).header("Prefer", "outlook.timezone=\"UTC\"")).await?.ok()?.json()
}

fn s(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

pub async fn calendars(token: &str) -> Result<Vec<GraphCalendar>> {
    let v = get(token, &format!("{BASE}/me/calendars?$top=100")).await?;
    let list = v.get("value").and_then(Value::as_array).ok_or_else(|| Error::Protocol("Graph: no calendars".into()))?;
    Ok(list
        .iter()
        .map(|c| GraphCalendar {
            id: s(c, "id"),
            name: s(c, "name"),
            color: c.get("hexColor").and_then(Value::as_str).filter(|h| h.starts_with('#')).map(str::to_string),
            can_edit: c.get("canEdit").and_then(Value::as_bool).unwrap_or(true),
        })
        .collect())
}

pub fn parse_event(v: &Value) -> Option<GraphEvent> {
    let start = ical::parse_iso(v.get("start")?.get("dateTime")?.as_str()?)?;
    let end = ical::parse_iso(v.get("end")?.get("dateTime")?.as_str()?)?;
    Some(GraphEvent {
        id: s(v, "id"),
        event: Event {
            uid: s(v, "iCalUId"),
            title: s(v, "subject"),
            location: v.get("location").map(|l| s(l, "displayName")).unwrap_or_default(),
            description: s(v, "bodyPreview"),
            start,
            end,
            all_day: v.get("isAllDay").and_then(Value::as_bool).unwrap_or(false),
            floating: false,
            recurring: matches!(v.get("type").and_then(Value::as_str), Some("occurrence" | "exception" | "seriesMaster")),
            // Times are exact (UTC); the event's own zone is kept to show it.
            tzid: crate::tz::to_iana(&s(v, "originalStartTimeZone")),
            tz_offset: None,
        },
    })
}

/// Event instances (recurrences expanded) overlapping [from, to).
pub async fn events(token: &str, calendar_id: &str, from: i64, to: i64) -> Result<Vec<GraphEvent>> {
    let mut url = format!(
        "{BASE}/me/calendars/{}/calendarView?startDateTime={}Z&endDateTime={}Z&$top=200&$select=id,iCalUId,subject,location,bodyPreview,start,end,isAllDay,type,originalStartTimeZone",
        encode(calendar_id),
        ical::fmt_iso(from),
        ical::fmt_iso(to)
    );
    let mut out = Vec::new();
    for _ in 0..50 {
        let v = get(token, &url).await?;
        for e in v.get("value").and_then(Value::as_array).into_iter().flatten() {
            if let Some(ev) = parse_event(e) {
                out.push(ev);
            }
        }
        match v.get("@odata.nextLink").and_then(Value::as_str) {
            Some(next) if next.starts_with(BASE) => url = next.to_string(),
            _ => break,
        }
    }
    Ok(out)
}

fn body(e: &Event) -> Value {
    // Wall-clock times in the event's zone (Graph accepts IANA names), so the
    // event keeps its zone; otherwise exact UTC times.
    let zoned = e.floating && !e.all_day && !e.tzid.is_empty();
    let when = |t: i64| json!({ "dateTime": ical::fmt_iso(t), "timeZone": if zoned { e.tzid.as_str() } else { "UTC" } });
    let end = if e.all_day { e.end.max(e.start + 86400) } else { e.end.max(e.start) };
    json!({
        "subject": e.title,
        "body": { "contentType": "text", "content": e.description },
        "location": { "displayName": e.location },
        "start": when(e.start),
        "end": when(end),
        "isAllDay": e.all_day,
    })
}

pub async fn create(token: &str, calendar_id: &str, e: &Event) -> Result<String> {
    let v = http::send(
        Request::new("POST", &format!("{BASE}/me/calendars/{}/events", encode(calendar_id)))
            .bearer(token)
            .body("application/json", body(e).to_string()),
    )
    .await?
    .ok()?
    .json()?;
    Ok(s(&v, "id"))
}

pub async fn update(token: &str, event_id: &str, e: &Event) -> Result<()> {
    http::send(Request::new("PATCH", &format!("{BASE}/me/events/{}", encode(event_id))).bearer(token).body("application/json", body(e).to_string()))
        .await?
        .ok()
        .map(|_| ())
}

pub async fn delete(token: &str, event_id: &str) -> Result<()> {
    let r = http::send(Request::new("DELETE", &format!("{BASE}/me/events/{}", encode(event_id))).bearer(token)).await?;
    if r.status == 404 {
        return Ok(());
    }
    r.ok().map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_graph_event() {
        let v: Value = serde_json::from_str(
            r#"{"id":"AAMk","iCalUId":"040000","subject":"1:1","bodyPreview":"agenda","isAllDay":false,"type":"occurrence",
                "start":{"dateTime":"2026-11-09T09:30:00.0000000","timeZone":"UTC"},
                "end":{"dateTime":"2026-11-09T10:00:00.0000000","timeZone":"UTC"},
                "location":{"displayName":"Room 2"}}"#,
        )
        .unwrap();
        let e = parse_event(&v).unwrap();
        assert_eq!(e.id, "AAMk");
        assert_eq!(e.event.start, 1794216600);
        assert_eq!(e.event.end - e.event.start, 1800);
        assert_eq!(e.event.location, "Room 2");
        assert!(e.event.recurring);
        let b = body(&e.event);
        assert_eq!(b["start"]["dateTime"], "2026-11-09T09:30:00");
        assert_eq!(b["start"]["timeZone"], "UTC");
    }
}
