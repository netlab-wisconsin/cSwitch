use crate::filter::{update_metric, FilterConfig};
use crate::overhead_profile::ThreadClockSample;
use crate::types::{DfSampleUpdate, FilteredMetric, MappingInfo};
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crossbeam_channel::Sender;

const MASK_48: u64 = (1u64 << 48) - 1;

const MSR_DF_PERF_CTL_0: u64 = 0xC0010240;
const MSR_DF_PERF_CTR_0: u64 = 0xC0010241;
const MSR_DF_PERF_CTL_1: u64 = 0xC0010242;
const MSR_DF_PERF_CTR_1: u64 = 0xC0010243;

#[derive(Clone, Copy)]
pub struct DfSamplerConfig {
    pub window_ms: u64,
    pub filter: FilterConfig,
}

#[derive(Clone, Copy, Debug)]
pub enum DfSamplerEvent {
    Sample(DfSampleUpdate),
    SweepComplete { epoch: u64, sampled_domains: usize },
}

#[derive(Clone, Copy)]
struct CcmResource {
    domain_id: u32,
    ccx_id: u32,
    ccm_id: u32,
    instance_id: u32,
    capacity_mib_s_x100: u32,
}

fn pack_df_perf_ctl(event_sel: u32, unit_mask: u32, en: u32) -> u64 {
    ((((event_sel >> 12) & 0x3) as u64) << 36)
        | ((((event_sel >> 8) & 0xF) as u64) << 32)
        | ((((unit_mask >> 8) & 0xF) as u64) << 24)
        | (((en & 0x1) as u64) << 22)
        | (((unit_mask & 0xFF) as u64) << 8)
        | ((event_sel & 0xFF) as u64)
}

