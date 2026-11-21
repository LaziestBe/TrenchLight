use anyhow::{Context, Result};
use sha1::{Digest, Sha1};
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

const HEX: &[u8; 16] = b"0123456789abcdef";

pub fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

pub fn sha1_file(path: &Path) -> Result<String> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut reader = BufReader::with_capacity(64 * 1024, file);
    let mut hasher = Sha1::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(to_hex(&hasher.finalize()))
}

pub fn sha1_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha1::new();
    hasher.update(bytes);
    to_hex(&hasher.finalize())
}

pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Runs `f(0..n)` over `threads` workers and stops at the first error.
pub fn parallel_for<F>(n: usize, threads: usize, f: F) -> Result<()>
where
    F: Fn(usize) -> Result<()> + Sync,
{
    if n == 0 {
        return Ok(());
    }
    let threads = threads.clamp(1, n);
    let next = AtomicUsize::new(0);
    let failure: std::sync::Mutex<Option<anyhow::Error>> = std::sync::Mutex::new(None);

    std::thread::scope(|scope| {
        for _ in 0..threads {
            let f = &f;
            let next = &next;
            let failure = &failure;
            scope.spawn(move || loop {
                let index = next.fetch_add(1, Ordering::Relaxed);
                if index >= n || failure.lock().unwrap().is_some() {
                    break;
                }
                if let Err(error) = f(index) {
                    failure.lock().unwrap().get_or_insert(error);
                    break;
                }
            });
        }
    });

    match failure.into_inner().unwrap() {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// Recursively copies `from` into `to` and returns the number of bytes copied.
/// Symbolic links are copied as regular files, which is what a modpack needs.
pub fn copy_dir(from: &Path, to: &Path) -> Result<u64> {
    std::fs::create_dir_all(to).with_context(|| format!("create {}", to.display()))?;
    let mut bytes = 0u64;
    for entry in std::fs::read_dir(from).with_context(|| format!("read {}", from.display()))? {
        let entry = entry?;
        let source = entry.path();
        let dest = to.join(entry.file_name());
        if source.is_dir() {
            bytes += copy_dir(&source, &dest)?;
        } else {
            std::fs::copy(&source, &dest)
                .with_context(|| format!("copy {} -> {}", source.display(), dest.display()))?;
            bytes += entry.metadata().map(|meta| meta.len()).unwrap_or(0);
        }
    }
    Ok(bytes)
}

#[cfg(windows)]
mod local_time {
    /// `SYSTEMTIME` from `minwindef.h`. `#[repr(C)]` keeps the u16 fields in
    /// the order the API writes them; `day_of_week` is ignored.
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct SystemTime {
        pub year: u16,
        pub month: u16,
        pub day_of_week: u16,
        pub day: u16,
        pub hour: u16,
        pub minute: u16,
        pub second: u16,
        pub milliseconds: u16,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn GetLocalTime(lp_system_time: *mut SystemTime);
    }

    /// Today, in local time, DST included, as the OS sees it.
    pub fn now() -> SystemTime {
        let mut stamp = SystemTime {
            year: 0,
            month: 0,
            day_of_week: 0,
            day: 0,
            hour: 0,
            minute: 0,
            second: 0,
            milliseconds: 0,
        };
        // SAFETY: the only requirement of `GetLocalTime` is a writable
        // `SYSTEMTIME`, and `stamp` is exactly that, correctly laid out.
        unsafe { GetLocalTime(&mut stamp) };
        stamp
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Local wall-clock time as `"YYYY-MM-DD HH:MM:SS"`.
///
/// On Windows this is the machine's local time (read through `GetLocalTime`),
/// so a stamp matches the user's clock and the file's mtime on disk.
///
/// History: builds before 2026-11-18 wrote **UTC** here while displaying it as
/// if it were local, which on this UTC−3 machine showed a dive made at 18:23
/// local as 21:23. Stamps already in `instance.json` are therefore still UTC;
/// they are not migrated, and readers treat every stamp as local (see
/// `app::human_age`), so an old stamp can be off by the zone offset.
///
/// Other platforms keep the old UTC behaviour: no crate is pulled in for this.
pub fn now_iso() -> String {
    iso_from_unix_with_offset(unix_now(), local_offset_secs())
}

/// Seconds to add to UTC to get the local wall clock (UTC−3,
/// gives `-10800`).
///
/// Derived from the OS rather than hardcoded, so DST is whatever the machine
/// says it is right now. Windows only; `0` (UTC) everywhere else.
#[cfg(windows)]
pub fn local_offset_secs() -> i64 {
    let wall = local_time::now();
    let wall_secs = days_from_civil(wall.year as i64, wall.month as i64, wall.day as i64) * 86_400
        + wall.hour as i64 * 3600
        + wall.minute as i64 * 60
        + wall.second as i64;
    // The two readings are taken microseconds apart, so when one lands just
    // before a second boundary and the other just after, the difference is
    // short by a second. Every real zone offset is a whole number of minutes,
    // so rounding to the minute absorbs that and nothing else.
    let raw = wall_secs - unix_now();
    (raw as f64 / 60.0).round() as i64 * 60
}

#[cfg(not(windows))]
pub fn local_offset_secs() -> i64 {
    0
}

/// `secs` are since the Unix epoch (UTC); `offset_secs` is added first to get
/// the zone's wall clock. Pure, so tests can pin the frame it works in.
fn iso_from_unix_with_offset(secs: i64, offset_secs: i64) -> String {
    iso_from_unix(secs + offset_secs)
}

/// A file's mtime (an instant) as the same local wall-clock stamp `now_iso`
/// writes, so `app::human_age` can read it back unchanged.
pub fn iso_from_system_time(time: std::time::SystemTime) -> String {
    let secs = match time.duration_since(std::time::UNIX_EPOCH) {
        Ok(since) => since.as_secs() as i64,
        // A file dated before 1970 is unusual but not an error.
        Err(before) => -(before.duration().as_secs() as i64),
    };
    iso_from_unix_with_offset(secs, local_offset_secs())
}

fn iso_from_unix(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (hour, minute, second) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}")
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// Days since 1970-01-01 for a civil date: Howard Hinnant's `days_from_civil`,
/// the inverse of `civil_from_days`.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha1_of_known_bytes() {
        assert_eq!(sha1_bytes(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
    }

    #[test]
    fn iso_epoch() {
        assert_eq!(iso_from_unix(0), "1970-01-01 00:00:00");
    }

    #[test]
    fn a_system_time_lands_in_the_same_wall_clock_frame() {
        let dive = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_795_037_016);
        assert_eq!(
            iso_from_system_time(dive),
            iso_from_unix_with_offset(1_795_037_016, local_offset_secs()),
            "a file mtime reads exactly like a written stamp"
        );
        let before_epoch = std::time::SystemTime::UNIX_EPOCH - std::time::Duration::from_secs(1);
        assert_eq!(
            iso_from_system_time(before_epoch),
            iso_from_unix_with_offset(-1, local_offset_secs())
        );
    }

    #[test]
    fn civil_days_round_trip() {
        for days in [-1, 0, 1, 20_600, 20_762] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m as i64, d as i64), days);
        }
    }

    /// The dive from the bug report: 18:23:36 local, which was
    /// 2026-11-18 21:23:36 UTC (local = UTC−3).
    const DIVE_UTC: i64 = 1_795_037_016;

    #[test]
    fn iso_with_zero_offset_is_the_epoch_as_utc() {
        assert_eq!(iso_from_unix_with_offset(0, 0), "1970-01-01 00:00:00");
        assert_eq!(
            iso_from_unix_with_offset(DIVE_UTC, 0),
            "2026-11-18 21:23:36"
        );
    }

    #[test]
    fn iso_with_negative_offset_is_the_bug_report_case() {
        // Before the fix `mcl list` printed the UTC frame, "21:23:36", for a
        // dive made at 18:23:36 local: exactly this off-by-three-hours. With
        // the offset applied it reads 18:23:36 again.
        assert_eq!(
            iso_from_unix_with_offset(DIVE_UTC, -3 * 3600),
            "2026-11-18 18:23:36"
        );
        // US eastern summer time, and the westernmost real zone.
        assert_eq!(
            iso_from_unix_with_offset(DIVE_UTC, -4 * 3600),
            "2026-11-18 17:23:36"
        );
        assert_eq!(
            iso_from_unix_with_offset(DIVE_UTC, -12 * 3600),
            "2026-11-18 09:23:36"
        );
    }

    #[test]
    fn iso_with_positive_offset() {
        // A DST-style single hour, a whole +3, and the half-hour zone India
        // actually uses.
        assert_eq!(
            iso_from_unix_with_offset(DIVE_UTC, 3600),
            "2026-11-18 22:23:36"
        );
        assert_eq!(
            iso_from_unix_with_offset(DIVE_UTC, 3 * 3600),
            "2026-11-19 00:23:36"
        );
        assert_eq!(
            iso_from_unix_with_offset(DIVE_UTC, 5 * 3600 + 1800),
            "2026-11-19 02:53:36"
        );
    }

    #[test]
    fn iso_rolls_over_midnight_and_back() {
        let midnight_utc = 1_794_960_000; // 2026-11-18 00:00:00 UTC
        assert_eq!(
            iso_from_unix_with_offset(midnight_utc, 0),
            "2026-11-18 00:00:00"
        );
        // Backwards over midnight, into the previous day.
        assert_eq!(
            iso_from_unix_with_offset(midnight_utc, -3 * 3600),
            "2026-11-17 21:00:00"
        );
        // Forwards over midnight, into the next day.
        assert_eq!(
            iso_from_unix_with_offset(midnight_utc + 23 * 3600 + 1800, 3600),
            "2026-11-19 00:30:00"
        );
        // And over a year boundary, which is the same arithmetic.
        let new_year_eve = 1_798_759_800; // 2026-12-31 23:30:00 UTC
        assert_eq!(
            iso_from_unix_with_offset(new_year_eve, 3600),
            "2027-01-01 00:30:00"
        );
        // One second before the epoch is a negative timestamp, not a panic.
        assert_eq!(
            iso_from_unix_with_offset(-1, 0),
            "1969-12-31 23:59:59"
        );
    }

    #[test]
    fn local_offset_is_a_whole_number_of_minutes() {
        assert_eq!(local_offset_secs() % 60, 0);
        assert!(local_offset_secs().abs() <= 14 * 3600);
    }
}
