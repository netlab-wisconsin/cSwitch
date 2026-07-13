use scx_rustland_la::host_map::{
    build_mapping, build_mapping_from_host_mapping, build_mapping_with_capacity,
    constrain_mapping_to_cpus, eligible_cpulist, parse_ccm_mapping,
    parse_ccm_mapping_with_capacity, parse_host_mapping, CcmMappingEntry, LLC_DOMAIN_TO_CCX,
};
use scx_rustland_la::types::{DomainInfo, TopologyLayout};
use std::collections::BTreeSet;

fn synthetic_topology() -> TopologyLayout {
    let mut domains = Vec::new();
    let mut cpu_to_domain = vec![None; 84];
    for domain_id in 0..12u32 {
        let base = domain_id * 7;
        let cpus: Vec<u32> = (base..base + 7).collect();
        for cpu in &cpus {
            cpu_to_domain[*cpu as usize] = Some(domain_id);
        }
        domains.push(DomainInfo {
            domain_id,
            kernel_l3_id: domain_id,
            rep_cpu: cpus[0],
            cpus,
            l3_size_mb: 32.0,
        });
    }
    TopologyLayout {
        nr_cpu_ids: 84,
        domains,
        cpu_to_domain,
    }
}

#[test]
fn host_mapping_separates_ccm_and_cs_capacities() {
    let path = std::env::temp_dir().join(format!("ccm_mapping_split_{}.txt", std::process::id()));
    let text = "\
ccm0
ccx0
capacity_mib_s 20000
ccm1
ccx1
capacity_mib_s 18000
missing
ccx2
cs0
capacity_mib_s 40000
cs11
capacity_mib_s 42000
cs16
capacity_mib_s 99000
";
    std::fs::write(&path, text).unwrap();

    let host_mapping = parse_host_mapping(&path).unwrap();
    assert_eq!(
        host_mapping
            .ccm_entries
            .get(&0)
            .map(|entry| entry.capacity_mib_s_x100),
        Some(2_000_000)
    );
    assert_eq!(
        host_mapping.cs_capacity_mib_s_x100.get(&0),
        Some(&4_000_000)
    );
    assert_eq!(
        host_mapping.cs_capacity_mib_s_x100.get(&11),
        Some(&4_200_000)
    );
    assert!(!host_mapping.cs_capacity_mib_s_x100.contains_key(&16));

    let mapping = build_mapping_from_host_mapping(&synthetic_topology(), &host_mapping).unwrap();
    assert_eq!(mapping.df_capacity_mib_s_x100(0), 2_000_000);
    assert_eq!(mapping.df_capacity_mib_s_x100(1), 1_800_000);
    assert_eq!(mapping.cs_capacity_mib_s_x100(0), 4_000_000);
    assert_eq!(mapping.cs_capacity_mib_s_x100(1), 2_000_000);
    assert_eq!(mapping.cs_capacity_mib_s_x100(11), 4_200_000);

    let _ = std::fs::remove_file(&path);
}

fn synthetic_topology_with_smt() -> TopologyLayout {
    let mut domains = Vec::new();
    let mut cpu_to_domain = vec![None; 168];
    for domain_id in 0..12u32 {
        let base = domain_id * 7;
        let mut cpus: Vec<u32> = (base..base + 7).collect();
        cpus.extend((base + 84)..(base + 91));
        for cpu in &cpus {
            cpu_to_domain[*cpu as usize] = Some(domain_id);
        }
        domains.push(DomainInfo {
            domain_id,
            kernel_l3_id: domain_id,
            rep_cpu: base,
            cpus,
            l3_size_mb: 32.0,
        });
    }
    TopologyLayout {
        nr_cpu_ids: 168,
        domains,
        cpu_to_domain,
    }
}

