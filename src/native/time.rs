//! `std.time` — wall-clock and monotonic time (M8).
//!
//! `now()` is whole seconds since the Unix epoch (UTC). `monotonic()` is seconds elapsed since the
//! first time it (or any time fn) was touched in this process — a steady stopwatch for measuring
//! durations, immune to wall-clock adjustments. `sleep_ms(n)` parks the thread. `format(epoch)`
//! renders epoch seconds as a UTC `"YYYY-MM-DD HH:MM:SS"` string, computed directly (no chrono).

use super::{Host, HostError, Kind, NativeFn, NativeRet, expect_args};
use std::sync::OnceLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Process-start reference for `monotonic()`. Set on first use.
static START: OnceLock<Instant> = OnceLock::new();

fn now(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "now", 0)?;
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    Ok(NativeRet::Int(secs))
}

fn monotonic(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "monotonic", 0)?;
    let start = START.get_or_init(Instant::now);
    Ok(NativeRet::Float(start.elapsed().as_secs_f64()))
}

fn sleep_ms(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "sleep_ms", 1)?;
    let ms = h.arg_int(0)?;
    if ms > 0 {
        std::thread::sleep(std::time::Duration::from_millis(ms as u64));
    }
    Ok(NativeRet::Nil)
}

fn format(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "format", 1)?;
    let epoch = h.arg_int(0)?;
    let days = epoch.div_euclid(86_400);
    let tod = epoch.rem_euclid(86_400);
    let (hh, mm, ss) = (tod / 3600, (tod % 3600) / 60, tod % 60);
    let (y, m, d) = civil_from_days(days);
    Ok(NativeRet::Str(format!(
        "{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}:{ss:02}"
    )))
}

/// Convert days since 1970-01-01 to a `(year, month, day)` civil date (proleptic Gregorian, UTC).
/// Howard Hinnant's `civil_from_days` algorithm — exact for the full i64 range.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Callable members. `(name, fn, kind)`.
pub const MEMBERS: &[(&str, NativeFn, Kind)] = &[
    ("now", now, Kind::Inline),
    ("monotonic", monotonic, Kind::Inline),
    ("sleep_ms", sleep_ms, Kind::TimedWait),
    ("format", format, Kind::Inline),
];

#[cfg(test)]
mod tests {
    use super::civil_from_days;

    #[test]
    fn civil_from_days_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(31), (1970, 2, 1));
        // 1700000000 secs = 19675 days, 80000 secs → 2023-11-14
        assert_eq!(civil_from_days(19_675), (2023, 11, 14));
        // leap day
        assert_eq!(civil_from_days(59), (1970, 3, 1));
    }
}
