//! The CSV dialect and timestamp forms of the rules, byte by byte.

pub const DAY: i64 = 86_400_000;

/// Splits one line (no trailing '\n') into exactly N fields, unquoting each.
/// Returns None for a malformed line: wrong field count, or a quote anywhere
/// except around a whole field. An empty line has one empty field, so it is
/// malformed for every N > 1.
#[inline]
pub fn split<const N: usize>(line: &[u8]) -> Option<[&[u8]; N]> {
    let mut out: [&[u8]; N] = [&[]; N];
    let mut n = 0usize;
    let mut start = 0usize;
    let mut quotes = 0u32;
    let len = line.len();
    let mut i = 0usize;
    loop {
        let end = i == len;
        let b = if end { b',' } else { line[i] };
        if b == b',' {
            if n == N {
                return None;
            }
            let f = &line[start..i];
            out[n] = if quotes == 0 {
                f
            } else if quotes == 2 && f.len() >= 2 && f[0] == b'"' && f[f.len() - 1] == b'"' {
                &f[1..f.len() - 1]
            } else {
                return None;
            };
            n += 1;
            quotes = 0;
            start = i + 1;
            if end {
                break;
            }
        } else if b == b'"' {
            quotes += 1;
        }
        i += 1;
    }
    if n == N {
        Some(out)
    } else {
        None
    }
}

#[inline]
fn d2(b: &[u8], i: usize) -> Option<i64> {
    let a = b[i].wrapping_sub(b'0');
    let c = b[i + 1].wrapping_sub(b'0');
    if a < 10 && c < 10 {
        Some(a as i64 * 10 + c as i64)
    } else {
        None
    }
}

pub fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => {
            if is_leap(y) {
                29
            } else {
                28
            }
        }
    }
}

/// Days since 1970-01-01 for a proleptic Gregorian date.
pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// F1 (`YYYY-MM-DDTHH:MM:SSZ`) and F2 (`...+HH:MM`), already length-checked.
#[inline]
fn parse_civil(b: &[u8]) -> Option<i64> {
    if b[4] != b'-' || b[7] != b'-' || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let y = d2(b, 0)? * 100 + d2(b, 2)?;
    let mo = d2(b, 5)?;
    let d = d2(b, 8)?;
    let h = d2(b, 11)?;
    let mi = d2(b, 14)?;
    let s = d2(b, 17)?;
    if !(1..=12).contains(&mo) || d < 1 || d > days_in_month(y, mo) || h > 23 || mi > 59 || s > 59 {
        return None;
    }
    let local = days_from_civil(y, mo, d) * DAY + h * 3_600_000 + mi * 60_000 + s * 1000;
    if b.len() == 20 {
        return if b[19] == b'Z' { Some(local) } else { None };
    }
    let sign = match b[19] {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    if b[22] != b':' {
        return None;
    }
    let oh = d2(b, 20)?;
    let om = d2(b, 23)?;
    if oh > 23 || om > 59 {
        return None;
    }
    Some(local - sign * (oh * 60 + om) * 60_000)
}

/// Any of F1, F2, F3 to epoch milliseconds; None for a bad timestamp. An F3
/// value too large for i64 is returned as None too: it is outside every window.
#[inline]
pub fn parse_ts(b: &[u8]) -> Option<i64> {
    let n = b.len();
    if (n == 20 || n == 25) && b[10] == b'T' {
        return parse_civil(b);
    }
    if n == 0 {
        return None;
    }
    let mut v: u64 = 0;
    for &c in b {
        let d = c.wrapping_sub(b'0');
        if d >= 10 {
            return None;
        }
        v = v.checked_mul(10)?.checked_add(d as u64)?;
    }
    if v > i64::MAX as u64 {
        None
    } else {
        Some(v as i64)
    }
}

/// A purchase price: ASCII digits only, value 1..=10,000,000.
#[inline]
pub fn parse_price(b: &[u8]) -> Option<u32> {
    if b.is_empty() {
        return None;
    }
    let mut v: u64 = 0;
    for &c in b {
        let d = c.wrapping_sub(b'0');
        if d >= 10 {
            return None;
        }
        v = v * 10 + d as u64;
        if v > 10_000_000 {
            // Keep scanning only to stay simple: any value over the cap is bad.
            return None;
        }
    }
    if v >= 1 {
        Some(v as u32)
    } else {
        None
    }
}

/// Civil date (y, m, d) for days since the epoch, for writing dates.
#[allow(dead_code)]
pub fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn timestamps() {
        let f1 = parse_ts(b"2026-10-03T23:58:10Z").unwrap();
        assert_eq!(parse_ts(b"2026-10-03T19:58:10-04:00"), Some(f1));
        assert_eq!(parse_ts(b"1791071890000"), Some(1791071890000));
        assert_eq!(parse_ts(b"0001791071890000"), Some(1791071890000));
        assert_eq!(parse_ts(b"2026-10-04T01:30:00+02:00"), parse_ts(b"2026-10-03T23:30:00Z"));
        for bad in [
            &b""[..], b"2026-10-03T23:58:10.000Z", b"2026-10-03t23:58:10Z", b"2026-10-03T23:58:10z",
            b"2026-10-03 23:58:10Z", b"2026-10-03T23:58:10", b"1791071890000.0", b"1.79e12", b"-5",
            b"2026-13-03T23:58:10Z", b"2026-02-30T23:58:10Z", b"2026-02-29T00:00:00Z",
            b"2026-10-03T24:00:00Z", b"2026-10-03T23:59:60Z", b"2026-10-03T23:58:10+24:00",
            b"2026-10-03T23:58:10+05:60", b"2026-10-03T23:58:10+0200", b"999999999999999999999",
        ] {
            assert_eq!(parse_ts(bad), None, "{}", String::from_utf8_lossy(bad));
        }
        assert!(parse_ts(b"2028-02-29T00:00:00Z").is_some());
        assert_eq!(parse_ts(b"2026-10-03T23:58:10-00:00"), Some(f1));
        assert_eq!(civil_from_days(days_from_civil(2028, 2, 29)), (2028, 2, 29));
    }
    #[test]
    fn csv() {
        assert_eq!(split::<2>(b"\"u4\",\"u1\""), Some([&b"u4"[..], b"u1"]));
        assert_eq!(split::<2>(b"u1,u5\""), None);
        assert_eq!(split::<2>(b""), None);
        assert_eq!(split::<2>(b"u1"), None);
        assert_eq!(split::<2>(b"u1,u2,u3"), None);
        assert_eq!(split::<2>(b"\"\",x"), Some([&b""[..], b"x"]));
        assert_eq!(split::<2>(b"\",x"), None);
        assert_eq!(split::<2>(b"a\"b\",x"), None);
        assert_eq!(parse_price(b"0250"), Some(250));
        assert_eq!(parse_price(b"0"), None);
        assert_eq!(parse_price(b"10000001"), None);
        assert_eq!(parse_price(b"10000000"), Some(10_000_000));
    }
}
