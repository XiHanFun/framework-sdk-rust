//! 截止时间的两种线上写法：`InvocationMetadata.deadline` 的文本（文法在 RFC-0003 规则 8，与 HTTP 载体的 `xihan-deadline` 头共用；
//! 写法与取较早者在 `invocation.md` §2，向量 `plugin/deadlines.json`），与 gRPC 的 `grpc-timeout` 头；以及信封的 `time`
//! （`invocation.md` §4，读法与截止时间同一条文法，写法是 SDK 共用的 `Z` 结尾、去掉小数末尾 0 的 RFC 3339，向量 `plugin/envelope-times.json`）。

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// 一天的秒数。
const DAY: i64 = 86_400;
/// 一秒的纳秒数。
const NANOS: u32 = 1_000_000_000;
/// `deadline` 允许的最早时刻 `0001-01-01T00:00:00Z`，Unix 纪元起的秒数。
const MIN_SECONDS: i64 = -62_135_596_800;
/// `deadline` 允许的最晚时刻 `9999-12-31T23:59:59Z`（不计小数），Unix 纪元起的秒数。
const MAX_SECONDS: i64 = 253_402_300_799;
/// 把纳秒截到本平台 [`SystemTime`] 的精度：Windows 是 100 纳秒（`FILETIME`），不足 100 纳秒的部分截掉、不进位。
#[cfg(windows)]
const fn truncate_to_system_time(nanos: u32) -> u32 {
    nanos - nanos % 100
}

/// 把纳秒截到本平台 [`SystemTime`] 的精度：Windows 以外的平台是 1 纳秒，原样返回。
#[cfg(not(windows))]
const fn truncate_to_system_time(nanos: u32) -> u32 {
    nanos
}

/// 一个 UTC 时刻：Unix 纪元起的秒数与纳秒（`0`–`999_999_999`）。
///
/// 读写 `deadline` 用它而不直接用 [`SystemTime`]：规范的范围从 0001 年起，Windows 的 `SystemTime` 只能表示 1601 年之后的时刻。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct UtcTime {
    seconds: i64,
    nanos: u32,
}

impl UtcTime {
    /// 换成本平台的 [`SystemTime`]：精度比纳秒粗的平台（Windows 是 100 纳秒）把多出的小数截断（`invocation.md` §2）；
    /// 本平台表示不了的更早时刻一定已经过去，取 Unix 纪元，作为截止时间同样是已过期。
    pub(crate) fn to_system_time(self) -> SystemTime {
        if self.seconds >= 0 {
            // 读得出的时刻不晚于 9999 年，各平台都表示得了
            return UNIX_EPOCH.checked_add(Duration::new(self.seconds.unsigned_abs(), self.nanos)).unwrap_or(UNIX_EPOCH);
        }
        let before = Duration::from_secs(self.seconds.unsigned_abs()) - Duration::from_nanos(u64::from(self.nanos));
        UNIX_EPOCH.checked_sub(before).unwrap_or(UNIX_EPOCH)
    }

    /// 换成本平台的 [`SystemTime`]，时刻不变：精度比纳秒粗的平台（Windows 是 100 纳秒）把多出的小数截断、不进位
    /// （`invocation.md` §4）；本平台表示不了（Windows 上 1601 年之前）为 `None`。
    pub(crate) fn to_exact_system_time(self) -> Option<SystemTime> {
        let nanos = truncate_to_system_time(self.nanos);
        // 纪元之前的时刻先退整秒、再加截断过的小数：直接减去「整秒减小数」的时长时，平台按自己的精度截断那段时长，结果朝纪元进位
        let whole = if self.seconds >= 0 {
            UNIX_EPOCH.checked_add(Duration::from_secs(self.seconds.unsigned_abs()))
        } else {
            UNIX_EPOCH.checked_sub(Duration::from_secs(self.seconds.unsigned_abs()))
        };
        whole?.checked_add(Duration::from_nanos(u64::from(nanos)))
    }