#[test]
fn parse_ccm_mapping_ignores_ccm8_and_requires_eight_entries() {
    let path = std::env::temp_dir().join(format!("ccm_mapping_{}.txt", std::process::id()));
    let text = "\
ccm0
ccx0
capacity_mib_s 24000
ccm1
ccx9
ccm2
ccx3
ccm3
ccx6
ccm4
ccx1
ccm5
ccx10
ccm6
ccx4
ccm7
ccx7
missing
ccx2
ccm8
ccx12
";
    std::fs::write(&path, text).unwrap();
    let mapping = parse_ccm_mapping(&path).unwrap();

    assert_eq!(mapping.len(), 8);
    assert_eq!(mapping.get(&0), Some(&0));
    assert_eq!(mapping.get(&7), Some(&7));
    assert!(!mapping.contains_key(&8));

    let mapping = parse_ccm_mapping_with_capacity(&path).unwrap();
    assert_eq!(
        mapping.get(&0),
        Some(&CcmMappingEntry {
            ccx_id: 0,
            capacity_mib_s_x100: 2_400_000
        })
    );
    assert_eq!(
        mapping.get(&1),
        Some(&CcmMappingEntry {
            ccx_id: 9,
            capacity_mib_s_x100: 2_000_000
        })
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn build_mapping_marks_only_mapped_ccxs_as_eligible() {
    let topo = synthetic_topology();
    let ccm_to_ccx = std::collections::BTreeMap::from([
        (0, 0),
        (1, 1),
        (2, 2),
        (3, 3),
        (4, 4),
        (5, 5),
        (6, 6),
        (7, 7),
    ]);

    let mapping = build_mapping(&topo, &ccm_to_ccx).unwrap();
    assert_eq!(mapping.domain_to_ccx, LLC_DOMAIN_TO_CCX);
    assert_eq!(
        mapping.eligible_domains,
        BTreeSet::from([0, 1, 2, 3, 4, 5, 6, 7])
    );
    assert_eq!(mapping.excluded_domains, BTreeSet::from([8, 9, 10, 11]));
    assert_eq!(mapping.domain_to_ccm[0], Some(0));
    assert_eq!(mapping.domain_to_ccm[8], None);
    assert_eq!(mapping.domain_to_df_capacity_mib_s_x100[0], Some(2_000_000));
    assert_eq!(mapping.domain_to_df_capacity_mib_s_x100[8], None);
    assert_eq!(eligible_cpulist(&mapping), "0-55");
}

#[test]
fn build_mapping_carries_per_ccm_capacity_to_domains() {
    let topo = synthetic_topology();
    let ccm_mapping = std::collections::BTreeMap::from([
        (
            0,
            CcmMappingEntry {
                ccx_id: 0,
                capacity_mib_s_x100: 2_400_000,
            },
        ),
        (
            1,
            CcmMappingEntry {
                ccx_id: 1,
                capacity_mib_s_x100: 1_800_000,
            },
        ),
    ]);

    let mapping = build_mapping_with_capacity(&topo, &ccm_mapping).unwrap();

    assert_eq!(mapping.domain_to_ccm[0], Some(0));
    assert_eq!(mapping.domain_to_ccm[1], Some(1));
    assert_eq!(mapping.df_capacity_mib_s_x100(0), 2_400_000);
    assert_eq!(mapping.df_capacity_mib_s_x100(1), 1_800_000);
    assert_eq!(mapping.df_capacity_mib_s_x100(8), 2_000_000);
}

#[test]
fn constrain_mapping_to_primary_host_cpus_keeps_only_0_to_83() {
    let topo = synthetic_topology_with_smt();
    let ccm_to_ccx = std::collections::BTreeMap::from([
        (0, 0),
        (1, 1),
        (2, 2),
        (3, 3),
        (4, 4),
        (5, 5),
        (6, 6),
        (7, 7),
        (8, 8),
        (9, 9),
        (10, 10),
        (11, 11),
    ]);

    let mut mapping = build_mapping(&topo, &ccm_to_ccx).unwrap();
    let allowed = (0u32..84).collect::<BTreeSet<_>>();
    constrain_mapping_to_cpus(&topo, &mut mapping, &allowed);

    assert_eq!(eligible_cpulist(&mapping), "0-83");
    assert_eq!(mapping.eligible_domains, (0u32..12).collect());
}

#[test]
fn constrain_mapping_drops_domains_with_no_remaining_allowed_cpu() {
    let topo = synthetic_topology_with_smt();
    let ccm_to_ccx = std::collections::BTreeMap::from([
        (0, 0),
        (1, 1),
        (2, 2),
        (3, 3),
        (4, 4),
        (5, 5),
        (6, 6),
        (7, 7),
        (8, 8),
        (9, 9),
        (10, 10),
        (11, 11),
    ]);

    let mut mapping = build_mapping(&topo, &ccm_to_ccx).unwrap();
    let allowed = (0u32..77).collect::<BTreeSet<_>>();
    constrain_mapping_to_cpus(&topo, &mut mapping, &allowed);

    assert!(!mapping.eligible_domains.contains(&11));
    assert!(mapping.excluded_domains.contains(&11));
}
