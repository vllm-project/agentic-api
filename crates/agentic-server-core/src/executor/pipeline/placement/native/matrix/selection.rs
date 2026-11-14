// Explicit one-case process selection; never silently truncate a full sweep.

#![cfg(test)]

use super::config::{Config, full_plan, pilot};

#[derive(Debug, PartialEq, Eq)]
pub(super) enum SelectionError {
    UnknownMode,
    UnexpectedCase,
    MissingCase,
    InvalidCase,
}

pub(super) fn select(mode: &str, requested: Option<&str>) -> Result<Vec<(usize, Config)>, SelectionError> {
    match mode {
        "pilot" | "full" | "plan" => {
            if requested.is_some() {
                return Err(SelectionError::UnexpectedCase);
            }
            let configs = if mode == "pilot" { pilot() } else { full_plan() };
            Ok(configs.into_iter().enumerate().collect())
        }
        "case" => {
            let raw = requested.ok_or(SelectionError::MissingCase)?;
            if raw.is_empty() || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(SelectionError::InvalidCase);
            }
            let index: usize = raw.parse().map_err(|_| SelectionError::InvalidCase)?;
            let config = full_plan().into_iter().nth(index).ok_or(SelectionError::InvalidCase)?;
            Ok(vec![(index, config)])
        }
        _ => Err(SelectionError::UnknownMode),
    }
}

#[test]
fn each_case_preserves_its_full_plan_index_and_exact_configuration() {
    for (index, config) in full_plan().into_iter().enumerate() {
        assert_eq!(select("case", Some(&index.to_string())).unwrap(), vec![(index, config)]);
    }
}

#[test]
fn full_pilot_and_plan_cannot_be_silently_filtered() {
    for mode in ["plan", "pilot", "full"] {
        assert_eq!(select(mode, Some("0")), Err(SelectionError::UnexpectedCase));
    }
    assert_eq!(select("full", None).unwrap().len(), 144);
    assert_eq!(select("pilot", None).unwrap().len(), 5);
}

#[test]
fn malformed_missing_and_out_of_range_cases_fail_before_execution() {
    assert_eq!(select("case", None), Err(SelectionError::MissingCase));
    for value in [
        "",
        "-1",
        "+1",
        " 1",
        "1 ",
        "1,2",
        "0x1",
        "144",
        "999999999999999999999999999999",
    ] {
        assert_eq!(select("case", Some(value)), Err(SelectionError::InvalidCase));
    }
    assert_eq!(select("typo", None), Err(SelectionError::UnknownMode));
}