    /// 从 [`SystemTime`] 换过来。
    pub(crate) fn from_system_time(time: SystemTime) -> Self {
        match time.duration_since(UNIX_EPOCH) {
            Ok(after) => Self { seconds: i64::try_from(after.as_secs()).unwrap_or(i64::MAX), nanos: after.subsec_nanos() },
            Err(error) => {
                let before = error.duration();
                let seconds = i64::try_from(before.as_secs()).unwrap_or(i64::MAX);
                match before.subsec_nanos() {
                    0 => Self { seconds: -seconds, nanos: 0 },
                    nanos => Self { seconds: -seconds - 1, nanos: NANOS - nanos },
                }
            }
        }
    }
}

/// 按截止时间文本的文法（RFC-0003 规则 8）严格读 `deadline`，返回 UTC 时刻（纳秒精度）：
/// `^[0-9]{4}-[0-9]{2}-[0-9]{2}[Tt][0-9]{2}:[0-9]{2}:[0-9]{2}(\.[0-9]{1,9})?([Zz]|[+-][0-9]{2}:[0-9]{2})\z`，年 `0001`–`9999`、
/// 月日按公历（含闰年）、时 `00`–`23`、分与秒 `00`–`59`（不认闰秒）、偏移的时 `00`–`23` 与分 `00`–`59`，换算成 UTC 之后（不计小数）
/// 在 `0001-01-01T00:00:00` 到 `9999-12-31T23:59:59` 之间。读不出（含空串）为 `None`，调用方当作没有截止时间。
pub(crate) fn parse_deadline(text: &str) -> Option<UtcTime> {
    let bytes = text.as_bytes();
    // 固定部分 yyyy-MM-ddTHH:mm:ss 是 19 个字节，之后至少还有一个字节的偏移
    if bytes.len() < 20 || bytes[4] != b'-' || bytes[7] != b'-' || !matches!(bytes[10], b'T' | b't') || bytes[13] != b':' || bytes[16] != b':' {
        return None;
    }
    let (year, month, day) = (number(&bytes[0..4])?, number(&bytes[5..7])?, number(&bytes[8..10])?);
    let (hour, minute, second) = (number(&bytes[11..13])?, number(&bytes[14..16])?, number(&bytes[17..19])?);
    // 每一段先按公历核对：换算公式会把越界的日期进位成别的日子
    if year == 0 || !(1..=12).contains(&month) || day == 0 || day > days_in_month(year, month) || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let mut rest = &bytes[19..];
    let mut nanos = 0u32;
    if let Some(fraction) = rest.strip_prefix(b".") {
        let digits = fraction.iter().take_while(|byte| byte.is_ascii_digit()).count();
        if !(1..=9).contains(&digits) {
            return None;
        }
        // 不足九位的小数按右补零换算成纳秒
        for (index, digit) in fraction[..digits].iter().enumerate() {
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
    // 范围按换算成 UTC 之后、不计小数的时刻判定，所以 9999-12-31T23:59:59.999999999Z 仍然合法
    (MIN_SECONDS..=MAX_SECONDS).contains(&seconds).then_some(UtcTime { seconds, nanos })
}

/// 按 .NET 的 `O` 格式写出截止时间（`invocation.md` §2）：UTC、七位小数、`+00:00` 偏移，如 `2026-11-18T03:25:00.1234567+00:00`；
/// 比 100 纳秒更细的部分截断、不进位。规范范围之外的时刻写成最近的端点，写出的文本总能被读回。
pub(crate) fn format_deadline(time: UtcTime) -> String {
    let time = time.clamp(UtcTime { seconds: MIN_SECONDS, nanos: 0 }, UtcTime { seconds: MAX_SECONDS, nanos: NANOS - 1 });
    let (days, of_day) = (time.seconds.div_euclid(DAY), time.seconds.rem_euclid(DAY));
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:07}+00:00", of_day / 3600, of_day % 3600 / 60, of_day % 60, time.nanos / 100)
}

/// 写出信封的 `time`（`invocation.md` §4）：UTC、以 `Z` 结尾，秒的小数部分去掉末尾的 0、全为 0 时不写小数点，如
/// `2026-11-25T03:25:00.5Z`；各语言的 SDK 对同一时刻写出同一份文本（精度以内）。
///
/// 不核对范围：年份不在 `0001`–`9999` 之间的时刻照样写出（`10000-…`、`0000-…`），由宿主逐项核对时拒绝那一项，
/// 不改变它与其余核对的先后（ADR-0293）。
pub(crate) fn format_envelope_time(time: UtcTime) -> String {
    let (days, of_day) = (time.seconds.div_euclid(DAY), time.seconds.rem_euclid(DAY));
    let (year, month, day) = civil_from_days(days);
    let mut text = format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}", of_day / 3600, of_day % 3600 / 60, of_day % 60);
    if time.nanos != 0 {
        let fraction = format!("{:09}", time.nanos);
        text.push('.');
        text.push_str(fraction.trim_end_matches('0'));
    }
    text.push('Z');
    text
}

