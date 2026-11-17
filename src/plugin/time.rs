//! 截止时间的两种线上写法：`InvocationMetadata.deadline` 的 RFC 3339 文本（`invocation.md` §2），与 gRPC 的 `grpc-timeout` 头。

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// 一天的秒数。
const DAY: i64 = 86_400;

/// 解析 RFC 3339 的时间（§5.6 `date-time`）：`YYYY-MM-DDTHH:MM:SS[.小数](Z|±HH:MM)`，`T` 与 `Z` 不区分大小写；不合法时为 `None`。
pub(crate) fn parse_rfc3339(text: &str) -> Option<SystemTime> {
    let bytes = text.as_bytes();
    if bytes.len() < 20 || bytes[4] != b'-' || bytes[7] != b'-' || !matches!(bytes[10], b'T' | b't') || bytes[13] != b':' || bytes[16] != b':' {
        return None;
    }
    let (year, month, day) = (number(&bytes[0..4])?, number(&bytes[5..7])?, number(&bytes[8..10])?);
    let (hour, minute, second) = (number(&bytes[11..13])?, number(&bytes[14..16])?, number(&bytes[17..19])?);
    // 闰秒 60 按下一秒处理：截止时间只需要不晚于上游
    if !(1..=12).contains(&month) || day == 0 || day > days_in_month(year, month) || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let mut rest = &bytes[19..];
    let mut nanos = 0u32;
    if let Some(fraction) = rest.strip_prefix(b".") {
        let digits = fraction.iter().take_while(|byte| byte.is_ascii_digit()).count();
        if digits == 0 {
            return None;
        }
        // 只取到纳秒，更细的位数截掉
        for (index, digit) in fraction[..digits].iter().take(9).enumerate() {
            nanos += u32::from(digit - b'0') * 10u32.pow(8 - index as u32);
        }
        rest = &fraction[digits..];
    }
    let offset = match rest {
        [b'Z' | b'z'] => 0,
        [sign @ (b'+' | b'-'), h1, h2, b':', m1, m2] => {
            let (hours, minutes) = (number(&[*h1, *h2])?, number(&[*m1, *m2])?);
            if hours > 23 || minutes > 59 {
                return None;
            }
            let offset = hours * 3600 + minutes * 60;
            if *sign == b'-' { -offset } else { offset }
        }
        _ => return None,
    };
    let seconds = days_from_civil(year, month, day) * DAY + hour * 3600 + minute * 60 + second - offset;
    let nanos = Duration::from_nanos(u64::from(nanos));
    if seconds >= 0 {
        UNIX_EPOCH.checked_add(Duration::from_secs(seconds.unsigned_abs()))?.checked_add(nanos)
    } else {
        UNIX_EPOCH.checked_sub(Duration::from_secs(seconds.unsigned_abs()))?.checked_add(nanos)
    }
}

/// 写成 RFC 3339 的 UTC 时间，带毫秒：`2026-11-17T08:30:00.125Z`。毫秒以下截掉，写出的截止时间不会晚于原来的（RFC-0003 规则 2）；
/// 1970 年之前的时间写成 1970-01-01T00:00:00.000Z。
pub(crate) fn format_rfc3339(time: SystemTime) -> String {
    let since = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    let seconds = i64::try_from(since.as_secs()).unwrap_or(i64::MAX / 2);
    let (days, of_day) = (seconds.div_euclid(DAY), seconds.rem_euclid(DAY));
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z", of_day / 3600, of_day % 3600 / 60, of_day % 60, since.subsec_millis())
}

/// 解析 gRPC 的 `grpc-timeout` 头：1 到 8 位十进制数字加单位 `H`、`M`、`S`、`m`、`u`、`n`；不合法时为 `None`。
pub(crate) fn parse_grpc_timeout(value: &str) -> Option<Duration> {
    // 只按 ASCII 切分：最后一个字节是单位
    if !value.is_ascii() {
        return None;
    }
    let (digits, unit) = value.split_at(value.len().checked_sub(1)?);
    if digits.is_empty() || digits.len() > 8 || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let amount: u64 = digits.parse().ok()?;
    match unit {
        "H" => Some(Duration::from_secs(amount * 3600)),
        "M" => Some(Duration::from_secs(amount * 60)),
        "S" => Some(Duration::from_secs(amount)),
        "m" => Some(Duration::from_millis(amount)),
        "u" => Some(Duration::from_micros(amount)),
        "n" => Some(Duration::from_nanos(amount)),
        _ => None,
    }
}

/// 定长的十进制数字。
fn number(digits: &[u8]) -> Option<i64> {
    digits.iter().try_fold(0i64, |value, digit| digit.is_ascii_digit().then(|| value * 10 + i64::from(digit - b'0')))
}

fn is_leap(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if is_leap(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// 公历日期到 1970-01-01 起的天数（Howard Hinnant 的 days_from_civil）。
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// 1970-01-01 起的天数到公历日期（Howard Hinnant 的 civil_from_days）。
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let days = days + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 { month_index + 3 } else { month_index - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(seconds: u64, millis: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(seconds) + Duration::from_millis(millis)
    }

    #[test]
    fn rfc3339_round_trips() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(UNIX_EPOCH));
        assert_eq!(parse_rfc3339("2026-11-17T08:30:00.125Z"), Some(at(1_794_904_200, 125)));
        assert_eq!(format_rfc3339(at(1_794_904_200, 125)), "2026-11-17T08:30:00.125Z");
        assert_eq!(parse_rfc3339("2026-11-17t16:30:00.125+08:00"), Some(at(1_794_904_200, 125)));
        assert_eq!(parse_rfc3339("2026-11-17T03:30:00.1250001-05:00"), Some(at(1_794_904_200, 125) + Duration::from_nanos(100)));
        assert_eq!(parse_rfc3339("2024-02-29T00:00:00z"), Some(at(1_709_164_800, 0)));
        assert_eq!(format_rfc3339(at(1_709_164_800, 999) + Duration::from_micros(999)), "2024-02-29T00:00:00.999Z");
        assert_eq!(parse_rfc3339("1969-12-31T23:59:59Z"), UNIX_EPOCH.checked_sub(Duration::from_secs(1)));
        for text in [
            "",
            "2026-11-17",
            "2026-11-17T08:30:00",
            "2026-11-17 08:30:00Z",
            "2026-13-01T00:00:00Z",
            "2023-02-29T00:00:00Z",
            "2026-11-17T24:00:00Z",
            "2026-11-17T08:30:00.Z",
            "2026-11-17T08:30:00+0800",
            "2026-11-17T08:30:00+24:00",
            "2026-11-17T08:30:00Zx",
            "２026-11-17T08:30:00Z",
        ] {
            assert_eq!(parse_rfc3339(text), None, "{text}");
        }
    }

    #[test]
    fn grpc_timeouts_follow_the_protocol() {
        assert_eq!(parse_grpc_timeout("15000m"), Some(Duration::from_secs(15)));
        assert_eq!(parse_grpc_timeout("2S"), Some(Duration::from_secs(2)));
        assert_eq!(parse_grpc_timeout("1H"), Some(Duration::from_secs(3600)));
        assert_eq!(parse_grpc_timeout("7n"), Some(Duration::from_nanos(7)));
        for value in ["", "m", "123456789m", "1x", "-1S", "1.5S"] {
            assert_eq!(parse_grpc_timeout(value), None, "{value}");
        }
    }
}
