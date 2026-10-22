//! Time zones for pickers and validation (the setup wizard, settings):
//! the system's tz database, or a built-in list when it has none.

/// Used when the system has no tz database (e.g. a scratch container).
const FALLBACK_ZONES: &[&str] = &[
    "UTC",
    "Africa/Cairo",
    "Africa/Johannesburg",
    "Africa/Lagos",
    "Africa/Nairobi",
    "America/Anchorage",
    "America/Argentina/Buenos_Aires",
    "America/Bogota",
    "America/Chicago",
    "America/Denver",
    "America/Halifax",
    "America/Los_Angeles",
    "America/Mexico_City",
    "America/New_York",
    "America/Phoenix",
    "America/Sao_Paulo",
    "America/Toronto",
    "America/Vancouver",
    "Asia/Bangkok",
    "Asia/Dubai",
    "Asia/Hong_Kong",
    "Asia/Jakarta",
    "Asia/Jerusalem",
    "Asia/Karachi",
    "Asia/Kolkata",
    "Asia/Manila",
    "Asia/Seoul",
    "Asia/Shanghai",
    "Asia/Singapore",
    "Asia/Tehran",
    "Asia/Tokyo",
    "Atlantic/Reykjavik",
    "Australia/Adelaide",
    "Australia/Brisbane",
    "Australia/Perth",
    "Australia/Sydney",
    "Europe/Amsterdam",
    "Europe/Athens",
    "Europe/Berlin",
    "Europe/Brussels",
    "Europe/Copenhagen",
    "Europe/Dublin",
    "Europe/Helsinki",
    "Europe/Istanbul",
    "Europe/Kyiv",
    "Europe/Lisbon",
    "Europe/London",
    "Europe/Madrid",
    "Europe/Moscow",
    "Europe/Oslo",
    "Europe/Paris",
    "Europe/Prague",
    "Europe/Rome",
    "Europe/Stockholm",
    "Europe/Vienna",
    "Europe/Warsaw",
    "Europe/Zurich",
    "Pacific/Auckland",
    "Pacific/Honolulu",
];

fn is_listed_zone(name: &str) -> bool {
    name == "UTC"
        || (name.contains('/')
            && !name.starts_with("posix")
            && !name.starts_with("right/")
            && !name.starts_with("Etc/")
            && !name.starts_with("SystemV/"))
}

/// IANA zone names for the picker (sorted, `UTC` first).
pub fn timezones() -> Vec<String> {
    let mut zones: Vec<String> = jiff::tz::db()
        .available()
        .map(|n| n.as_str().to_string())
        .filter(|n| is_listed_zone(n))
        .collect();
    if zones.len() < 10 {
        zones = FALLBACK_ZONES.iter().map(|z| (*z).to_string()).collect();
    }
    zones.sort();
    zones.dedup();
    zones.retain(|z| z != "UTC");
    zones.insert(0, "UTC".into());
    zones
}

/// Is `name` a time zone we can use?
pub fn valid_timezone(name: &str) -> bool {
    if name.is_empty() || name.len() > 64 {
        return false;
    }
    name == "UTC" || jiff::tz::TimeZone::get(name).is_ok() || FALLBACK_ZONES.contains(&name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timezone_validation() {
        assert!(valid_timezone("UTC"));
        assert!(valid_timezone("Europe/Oslo"));
        assert!(!valid_timezone("Mars/Olympus_Mons"));
        assert!(!valid_timezone(""));
    }

    #[test]
    fn timezone_list_starts_with_utc() {
        let zones = timezones();
        assert_eq!(zones.first().map(String::as_str), Some("UTC"));
        assert!(zones.iter().any(|z| z == "Europe/Oslo"));
    }
}
