// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use super::Quiet;

#[test]
fn a_server_that_served_nothing_is_never_trimmed() {
    let mut quiet = Quiet::default();
    assert!(!quiet.check(0, 0));
    assert!(!quiet.check(0, 0));
}

#[test]
fn a_burst_is_trimmed_once_at_the_first_check_with_no_new_request() {
    let mut quiet = Quiet::default();
    // Requests started and ended between checks.
    assert!(!quiet.check(40, 0));
    assert!(!quiet.check(90, 0));
    assert!(quiet.check(90, 0));
    assert!(!quiet.check(90, 0), "one trim per burst");
    assert!(!quiet.check(92, 0));
    assert!(quiet.check(92, 0), "the next burst is trimmed too");
}

#[test]
fn a_request_still_in_flight_holds_the_trim_until_it_ends() {
    let mut quiet = Quiet::default();
    // One request started (1 event) and outlasts several checks.
    assert!(!quiet.check(1, 1));
    assert!(!quiet.check(1, 1));
    assert!(!quiet.check(1, 1));
    // Its end is an event of its own.
    assert!(!quiet.check(2, 0));
    assert!(quiet.check(2, 0));
}
