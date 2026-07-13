use crate::types::{DomainInfo, TopologyLayout, MAX_CPU_IDS, MAX_DOMAINS};
use anyhow::{bail, Context, Result};
use scx_utils::{Topology, NR_CPU_IDS};
use std::path::PathBuf;
use std::sync::Arc;

fn parse_l3_size_mb(cpu: u32) -> f64 {
    let path = PathBuf::from(format!(
        "/sys/devices/system/cpu/cpu{cpu}/cache/index3/size"
    ));
    let Ok(text) = std::fs::read_to_string(path) else {
        return 0.0;
    };
    parse_l3_size_mb_text(&text)
}

pub fn parse_l3_size_mb_text(text: &str) -> f64 {
    let value = text.trim().to_ascii_uppercase();
    if let Some(num) = value.strip_suffix('K') {
        return num.parse::<f64>().unwrap_or(0.0) / 1024.0;
    }
    if let Some(num) = value.strip_suffix('M') {
        return num.parse::<f64>().unwrap_or(0.0);
    }
    if let Some(num) = value.strip_suffix('G') {
        return num.parse::<f64>().unwrap_or(0.0) * 1024.0;
    }
    value.parse::<f64>().unwrap_or(0.0)
}

pub fn discover() -> Result<TopologyLayout> {
    let topo = Topology::new().context("failed to query topology through scx_utils")?;
    let mut llcs: Vec<_> = topo.all_llcs.values().cloned().collect();

    llcs.sort_by_key(|llc: &Arc<_>| llc.all_cpus.keys().min().copied().unwrap_or(usize::MAX));
    if llcs.is_empty() {
        bail!("no LLC domains discovered");
    }
    if llcs.len() > MAX_DOMAINS {
        bail!("too many LLC domains: {}", llcs.len());
    }

    let nr_cpu_ids = (*NR_CPU_IDS).min(MAX_CPU_IDS);
    let mut cpu_to_domain = vec![None; nr_cpu_ids];
    let mut domains = Vec::with_capacity(llcs.len());

    for (domain_id, llc) in llcs.into_iter().enumerate() {
        let mut cpus: Vec<u32> = llc.all_cpus.keys().map(|cpu| *cpu as u32).collect();
        cpus.sort_unstable();
        let rep_cpu = *cpus.first().context("domain without CPUs")?;
        let kernel_l3_id = llc.kernel_id as u32;
        for &cpu in &cpus {
            if (cpu as usize) < cpu_to_domain.len() {
                cpu_to_domain[cpu as usize] = Some(domain_id as u32);
            }
        }
        domains.push(DomainInfo {
            domain_id: domain_id as u32,
            kernel_l3_id,
            rep_cpu,
            cpus,
            l3_size_mb: parse_l3_size_mb(rep_cpu),
        });
    }

    Ok(TopologyLayout {
        nr_cpu_ids,
        domains,
        cpu_to_domain,
    })
}