fn wrmsr(fd: RawFd, msr: u64, value: u64) -> std::io::Result<()> {
    // SAFETY: `fd` must be an open MSR device descriptor owned by the sampler thread. The
    // x86 MSR character device expects a seek to the register offset followed by an 8-byte
    // read or write.
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
    // SAFETY: same assumptions as `wrmsr()`: the caller provides a live MSR descriptor and
    // the kernel fills exactly one 64-bit counter value for the requested offset.
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

fn disable_counters(fd: RawFd) {
    let _ = wrmsr(fd, MSR_DF_PERF_CTL_0, 0);
    let _ = wrmsr(fd, MSR_DF_PERF_CTL_1, 0);
}

pub fn beats_to_mib_s_x100(beats: u64, bytes_per_beat: u32, elapsed_secs: f64) -> u32 {
    if elapsed_secs <= 0.0 {
        return 0;
    }
    (((beats as f64) * bytes_per_beat as f64 / (1024.0 * 1024.0) / elapsed_secs) * 100.0) as u32
}

pub fn pressure_from_bw_x100(
    read_mib_s_x100: u32,
    write_mib_s_x100: u32,
    capacity_mib_s_x100: u32,
) -> u32 {
    (((read_mib_s_x100 + write_mib_s_x100) as u64 * 10_000) / capacity_mib_s_x100.max(1) as u64)
        .min(10_000) as u32
}

fn sample_resource(
    fd: RawFd,
    resource: CcmResource,
    cfg: DfSamplerConfig,
) -> std::io::Result<(u32, u32, u32)> {
    let profiler = crate::overhead_profile::global();
    let total_start = profiler.as_ref().map(|_| ThreadClockSample::capture());
    let result = (|| -> std::io::Result<(u32, u32, u32)> {
        let _msr_guard = crate::df_msr::lock();
        let event_sel = (resource.instance_id << 6) | 0x1e;
        let read_cfg = pack_df_perf_ctl(event_sel, 0xffe, 1);
        let write_cfg = pack_df_perf_ctl(event_sel, 0xfff, 1);

        let setup_start = profiler.as_ref().map(|_| ThreadClockSample::capture());
        wrmsr(fd, MSR_DF_PERF_CTL_0, read_cfg)?;
        wrmsr(fd, MSR_DF_PERF_CTL_1, write_cfg)?;
        wrmsr(fd, MSR_DF_PERF_CTR_0, 0)?;
        wrmsr(fd, MSR_DF_PERF_CTR_1, 0)?;
        if let (Some(profiler), Some(start)) = (&profiler, setup_start) {
            profiler.record_scope("df_ccm_sample_resource_setup", start, true);
        }

        let start = std::time::Instant::now();
        let sleep_start = profiler.as_ref().map(|_| ThreadClockSample::capture());
        thread::sleep(Duration::from_millis(cfg.window_ms.max(1)));
        if let (Some(profiler), Some(start)) = (&profiler, sleep_start) {
            profiler.record_scope("df_ccm_sample_resource_window_sleep", start, true);
        }
        let readout_start = profiler.as_ref().map(|_| ThreadClockSample::capture());
        let read_beats = rdmsr(fd, MSR_DF_PERF_CTR_0)? & MASK_48;
        let write_beats = rdmsr(fd, MSR_DF_PERF_CTR_1)? & MASK_48;
        let elapsed = start.elapsed().as_secs_f64().max(1e-9);

        disable_counters(fd);

        let read_mib_s_x100 = beats_to_mib_s_x100(read_beats, 32, elapsed);
        let write_mib_s_x100 = beats_to_mib_s_x100(write_beats, 64, elapsed);
        let raw_pressure_x100 = pressure_from_bw_x100(
            read_mib_s_x100,
            write_mib_s_x100,
            resource.capacity_mib_s_x100,
        );
        if let (Some(profiler), Some(start)) = (&profiler, readout_start) {
            profiler.record_scope("df_ccm_sample_resource_readout", start, true);
        }

        Ok((read_mib_s_x100, write_mib_s_x100, raw_pressure_x100))
    })();
    if let (Some(profiler), Some(start)) = (profiler, total_start) {
        profiler.record_scope("df_ccm_sample_resource_total", start, result.is_ok());
    }
    result
}

fn resources_from_mapping(mapping: &MappingInfo) -> Vec<CcmResource> {
    mapping
        .domain_to_ccm
        .iter()
        .enumerate()
        .filter_map(|(domain_idx, ccm_id)| {
            ccm_id.map(|ccm_id| CcmResource {
                domain_id: domain_idx as u32,
                ccx_id: mapping.domain_to_ccx[domain_idx],
                ccm_id,
                instance_id: 0x10 + ccm_id,
                capacity_mib_s_x100: mapping.df_capacity_mib_s_x100(domain_idx as u32),
            })
        })
        .collect()
}

fn maybe_pin_current_thread(cpu: Option<u32>) -> std::io::Result<()> {
    let Some(cpu) = cpu else {
        return Ok(());
    };
    let mut cpuset = unsafe { std::mem::zeroed::<libc::cpu_set_t>() };
    unsafe {
        libc::CPU_ZERO(&mut cpuset);
        libc::CPU_SET(cpu as usize, &mut cpuset);
    }
    let ret =
        unsafe { libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &cpuset) };
    if ret != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

pub fn start(
    mapping: MappingInfo,
    cfg: DfSamplerConfig,
    stop: Arc<AtomicBool>,
    tx: Sender<DfSamplerEvent>,
    pin_cpu: Option<u32>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        if let Err(err) = maybe_pin_current_thread(pin_cpu) {
            log::warn!("df_ccm_sampler failed to pin control thread: {err}");
        }
        let resources = resources_from_mapping(&mapping);
        let path = "/dev/cpu/0/msr";
        let Ok(file) = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
        else {
            return;
        };
        let fd = file.into_raw_fd();
        let mut metrics = vec![FilteredMetric::default(); mapping.domain_to_ccm.len()];
        let mut epoch = 0u64;

        while !stop.load(Ordering::Relaxed) {
            let profiler = crate::overhead_profile::global();
            let sweep_start = profiler.as_ref().map(|_| ThreadClockSample::capture());
            let mut sampled_domains = 0usize;
            for resource in &resources {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let ts_ns = crate::types::now_ns();
                if let Ok((read_bw_mib_s_x100, write_bw_mib_s_x100, raw_pressure_x100)) =
                    sample_resource(fd, *resource, cfg)
                {
                    let metric = update_metric(
                        &metrics[resource.domain_id as usize],
                        raw_pressure_x100,
                        ts_ns,
                        cfg.filter,
                    );
                    metrics[resource.domain_id as usize] = metric;
                    sampled_domains = sampled_domains.saturating_add(1);
                    let _ = tx.send(DfSamplerEvent::Sample(DfSampleUpdate {
                        domain_id: resource.domain_id,
                        ccx_id: resource.ccx_id,
                        ccm_id: resource.ccm_id,
                        raw_read_bw_mib_s_x100: read_bw_mib_s_x100,
                        raw_write_bw_mib_s_x100: write_bw_mib_s_x100,
                        metric,
                    }));
                } else {
                    if let Some(profiler) = &profiler {
                        profiler.increment_counter("df_ccm_sample_resource_error");
                    }
                    disable_counters(fd);
                }
            }
            if let Some(profiler) = &profiler {
                profiler.add_counter("df_ccm_sweep_sampled_domains", sampled_domains as u64);
            }
            epoch = epoch.saturating_add(1);
            let _ = tx.send(DfSamplerEvent::SweepComplete {
                epoch,
                sampled_domains,
            });
            if let (Some(profiler), Some(start)) = (profiler, sweep_start) {
                profiler.record_scope("df_ccm_sweep_total", start, true);
            }
        }

        disable_counters(fd);
        // SAFETY: `fd` ownership was taken from `into_raw_fd()` and remains local to this
        // thread, so closing it here is the matching single close for that descriptor.
        unsafe {
            libc::close(fd);
        }
    })
}

use std::os::fd::IntoRawFd;
