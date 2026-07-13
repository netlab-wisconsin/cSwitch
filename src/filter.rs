use crate::types::{FilteredMetric, HotCoolState};

#[derive(Clone, Copy, Debug)]
pub struct FilterConfig {
    pub alpha_pct: u32,
    pub hot_pct_x100: u32,
    pub cool_pct_x100: u32,
    pub hot_persist: u32,
    pub cool_persist: u32,
}

pub fn update_metric(
    prev: &FilteredMetric,
    raw_x100: u32,
    sample_ts_ns: u64,
    cfg: FilterConfig,
) -> FilteredMetric {
    let alpha = cfg.alpha_pct.min(100);
    let ewma_x100 = if !prev.valid || prev.sample_ts_ns == 0 {
        raw_x100
    } else {
        ((alpha * raw_x100) + ((100 - alpha) * prev.ewma_x100)) / 100
    };

    let mut next = FilteredMetric {
        raw_x100,
        ewma_x100,
        state: prev.state,
        hot_count: prev.hot_count,
        cool_count: prev.cool_count,
        sample_ts_ns,
        valid: true,
    };

    if ewma_x100 >= cfg.hot_pct_x100 {
        next.hot_count = next.hot_count.saturating_add(1);
        next.cool_count = 0;
        if next.hot_count >= cfg.hot_persist {
            next.state = HotCoolState::Hot;
        }
    } else if ewma_x100 <= cfg.cool_pct_x100 {
        next.cool_count = next.cool_count.saturating_add(1);
        next.hot_count = 0;
        if next.cool_count >= cfg.cool_persist {
            next.state = HotCoolState::Cool;
        }
    } else {
        next.hot_count = next.hot_count.min(cfg.hot_persist.saturating_sub(1));
        next.cool_count = next.cool_count.min(cfg.cool_persist.saturating_sub(1));
    }

    next
}

pub fn is_stale(sample_ts_ns: u64, now_ns: u64, stale_ms: u64) -> bool {
    if sample_ts_ns == 0 || now_ns < sample_ts_ns {
        return true;
    }
    now_ns - sample_ts_ns > stale_ms.saturating_mul(1_000_000)
}
