//! Local times and dates for chat rows and message rows (plan section 8:
//! `jiff`, which grammers already depends on).

use gpui::SharedString;
use jiff::civil::Date;
use jiff::tz::TimeZone;
use jiff::{Timestamp, Zoned};

/// The system time zone. jiff caches the lookup.
pub(crate) fn local_tz() -> TimeZone {
    TimeZone::system()
}

fn zoned(unix: i64, tz: &TimeZone) -> Option<Zoned> {
    Some(Timestamp::from_second(unix).ok()?.to_zoned(tz.clone()))
}

/// The local calendar day of `unix`, for day separators.
pub(crate) fn day(unix: i64, tz: &TimeZone) -> Option<Date> {
    zoned(unix, tz).map(|z| z.date())
}

/// `14:05`.
pub(crate) fn time_of_day(unix: i64, tz: &TimeZone) -> SharedString {
    zoned(unix, tz).map_or_else(SharedString::default, |z| {
        z.strftime("%H:%M").to_string().into()
    })
}

/// A day separator: "Today", "Yesterday", or `Mon, 3 Nov 2026`.
pub(crate) fn day_label(day: Date, today: Date) -> SharedString {
    if day == today {
        return "Today".into();
    }
    if today.yesterday().is_ok_and(|y| y == day) {
        return "Yesterday".into();
    }
    day.strftime("%a, %-d %b %Y").to_string().into()
}

/// The chat list's time column: the time today, the weekday within a week,
/// else a short date. Empty for chats without a dated message.
pub(crate) fn list_time(unix: i64, now: i64, tz: &TimeZone) -> SharedString {
    if unix <= 0 {
        return SharedString::default();
    }
    let (Some(then), Some(today)) = (zoned(unix, tz), day(now, tz)) else {
        return SharedString::default();
    };
    let then_day = then.date();
    if then_day == today {
        return then.strftime("%H:%M").to_string().into();
    }
    let within_week = then_day
        .until(today)
        .is_ok_and(|span| span.get_days() < 7 && span.get_days() > 0);
    if within_week {
        return then.strftime("%a").to_string().into();
    }
    then.strftime("%d.%m.%y").to_string().into()
}

/// Seconds since the epoch, now.
pub(crate) fn now() -> i64 {
    Timestamp::now().as_second()
}

#[cfg(test)]
mod tests {
    use super::*;

    const UTC: TimeZone = TimeZone::UTC;
    /// 2026-11-16 14:05:00 UTC, a Monday.
    const NOW: i64 = 1_794_837_900;

    #[test]
    fn times_and_days() {
        assert_eq!(time_of_day(NOW, &UTC), "14:05");
        let today = day(NOW, &UTC).unwrap();
        assert_eq!(day_label(today, today), "Today");
        assert_eq!(
            day_label(day(NOW - 86_400, &UTC).unwrap(), today),
            "Yesterday"
        );
        assert_eq!(
            day_label(day(NOW - 3 * 86_400, &UTC).unwrap(), today),
            "Fri, 13 Nov 2026"
        );
    }

    #[test]
    fn list_times() {
        assert_eq!(list_time(NOW - 60, NOW, &UTC), "14:04");
        assert_eq!(list_time(NOW - 2 * 86_400, NOW, &UTC), "Sat");
        assert_eq!(list_time(NOW - 30 * 86_400, NOW, &UTC), "17.10.26");
        assert_eq!(list_time(0, NOW, &UTC), "");
        assert_eq!(list_time(i64::MAX, NOW, &UTC), "", "out of range");
    }
}
