//! Time zone names. Outlook and Microsoft Graph use Windows names ("W. Europe
//! Standard Time"); everything else here (and the web view's Intl, which does
//! the actual conversions) uses IANA names ("Europe/Berlin"). The mapping is
//! Unicode CLDR's (data/windows_zones.txt, scripts/gen-windows-zones.py).

static WINDOWS_ZONES: &str = include_str!("../data/windows_zones.txt");

/// An IANA name for `name` (IANA names pass through), or "" if unknown.
pub fn to_iana(name: &str) -> String {
    let name = name.trim().trim_matches('"');
    if name.is_empty() {
        return String::new();
    }
    if name.contains('/') || name.eq_ignore_ascii_case("UTC") {
        // Plain IANA-style names only (they go to the UI and to servers).
        let ok = name.len() <= 64 && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"/_+-".contains(&b));
        return if ok { name.to_string() } else { String::new() };
    }
    WINDOWS_ZONES
        .lines()
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| l.split_once('\t'))
        .find(|(w, _)| w.eq_ignore_ascii_case(name))
        .map(|(_, iana)| iana.to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::to_iana;

    #[test]
    fn maps_windows_names_and_keeps_iana() {
        assert_eq!(to_iana("W. Europe Standard Time"), "Europe/Berlin");
        assert_eq!(to_iana("\"Eastern Standard Time\""), "America/New_York");
        assert_eq!(to_iana("Europe/Madrid"), "Europe/Madrid");
        assert_eq!(to_iana("UTC"), "UTC");
        assert_eq!(to_iana("Custom Zone 1"), "");
        assert_eq!(to_iana("Evil/Zone\";X"), "", "only plain names");
    }
}
