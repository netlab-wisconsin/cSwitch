use scx_rustland_la::filter::{is_stale, update_metric, FilterConfig};
use scx_rustland_la::types::{FilteredMetric, HotCoolState};

fn cfg() -> FilterConfig {
    FilterConfig {
        alpha_pct: 30,
        hot_pct_x100: 7_000,
        cool_pct_x100: 5_500,
        hot_persist: 3,
        cool_persist: 3,
    }
}

#[test]
fn ewma_initializes_from_first_sample() {
    let metric = update_metric(&FilteredMetric::default(), 8_000, 1_000, cfg());
    assert_eq!(metric.raw_x100, 8_000);
    assert_eq!(metric.ewma_x100, 8_000);
    assert!(metric.valid);
}

#[test]
fn hot_transition_requires_persistence() {
    let mut metric = FilteredMetric::default();
    for idx in 0..2 {
        metric = update_metric(&metric, 8_000, 1_000 + idx, cfg());
        assert_eq!(metric.state, HotCoolState::Cool);
    }
    metric = update_metric(&metric, 8_000, 2_000, cfg());
    assert_eq!(metric.state, HotCoolState::Hot);
    assert_eq!(metric.hot_count, 3);
}

#[test]
fn cool_transition_requires_persistence() {
    let mut metric = FilteredMetric::default();
    for ts in 0..3 {
        metric = update_metric(&metric, 8_000, ts, cfg());
    }
    assert_eq!(metric.state, HotCoolState::Hot);

    for idx in 0..3 {
        metric = update_metric(&metric, 0, 10 + idx, cfg());
        assert_eq!(metric.state, HotCoolState::Hot);
    }
    metric = update_metric(&metric, 0, 20, cfg());
    assert_eq!(metric.state, HotCoolState::Cool);
    assert_eq!(metric.cool_count, 3);
}

#[test]
fn hysteresis_prevents_flapping_in_middle_band() {
    let mut metric = FilteredMetric::default();
    for ts in 0..3 {
        metric = update_metric(&metric, 8_000, ts, cfg());
    }
    assert_eq!(metric.state, HotCoolState::Hot);

    metric = update_metric(&metric, 4_000, 10, cfg());
    assert_eq!(metric.state, HotCoolState::Hot);
    assert!(metric.hot_count < cfg().hot_persist);
    assert!(metric.cool_count < cfg().cool_persist);
}

#[test]
fn stale_detection_uses_millisecond_threshold() {
    assert!(!is_stale(1_000_000, 40_000_000, 40));
    assert!(is_stale(1_000_000, 42_000_001, 40));
    assert!(is_stale(0, 1, 40));
}
