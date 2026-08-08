use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Default)]
pub(crate) struct FileTimes {
    pub modified: String,
    pub created: String,
    pub accessed: String,
}

impl FileTimes {
    pub(crate) fn read(path: &Path) -> Self {
        #[cfg(test)]
        STAT_CALLS.with(|calls| calls.set(calls.get() + 1));

        let Ok(metadata) = fs::metadata(path) else {
            return Self::default();
        };
        if !metadata.is_file() {
            return Self::default();
        }

        platform::format(&metadata)
    }
}

#[cfg(test)]
thread_local! {
    static STAT_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn reset_stat_calls() {
    STAT_CALLS.with(|calls| calls.set(0));
}

#[cfg(test)]
pub(crate) fn stat_calls() -> usize {
    STAT_CALLS.with(std::cell::Cell::get)
}

#[cfg(windows)]
mod platform {
    use super::FileTimes;
    use std::fs::Metadata;
    use std::mem::zeroed;
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Foundation::{FILETIME, SYSTEMTIME};
    use windows_sys::Win32::System::Time::{
        FileTimeToSystemTime, SystemTimeToTzSpecificLocalTimeEx,
    };

    pub(super) fn format(metadata: &Metadata) -> FileTimes {
        FileTimes {
            modified: format_file_time(metadata.last_write_time()),
            created: format_file_time(metadata.creation_time()),
            accessed: format_file_time(metadata.last_access_time()),
        }
    }

    fn format_file_time(ticks: u64) -> String {
        let file_time = FILETIME {
            dwLowDateTime: ticks as u32,
            dwHighDateTime: (ticks >> 32) as u32,
        };
        let mut utc: SYSTEMTIME = unsafe { zeroed() };
        let mut local: SYSTEMTIME = unsafe { zeroed() };
        if unsafe { FileTimeToSystemTime(&file_time, &mut utc) } == 0
            || unsafe { SystemTimeToTzSpecificLocalTimeEx(std::ptr::null(), &utc, &mut local) } == 0
        {
            return String::new();
        }

        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:07}",
            local.wYear,
            local.wMonth,
            local.wDay,
            local.wHour,
            local.wMinute,
            local.wSecond,
            ticks % 10_000_000
        )
    }
}

#[cfg(unix)]
mod platform {
    use super::FileTimes;
    use std::fs::Metadata;
    use std::mem::zeroed;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    pub(super) fn format(metadata: &Metadata) -> FileTimes {
        let modified = metadata.modified().ok();
        let created = metadata
            .created()
            .ok()
            .or_else(|| synthesized_creation(metadata));
        let accessed = metadata.accessed().ok();
        FileTimes {
            modified: modified.map(format_system_time).unwrap_or_default(),
            created: created.map(format_system_time).unwrap_or_default(),
            accessed: accessed.map(format_system_time).unwrap_or_default(),
        }
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn synthesized_creation(metadata: &Metadata) -> Option<SystemTime> {
        use std::os::unix::fs::MetadataExt;

        let modified = unix_system_time(metadata.mtime(), metadata.mtime_nsec());
        let changed = unix_system_time(metadata.ctime(), metadata.ctime_nsec());
        Some(modified.min(changed))
    }

    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    fn synthesized_creation(metadata: &Metadata) -> Option<SystemTime> {
        metadata.modified().ok()
    }

    fn unix_system_time(seconds: i64, nanoseconds: i64) -> SystemTime {
        if seconds >= 0 {
            UNIX_EPOCH
                + Duration::new(
                    seconds as u64,
                    u32::try_from(nanoseconds).unwrap_or_default(),
                )
        } else if nanoseconds == 0 {
            UNIX_EPOCH - Duration::from_secs(seconds.unsigned_abs())
        } else {
            UNIX_EPOCH
                - Duration::new(
                    seconds.unsigned_abs().saturating_sub(1),
                    1_000_000_000 - u32::try_from(nanoseconds).unwrap_or_default(),
                )
        }
    }

    fn format_system_time(value: SystemTime) -> String {
        let (seconds, nanoseconds) = match value.duration_since(UNIX_EPOCH) {
            Ok(duration) => (
                i64::try_from(duration.as_secs()).unwrap_or(i64::MAX),
                duration.subsec_nanos(),
            ),
            Err(error) => {
                let duration = error.duration();
                if duration.subsec_nanos() == 0 {
                    (-i64::try_from(duration.as_secs()).unwrap_or(i64::MAX), 0)
                } else {
                    (
                        -i64::try_from(duration.as_secs()).unwrap_or(i64::MAX) - 1,
                        1_000_000_000 - duration.subsec_nanos(),
                    )
                }
            }
        };
        let timestamp = match libc::time_t::try_from(seconds) {
            Ok(timestamp) => timestamp,
            Err(_) => return String::new(),
        };
        let mut local: libc::tm = unsafe { zeroed() };
        if unsafe { libc::localtime_r(&timestamp, &mut local) }.is_null() {
            return String::new();
        }

        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:07}",
            local.tm_year + 1900,
            local.tm_mon + 1,
            local.tm_mday,
            local.tm_hour,
            local.tm_min,
            local.tm_sec,
            nanoseconds / 100
        )
    }
}
