use chrono::{DateTime, Datelike, Local, NaiveDate, SecondsFormat, TimeZone, Utc};
use std::{
    process,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

pub const SECONDS_PER_DAY: i64 = 24 * 60 * 60;
const TOGGL_MAX_BACKFILL_DAYS: u32 = 85;
static RUN_ID_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub fn current_rfc3339_utc() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

pub fn unique_run_id(mode: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let sequence = RUN_ID_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("{mode}-{nanos}-{}-{sequence}", process::id())
}

pub fn initial_backfill_since(now_unix_seconds: i64, initial_backfill_days: u32) -> i64 {
    let bounded_days = initial_backfill_days.min(TOGGL_MAX_BACKFILL_DAYS);
    now_unix_seconds.saturating_sub(i64::from(bounded_days) * SECONDS_PER_DAY)
}

pub fn current_month_start_since(now_unix_seconds: i64) -> i64 {
    Utc.timestamp_opt(now_unix_seconds, 0)
        .single()
        .and_then(|datetime| {
            NaiveDate::from_ymd_opt(datetime.year(), datetime.month(), 1)
                .and_then(|date| date.and_hms_opt(0, 0, 0))
        })
        .map(|datetime| Utc.from_utc_datetime(&datetime).timestamp())
        .unwrap_or(now_unix_seconds)
}

pub fn month_start_since(month: &str) -> Option<i64> {
    let (month, year) = month.trim().split_once('.')?;
    let month = month.parse::<u32>().ok()?;
    let year = year.parse::<i32>().ok()?;
    let date = NaiveDate::from_ymd_opt(year, month, 1)?;
    date.and_hms_opt(0, 0, 0)
        .map(|datetime| Utc.from_utc_datetime(&datetime).timestamp())
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

#[cfg(test)]
mod tests {
    use super::{
        current_month_start_since, initial_backfill_since, unique_run_id, SECONDS_PER_DAY,
    };
    use std::collections::HashSet;

    #[test]
    fn initial_backfill_since_can_cross_month_boundary() {
        let june_first_2026 = 1_780_272_000;

        assert_eq!(
            initial_backfill_since(june_first_2026, 7),
            june_first_2026 - 7 * SECONDS_PER_DAY
        );
    }

    #[test]
    fn current_month_start_since_uses_utc_month() {
        let june_first_2026 = 1_780_272_000;
        let june_second_2026 = june_first_2026 + SECONDS_PER_DAY;

        assert_eq!(current_month_start_since(june_second_2026), june_first_2026);
    }

    #[test]
    fn unique_run_id_does_not_collide_under_rapid_generation() {
        let mut ids = HashSet::new();

        for _ in 0..1_000 {
            let id = unique_run_id("sync");
            assert!(id.starts_with("sync-"));
            assert!(ids.insert(id));
        }
    }
}