/// 生效的截止时间（`invocation.md` §2）：`metadata` 按 [`parse_deadline`] 读、读不出当作没有，与 gRPC 调用自身的截止时间取较早的一个；
/// 两者都没有为 `None`。
pub(crate) fn effective_deadline(metadata: &str, grpc: Option<SystemTime>) -> Option<SystemTime> {
    let parsed = parse_deadline(metadata).map(UtcTime::to_system_time);
    match (parsed, grpc) {
        (Some(parsed), Some(grpc)) => Some(parsed.min(grpc)),
        (parsed, grpc) => parsed.or(grpc),
    }
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

/// 定长的 ASCII 十进制数字（全角数字等其他数字字符不算）。
fn number(digits: &[u8]) -> Option<i64> {
    digits.iter().try_fold(0i64, |value, digit| digit.is_ascii_digit().then(|| value * 10 + i64::from(digit - b'0')))
}

/// 闰年：四年一闰、百年不闰、四百年再闰。
fn is_leap(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

/// 公历某年某月的天数。
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
    use serde_json::Value;

    use super::*;

    /// 读规范子模块里的向量 `plugin/deadlines.json`。
    fn vectors() -> Value {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/spec/conformance/vectors/plugin/deadlines.json");
        let text = std::fs::read_to_string(path).unwrap_or_else(|error| panic!("cannot read {path}: {error}; run `git submodule update --init`"));
        serde_json::from_str(&text).unwrap()
    }

    /// 向量 `utc` 的写法：九位小数加 `Z`。与 [`parse_deadline`] 走不同的换算（`civil_from_days`），用来独立核对读出的时刻。
    fn utc_text(time: UtcTime) -> String {
        let (days, of_day) = (time.seconds.div_euclid(DAY), time.seconds.rem_euclid(DAY));
        let (year, month, day) = civil_from_days(days);
        format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:09}Z", of_day / 3600, of_day % 3600 / 60, of_day % 60, time.nanos)
    }

    /// 向量里的 `utc` 换成本平台的时刻：精度更细的小数按平台截断后比较（`invocation.md` §2）。
    fn system_time(utc: &str) -> SystemTime {
        parse_deadline(utc).unwrap_or_else(|| panic!("{utc}")).to_system_time()
    }

    #[test]
    fn deadlines_follow_the_vectors() {
        let vectors = vectors();
        for case in vectors["valid"].as_array().unwrap() {
            let (text, utc) = (case["text"].as_str().unwrap(), case["utc"].as_str().unwrap());
            let parsed = parse_deadline(text).unwrap_or_else(|| panic!("{text} must be read"));
            assert_eq!(utc_text(parsed), utc, "{text}");
        }
        for text in vectors["invalid"].as_array().unwrap() {
            let text = text.as_str().unwrap();
            assert_eq!(parse_deadline(text), None, "{text:?}");
        }
        for case in vectors["effective"].as_array().unwrap() {
            let metadata = case["metadata"].as_str().unwrap();
            let grpc = case["grpc"].as_str().map(system_time);
            let expected = case["expected"].as_str().map(system_time);
            assert_eq!(effective_deadline(metadata, grpc), expected, "{case}");
        }
        for case in vectors["written"].as_array().unwrap() {
            let (utc, text) = (case["utc"].as_str().unwrap(), case["text"].as_str().unwrap());
            let time = parse_deadline(utc).unwrap();
            assert_eq!(format_deadline(time), text, "{utc}");
            // 本平台表示得了的时刻经 SystemTime 往返写出的文本相同：回调写的就是上下文里的 SystemTime
            if time.seconds >= 0 {
                assert_eq!(format_deadline(UtcTime::from_system_time(time.to_system_time())), text, "{utc}");
            }
        }
    }

    #[test]
    fn system_times_convert_both_ways() {
        let before = UNIX_EPOCH - Duration::new(1, 500_000_000);
        assert_eq!(UtcTime::from_system_time(before), UtcTime { seconds: -2, nanos: 500_000_000 });
        assert_eq!(UtcTime::from_system_time(before).to_system_time(), before);
        assert_eq!(format_deadline(UtcTime::from_system_time(UNIX_EPOCH + Duration::from_nanos(1_234_567_891))), "1970-01-01T00:00:01.2345678+00:00");
        // 规范范围之外的时刻写成最近的端点
        assert_eq!(format_deadline(UtcTime { seconds: MAX_SECONDS + DAY, nanos: 0 }), "9999-12-31T23:59:59.9999999+00:00");
        assert_eq!(format_deadline(UtcTime { seconds: MIN_SECONDS - DAY, nanos: 0 }), "0001-01-01T00:00:00.0000000+00:00");
        // 表示不了的更早时刻作为截止时间同样已经过去
        assert!(parse_deadline("0001-01-01T00:00:00Z").unwrap().to_system_time() <= UNIX_EPOCH);
    }

    /// 作者给的时刻与 SDK 写出的 `time`：换算成 UTC、以 `Z` 结尾，秒的小数部分去掉末尾的 0、全为 0 时不写小数点；
    /// 各语言的 SDK 对同一时刻写出同一份文本（精度以内）。这里按纳秒写，经 [`SystemTime`] 时再按本平台的精度截断。
    const WRITTEN: [(&str, &str); 13] = [
        ("2026-11-25T03:25:00Z", "2026-11-25T03:25:00Z"),
        ("2026-11-25T03:25:00.5Z", "2026-11-25T03:25:00.5Z"),
        ("2026-11-25T03:25:00.120Z", "2026-11-25T03:25:00.12Z"),
        ("2026-11-25T03:25:00.001Z", "2026-11-25T03:25:00.001Z"),
        ("2026-11-25T03:25:00.123456Z", "2026-11-25T03:25:00.123456Z"),
        ("2026-11-25T03:25:00.123456789Z", "2026-11-25T03:25:00.123456789Z"),
        ("2026-11-25T03:25:00.0000001Z", "2026-11-25T03:25:00.0000001Z"),
        ("2026-11-25T11:25:00.25+08:00", "2026-11-25T03:25:00.25Z"),
        ("2026-11-24T22:25:00-05:00", "2026-11-25T03:25:00Z"),
        ("2026-11-25T00:00:00+14:00", "2026-11-24T10:00:00Z"),
        ("0001-01-01T00:00:00Z", "0001-01-01T00:00:00Z"),
        ("9999-12-31T23:59:59.999Z", "9999-12-31T23:59:59.999Z"),
        ("9999-12-31T23:59:59.999999999Z", "9999-12-31T23:59:59.999999999Z"),
    ];

    /// 读规范子模块里的向量 `plugin/envelope-times.json`。
    fn envelope_times() -> Value {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/spec/conformance/vectors/plugin/envelope-times.json");
        let text = std::fs::read_to_string(path).unwrap_or_else(|error| panic!("cannot read {path}: {error}; run `git submodule update --init`"));
        serde_json::from_str(&text).unwrap()
    }

    /// 时刻经本平台的 [`SystemTime`] 往返：小数按平台的精度截断，表示不了的为 `None`。
    fn through_system_time(time: UtcTime) -> Option<UtcTime> {
        time.to_exact_system_time().map(UtcTime::from_system_time)
    }

    /// 截到本平台的精度。
    fn truncated(time: UtcTime) -> UtcTime {
        UtcTime { seconds: time.seconds, nanos: truncate_to_system_time(time.nanos) }
    }

    #[test]
    fn envelope_times_are_written_in_utc_without_trailing_zeros() {
        for (instant, text) in WRITTEN {
            let time = parse_deadline(instant).unwrap_or_else(|| panic!("{instant}"));
            assert_eq!(format_envelope_time(time), text, "{instant}");
            // 写出的文本合文法、读回同一时刻
            assert_eq!(parse_deadline(text), Some(time), "{text}");
            // 经 SystemTime 写出时按本平台的精度截断：Windows 上最多七位
            if let Some(system) = through_system_time(time) {
                assert_eq!(system, truncated(time), "{instant}");
                assert_eq!(parse_deadline(&format_envelope_time(system)), Some(system), "{instant}");
            }
        }
        // SDK 不在本地核对：年份越界的时刻照样写出，由宿主拒绝那一项
        assert_eq!(format_envelope_time(UtcTime { seconds: MAX_SECONDS + 1, nanos: 0 }), "10000-01-01T00:00:00Z");
        assert_eq!(format_envelope_time(UtcTime { seconds: MIN_SECONDS - DAY, nanos: 500_000_000 }), "0000-12-31T00:00:00.5Z");
        assert_eq!(format_envelope_time(UtcTime::from_system_time(UNIX_EPOCH - Duration::new(1, 500_000_000))), "1969-12-31T23:59:58.5Z");
    }

    #[test]
    fn envelope_times_follow_the_vectors() {
        let vectors = envelope_times();
        for case in vectors["valid"].as_array().unwrap() {
            let text = case["text"].as_str().unwrap();
            let Some(utc) = case["utc"].as_str() else {
                assert_eq!(text, "", "only the empty text is no time");
                continue;
            };
            let parsed = parse_deadline(text).unwrap_or_else(|| panic!("{text} must be read"));
            assert_eq!(utc_text(parsed), utc, "{text}");
            assert_eq!(parse_deadline(&format_envelope_time(parsed)), Some(parsed), "{text}");
        }
        for text in vectors["invalid"].as_array().unwrap() {
            let text = text.as_str().unwrap();
            assert_eq!(parse_deadline(text), None, "{text:?}");
        }
        for case in vectors["written"].as_array().unwrap() {
            let text = case["text"].as_str().unwrap();
            match case["instant"].as_str() {
                Some(instant) => assert_eq!(parse_deadline(text), parse_deadline(instant), "{text}"),
                None => assert_eq!(text, "", "no time is written as an empty string"),
            }
        }
    }

    #[test]
    fn envelope_times_convert_to_system_time_exactly() {
        // 纪元之前不足一个精度单位的小数截断，时刻往前、不朝纪元进位
        let before = parse_deadline("1969-12-31T23:59:59.00000005Z").unwrap();
        assert_eq!(through_system_time(before), Some(truncated(before)));
        assert_eq!(before.to_exact_system_time(), Some(UNIX_EPOCH - Duration::from_secs(1) + Duration::from_nanos(u64::from(truncated(before).nanos))));
        let latest = parse_deadline("9999-12-31T23:59:59.999999999Z").unwrap();
        assert_eq!(through_system_time(latest), Some(truncated(latest)));
        // 0001 年：Windows 的 SystemTime 从 1601 年起，表示不了；其余平台照常
        let earliest = parse_deadline("0001-01-01T00:00:00Z").unwrap();
        assert_eq!(through_system_time(earliest), if cfg!(windows) { None } else { Some(earliest) });
        let windows_epoch = parse_deadline("1601-01-01T00:00:00Z").unwrap();
        assert_eq!(through_system_time(windows_epoch), Some(windows_epoch));
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
