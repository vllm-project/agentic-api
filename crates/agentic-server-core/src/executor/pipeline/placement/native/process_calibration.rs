// Opt-in observer calibration, separate from correctness and workload timing.

#![cfg(test)]

use std::hint::black_box;
use std::time::{Duration, Instant};

use super::process::{CpuTime, Process, clock_ticks_per_second, cpu_seconds};
use super::profile::percentile;

fn burn(duration: Duration) {
    let started = Instant::now();
    let mut value = 1u64;
    while started.elapsed() < duration {
        for _ in 0..1024 {
            value = black_box(value.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1));
        }
    }
    black_box(value);
}

fn reading_cost<T>(mut read: impl FnMut() -> T) -> serde_json::Value {
    // Warm the entry point before measuring its observer cost.
    for _ in 0..16 {
        black_box(read());
    }
    let mut wall_ns = Vec::new();
    let mut increments = Vec::new();
    let mut before = CpuTime::read();
    for _ in 0..256 {
        let started = Instant::now();
        black_box(read());
        wall_ns.push(started.elapsed().as_nanos());
        let after = CpuTime::read();
        if let Some(delta) = CpuTime::delta(before, after) {
            increments.push(delta.total);
        }
        before = after;
    }
    serde_json::json!({
        "samples": wall_ns.len(),
        "call_wall_p50_ns": percentile(&mut wall_ns, 50),
        "call_wall_p99_ns": percentile(&mut wall_ns, 99),
        "call_wall_max_ns": wall_ns.iter().max(),
        "process_cpu_delta_available": increments.len(),
        "process_cpu_delta_zero": increments.iter().filter(|&&v| v == 0).count(),
        "min_observed_positive_increment_us_not_clock_resolution": increments.iter().filter(|&&v| v > 0).min(),
        "increment_includes_loop_and_cpu_observer": true,
    })
}

#[test]
#[ignore = "explicit process CPU observer calibration; not a placement benchmark"]
fn calibrate_process_cpu_observer() {
    if CpuTime::read().is_none() {
        println!("PLACEMENT_CPU_CALIBRATION_UNAVAILABLE");
        return;
    }
    let cpu_read = reading_cost(CpuTime::read);
    let proc_read = reading_cost(Process::read);
    let hz = clock_ticks_per_second();
    let mut controls = Vec::new();
    for mode in ["caller_busy", "joined_worker_busy", "sleep"] {
        let before = Process::read();
        let cpu_before = CpuTime::read();
        let started = Instant::now();
        let worker_cpu = match mode {
            "caller_busy" => {
                burn(Duration::from_millis(40));
                None
            }
            "joined_worker_busy" => std::thread::spawn(|| {
                #[cfg(target_os = "linux")]
                let before = CpuTime::read_for(nix::sys::resource::UsageWho::RUSAGE_THREAD);
                burn(Duration::from_millis(40));
                #[cfg(target_os = "linux")]
                {
                    CpuTime::delta(before, CpuTime::read_for(nix::sys::resource::UsageWho::RUSAGE_THREAD))
                }
                #[cfg(not(target_os = "linux"))]
                {
                    None
                }
            })
            .join()
            .expect("calibration worker joined"),
            "sleep" => {
                std::thread::sleep(Duration::from_millis(40));
                None
            }
            _ => unreachable!(),
        };
        let elapsed = started.elapsed();
        let cpu = CpuTime::delta(cpu_before, CpuTime::read()).expect("monotonic process CPU counters");
        let after = Process::read();
        if mode != "sleep" {
            assert!(cpu.total > 0, "busy CPU control must be observable");
        }
        if let Some(worker) = worker_cpu {
            assert!(worker.total > 0);
            assert!(
                cpu.total.saturating_add(2) >= worker.total,
                "process-wide counters must retain the joined worker's CPU"
            );
        }
        controls.push(serde_json::json!({
            "mode": mode, "wall_ns": elapsed.as_nanos(), "process_cpu_time": cpu,
            "joined_worker_thread_cpu_time": worker_cpu,
            "proc_tick_user_seconds": cpu_seconds(before.user_ticks, after.user_ticks, hz),
            "proc_tick_system_seconds": cpu_seconds(before.system_ticks, after.system_ticks, hz),
        }));
    }
    println!(
        "PLACEMENT_CPU_CALIBRATION {}",
        serde_json::json!({
            "schema": 1, "source": "linux_getrusage_rusage_self", "cpu_unit": "microseconds",
            "scope": "all_process_threads_including_joined_workers_not_child_processes",
            "proc_tick_hz": hz, "cpu_read_cost": cpu_read, "proc_snapshot_read_cost": proc_read,
            "controls": controls,
            "cost_subtracted_from_trials": false, "inner_pipeline_observer_calibrated": false,
            "microsecond_representation_is_not_a_clock_resolution_claim": true,
        })
    );
}
