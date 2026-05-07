use chrono::{DateTime, Datelike, Local, SecondsFormat, TimeZone, Utc};

pub const SECONDS_PER_DAY: i64 = 24 * 60 * 60;
const TOGGL_MAX_BACKFILL_DAYS: u32 = 85;

pub fn current_rfc3339_utc() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

pub fn initial_backfill_since(now_unix_seconds: i64, initial_backfill_days: u32) -> i64 {
    let bounded_days = initial_backfill_days.min(TOGGL_MAX_BACKFILL_DAYS);
    let configured_since =
        now_unix_seconds.saturating_sub(i64::from(bounded_days) * SECONDS_PER_DAY);
    configured_since.max(start_of_utc_month(now_unix_seconds))
}

pub fn parse_rfc3339_utc(value: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|datetime| datetime.timestamp())
}

pub fn format_unix_utc(timestamp: i64) -> String {
    Utc.timestamp_opt(timestamp, 0)
        .single()
        .map(|datetime| datetime.to_rfc3339_opts(SecondsFormat::Secs, true))
        .unwrap_or_else(|| timestamp.to_string())
}

pub fn split_status_datetime(value: &Option<String>) -> (String, String) {
    let Some(value) = value.as_deref() else {
        return ("-".to_owned(), "-".to_owned());
    };
    if let Ok(datetime) = DateTime::parse_from_rfc3339(value) {
        let local = datetime.with_timezone(&Local);
        return (
            local.format("%Y-%m-%d").to_string(),
            local.format("%H:%M").to_string(),
        );
    }
    (
        value.get(0..10).unwrap_or("-").to_owned(),
        value.get(11..16).unwrap_or("-").to_owned(),
    )
}

pub fn format_duration(seconds: i64) -> String {
    if seconds <= 0 {
        return "-".to_owned();
    }
    let minutes = (seconds + 59) / 60;
    if minutes < 60 {
        format!("{minutes}m")
    } else if minutes % 60 == 0 {
        format!("{}h", minutes / 60)
    } else {
        format!("{}h {}m", minutes / 60, minutes % 60)
    }
}

fn start_of_utc_month(unix_seconds: i64) -> i64 {
    let Some(datetime) = Utc.timestamp_opt(unix_seconds, 0).single() else {
        return unix_seconds;
    };
    Utc.with_ymd_and_hms(datetime.year(), datetime.month(), 1, 0, 0, 0)
        .single()
        .map(|start| start.timestamp())
        .unwrap_or(unix_seconds)
}
