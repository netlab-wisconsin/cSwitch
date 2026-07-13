use crate::types::{
    cpus_to_cpulist, MappingInfo, TopologyLayout, DEFAULT_DF_CAPACITY_MIB_S_X100, MAX_DOMAINS,
};
use anyhow::{bail, Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

// pub const LLC_DOMAIN_TO_CCX: [u32; 12] = [0, 9, 3, 6, 1, 10, 4, 7, 2, 11, 5, 8];
pub const LLC_DOMAIN_TO_CCX: [u32; 12] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CcmMappingEntry {
    pub ccx_id: u32,
    pub capacity_mib_s_x100: u32,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct HostMapping {
    pub ccm_entries: BTreeMap<u32, CcmMappingEntry>,
    pub cs_capacity_mib_s_x100: BTreeMap<u32, u32>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MappingSection {
    Ccm(u32),
    Cs(u32),
    None,
}

pub fn parse_ccm_mapping(path: &Path) -> Result<BTreeMap<u32, u32>> {
    Ok(parse_ccm_mapping_with_capacity(path)?
        .into_iter()
        .map(|(ccm_id, entry)| (ccm_id, entry.ccx_id))
        .collect())
}

pub fn parse_ccm_mapping_with_capacity(path: &Path) -> Result<BTreeMap<u32, CcmMappingEntry>> {
    Ok(parse_host_mapping(path)?.ccm_entries)
}

pub fn parse_host_mapping(path: &Path) -> Result<HostMapping> {
    let text =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut current_section = MappingSection::None;
    let mut out = HostMapping::default();

    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("ccm") {
            if let Ok(ccm_id) = rest.parse::<u32>() {
                current_section = if ccm_id <= 7 {
                    MappingSection::Ccm(ccm_id)
                } else {
                    MappingSection::None
                };
            } else {
                current_section = MappingSection::None;
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("cs") {
            if let Ok(cs_id) = rest.parse::<u32>() {
                current_section = if cs_id < MAX_DOMAINS as u32 {
                    MappingSection::Cs(cs_id)
                } else {
                    MappingSection::None
                };
            } else {
                current_section = MappingSection::None;
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("ccx") {
            if let (MappingSection::Ccm(ccm_id), Ok(ccx_id)) =
                (current_section, rest.parse::<u32>())
            {
                out.ccm_entries.insert(
                    ccm_id,
                    CcmMappingEntry {
                        ccx_id,
                        capacity_mib_s_x100: DEFAULT_DF_CAPACITY_MIB_S_X100,
                    },
                );
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("capacity_mib_s") {
            let value = rest
                .trim()
                .parse::<u32>()
                .with_context(|| format!("invalid capacity_mib_s line: {line}"))?;
            if let MappingSection::Ccm(ccm_id) = current_section {
                if let Some(entry) = out.ccm_entries.get_mut(&ccm_id) {
                    entry.capacity_mib_s_x100 = value.saturating_mul(100);
                }
            } else if let MappingSection::Cs(cs_id) = current_section {
                out.cs_capacity_mib_s_x100
                    .insert(cs_id, value.saturating_mul(100));
            }
            continue;
        }
        if line == "missing" {
            current_section = MappingSection::None;
        }
    }

    Ok(out)
}

pub fn build_mapping(
    topo: &TopologyLayout,
    ccm_to_ccx: &BTreeMap<u32, u32>,
) -> Result<MappingInfo> {
    let ccm_mapping = ccm_to_ccx
        .iter()
        .map(|(&ccm_id, &ccx_id)| {
            (
                ccm_id,
                CcmMappingEntry {
                    ccx_id,
                    capacity_mib_s_x100: DEFAULT_DF_CAPACITY_MIB_S_X100,
                },
            )
        })
        .collect::<BTreeMap<_, _>>();
    build_mapping_with_capacity(topo, &ccm_mapping)
}

pub fn build_mapping_with_capacity(
    topo: &TopologyLayout,
    ccm_mapping: &BTreeMap<u32, CcmMappingEntry>,
) -> Result<MappingInfo> {
    build_mapping_with_capacities(topo, ccm_mapping, &BTreeMap::new())
}

pub fn build_mapping_from_host_mapping(
    topo: &TopologyLayout,
    host_mapping: &HostMapping,
) -> Result<MappingInfo> {
    build_mapping_with_capacities(
        topo,
        &host_mapping.ccm_entries,
        &host_mapping.cs_capacity_mib_s_x100,
    )
}

fn build_mapping_with_capacities(
    topo: &TopologyLayout,
    ccm_mapping: &BTreeMap<u32, CcmMappingEntry>,
    cs_capacity_mib_s_x100: &BTreeMap<u32, u32>,
) -> Result<MappingInfo> {
    if topo.domains.len() != LLC_DOMAIN_TO_CCX.len() {
        bail!(
            "this host mapping expects {} LLC domains, found {}",
            LLC_DOMAIN_TO_CCX.len(),
            topo.domains.len()
        );
    }

    let mut ccx_to_ccm = BTreeMap::new();
    let mut ccm_capacity = BTreeMap::new();
    for (&ccm_id, entry) in ccm_mapping {
        ccx_to_ccm.insert(entry.ccx_id, ccm_id);
        ccm_capacity.insert(ccm_id, entry.capacity_mib_s_x100);
    }

    let mut eligible_domains = BTreeSet::new();
    let mut excluded_domains = BTreeSet::new();
    let mut eligible_cpus = BTreeSet::new();
    let mut domain_to_ccm = Vec::with_capacity(topo.domains.len());
    let mut domain_to_df_capacity_mib_s_x100 = Vec::with_capacity(topo.domains.len());

    for (idx, &ccx_id) in LLC_DOMAIN_TO_CCX.iter().enumerate() {
        let domain_id = idx as u32;
        let maybe_ccm = ccx_to_ccm.get(&ccx_id).copied();
        if maybe_ccm.is_some() {
            eligible_domains.insert(domain_id);
            for cpu in &topo.domains[idx].cpus {
                eligible_cpus.insert(*cpu);
            }
        } else {
            excluded_domains.insert(domain_id);
        }
        domain_to_ccm.push(maybe_ccm);
        domain_to_df_capacity_mib_s_x100
            .push(maybe_ccm.and_then(|ccm_id| ccm_capacity.get(&ccm_id).copied()));
    }

    let configured_cs_link_count = cs_capacity_mib_s_x100
        .keys()
        .next_back()
        .map(|cs_id| *cs_id as usize + 1)
        .unwrap_or(0);
    let cs_link_count = topo
        .domains
        .len()
        .max(configured_cs_link_count)
        .min(MAX_DOMAINS);
    let mut cs_link_capacity_mib_s_x100 = vec![None; cs_link_count];
    for (&cs_id, &capacity_mib_s_x100) in cs_capacity_mib_s_x100 {
        if let Some(slot) = cs_link_capacity_mib_s_x100.get_mut(cs_id as usize) {
            *slot = Some(capacity_mib_s_x100);
        }
    }

    Ok(MappingInfo {
        domain_to_ccx: LLC_DOMAIN_TO_CCX.to_vec(),
        domain_to_ccm,
        domain_to_df_capacity_mib_s_x100,
        cs_link_capacity_mib_s_x100,
        eligible_domains,
        excluded_domains,
        eligible_cpus,
    })
}

fn parse_cpu_list(text: &str) -> BTreeSet<u32> {
    let mut cpus = BTreeSet::new();
    for part in text.trim().split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some((start, end)) = part.split_once('-') {
            let Ok(start) = start.trim().parse::<u32>() else {
                continue;
            };
            let Ok(end) = end.trim().parse::<u32>() else {
                continue;
            };
            for cpu in start..=end {
                cpus.insert(cpu);
            }
            continue;
        }
        if let Ok(cpu) = part.parse::<u32>() {
            cpus.insert(cpu);
        }
    }
    cpus
}

fn primary_sibling_cpu(cpu: u32) -> Result<u32> {
    let path = format!("/sys/devices/system/cpu/cpu{cpu}/topology/thread_siblings_list");
    let text =
        fs::read_to_string(&path).with_context(|| format!("failed to read sibling list {path}"))?;
    parse_cpu_list(&text)
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("no siblings listed in {path}"))
}

fn rebuild_domain_sets(topo: &TopologyLayout, mapping: &mut MappingInfo) {
    let mut eligible_domains = BTreeSet::new();
    let mut excluded_domains = BTreeSet::new();

    for (idx, maybe_ccm) in mapping.domain_to_ccm.iter().enumerate() {
        let domain_id = idx as u32;
        let has_cpu = topo
            .domains
            .get(idx)
            .map(|domain| {
                domain
                    .cpus
                    .iter()
                    .any(|cpu| mapping.eligible_cpus.contains(cpu))
            })
            .unwrap_or(false);
        if maybe_ccm.is_some() && has_cpu {
            eligible_domains.insert(domain_id);
        } else {
            excluded_domains.insert(domain_id);
        }
    }

    mapping.eligible_domains = eligible_domains;
    mapping.excluded_domains = excluded_domains;
}

pub fn constrain_mapping_to_cpus(
    topo: &TopologyLayout,
    mapping: &mut MappingInfo,
    allowed_cpus: &BTreeSet<u32>,
) {
    mapping.eligible_cpus = mapping
        .eligible_cpus
        .intersection(allowed_cpus)
        .copied()
        .collect();
    rebuild_domain_sets(topo, mapping);
}

pub fn apply_host_cpu_filters(
    topo: &TopologyLayout,
    mapping: &mut MappingInfo,
    managed_cpu_max: u32,
    primary_smt_only: bool,
) -> Result<()> {
    let mut allowed_cpus = BTreeSet::new();
    for &cpu in &mapping.eligible_cpus {
        if cpu > managed_cpu_max {
            continue;
        }
        if primary_smt_only && primary_sibling_cpu(cpu)? != cpu {
            continue;
        }
        allowed_cpus.insert(cpu);
    }
    constrain_mapping_to_cpus(topo, mapping, &allowed_cpus);
    if mapping.eligible_cpus.is_empty() {
        bail!(
            "host CPU filters removed every eligible CPU (managed_cpu_max={}, primary_smt_only={})",
            managed_cpu_max,
            primary_smt_only
        );
    }
    Ok(())
}

pub fn eligible_cpulist(mapping: &MappingInfo) -> String {
    cpus_to_cpulist(&mapping.eligible_cpus)
}
