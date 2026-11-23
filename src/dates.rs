//! Port of swingmusic/utils/dates.py.
//! diff_for_humans thresholds pinned to pendulum en vectors collected from live
//! python (2026-11-17) — see tests. "ago"/"in" wrapping = pendulum custom locale.

/// seconds_to_time_string(): "1 hr, 2 mins" / "1 hr" / "5 mins" / "7 sec".
pub fn seconds_to_time_string(seconds: i64) -> String {
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let remaining_seconds = seconds % 60;

    if hours > 0 {
        if minutes > 0 {
            return format!(
                "{} hr{}, {} min{}",
                hours,
                if hours > 1 { "s" } else { "" },
                minutes,
                if minutes > 1 { "s" } else { "" }
            );
        }
        return format!("{} hr{}", hours, if hours > 1 { "s" } else { "" });
    }

    if minutes > 0 {
        return format!("{} min{}", minutes, if minutes > 1 { "s" } else { "" });
    }

    format!("{} sec", remaining_seconds)
}

/// pendulum diff_for_humans phrase (past direction), en locale.
/// ponytail: threshold model pinned to verified vectors; upgrade to calendar
/// precise_diff only if a boundary mismatch shows up in parity tests.
fn phrase(seconds: i64) -> String {
    if seconds < 45 {
        "a few seconds".to_string()
    } else if seconds < 60 {
        format!("{} seconds", seconds)
    } else if seconds < 120 {
        "1 minute".to_string()
    } else if seconds < 3600 {
        format!("{} minutes", seconds / 60)
    } else if seconds < 7200 {
        "1 hour".to_string()
    } else if seconds < 86400 {
        format!("{} hours", seconds / 3600)
    } else if seconds < 172800 {
        "1 day".to_string()
    } else if seconds < 604800 {
        format!("{} days", seconds / 86400)
    } else if seconds < 1209600 {
        "1 week".to_string()
    } else if seconds < 2419200 {
        format!("{} weeks", seconds / 604800)
    } else if seconds < 2592000 {
        "4 weeks".to_string()
    } else if seconds < 5184000 {
        "1 month".to_string()
    } else if seconds < 31536000 {
        format!("{} months", seconds / 2592000)
    } else if seconds < 63072000 {
        "1 year".to_string()
    } else {
        format!("{} years", seconds / 31536000)
    }
}

/// timestamp_to_time_passed(): "2 minutes ago" / "in 2 minutes".
pub fn timestamp_to_time_passed(timestamp: i64, now: i64) -> String {
    let diff = now - timestamp;
    if diff >= 0 {
        format!("{} ago", phrase(diff))
    } else {
        format!("in {}", phrase(-diff))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_string_vectors() {
        assert_eq!(seconds_to_time_string(0), "0 sec");
        assert_eq!(seconds_to_time_string(7), "7 sec");
        assert_eq!(seconds_to_time_string(60), "1 min");
        assert_eq!(seconds_to_time_string(120), "2 mins");
        assert_eq!(seconds_to_time_string(3600), "1 hr");
        assert_eq!(seconds_to_time_string(3720), "1 hr, 2 mins");
        assert_eq!(seconds_to_time_string(7260), "2 hrs, 1 min");
        assert_eq!(seconds_to_time_string(7320), "2 hrs, 2 mins");
    }

    #[test]
    fn diff_for_humans_vectors_live() {
        // /tmp/vec5.txt — generated from live pendulum en
        let past = |s: i64| timestamp_to_time_passed(1_000_000 - s, 1_000_000);
        assert_eq!(past(1), "a few seconds ago");
        assert_eq!(past(30), "a few seconds ago");
        assert_eq!(past(44), "a few seconds ago");
        assert_eq!(past(45), "45 seconds ago");
        assert_eq!(past(59), "59 seconds ago");
        assert_eq!(past(60), "1 minute ago");
        assert_eq!(past(89), "1 minute ago");
        assert_eq!(past(119), "1 minute ago");
        assert_eq!(past(120), "2 minutes ago");
        assert_eq!(past(3540), "59 minutes ago");
        assert_eq!(past(3600), "1 hour ago");
        assert_eq!(past(5400), "1 hour ago");
        assert_eq!(past(7200), "2 hours ago");
        assert_eq!(past(86399), "23 hours ago");
        assert_eq!(past(86400), "1 day ago");
        assert_eq!(past(129600), "1 day ago");
        assert_eq!(past(172800), "2 days ago");
        assert_eq!(past(604800), "1 week ago");
        assert_eq!(past(1209600), "2 weeks ago");
        assert_eq!(past(2419200), "4 weeks ago");
        assert_eq!(past(2592000), "1 month ago");
        assert_eq!(past(3888000), "1 month ago");
        assert_eq!(past(5184000), "2 months ago");
        assert_eq!(past(7776000), "3 months ago");
        assert_eq!(past(15552000), "6 months ago");
        assert_eq!(past(31536000), "1 year ago");
        assert_eq!(past(34560000), "1 year ago");
        assert_eq!(past(63072000), "2 years ago");
        assert_eq!(past(94608000), "3 years ago");
    }
}
