// Process-only observations. Never return raw proc text or machine identifiers.

#![cfg(test)]

use std::process::Command;

/// Process-wide counters, not caller-thread time or elapsed wall time.
/// Microseconds are the API's representation, not a claimed clock resolution.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub(super) struct CpuTime {
    #[serde(rename = "user_us")]
    pub user: u64,
    #[serde(rename = "system_us")]
    pub system: u64,
    #[serde(rename = "total_us")]
    pub total: u64,
}

impl CpuTime {
    pub fn read() -> Option<Self> {
        #[cfg(target_os = "linux")]
        {
            Self::read_for(nix::sys::resource::UsageWho::RUSAGE_SELF)
        }
        #[cfg(not(target_os = "linux"))]
        {
            None
        }
    }

    #[cfg(target_os = "linux")]
    pub(super) fn read_for(who: nix::sys::resource::UsageWho) -> Option<Self> {
        use nix::sys::resource::getrusage;
        use nix::sys::time::TimeValLike;

        let usage = getrusage(who).ok()?;
        let user = u64::try_from(usage.user_time().num_microseconds()).ok()?;
        let system = u64::try_from(usage.system_time().num_microseconds()).ok()?;
        Some(Self {
            user,
            system,
            total: user.checked_add(system)?,
        })
    }

    pub fn delta(before: Option<Self>, after: Option<Self>) -> Option<Self> {
        let before = before?;
        let after = after?;
        let user = after.user.checked_sub(before.user)?;
        let system = after.system.checked_sub(before.system)?;
        Some(Self {
            user,
            system,
            total: user.checked_add(system)?,
        })
    }
}

#[derive(serde::Serialize)]
pub(super) struct CpuObservation {
    time: Option<CpuTime>,
    source: &'static str,
    unit: &'static str,
    scope: &'static str,
    observer_cost_subtracted: bool,
}

impl CpuObservation {
    pub fn new(time: Option<CpuTime>, scope: &'static str) -> Self {
        Self {
            time,
            source: "linux_getrusage_rusage_self",
            unit: "microseconds_not_claimed_resolution",
            scope,
            observer_cost_subtracted: false,
        }
    }
}

#[derive(Default, Clone, Copy)]
pub(super) struct Process {
    pub user_ticks: Option<u64>,
    pub system_ticks: Option<u64>,
    pub rss_high_water_kib: Option<u64>,
    pub threads_observed: Option<u64>,
}

impl Process {
    pub fn read() -> Self {
        if !cfg!(target_os = "linux") {
            return Self::default();
        }
        let ticks = std::fs::read_to_string("/proc/self/stat")
            .ok()
            .and_then(|text| cpu_ticks(&text));
        let status = std::fs::read_to_string("/proc/self/status").ok();
        Self {
            user_ticks: ticks.map(|value| value.0),
            system_ticks: ticks.map(|value| value.1),
            rss_high_water_kib: status
                .as_deref()
                .and_then(|text| status_value(text, "VmHWM:", Some("kB"))),
            threads_observed: status.as_deref().and_then(|text| status_value(text, "Threads:", None)),
        }
    }
}

pub(super) fn clock_ticks_per_second() -> Option<u64> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    let result = Command::new("getconf").arg("CLK_TCK").output().ok()?;
    if !result.status.success() {
        return None;
    }
    let value = std::str::from_utf8(&result.stdout).ok()?.trim().parse().ok()?;
    (value > 0).then_some(value)
}

pub(super) fn cpu_seconds(before: Option<u64>, after: Option<u64>, hz: Option<u64>) -> Option<f64> {
    let ticks = after?.checked_sub(before?)?;
    let ticks = u32::try_from(ticks).ok()?;
    let hz = u32::try_from(hz?).ok()?;
    (hz > 0).then(|| f64::from(ticks) / f64::from(hz))
}

fn cpu_ticks(text: &str) -> Option<(u64, u64)> {
    // The comm field may contain spaces and parentheses; fields after its last
    // closing parenthesis begin with state (field 3), not the process ID.
    let fields: Vec<_> = text.get(text.rfind(')')? + 1..)?.split_whitespace().collect();
    Some((fields.get(11)?.parse().ok()?, fields.get(12)?.parse().ok()?))
}

fn status_value(text: &str, label: &str, unit: Option<&str>) -> Option<u64> {
    let line = text.lines().find_map(|line| line.strip_prefix(label))?;
    let mut fields = line.split_whitespace();
    let value = fields.next()?.parse().ok()?;
    if fields.next() != unit || fields.next().is_some() {
        return None;
    }
    Some(value)
}

#[test]
fn proc_stat_uses_the_whole_comm_field_and_correct_tick_offsets() {
    let fields = (0..20).map(|index| index.to_string()).collect::<Vec<_>>().join(" ");
    assert_eq!(cpu_ticks(&format!("999 (test (worker) name) {fields}")), Some((11, 12)));
    assert_eq!(cpu_ticks("999 truncated"), None);
}

#[test]
fn status_units_and_missing_values_are_not_invented() {
    assert_eq!(
        status_value("VmHWM: 512 kB\nThreads: 7\n", "VmHWM:", Some("kB")),
        Some(512)
    );
    assert_eq!(status_value("Threads: 7\n", "Threads:", None), Some(7));
    assert_eq!(status_value("VmHWM: 512 MB\n", "VmHWM:", Some("kB")), None);
    assert_eq!(status_value("Threads: unknown\n", "Threads:", None), None);
}

#[test]
fn cpu_delta_requires_observed_frequency_and_monotonic_counters() {
    assert_eq!(cpu_seconds(Some(10), Some(20), Some(100)), Some(0.1));
    assert_eq!(cpu_seconds(Some(20), Some(10), Some(100)), None);
    assert_eq!(cpu_seconds(Some(10), Some(20), None), None);
    assert_eq!(cpu_seconds(Some(10), Some(20), Some(0)), None);
}

#[test]
fn resource_delta_is_checked_without_ticks_or_wall_clock_substitution() {
    let before = CpuTime {
        user: 100,
        system: 30,
        total: 130,
    };
    let after = CpuTime {
        user: 103,
        system: 34,
        total: 137,
    };
    assert_eq!(
        CpuTime::delta(Some(before), Some(after)),
        Some(CpuTime {
            user: 3,
            system: 4,
            total: 7
        })
    );
    assert_eq!(CpuTime::delta(Some(after), Some(before)), None);
    assert_eq!(CpuTime::delta(None, Some(after)), None);
    assert_eq!(CpuTime::delta(Some(before), None), None);
    assert_eq!(
        CpuTime::delta(Some(before), Some(before)),
        Some(CpuTime {
            user: 0,
            system: 0,
            total: 0
        })
    );
    let huge = CpuTime {
        user: u64::MAX,
        system: 1,
        total: 0,
    };
    let zero = CpuTime {
        user: 0,
        system: 0,
        total: 0,
    };
    assert_eq!(CpuTime::delta(Some(zero), Some(huge)), None);
    let observed = CpuObservation::new(CpuTime::delta(Some(before), Some(after)), "test_scope");
    let row = serde_json::to_value(observed).unwrap();
    assert_eq!(
        row["time"],
        serde_json::json!({"user_us": 3, "system_us": 4, "total_us": 7})
    );
    assert_eq!(row["unit"], "microseconds_not_claimed_resolution");
    assert_eq!(row["scope"], "test_scope");
    assert_eq!(row["observer_cost_subtracted"], false);
    assert!(serde_json::to_value(CpuObservation::new(None, "unsupported")).unwrap()["time"].is_null());
}
