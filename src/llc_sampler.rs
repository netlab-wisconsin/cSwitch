use crate::filter::{update_metric, FilterConfig};
use crate::overhead_profile::ThreadClockSample;
use crate::types::{FilteredMetric, LlcSampleUpdate, TopologyLayout};
use std::os::fd::{IntoRawFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::Result;
use crossbeam_channel::Sender;

const MASK_48: u64 = (1u64 << 48) - 1;

const MSR_CHL3_PMC_CFG_0: u64 = 0xC0010230;
const MSR_CHL3_PMC_CTR_0: u64 = 0xC0010231;
const MSR_CHL3_PMC_STRIDE: u64 = 0x2;

const L3_EVT_LOOKUP_STATE: u64 = 0x04;
const L3_EVT_XI_SAMPLED_LAT: u64 = 0xAC;
const L3_EVT_XI_SAMPLED_LAT_REQ: u64 = 0xAD;

const UMASK_L3_LOOKUP_ALL: u64 = 0xFF;
const UMASK_L3_LOOKUP_MISS: u64 = 0x01;
const UMASK_LAT_NEAR_CACHE: u64 = 0x04;
const UMASK_LAT_DRAM_NEAR: u64 = 0x01;

const L3_CFG_TEMPLATE_ALL_CORES_ALL_SLICES: u64 = 0x303C00000400000;
const L3_CFG_CHECK_MASK: u64 = L3_CFG_TEMPLATE_ALL_CORES_ALL_SLICES | (0xFFu64 << 8) | 0xFFu64;

#[derive(Clone, Copy)]
pub struct LlcSamplerConfig {
    pub window_ms: u64,
    pub period_ms: u64,
    pub dram_hot_ns_x100: u32,
    pub miss_ppm_hot: u32,
    pub filter: FilterConfig,
}

#[derive(Clone, Copy, Debug)]
struct RawDomainSample {
    l3_to_l3_ns_x100: u32,
    dram_lat_ns_x100: u32,
    l3_bw_mib_s_x100: u32,
    miss_ratio_ppm: u32,
    l3_req_per_s: u64,
    l3_miss_per_s: u64,
    raw_pressure_x100: u32,
}

fn pack_l3_pmc_ctl(event_code: u64, umask: u64) -> u64 {
    L3_CFG_TEMPLATE_ALL_CORES_ALL_SLICES | ((umask & 0xFF) << 8) | (event_code & 0xFF)
}

fn scaling_factor_ns_x100(l3_size_mb: f64) -> u32 {
    if l3_size_mb >= 32.0 {
        1000
    } else {
        3000
    }
}

fn clamp_pressure_x100(value: u64, threshold: u32) -> u32 {
    if threshold == 0 {
        return 0;
    }
    ((value.saturating_mul(10_000)) / threshold as u64).min(10_000) as u32
}

fn wrmsr(fd: RawFd, msr: u64, value: u64) -> std::io::Result<()> {
    // SAFETY: `fd` is expected to be an open `/dev/cpu/<n>/msr` descriptor owned by this
    // sampler thread. We seek to the target MSR offset and write exactly eight bytes, which
    // is the kernel ABI for MSR access on x86.
    unsafe {
        if libc::lseek(fd, msr as libc::off_t, libc::SEEK_SET) < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let bytes = value.to_ne_bytes();
        let wrote = libc::write(fd, bytes.as_ptr().cast(), bytes.len());
        if wrote != bytes.len() as isize {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

fn rdmsr(fd: RawFd, msr: u64) -> std::io::Result<u64> {
    let mut bytes = [0u8; 8];
    // SAFETY: same contract as `wrmsr()`: `fd` must be a live MSR device descriptor and
    // the kernel returns exactly one 64-bit MSR value at the requested offset.
    unsafe {
        if libc::lseek(fd, msr as libc::off_t, libc::SEEK_SET) < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let read = libc::read(fd, bytes.as_mut_ptr().cast(), bytes.len());
        if read != bytes.len() as isize {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(u64::from_ne_bytes(bytes))
}

fn disable_l3_counters(fd: RawFd, n: usize) {
    for idx in 0..n {
        let _ = wrmsr(
            fd,
            MSR_CHL3_PMC_CFG_0 + (idx as u64 * MSR_CHL3_PMC_STRIDE),
            0,
        );
    }
}

fn sample_domain(
    fd: RawFd,
    l3_size_mb: f64,
    cfg: LlcSamplerConfig,
) -> Result<Option<RawDomainSample>> {
    let profiler = crate::overhead_profile::global();
    let total_start = profiler.as_ref().map(|_| ThreadClockSample::capture());
    let counter_count = 6usize;
    let actual_cfgs = [
        pack_l3_pmc_ctl(L3_EVT_XI_SAMPLED_LAT, UMASK_LAT_NEAR_CACHE),
        pack_l3_pmc_ctl(L3_EVT_XI_SAMPLED_LAT_REQ, UMASK_LAT_NEAR_CACHE),
        pack_l3_pmc_ctl(L3_EVT_XI_SAMPLED_LAT, UMASK_LAT_DRAM_NEAR),
        pack_l3_pmc_ctl(L3_EVT_XI_SAMPLED_LAT_REQ, UMASK_LAT_DRAM_NEAR),
        pack_l3_pmc_ctl(L3_EVT_LOOKUP_STATE, UMASK_L3_LOOKUP_ALL),
        pack_l3_pmc_ctl(L3_EVT_LOOKUP_STATE, UMASK_L3_LOOKUP_MISS),
    ];

    let result = (|| -> Result<Option<RawDomainSample>> {
        let setup_start = profiler.as_ref().map(|_| ThreadClockSample::capture());
        disable_l3_counters(fd, counter_count);
        for (idx, value) in actual_cfgs.into_iter().enumerate() {
            let cfg_msr = MSR_CHL3_PMC_CFG_0 + (idx as u64 * MSR_CHL3_PMC_STRIDE);
            wrmsr(fd, cfg_msr, value)?;
            wrmsr(
                fd,
                MSR_CHL3_PMC_CTR_0 + (idx as u64 * MSR_CHL3_PMC_STRIDE),
                0,
            )?;
            let programmed = rdmsr(fd, cfg_msr)?;
            if (programmed & L3_CFG_CHECK_MASK) != (value & L3_CFG_CHECK_MASK) {
                anyhow::bail!(
                    "L3 PMC cfg verify failed idx={} expected=0x{:x} got=0x{:x}",
                    idx,
                    value & L3_CFG_CHECK_MASK,
                    programmed & L3_CFG_CHECK_MASK
                );
            }
        }
        if let (Some(profiler), Some(start)) = (&profiler, setup_start) {
            profiler.record_scope("llc_sample_domain_setup", start, true);
        }

        let start = std::time::Instant::now();
        let sleep_start = profiler.as_ref().map(|_| ThreadClockSample::capture());
        thread::sleep(Duration::from_millis(cfg.window_ms.max(1)));
        if let (Some(profiler), Some(start)) = (&profiler, sleep_start) {
            profiler.record_scope("llc_sample_domain_window_sleep", start, true);
        }

        let readout_start = profiler.as_ref().map(|_| ThreadClockSample::capture());
        for (idx, expected) in actual_cfgs.into_iter().enumerate() {
            let cfg_msr = MSR_CHL3_PMC_CFG_0 + (idx as u64 * MSR_CHL3_PMC_STRIDE);
            let current = rdmsr(fd, cfg_msr)?;
            if (current & L3_CFG_CHECK_MASK) != (expected & L3_CFG_CHECK_MASK) {
                anyhow::bail!(
                    "L3 PMC cfg clobbered idx={} expected=0x{:x} got=0x{:x}",
                    idx,
                    expected & L3_CFG_CHECK_MASK,
                    current & L3_CFG_CHECK_MASK
                );
            }
        }

        let near_acc = rdmsr(fd, MSR_CHL3_PMC_CTR_0)? & MASK_48;
        let near_req = rdmsr(fd, MSR_CHL3_PMC_CTR_0 + MSR_CHL3_PMC_STRIDE)? & MASK_48;
        let dram_acc = rdmsr(fd, MSR_CHL3_PMC_CTR_0 + 2 * MSR_CHL3_PMC_STRIDE)? & MASK_48;
        let dram_req = rdmsr(fd, MSR_CHL3_PMC_CTR_0 + 3 * MSR_CHL3_PMC_STRIDE)? & MASK_48;
        let lookup_all = rdmsr(fd, MSR_CHL3_PMC_CTR_0 + 4 * MSR_CHL3_PMC_STRIDE)? & MASK_48;
        let lookup_miss = rdmsr(fd, MSR_CHL3_PMC_CTR_0 + 5 * MSR_CHL3_PMC_STRIDE)? & MASK_48;
        let elapsed_s = start.elapsed().as_secs_f64().max(1e-9);

        /*
         * Keep parity with dfmon behavior: when req/denominator counters are empty in
         * the window, treat the sample as invalid instead of publishing zeros.
         */
        if near_req == 0 || dram_req == 0 || lookup_all == 0 {
            if let Some(profiler) = &profiler {
                profiler.increment_counter("llc_sample_domain_invalid_sparse");
                if let Some(start) = readout_start {
                    profiler.record_scope("llc_sample_domain_readout", start, true);
                }
            }
            return Ok(None);
        }

        let scale_ns_x100 = scaling_factor_ns_x100(l3_size_mb) as u64;
        let l3_to_l3_ns_x100 = if near_req > 0 {
            (near_acc.saturating_mul(scale_ns_x100) / near_req) as u32
        } else {
            0
        };
        let dram_lat_ns_x100 = if dram_req > 0 {
            (dram_acc.saturating_mul(scale_ns_x100) / dram_req) as u32
        } else {
            0
        };
        let miss_ratio_ppm = if lookup_all > 0 {
            ((lookup_miss.saturating_mul(1_000_000)) / lookup_all).min(1_000_000) as u32
        } else {
            0
        };
        let _dram_pressure = clamp_pressure_x100(
            (dram_lat_ns_x100.max(10000) - 10000) as u64,
            cfg.dram_hot_ns_x100,
        );
        // For pressure, use miss rate (misses per second) scaled as ppm-like integer,
        // not miss ratio. This behaves similarly to bandwidth (rate over time).
        let miss_ppm = ((lookup_miss as f64 / elapsed_s).min(u32::MAX as f64)) as u32;
        let miss_pressure = clamp_pressure_x100(miss_ppm as u64, cfg.miss_ppm_hot);
        let raw_pressure = _dram_pressure.max(miss_pressure);
        // Approximate L3 miss bandwidth from miss lookups:
        //   BW = (delta_lookup_miss / delta_t) * cacheline_size(64B).
        let l3_bw_mib_s_x100 =
            ((lookup_miss as f64 * 64.0) / (1024.0 * 1024.0) / elapsed_s * 100.0) as u32;
        let l3_req_per_s = (lookup_all as f64 / elapsed_s) as u64;
        let l3_miss_per_s = (lookup_miss as f64 / elapsed_s) as u64;
        if let (Some(profiler), Some(start)) = (&profiler, readout_start) {
            profiler.record_scope("llc_sample_domain_readout", start, true);
        }

        Ok(Some(RawDomainSample {
            l3_to_l3_ns_x100,
            dram_lat_ns_x100,
            l3_bw_mib_s_x100,
            miss_ratio_ppm,
            l3_req_per_s,
            l3_miss_per_s,
            raw_pressure_x100: raw_pressure,
        }))
    })();

    disable_l3_counters(fd, counter_count);
    if let Some(profiler) = &profiler {
        if let Err(err) = &result {
            if err.to_string().contains("clobbered") {
                profiler.increment_counter("llc_sample_domain_msr_clobber_fail");
            } else {
                profiler.increment_counter("llc_sample_domain_sample_error");
            }
        }
    }
    if let (Some(profiler), Some(start)) = (profiler, total_start) {
        profiler.record_scope("llc_sample_domain_total", start, result.is_ok());
    }
    result
}

pub fn start(
    topo: TopologyLayout,
    cfg: LlcSamplerConfig,
    stop: Arc<AtomicBool>,
    tx: Sender<LlcSampleUpdate>,
) -> Vec<thread::JoinHandle<()>> {
    topo.domains
        .into_iter()
        .map(|domain| {
            let stop = Arc::clone(&stop);
            let tx = tx.clone();
            thread::spawn(move || {
                let mut metric = FilteredMetric::default();
                let path = format!("/dev/cpu/{}/msr", domain.rep_cpu);
                let Ok(file) = std::fs::OpenOptions::new().read(true).write(true).open(&path) else {
                    log::warn!("llc_sampler domain={} failed to open {}", domain.domain_id, path);
                    return;
                };
                let fd = file.into_raw_fd();
                while !stop.load(Ordering::Relaxed) {
                    let sample_ts_ns = crate::types::now_ns();
                    match sample_domain(fd, domain.l3_size_mb, cfg) {
                        Ok(Some(raw)) => {
                            metric =
                                update_metric(&metric, raw.raw_pressure_x100, sample_ts_ns, cfg.filter);
                            let _ = tx.send(LlcSampleUpdate {
                                domain_id: domain.domain_id,
                                raw_l3_to_l3_ns_x100: raw.l3_to_l3_ns_x100,
                                raw_dram_lat_ns_x100: raw.dram_lat_ns_x100,
                                raw_l3_bw_mib_s_x100: raw.l3_bw_mib_s_x100,
                                raw_miss_ratio_ppm: raw.miss_ratio_ppm,
                                raw_l3_req_per_s: raw.l3_req_per_s,
                                raw_l3_miss_per_s: raw.l3_miss_per_s,
                                metric,
                            });
                        }
                        Ok(None) => {
                            /* Sparse window: keep previous metric and don't publish zeros. */
                        }
                        Err(_) => {
                            log::warn!(
                                "llc_sampler domain={} cpu={} sample failed (possible MSR contention)",
                                domain.domain_id,
                                domain.rep_cpu
                            );
                        }
                    }

                    let sleep_ms = cfg.period_ms.saturating_sub(cfg.window_ms).max(1);
                    thread::sleep(Duration::from_millis(sleep_ms));
                }
                disable_l3_counters(fd, 6);
                // SAFETY: `fd` ownership comes from `into_raw_fd()` above and remains local
                // to this sampler thread. It is closed exactly once here.
                unsafe {
                    libc::close(fd);
                }
            })
        })
        .collect()
}
