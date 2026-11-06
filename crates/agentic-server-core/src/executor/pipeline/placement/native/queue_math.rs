// Offline queue-residence envelopes, not admission samples or heap accounting.

#![cfg(test)]

use std::collections::BTreeMap;

#[derive(Clone, Copy, serde::Serialize)]
pub(super) struct Interval {
    pub send_before: u128,
    pub send_after: u128,
    pub receive_before: u128,
    pub receive_after: u128,
    pub logical_bytes: u32,
}

#[derive(serde::Serialize)]
pub(super) struct Distribution {
    pub entries_ns: BTreeMap<usize, u128>,
    pub logical_bytes_ns: BTreeMap<u64, u128>,
    pub entries_p99: Option<usize>,
    pub logical_bytes_p99: Option<u64>,
    pub entry_area_ns: u128,
    pub logical_byte_area_ns: u128,
}

#[derive(serde::Serialize)]
pub(super) struct Envelope {
    pub window_ns: u128,
    pub entries_capacity: usize,
    pub logical_byte_budget: u32,
    pub lower: Distribution,
    pub upper: Distribution,
    pub exact_entries_p99_if_bounds_agree: Option<usize>,
    pub exact_logical_bytes_p99_if_bounds_agree: Option<u64>,
    pub intervals: Vec<Interval>,
}

fn quantile<T: Copy + Ord>(histogram: &BTreeMap<T, u128>, window: u128) -> Option<T> {
    if window == 0 {
        return None;
    }
    let threshold = window.checked_mul(99)?.div_ceil(100);
    let mut cumulative = 0u128;
    for (&value, &weight) in histogram {
        cumulative = cumulative.checked_add(weight)?;
        if cumulative >= threshold {
            return Some(value);
        }
    }
    None
}

fn weighted(
    records: &[Interval],
    window: u128,
    capacity: usize,
    budget: u32,
    lower: bool,
) -> Result<Distribution, &'static str> {
    let mut events = Vec::with_capacity(2 * records.len() + 1);
    for record in records {
        let (start, end) = if lower {
            (record.send_after, record.receive_before)
        } else {
            (record.send_before, record.receive_after)
        };
        if start < end {
            events.push((start, 1i64, i64::from(record.logical_bytes)));
            events.push((end, -1, -i64::from(record.logical_bytes)));
        }
    }
    events.push((window, 0, 0));
    events.sort_unstable_by_key(|event| event.0);
    let mut entries_ns = BTreeMap::new();
    let mut logical_bytes_ns = BTreeMap::new();
    let (mut last, mut entries, mut bytes) = (0u128, 0i64, 0i64);
    let (mut entry_area_ns, mut logical_byte_area_ns) = (0u128, 0u128);
    for (at, entry_delta, byte_delta) in events {
        let duration = at.checked_sub(last).ok_or("non-monotonic event")?;
        if duration > 0 {
            let actual_entries = usize::try_from(entries).map_err(|_| "negative entries")?;
            let actual_bytes = u64::try_from(bytes).map_err(|_| "negative bytes")?;
            if lower && (actual_entries > capacity || actual_bytes > u64::from(budget)) {
                return Err("impossible guaranteed backlog");
            }
            // Actual queues cannot exceed these independently enforced bounds.
            // Upper residence intervals may overlap while wrappers are scheduled.
            let bounded_entries = actual_entries.min(capacity);
            let bounded_bytes = actual_bytes.min(u64::from(budget));
            *entries_ns.entry(bounded_entries).or_insert(0) += duration;
            *logical_bytes_ns.entry(bounded_bytes).or_insert(0) += duration;
            entry_area_ns = entry_area_ns
                .checked_add(duration.checked_mul(bounded_entries as u128).ok_or("area overflow")?)
                .ok_or("area overflow")?;
            logical_byte_area_ns = logical_byte_area_ns
                .checked_add(duration.checked_mul(u128::from(bounded_bytes)).ok_or("area overflow")?)
                .ok_or("area overflow")?;
        }
        entries += entry_delta;
        bytes += byte_delta;
        last = at;
    }
    if entries != 0 || bytes != 0 {
        return Err("unterminated residence interval");
    }
    Ok(Distribution {
        entries_p99: quantile(&entries_ns, window),
        logical_bytes_p99: quantile(&logical_bytes_ns, window),
        entries_ns,
        logical_bytes_ns,
        entry_area_ns,
        logical_byte_area_ns,
    })
}

