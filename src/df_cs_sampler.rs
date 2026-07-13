use crate::filter::{update_metric, FilterConfig};
use crate::overhead_profile::ThreadClockSample;
use crate::types::{DfSampleUpdate, FilteredMetric, MappingInfo, TopologyLayout, MAX_DOMAINS};
use std::os::fd::IntoRawFd;
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

#[derive(Clone, Copy)]
struct CsResource {
    domain_id: u32,
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

fn beats_to_mib_s_x100(beats: u64, bytes_per_beat: u32, elapsed_secs: f64) -> u32 {
    if elapsed_secs <= 0.0 {
        return 0;
    }
    (((beats as f64) * bytes_per_beat as f64 / (1024.0 * 1024.0) / elapsed_secs) * 100.0) as u32
}

fn pressure_from_bw_x100(
    read_mib_s_x100: u32,
    write_mib_s_x100: u32,
    capacity_mib_s_x100: u32,
) -> u32 {
    (((read_mib_s_x100 + write_mib_s_x100) as u64 * 10_000) / capacity_mib_s_x100.max(1) as u64)
        .min(10_000) as u32
}

fn sample_resource(
    fd: RawFd,
    resource: CsResource,
    cfg: DfSamplerConfig,
) -> std::io::Result<(u32, u32, u32)> {
    let profiler = crate::overhead_profile::global();
    let total_start = profiler.as_ref().map(|_| ThreadClockSample::capture());
    let result = (|| -> std::io::Result<(u32, u32, u32)> {
        let _msr_guard = crate::df_msr::lock();
        let event_sel = (resource.instance_id << 6) | 0x1f;
        let read_cfg = pack_df_perf_ctl(event_sel, 0xffe, 1);
        let write_cfg = pack_df_perf_ctl(event_sel, 0xfff, 1);

        let setup_start = profiler.as_ref().map(|_| ThreadClockSample::capture());
        wrmsr(fd, MSR_DF_PERF_CTL_0, read_cfg)?;
        wrmsr(fd, MSR_DF_PERF_CTL_1, write_cfg)?;
        wrmsr(fd, MSR_DF_PERF_CTR_0, 0)?;
        wrmsr(fd, MSR_DF_PERF_CTR_1, 0)?;
        if let (Some(profiler), Some(start)) = (&profiler, setup_start) {
            profiler.record_scope("df_cs_sample_resource_setup", start, true);
        }

        let start = std::time::Instant::now();
        let sleep_start = profiler.as_ref().map(|_| ThreadClockSample::capture());
        thread::sleep(Duration::from_millis(cfg.window_ms.max(1)));
        if let (Some(profiler), Some(start)) = (&profiler, sleep_start) {
            profiler.record_scope("df_cs_sample_resource_window_sleep", start, true);
        }
        let readout_start = profiler.as_ref().map(|_| ThreadClockSample::capture());
        let read_beats = rdmsr(fd, MSR_DF_PERF_CTR_0)? & MASK_48;
        let write_beats = rdmsr(fd, MSR_DF_PERF_CTR_1)? & MASK_48;
        let elapsed = start.elapsed().as_secs_f64().max(1e-9);

        disable_counters(fd);

        let read_mib_s_x100 = beats_to_mib_s_x100(read_beats, 64, elapsed);
        let write_mib_s_x100 = beats_to_mib_s_x100(write_beats, 64, elapsed);
        let raw_pressure_x100 = pressure_from_bw_x100(
            read_mib_s_x100,
            write_mib_s_x100,
            resource.capacity_mib_s_x100,
        );
        if let (Some(profiler), Some(start)) = (&profiler, readout_start) {
            profiler.record_scope("df_cs_sample_resource_readout", start, true);
        }

        Ok((read_mib_s_x100, write_mib_s_x100, raw_pressure_x100))
    })();
    if let (Some(profiler), Some(start)) = (profiler, total_start) {
        profiler.record_scope("df_cs_sample_resource_total", start, result.is_ok());
    }
    result
}

fn resources_from_topology(topo: &TopologyLayout, mapping: &MappingInfo) -> Vec<CsResource> {
    let link_count = topo
        .domains
        .len()
        .max(mapping.cs_link_count())
        .min(MAX_DOMAINS);
    (0..link_count as u32)
        .map(|link_id| CsResource {
            domain_id: link_id,
            instance_id: link_id,
            capacity_mib_s_x100: mapping.cs_capacity_mib_s_x100(link_id),
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
    topo: TopologyLayout,
    mapping: MappingInfo,
    cfg: DfSamplerConfig,
    stop: Arc<AtomicBool>,
    tx: Sender<DfSampleUpdate>,
    pin_cpu: Option<u32>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        if let Err(err) = maybe_pin_current_thread(pin_cpu) {
            log::warn!("df_cs_sampler failed to pin control thread: {err}");
        }
        let resources = resources_from_topology(&topo, &mapping);
        let path = "/dev/cpu/0/msr";
        let Ok(file) = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
        else {
            return;
        };
        let fd = file.into_raw_fd();
        let mut metrics = vec![FilteredMetric::default(); resources.len().min(MAX_DOMAINS)];

        while !stop.load(Ordering::Relaxed) {
            let profiler = crate::overhead_profile::global();
            let sweep_start = profiler.as_ref().map(|_| ThreadClockSample::capture());
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
                    let _ = tx.send(DfSampleUpdate {
                        domain_id: resource.domain_id,
                        ccx_id: resource.domain_id,
                        ccm_id: resource.instance_id,
                        raw_read_bw_mib_s_x100: read_bw_mib_s_x100,
                        raw_write_bw_mib_s_x100: write_bw_mib_s_x100,
                        metric,
                    });
                } else {
                    if let Some(profiler) = &profiler {
                        profiler.increment_counter("df_cs_sample_resource_error");
                    }
                    disable_counters(fd);
                }
            }
            if let (Some(profiler), Some(start)) = (profiler, sweep_start) {
                profiler.record_scope("df_cs_sweep_total", start, true);
            }
        }

        disable_counters(fd);
        unsafe {
            libc::close(fd);
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{DomainInfo, MappingInfo};
    use std::collections::BTreeSet;

    fn topology(domain_count: usize) -> TopologyLayout {
        TopologyLayout {
            nr_cpu_ids: domain_count,
            domains: (0..domain_count as u32)
                .map(|domain_id| DomainInfo {
                    domain_id,
                    kernel_l3_id: domain_id,
                    rep_cpu: domain_id,
                    cpus: vec![domain_id],
                    l3_size_mb: 32.0,
                })
                .collect(),
            cpu_to_domain: (0..domain_count as u32).map(Some).collect(),
        }
    }

    fn mapping() -> MappingInfo {
        MappingInfo {
            domain_to_ccx: (0..12).collect(),
            domain_to_ccm: vec![
                Some(0),
                Some(4),
                None,
                Some(2),
                Some(6),
                None,
                Some(3),
                Some(7),
                None,
                Some(1),
                Some(5),
                None,
            ],
            domain_to_df_capacity_mib_s_x100: vec![
                Some(10_000_00),
                Some(11_000_00),
                None,
                Some(12_000_00),
                Some(13_000_00),
                None,
                Some(14_000_00),
                Some(15_000_00),
                None,
                Some(16_000_00),
                Some(17_000_00),
                None,
            ],
            cs_link_capacity_mib_s_x100: vec![
                Some(30_000_00),
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                Some(40_000_00),
            ],
            eligible_domains: BTreeSet::new(),
            excluded_domains: BTreeSet::new(),
            eligible_cpus: BTreeSet::new(),
        }
    }

    #[test]
    fn cs_resources_cover_all_topology_cs_links_not_only_mapped_ccms() {
        let topo = topology(12);
        let mapping = mapping();

        let resources = resources_from_topology(&topo, &mapping);

        assert_eq!(resources.len(), 12);
        assert_eq!(
            resources
                .iter()
                .map(|resource| resource.instance_id)
                .collect::<Vec<_>>(),
            (0..12).collect::<Vec<_>>()
        );
        assert_eq!(resources[0].capacity_mib_s_x100, 30_000_00);
        assert_eq!(resources[8].capacity_mib_s_x100, 2_000_000);
        assert_eq!(resources[11].capacity_mib_s_x100, 40_000_00);
    }
}
