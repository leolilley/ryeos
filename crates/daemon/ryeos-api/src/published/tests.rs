use super::*;

#[test]
fn the_table_matches_only_its_three_routes() {
    let hash = "a".repeat(64);
    let id = "1".repeat(32);
    let (route, captures) = match_route("/published").unwrap();
    assert_eq!(route, PublishedRoute::List);
    assert!(captures.is_empty());
    assert_eq!(match_route("/published/").unwrap().0, PublishedRoute::List);
    let (route, captures) = match_route(&format!("/published/{id}")).unwrap();
    assert_eq!(route, PublishedRoute::Describe);
    assert_eq!(captures["id"], id);
    let (route, captures) = match_route(&format!("/published/{id}/blob/{hash}")).unwrap();
    assert_eq!(route, PublishedRoute::Blob);
    assert_eq!(
        (captures["id"].as_str(), captures["hash"].as_str()),
        (id.as_str(), hash.as_str())
    );
    for path in [
        format!("/published/{id}/blob"),
        format!("/published/{id}/blob/{hash}/more"),
        format!("/published/{id}/other/{hash}"),
        format!("/published//blob/{hash}"),
        "/publishedx".to_owned(),
        "/ui/api/published".to_owned(),
    ] {
        assert!(match_route(&path).is_none(), "{path}");
    }
}

#[test]
fn only_the_published_prefix_is_owned_by_the_table() {
    assert!(is_published_path("/published"));
    assert!(is_published_path("/published/abc"));
    assert!(!is_published_path("/publishedx"));
    assert!(!is_published_path("/public-key"));
    assert!(!is_published_path("/"));
}

#[test]
fn the_cap_reserves_before_serving_and_rolls_over_24_hours() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("served.ledger");
    let t0 = 1_800_000_000_000_i64;
    let mut ledger = ByteLedger::load(path.clone(), t0);
    assert_eq!(ledger.reserve(t0, 600, 1000).unwrap(), Ok(()));
    assert_eq!(ledger.reserve(t0 + 1, 400, 1000).unwrap(), Ok(()));
    // The next byte does not fit, and a refused reservation costs nothing.
    assert_eq!(
        ledger.reserve(t0 + 2, 1, 1000).unwrap(),
        Err(CapExhausted {
            used: 1000,
            cap: 1000
        })
    );
    assert_eq!(ledger.used(t0 + 3), 1000);
    // A restart keeps the day.
    let mut reloaded = ByteLedger::load(path.clone(), t0 + 10);
    assert_eq!(reloaded.used(t0 + 10), 1000);
    assert!(reloaded.reserve(t0 + 10, 1, 1000).unwrap().is_err());
    // After the window, the first reservation frees first.
    assert_eq!(reloaded.used(t0 + DAY_MS), 400);
    assert_eq!(reloaded.reserve(t0 + DAY_MS, 600, 1000).unwrap(), Ok(()));
    assert!(reloaded.reserve(t0 + DAY_MS, 1, 1000).unwrap().is_err());
    // Past the day, everything is free.
    assert_eq!(
        ByteLedger::load(path, t0 + 3 * DAY_MS).used(t0 + 3 * DAY_MS),
        0
    );
}

#[test]
fn the_journal_is_compacted_without_losing_the_window() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("served.ledger");
    let t0 = 1_800_000_000_000_i64;
    let mut ledger = ByteLedger::load(path.clone(), t0);
    for step in 0..(COMPACT_AFTER_APPENDS as i64 + 5) {
        ledger.reserve(t0 + step, 1, u64::MAX).unwrap().unwrap();
    }
    let used = ledger.used(t0 + 100_000);
    assert_eq!(used, COMPACT_AFTER_APPENDS as u64 + 5);
    assert_eq!(
        ByteLedger::load(path, t0 + 100_000).used(t0 + 100_000),
        used
    );
}

#[test]
fn a_garbled_journal_line_is_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("served.ledger");
    std::fs::write(
        &path,
        "1800000000000 10\nnot a line\n1800000000001 5 extra\n",
    )
    .unwrap();
    let mut ledger = ByteLedger::load(path, 1_800_000_000_002);
    assert_eq!(ledger.used(1_800_000_000_002), 10);
}

#[test]
fn a_full_limiter_answers_publication_busy_and_frees_with_the_slot() {
    let limiter = Arc::new(tokio::sync::Semaphore::new(2));
    let first = take_slot(&limiter).unwrap();
    let _second = take_slot(&limiter).unwrap();
    let busy = take_slot(&limiter).unwrap_err();
    assert_eq!(busy.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(busy.code, "publication_busy");
    assert!(busy.retry_after);
    drop(first);
    assert!(take_slot(&limiter).is_ok());
}