pub(super) fn summarize(
    intervals: Vec<Interval>,
    window_ns: u128,
    entries_capacity: usize,
    logical_byte_budget: u32,
) -> Result<Envelope, &'static str> {
    if entries_capacity == 0 || logical_byte_budget == 0 {
        return Err("zero bound");
    }
    let (mut last_send, mut last_receive) = (0, 0);
    for record in &intervals {
        if record.send_before > record.send_after
            || record.receive_before > record.receive_after
            || record.send_before < last_send
            || record.receive_before < last_receive
            || record.send_before > record.receive_after
            || record.send_after > window_ns
            || record.receive_after > window_ns
            || record.logical_bytes > logical_byte_budget
        {
            return Err("invalid FIFO observation interval");
        }
        last_send = record.send_after;
        last_receive = record.receive_after;
    }
    let lower = weighted(&intervals, window_ns, entries_capacity, logical_byte_budget, true)?;
    let upper = weighted(&intervals, window_ns, entries_capacity, logical_byte_budget, false)?;
    if lower.entry_area_ns > upper.entry_area_ns || lower.logical_byte_area_ns > upper.logical_byte_area_ns {
        return Err("inverted envelope");
    }
    Ok(Envelope {
        exact_entries_p99_if_bounds_agree: lower.entries_p99.filter(|value| Some(*value) == upper.entries_p99),
        exact_logical_bytes_p99_if_bounds_agree: lower
            .logical_bytes_p99
            .filter(|value| Some(*value) == upper.logical_bytes_p99),
        window_ns,
        entries_capacity,
        logical_byte_budget,
        lower,
        upper,
        intervals,
    })
}

#[test]
fn duration_weights_include_idle_and_do_not_count_reservations_or_service() {
    let records = vec![Interval {
        send_before: 2,
        send_after: 4,
        receive_before: 6,
        receive_after: 8,
        logical_bytes: 5,
    }];
    let value = summarize(records, 100, 1, 10).unwrap();
    assert_eq!(value.lower.entries_ns, BTreeMap::from([(0, 98), (1, 2)]));
    assert_eq!(value.upper.entries_ns, BTreeMap::from([(0, 94), (1, 6)]));
    assert_eq!(value.lower.entry_area_ns, 2);
    assert_eq!(value.upper.entry_area_ns, 6);
    assert_eq!(value.exact_entries_p99_if_bounds_agree, Some(1));
}

#[test]
fn wrapper_overlap_is_capped_but_ambiguous_quantiles_remain_unknown() {
    let records = vec![
        Interval {
            send_before: 1,
            send_after: 2,
            receive_before: 0,
            receive_after: 6,
            logical_bytes: 7,
        },
        Interval {
            send_before: 2,
            send_after: 7,
            receive_before: 6,
            receive_after: 9,
            logical_bytes: 8,
        },
    ];
    let value = summarize(records, 10, 1, 10).unwrap();
    assert_eq!(value.lower.entries_p99, Some(0));
    assert_eq!(value.upper.entries_p99, Some(1));
    assert_eq!(value.exact_entries_p99_if_bounds_agree, None);
    assert!(value.upper.logical_bytes_ns.keys().all(|&bytes| bytes <= 10));
    assert_eq!(value.upper.logical_bytes_ns.get(&10), Some(&4));
}

#[test]
fn every_known_linearization_lies_inside_the_weighted_envelope() {
    let envelope = summarize(
        vec![Interval {
            send_before: 2,
            send_after: 4,
            receive_before: 6,
            receive_after: 8,
            logical_bytes: 5,
        }],
        100,
        1,
        10,
    )
    .unwrap();
    for sent in 2..=4 {
        for received in 6..=8 {
            let actual = summarize(
                vec![Interval {
                    send_before: sent,
                    send_after: sent,
                    receive_before: received,
                    receive_after: received,
                    logical_bytes: 5,
                }],
                100,
                1,
                10,
            )
            .unwrap();
            assert!(envelope.lower.entry_area_ns <= actual.lower.entry_area_ns);
            assert!(actual.upper.entry_area_ns <= envelope.upper.entry_area_ns);
            assert_eq!(actual.lower.entry_area_ns, actual.upper.entry_area_ns);
            assert!(envelope.lower.entries_p99 <= actual.lower.entries_p99);
            assert!(actual.upper.entries_p99 <= envelope.upper.entries_p99);
        }
    }
}

#[test]
fn zero_time_and_malformed_or_incomplete_observations_are_not_certified() {
    let value = summarize(Vec::new(), 0, 1, 10).unwrap();
    assert_eq!(value.lower.entries_p99, None);
    assert_eq!(value.exact_entries_p99_if_bounds_agree, None);
    let invalid = Interval {
        send_before: 3,
        send_after: 2,
        receive_before: 6,
        receive_after: 8,
        logical_bytes: 5,
    };
    assert!(summarize(vec![invalid], 100, 1, 10).is_err());
    assert!(summarize(Vec::new(), 100, 0, 10).is_err());
}
