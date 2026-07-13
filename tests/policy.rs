use scx_rustland_la::bpf::QueuedTask;
use scx_rustland_la::policy::{
    choose_placement, choose_placement_with_planned_cpus, task_signature_effect,
};
use scx_rustland_la::types::{
    pct_to_x100, CcmDfStateValue, CpuStateValue, DecisionReason, DomainInfo,
    DomainSignatureAggregate, HotCoolState, LlcStateValue, ManagedThreadState, MappingInfo,
    PlannedCpuState, PlannedDomainState, PolicyConfig, QueueTrigger, ThreadSignature, TickDecision,
    TopologyLayout, DEFAULT_SLICE_US,
};
use std::collections::{BTreeMap, BTreeSet};

fn topo() -> TopologyLayout {
    TopologyLayout {
        nr_cpu_ids: 4,
        domains: vec![
            DomainInfo {
                domain_id: 0,
                kernel_l3_id: 0,
                rep_cpu: 0,
                cpus: vec![0, 1],
                l3_size_mb: 32.0,
            },
            DomainInfo {
                domain_id: 1,
                kernel_l3_id: 1,
                rep_cpu: 2,
                cpus: vec![2, 3],
                l3_size_mb: 32.0,
            },
        ],
        cpu_to_domain: vec![Some(0), Some(0), Some(1), Some(1)],
    }
}

fn mapping() -> MappingInfo {
    MappingInfo {
        domain_to_ccx: vec![0, 1],
        domain_to_ccm: vec![Some(0), Some(1)],
        domain_to_df_capacity_mib_s_x100: vec![Some(2_000_000), Some(2_000_000)],
        cs_link_capacity_mib_s_x100: vec![],
        eligible_domains: BTreeSet::from([0, 1]),
        excluded_domains: BTreeSet::new(),
        eligible_cpus: BTreeSet::from([0, 1, 2, 3]),
    }
}

fn llc(llc_pressure_basis_points: u32, state: HotCoolState) -> Option<LlcStateValue> {
    Some(LlcStateValue {
        sample_ts_ns: 1_000_000,
        raw_l3_bw_mib_s_x100: llc_pressure_basis_points,
        raw_pressure_pct_x100: llc_pressure_basis_points,
        ewma_pressure_pct_x100: llc_pressure_basis_points,
        state: state as u32,
        valid: 1,
        ..LlcStateValue::default()
    })
}

fn df(df_read_centi_mib_per_s: u32) -> Option<CcmDfStateValue> {
    Some(CcmDfStateValue {
        sample_ts_ns: 1_000_000,
        raw_read_bw_mib_s_x100: df_read_centi_mib_per_s,
        raw_write_bw_mib_s_x100: 0,
        raw_pressure_pct_x100: df_read_centi_mib_per_s,
        ewma_pressure_pct_x100: df_read_centi_mib_per_s,
        state: HotCoolState::Cool as u32,
        valid: 1,
        ..CcmDfStateValue::default()
    })
}

fn task_with_bw(current_cpu: i32, current_domain: i32, ewma_l2_bw_mib_s_x100: u32) -> QueuedTask {
    QueuedTask {
        tid: 1001,
        tgid: 1000,
        current_cpu,
        current_domain,
        nr_cpus_allowed: 4,
        flags: 0,
        start_ts: 0,
        stop_ts: 0,
        exec_runtime: 0,
        weight: 100,
        vtime: 0,
        enq_cnt: 6,
        last_l2_bw_mib_s_x100: ewma_l2_bw_mib_s_x100,
        ewma_l2_bw_mib_s_x100,
        last_fill_bw_mib_s_x100: [0; 3],
        ewma_fill_bw_mib_s_x100: [0; 3],
        last_ipc_x1000: 4_000,
        ewma_ipc_x1000: 4_000,
        last_stall_pct_x100: 0,
        ewma_stall_pct_x100: 0,
        trigger: 0,
        tick_seq: 0,
        last_update_ns: 1_000_000,
        comm: [0; 16],
    }
}

fn task(current_cpu: i32, current_domain: i32) -> QueuedTask {
    task_with_bw(current_cpu, current_domain, 900_000)
}

fn tick_task(current_cpu: i32, current_domain: i32, ewma_l2_bw_mib_s_x100: u32) -> QueuedTask {
    let mut task = task_with_bw(current_cpu, current_domain, ewma_l2_bw_mib_s_x100);
    task.trigger = QueueTrigger::Tick as u32;
    task.tick_seq = 7;
    task
}

fn villain_reslice_task(
    current_cpu: i32,
    current_domain: i32,
    ewma_l2_bw_mib_s_x100: u32,
) -> QueuedTask {
    let mut task = tick_task(current_cpu, current_domain, ewma_l2_bw_mib_s_x100);
    task.trigger = QueueTrigger::VillainReslice as u32;
    task
}

fn set_task_identity(task: &mut QueuedTask, tid: u32, tgid: u32, comm: &str) {
    task.tid = tid as i32;
    task.tgid = tgid as i32;
    task.comm = [0; 16];
    for (idx, byte) in comm.as_bytes().iter().take(task.comm.len() - 1).enumerate() {
        task.comm[idx] = *byte as i8;
    }
}

fn cpu_states() -> Vec<CpuStateValue> {
    vec![
        CpuStateValue {
            domain_id: 0,
            idle: 0,
            cpu_dsq_depth: 2,
            current_tid: 11,
            last_update_ns: 0,
        },
        CpuStateValue {
            domain_id: 0,
            idle: 0,
            cpu_dsq_depth: 1,
            current_tid: 12,
            last_update_ns: 0,
        },
        CpuStateValue {
            domain_id: 1,
            idle: 1,
            cpu_dsq_depth: 0,
            current_tid: 0,
            last_update_ns: 0,
        },
        CpuStateValue {
            domain_id: 1,
            idle: 1,
            cpu_dsq_depth: 0,
            current_tid: 0,
            last_update_ns: 0,
        },
    ]
}

fn cpu_states_with_busy_peer_on_current_cpu() -> Vec<CpuStateValue> {
    vec![
        CpuStateValue {
            domain_id: 0,
            idle: 0,
            cpu_dsq_depth: 2,
            current_tid: 11,
            last_update_ns: 0,
        },
        CpuStateValue {
            domain_id: 0,
            idle: 0,
            cpu_dsq_depth: 1,
            current_tid: 12,
            last_update_ns: 0,
        },
        CpuStateValue {
            domain_id: 1,
            idle: 0,
            cpu_dsq_depth: 1,
            current_tid: 9999,
            last_update_ns: 0,
        },
        CpuStateValue {
            domain_id: 1,
            idle: 1,
            cpu_dsq_depth: 0,
            current_tid: 0,
            last_update_ns: 0,
        },
    ]
}

fn cpu_states_with_busy_current_and_no_idle_peer_on_current_domain() -> Vec<CpuStateValue> {
    vec![
        CpuStateValue {
            domain_id: 0,
            idle: 0,
            cpu_dsq_depth: 2,
            current_tid: 11,
            last_update_ns: 0,
        },
        CpuStateValue {
            domain_id: 0,
            idle: 0,
            cpu_dsq_depth: 1,
            current_tid: 12,
            last_update_ns: 0,
        },
        CpuStateValue {
            domain_id: 1,
            idle: 0,
            cpu_dsq_depth: 1,
            current_tid: 9999,
            last_update_ns: 0,
        },
        CpuStateValue {
            domain_id: 1,
            idle: 0,
            cpu_dsq_depth: 0,
            current_tid: 0,
            last_update_ns: 0,
        },
    ]
}

fn cpu_states_with_idle_sibling_on_current_domain() -> Vec<CpuStateValue> {
    vec![
        CpuStateValue {
            domain_id: 0,
            idle: 0,
            cpu_dsq_depth: 2,
            current_tid: 11,
            last_update_ns: 0,
        },
        CpuStateValue {
            domain_id: 0,
            idle: 0,
            cpu_dsq_depth: 1,
            current_tid: 12,
            last_update_ns: 0,
        },
        CpuStateValue {
            domain_id: 1,
            idle: 0,
            cpu_dsq_depth: 1,
            current_tid: 1001,
            last_update_ns: 0,
        },
        CpuStateValue {
            domain_id: 1,
            idle: 1,
            cpu_dsq_depth: 0,
            current_tid: 0,
            last_update_ns: 0,
        },
    ]
}

fn cpu_states_domain0_spinning_domain1_free() -> Vec<CpuStateValue> {
    vec![
        CpuStateValue {
            domain_id: 0,
            idle: 0,
            cpu_dsq_depth: 2,
            current_tid: 2001,
            last_update_ns: 0,
        },
        CpuStateValue {
            domain_id: 0,
            idle: 0,
            cpu_dsq_depth: 2,
            current_tid: 2002,
            last_update_ns: 0,
        },
        CpuStateValue {
            domain_id: 1,
            idle: 1,
            cpu_dsq_depth: 0,
            current_tid: 0,
            last_update_ns: 0,
        },
        CpuStateValue {
            domain_id: 1,
            idle: 0,
            cpu_dsq_depth: 1,
            current_tid: 3001,
            last_update_ns: 0,
        },
    ]
}

fn cpu_states_domain0_free_domain1_full() -> Vec<CpuStateValue> {
    vec![
        CpuStateValue {
            domain_id: 0,
            idle: 1,
            cpu_dsq_depth: 0,
            current_tid: 0,
            last_update_ns: 0,
        },
        CpuStateValue {
            domain_id: 0,
            idle: 1,
            cpu_dsq_depth: 0,
            current_tid: 0,
            last_update_ns: 0,
        },
        CpuStateValue {
            domain_id: 1,
            idle: 0,
            cpu_dsq_depth: 1,
            current_tid: 1001,
            last_update_ns: 0,
        },
        CpuStateValue {
            domain_id: 1,
            idle: 0,
            cpu_dsq_depth: 1,
            current_tid: 2002,
            last_update_ns: 0,
        },
    ]
}

fn pdf_current_task_on_domain0_with_idle_destination() -> Vec<CpuStateValue> {
    vec![
        CpuStateValue {
            domain_id: 0,
            idle: 0,
            cpu_dsq_depth: 0,
            current_tid: 1001,
            last_update_ns: 0,
        },
        CpuStateValue {
            domain_id: 0,
            idle: 1,
            cpu_dsq_depth: 0,
            current_tid: 0,
            last_update_ns: 0,
        },
        CpuStateValue {
            domain_id: 1,
            idle: 1,
            cpu_dsq_depth: 0,
            current_tid: 0,
            last_update_ns: 0,
        },
        CpuStateValue {
            domain_id: 1,
            idle: 1,
            cpu_dsq_depth: 0,
            current_tid: 0,
            last_update_ns: 0,
        },
    ]
}

fn planned_cpu_states_with_counts(counts: &[(u32, usize)]) -> Vec<PlannedCpuState> {
    let mut states = vec![PlannedCpuState::default(); 4];
    for &(cpu, count) in counts {
        let Some(state) = states.get_mut(cpu as usize) else {
            continue;
        };
        state.jobs = (0..count as u32).map(|idx| 10_000 + idx).collect();
    }
    states
}

fn next_seeded_u32(seed: &mut u64) -> u32 {
    *seed = seed
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    (*seed >> 32) as u32
}

fn policy() -> PolicyConfig {
    PolicyConfig {
        l2_need_mib_s_x100: 512 * 100,
        migrate_margin_x100: pct_to_x100(15),
        cpu_high_util_x100: pct_to_x100(85),
        cpu_rebalance_job_delta: 2,
        stall_victim_min_pct_x100: 5_000,
        stall_victim_delta_pct_x100: 1_500,
        llc_stale_ms: 100,
        df_stale_ms: 100,
        migrate_settle_ms: 80,
        signature_snapshots: true,
        cs_villain_throttle: true,
        tick_reeval_every: 1,
        tick_defer_max: 1,
        cs_villain_reslice_ns: 1_000_000,
        cs_villain_refill_divisor: 4,
        cs_villain_settle_ns: 20_000_000,
        cs_villain_release_samples: 2,
        tick_move_phase_mod: 2,
    }
}

fn aggressive_policy() -> PolicyConfig {
    let mut cfg = policy();
    cfg.migrate_margin_x100 = 0;
    cfg
}

fn domain_task_counts() -> Vec<u32> {
    vec![2, 1]
}

fn agg(df_centi_mib_per_s: u32, llc_pressure_basis_points: u32) -> DomainSignatureAggregate {
    DomainSignatureAggregate {
        df_pressure_x100: df_centi_mib_per_s,
        llc_pressure_x100: llc_pressure_basis_points,
        fill_bw_mib_s_x100: [0; 3],
        contributor_count: 1,
        stable_count: 1,
    }
}

fn domain_signature_sums() -> Vec<DomainSignatureAggregate> {
    vec![agg(8_000, 6_000), agg(2_000, 2_000)]
}

fn planned_states(
    counts: &[u32],
    aggregates: &[DomainSignatureAggregate],
) -> Vec<PlannedDomainState> {
    counts
        .iter()
        .enumerate()
        .map(|(idx, task_count)| {
            let aggregate = aggregates.get(idx).cloned().unwrap_or_default();
            PlannedDomainState {
                df_pressure_x100: aggregate.df_pressure_x100,
                llc_pressure_x100: aggregate.llc_pressure_x100,
                fill_bw_mib_s_x100: aggregate.fill_bw_mib_s_x100,
                contributor_count: aggregate.contributor_count,
                stable_count: aggregate.stable_count,
                task_count: *task_count,
                incoming_migrations: 0,
                outgoing_migrations: 0,
                migration_budget: 4,
            }
        })
        .collect()
}

fn stable_meta(df_centi_mib_per_s: u32, llc_pressure_basis_points: u32) -> ManagedThreadState {
    ManagedThreadState {
        signature: ThreadSignature {
            valid: true,
            stable: true,
            projected_df_pressure_x100: df_centi_mib_per_s,
            projected_llc_pressure_x100: llc_pressure_basis_points,
            ..ThreadSignature::default()
        },
        ..ManagedThreadState::default()
    }
}

fn stable_meta_with_domain_df(
    domain0_df_centi_mib_per_s: u32,
    domain1_df_centi_mib_per_s: u32,
    llc_pressure_basis_points: u32,
) -> ManagedThreadState {
    let mut meta = stable_meta(domain0_df_centi_mib_per_s, llc_pressure_basis_points);
    meta.signature.df_domain_delta_x100[0] = domain0_df_centi_mib_per_s;
    meta.signature.df_domain_delta_x100[1] = domain1_df_centi_mib_per_s;
    meta
}

#[derive(Clone, Debug)]
struct MockIoDomain {
    // Human-readable chiplet/CCD label used by tests, not scheduler input.
    label: &'static str,
    // Linux CPU ids that belong to this mock LLC/CCD domain.
    cpus: Vec<u32>,
    // Mock CCX/CCM ids copied into MappingInfo / CcmDfStateValue.
    ccx_id: u32,
    ccm_id: u32,
    // Live Data Fabric read bandwidth in centi-MiB/s; 100_00 means 100 MiB/s.
    live_df_read_centi_mib_per_s: u32,
    // Live Data Fabric write bandwidth in centi-MiB/s; 100_00 means 100 MiB/s.
    live_df_write_centi_mib_per_s: u32,
    // Live LLC pressure in basis points; 100_00 means 100%.
    llc_pressure_basis_points: u32,
    // Hot/cool state derived from llc_pressure_basis_points for classifying tasks.
    llc_state: HotCoolState,
    // Number of scheduler-managed active tasks already planned on this domain.
    managed_task_count: u32,
    // Managed-thread aggregate DF signature in centi-MiB/s.
    signature_df_centi_mib_per_s: u32,
    // Managed-thread aggregate LLC pressure signature in basis points.
    signature_llc_pressure_basis_points: u32,
    // Per-CPU planned job counts as (Linux CPU id, queued/planned jobs).
    planned_cpu_job_counts: Vec<(u32, usize)>,
}

#[derive(Clone, Debug)]
struct MockIoLoadMap {
    domains: Vec<MockIoDomain>,
}

const MAX_THREAD_DF_READ_CENTI_MIB_PER_S: u32 = 10_000_00;
const SERIES_FULLY_STRESSED_DF_READ_CENTI_MIB_PER_S: u32 = 25_000_00;

#[derive(Clone, Debug)]
struct MockThreadIoLoad {
    tid: u32,
    domain_label: &'static str,
    cpu: u32,
    // Per-thread Data Fabric read bandwidth in centi-MiB/s; max 10_000_00.
    df_read_centi_mib_per_s: u32,
    // Per-thread LLC pressure contribution in basis points.
    llc_pressure_basis_points: u32,
}

impl MockThreadIoLoad {
    fn task(&self, map: &MockIoLoadMap) -> QueuedTask {
        let current_domain = map.domain_id(self.domain_label);
        let mut task = task_with_bw(
            self.cpu as i32,
            current_domain as i32,
            self.df_read_centi_mib_per_s,
        );
        task.tid = self.tid as i32;
        task.tgid = self.tid as i32;
        task
    }

    fn meta(&self) -> ManagedThreadState {
        let mut meta = stable_meta(self.df_read_centi_mib_per_s, self.llc_pressure_basis_points);
        meta.tid = self.tid;
        meta.tgid = self.tid;
        meta.signature.sample_count = 2;
        meta.signature.confidence_x100 = 90_00;
        meta
    }
}

impl MockIoLoadMap {
    fn from_thread_loads(threads: &[MockThreadIoLoad]) -> Self {
        let mut map = Self {
            domains: vec![
                MockIoDomain {
                    label: "ccd0",
                    cpus: vec![0, 1],
                    ccx_id: 0,
                    ccm_id: 0,
                    live_df_read_centi_mib_per_s: 0,
                    live_df_write_centi_mib_per_s: 0,
                    llc_pressure_basis_points: 0,
                    llc_state: HotCoolState::Cool,
                    managed_task_count: 0,
                    signature_df_centi_mib_per_s: 0,
                    signature_llc_pressure_basis_points: 0,
                    planned_cpu_job_counts: vec![],
                },
                MockIoDomain {
                    label: "ccd1",
                    cpus: vec![7, 8],
                    ccx_id: 1,
                    ccm_id: 1,
                    live_df_read_centi_mib_per_s: 0,
                    live_df_write_centi_mib_per_s: 0,
                    llc_pressure_basis_points: 0,
                    llc_state: HotCoolState::Cool,
                    managed_task_count: 0,
                    signature_df_centi_mib_per_s: 0,
                    signature_llc_pressure_basis_points: 0,
                    planned_cpu_job_counts: vec![],
                },
                MockIoDomain {
                    label: "ccd3",
                    cpus: vec![21, 22],
                    ccx_id: 3,
                    ccm_id: 3,
                    live_df_read_centi_mib_per_s: 0,
                    live_df_write_centi_mib_per_s: 0,
                    llc_pressure_basis_points: 0,
                    llc_state: HotCoolState::Cool,
                    managed_task_count: 0,
                    signature_df_centi_mib_per_s: 0,
                    signature_llc_pressure_basis_points: 0,
                    planned_cpu_job_counts: vec![],
                },
                MockIoDomain {
                    label: "ccd4",
                    cpus: vec![28, 29],
                    ccx_id: 4,
                    ccm_id: 4,
                    live_df_read_centi_mib_per_s: 0,
                    live_df_write_centi_mib_per_s: 0,
                    llc_pressure_basis_points: 0,
                    llc_state: HotCoolState::Cool,
                    managed_task_count: 0,
                    signature_df_centi_mib_per_s: 0,
                    signature_llc_pressure_basis_points: 0,
                    planned_cpu_job_counts: vec![],
                },
            ],
        };
        let mut planned_cpu_counts_by_domain =
            vec![BTreeMap::<u32, usize>::new(); map.domains.len()];

        for thread in threads {
            assert!(
                thread.df_read_centi_mib_per_s <= MAX_THREAD_DF_READ_CENTI_MIB_PER_S,
                "mock thread {} exceeds 10,000 MiB/s: {} centi-MiB/s",
                thread.tid,
                thread.df_read_centi_mib_per_s
            );
            let domain_idx = map.domain_id(thread.domain_label) as usize;
            assert!(
                map.domains[domain_idx].cpus.contains(&thread.cpu),
                "mock thread {} uses CPU {} outside {}",
                thread.tid,
                thread.cpu,
                thread.domain_label
            );

            let domain = &mut map.domains[domain_idx];
            domain.live_df_read_centi_mib_per_s = domain
                .live_df_read_centi_mib_per_s
                .saturating_add(thread.df_read_centi_mib_per_s);
            domain.signature_df_centi_mib_per_s = domain
                .signature_df_centi_mib_per_s
                .saturating_add(thread.df_read_centi_mib_per_s);
            domain.llc_pressure_basis_points = domain
                .llc_pressure_basis_points
                .saturating_add(thread.llc_pressure_basis_points)
                .min(95_00);
            domain.signature_llc_pressure_basis_points = domain.llc_pressure_basis_points;
            domain.managed_task_count = domain.managed_task_count.saturating_add(1);
            *planned_cpu_counts_by_domain[domain_idx]
                .entry(thread.cpu)
                .or_insert(0) += 1;
        }

        for (domain_idx, planned_cpu_counts) in planned_cpu_counts_by_domain.into_iter().enumerate()
        {
            let domain = &mut map.domains[domain_idx];
            assert!(
                domain.live_df_read_centi_mib_per_s
                    <= SERIES_FULLY_STRESSED_DF_READ_CENTI_MIB_PER_S,
                "mock domain {} exceeds fully-stressed DF read level: {} centi-MiB/s",
                domain.label,
                domain.live_df_read_centi_mib_per_s
            );
            domain.llc_state = if domain.llc_pressure_basis_points >= 80_00 {
                HotCoolState::Hot
            } else {
                HotCoolState::Cool
            };
            domain.planned_cpu_job_counts = planned_cpu_counts.into_iter().collect();
        }

        map
    }

    fn cpu_states_for_thread_loads(&self, threads: &[MockThreadIoLoad]) -> Vec<CpuStateValue> {
        let topo = self.topo();
        let mut states = vec![CpuStateValue::default(); topo.nr_cpu_ids];
        for domain in &topo.domains {
            for &cpu in &domain.cpus {
                states[cpu as usize] = CpuStateValue {
                    domain_id: domain.domain_id,
                    idle: 1,
                    cpu_dsq_depth: 0,
                    current_tid: 0,
                    last_update_ns: 1_000_000,
                };
            }
        }

        for thread in threads {
            let domain_id = self.domain_id(thread.domain_label);
            let state = &mut states[thread.cpu as usize];
            state.domain_id = domain_id;
            state.idle = 0;
            state.cpu_dsq_depth = state.cpu_dsq_depth.saturating_add(1);
            if state.current_tid == 0 {
                state.current_tid = thread.tid;
            }
            state.last_update_ns = 1_000_000;
        }

        states
    }

    fn planned_cpu_states_for_thread_loads(
        &self,
        threads: &[MockThreadIoLoad],
    ) -> Vec<PlannedCpuState> {
        let mut states = vec![PlannedCpuState::default(); self.topo().nr_cpu_ids];
        for thread in threads {
            states[thread.cpu as usize].jobs.push(thread.tid);
        }
        states
    }

    fn from_thread_loads_on_full_ccd_topology(threads: &[MockThreadIoLoad]) -> Self {
        let mut map = Self {
            domains: vec![
                MockIoDomain {
                    label: "ccd0",
                    cpus: (0..=6).collect(),
                    ccx_id: 0,
                    ccm_id: 0,
                    live_df_read_centi_mib_per_s: 0,
                    live_df_write_centi_mib_per_s: 0,
                    llc_pressure_basis_points: 0,
                    llc_state: HotCoolState::Cool,
                    managed_task_count: 0,
                    signature_df_centi_mib_per_s: 0,
                    signature_llc_pressure_basis_points: 0,
                    planned_cpu_job_counts: vec![],
                },
                MockIoDomain {
                    label: "ccd1",
                    cpus: (7..=13).collect(),
                    ccx_id: 1,
                    ccm_id: 1,
                    live_df_read_centi_mib_per_s: 0,
                    live_df_write_centi_mib_per_s: 0,
                    llc_pressure_basis_points: 0,
                    llc_state: HotCoolState::Cool,
                    managed_task_count: 0,
                    signature_df_centi_mib_per_s: 0,
                    signature_llc_pressure_basis_points: 0,
                    planned_cpu_job_counts: vec![],
                },
                MockIoDomain {
                    label: "ccd3",
                    cpus: (21..=27).collect(),
                    ccx_id: 3,
                    ccm_id: 3,
                    live_df_read_centi_mib_per_s: 0,
                    live_df_write_centi_mib_per_s: 0,
                    llc_pressure_basis_points: 0,
                    llc_state: HotCoolState::Cool,
                    managed_task_count: 0,
                    signature_df_centi_mib_per_s: 0,
                    signature_llc_pressure_basis_points: 0,
                    planned_cpu_job_counts: vec![],
                },
                MockIoDomain {
                    label: "ccd4",
                    cpus: (28..=34).collect(),
                    ccx_id: 4,
                    ccm_id: 4,
                    live_df_read_centi_mib_per_s: 0,
                    live_df_write_centi_mib_per_s: 0,
                    llc_pressure_basis_points: 0,
                    llc_state: HotCoolState::Cool,
                    managed_task_count: 0,
                    signature_df_centi_mib_per_s: 0,
                    signature_llc_pressure_basis_points: 0,
                    planned_cpu_job_counts: vec![],
                },
            ],
        };
        let mut planned_cpu_counts_by_domain =
            vec![BTreeMap::<u32, usize>::new(); map.domains.len()];

        for thread in threads {
            assert!(
                thread.df_read_centi_mib_per_s <= MAX_THREAD_DF_READ_CENTI_MIB_PER_S,
                "mock thread {} exceeds 10,000 MiB/s: {} centi-MiB/s",
                thread.tid,
                thread.df_read_centi_mib_per_s
            );
            let domain_idx = map.domain_id(thread.domain_label) as usize;
            assert!(
                map.domains[domain_idx].cpus.contains(&thread.cpu),
                "mock thread {} uses CPU {} outside {}",
                thread.tid,
                thread.cpu,
                thread.domain_label
            );

            let domain = &mut map.domains[domain_idx];
            domain.live_df_read_centi_mib_per_s = domain
                .live_df_read_centi_mib_per_s
                .saturating_add(thread.df_read_centi_mib_per_s);
            domain.signature_df_centi_mib_per_s = domain
                .signature_df_centi_mib_per_s
                .saturating_add(thread.df_read_centi_mib_per_s);
            domain.llc_pressure_basis_points = domain
                .llc_pressure_basis_points
                .saturating_add(thread.llc_pressure_basis_points)
                .min(95_00);
            domain.signature_llc_pressure_basis_points = domain.llc_pressure_basis_points;
            domain.managed_task_count = domain.managed_task_count.saturating_add(1);
            *planned_cpu_counts_by_domain[domain_idx]
                .entry(thread.cpu)
                .or_insert(0) += 1;
        }

        for (domain_idx, planned_cpu_counts) in planned_cpu_counts_by_domain.into_iter().enumerate()
        {
            let domain = &mut map.domains[domain_idx];
            assert!(
                domain.live_df_read_centi_mib_per_s
                    <= SERIES_FULLY_STRESSED_DF_READ_CENTI_MIB_PER_S,
                "mock domain {} exceeds fully-stressed DF read level: {} centi-MiB/s",
                domain.label,
                domain.live_df_read_centi_mib_per_s
            );
            domain.llc_state = if domain.llc_pressure_basis_points >= 80_00 {
                HotCoolState::Hot
            } else {
                HotCoolState::Cool
            };
            domain.planned_cpu_job_counts = planned_cpu_counts.into_iter().collect();
        }

        map
    }

    // Builds the primary eval-style asymmetric IO scenario. ccd0 is the hot
    // source domain, ccd3 is the coolest free target, ccd4 is the next-best
    // legal fallback, and ccd1 is cool but already partially occupied.
    // Expected behavior in tests: prefer the lowest-pressure legal/free domain,
    // then respect affinity and free-slot constraints when that ideal target is
    // unavailable.
    fn eval_asymmetric() -> Self {
        Self {
            domains: vec![
                // Raw mock data: live DF read = 25,000 MiB/s, write = 0 MiB/s.
                // This is 100% of the assumed fully-stressed DF read level;
                // LLC pressure = 90%. Signature data mirrors the live load.
                MockIoDomain {
                    label: "ccd0",
                    cpus: vec![0, 1],
                    ccx_id: 0,
                    ccm_id: 0,
                    live_df_read_centi_mib_per_s: 25_000_00,
                    live_df_write_centi_mib_per_s: 0,
                    llc_pressure_basis_points: 90_00,
                    llc_state: HotCoolState::Hot,
                    managed_task_count: 2,
                    signature_df_centi_mib_per_s: 25_000_00,
                    signature_llc_pressure_basis_points: 90_00,
                    planned_cpu_job_counts: vec![(0, 1), (1, 1)],
                },
                // Raw mock data: live DF read = 14,000 MiB/s, write = 0 MiB/s,
                // about 56% of the 25,000 MiB/s full-stress read level.
                // LLC pressure = 30%. One CPU already has a planned job.
                MockIoDomain {
                    label: "ccd1",
                    cpus: vec![7, 8],
                    ccx_id: 1,
                    ccm_id: 1,
                    live_df_read_centi_mib_per_s: 14_000_00,
                    live_df_write_centi_mib_per_s: 0,
                    llc_pressure_basis_points: 30_00,
                    llc_state: HotCoolState::Cool,
                    managed_task_count: 1,
                    signature_df_centi_mib_per_s: 14_000_00,
                    signature_llc_pressure_basis_points: 30_00,
                    planned_cpu_job_counts: vec![(7, 1)],
                },
                // Raw mock data: live DF read = 4,000 MiB/s, write = 0 MiB/s,
                // about 16% of the 25,000 MiB/s full-stress read level.
                // LLC pressure = 10%. This is the coolest completely free CCD.
                MockIoDomain {
                    label: "ccd3",
                    cpus: vec![21, 22],
                    ccx_id: 3,
                    ccm_id: 3,
                    live_df_read_centi_mib_per_s: 4_000_00,
                    live_df_write_centi_mib_per_s: 0,
                    llc_pressure_basis_points: 10_00,
                    llc_state: HotCoolState::Cool,
                    managed_task_count: 0,
                    signature_df_centi_mib_per_s: 4_000_00,
                    signature_llc_pressure_basis_points: 10_00,
                    planned_cpu_job_counts: vec![],
                },
                // Raw mock data: live DF read = 8,000 MiB/s, write = 0 MiB/s,
                // about 32% of the 25,000 MiB/s full-stress read level.
                // LLC pressure = 20%. This is the next-best free fallback CCD.
                MockIoDomain {
                    label: "ccd4",
                    cpus: vec![28, 29],
                    ccx_id: 4,
                    ccm_id: 4,
                    live_df_read_centi_mib_per_s: 8_000_00,
                    live_df_write_centi_mib_per_s: 0,
                    llc_pressure_basis_points: 20_00,
                    llc_state: HotCoolState::Cool,
                    managed_task_count: 0,
                    signature_df_centi_mib_per_s: 8_000_00,
                    signature_llc_pressure_basis_points: 20_00,
                    planned_cpu_job_counts: vec![],
                },
            ],
        }
    }

    // Builds a skewed-noise eval scenario. ccd4 carries dominant DF/LLC
    // pressure, ccd3 has moderate pressure and a planned job, and ccd0/ccd1 are
    // low-pressure alternatives. Expected behavior in tests: avoid the noisy
    // high-pressure source chiplet and land on the coolest available chiplet.
    fn eval_skewed() -> Self {
        Self {
            domains: vec![
                // Raw mock data: live DF read/write = 0 MiB/s, LLC pressure =
                // 5%. This is the quietest free destination in the map.
                MockIoDomain {
                    label: "ccd0",
                    cpus: vec![0, 1],
                    ccx_id: 0,
                    ccm_id: 0,
                    live_df_read_centi_mib_per_s: 0,
                    live_df_write_centi_mib_per_s: 0,
                    llc_pressure_basis_points: 5_00,
                    llc_state: HotCoolState::Cool,
                    managed_task_count: 0,
                    signature_df_centi_mib_per_s: 0,
                    signature_llc_pressure_basis_points: 5_00,
                    planned_cpu_job_counts: vec![],
                },
                // Raw mock data: live DF read = 5,000 MiB/s, write = 0 MiB/s,
                // about 20% of the 25,000 MiB/s full-stress read level.
                // LLC pressure = 15%. This remains a low-pressure alternative.
                MockIoDomain {
                    label: "ccd1",
                    cpus: vec![7, 8],
                    ccx_id: 1,
                    ccm_id: 1,
                    live_df_read_centi_mib_per_s: 5_000_00,
                    live_df_write_centi_mib_per_s: 0,
                    llc_pressure_basis_points: 15_00,
                    llc_state: HotCoolState::Cool,
                    managed_task_count: 0,
                    signature_df_centi_mib_per_s: 5_000_00,
                    signature_llc_pressure_basis_points: 15_00,
                    planned_cpu_job_counts: vec![],
                },
                // Raw mock data: live DF read = 15,000 MiB/s, write = 0 MiB/s,
                // about 60% of the 25,000 MiB/s full-stress read level.
                // LLC pressure = 55%. This CCD is moderately noisy and occupied.
                MockIoDomain {
                    label: "ccd3",
                    cpus: vec![21, 22],
                    ccx_id: 3,
                    ccm_id: 3,
                    live_df_read_centi_mib_per_s: 15_000_00,
                    live_df_write_centi_mib_per_s: 0,
                    llc_pressure_basis_points: 55_00,
                    llc_state: HotCoolState::Cool,
                    managed_task_count: 1,
                    signature_df_centi_mib_per_s: 15_000_00,
                    signature_llc_pressure_basis_points: 55_00,
                    planned_cpu_job_counts: vec![(21, 1)],
                },
                // Raw mock data: live DF read = 25,000 MiB/s, write = 0 MiB/s.
                // This is 100% of the assumed fully-stressed DF read level;
                // LLC pressure = 90%. This is the high-pressure source CCD.
                MockIoDomain {
                    label: "ccd4",
                    cpus: vec![28, 29],
                    ccx_id: 4,
                    ccm_id: 4,
                    live_df_read_centi_mib_per_s: 25_000_00,
                    live_df_write_centi_mib_per_s: 0,
                    llc_pressure_basis_points: 90_00,
                    llc_state: HotCoolState::Hot,
                    managed_task_count: 2,
                    signature_df_centi_mib_per_s: 25_000_00,
                    signature_llc_pressure_basis_points: 90_00,
                    planned_cpu_job_counts: vec![(28, 1), (29, 1)],
                },
            ],
        }
    }

    fn domain_id(&self, label: &str) -> u32 {
        self.domains
            .iter()
            .position(|domain| domain.label == label)
            .unwrap_or_else(|| panic!("unknown mock IO domain label: {label}")) as u32
    }

    fn domain_mut(&mut self, label: &str) -> &mut MockIoDomain {
        self.domains
            .iter_mut()
            .find(|domain| domain.label == label)
            .unwrap_or_else(|| panic!("unknown mock IO domain label: {label}"))
    }

    fn topo(&self) -> TopologyLayout {
        let nr_cpu_ids = self
            .domains
            .iter()
            .flat_map(|domain| domain.cpus.iter().copied())
            .max()
            .map(|cpu| cpu as usize + 1)
            .unwrap_or(0);
        let mut cpu_to_domain = vec![None; nr_cpu_ids];
        let domains = self
            .domains
            .iter()
            .enumerate()
            .map(|(domain_idx, domain)| {
                for &cpu in &domain.cpus {
                    cpu_to_domain[cpu as usize] = Some(domain_idx as u32);
                }
                DomainInfo {
                    domain_id: domain_idx as u32,
                    kernel_l3_id: domain_idx as u32,
                    rep_cpu: domain.cpus[0],
                    cpus: domain.cpus.clone(),
                    l3_size_mb: 32.0,
                }
            })
            .collect();
        TopologyLayout {
            nr_cpu_ids,
            domains,
            cpu_to_domain,
        }
    }

    fn mapping(&self) -> MappingInfo {
        let mut eligible_domains = BTreeSet::new();
        let mut eligible_cpus = BTreeSet::new();
        for (domain_idx, domain) in self.domains.iter().enumerate() {
            eligible_domains.insert(domain_idx as u32);
            eligible_cpus.extend(domain.cpus.iter().copied());
        }
        MappingInfo {
            domain_to_ccx: self.domains.iter().map(|domain| domain.ccx_id).collect(),
            domain_to_ccm: self
                .domains
                .iter()
                .map(|domain| Some(domain.ccm_id))
                .collect(),
            domain_to_df_capacity_mib_s_x100: vec![Some(2_000_000); self.domains.len()],
            cs_link_capacity_mib_s_x100: vec![],
            eligible_domains,
            excluded_domains: BTreeSet::new(),
            eligible_cpus,
        }
    }

    fn df_states(&self) -> Vec<Option<CcmDfStateValue>> {
        self.domains
            .iter()
            .map(|domain| {
                let total_df_centi_mib_per_s = domain
                    .live_df_read_centi_mib_per_s
                    .saturating_add(domain.live_df_write_centi_mib_per_s);
                Some(CcmDfStateValue {
                    sample_ts_ns: 1_000_000,
                    ccx_id: domain.ccx_id,
                    ccm_id: domain.ccm_id,
                    raw_read_bw_mib_s_x100: domain.live_df_read_centi_mib_per_s,
                    raw_write_bw_mib_s_x100: domain.live_df_write_centi_mib_per_s,
                    raw_pressure_pct_x100: total_df_centi_mib_per_s,
                    ewma_pressure_pct_x100: total_df_centi_mib_per_s,
                    state: domain.llc_state as u32,
                    valid: 1,
                    ..CcmDfStateValue::default()
                })
            })
            .collect()
    }

    fn llc_states(&self) -> Vec<Option<LlcStateValue>> {
        self.domains
            .iter()
            .map(|domain| {
                Some(LlcStateValue {
                    sample_ts_ns: 1_000_000,
                    raw_l3_bw_mib_s_x100: domain.llc_pressure_basis_points,
                    raw_pressure_pct_x100: domain.llc_pressure_basis_points,
                    ewma_pressure_pct_x100: domain.llc_pressure_basis_points,
                    state: domain.llc_state as u32,
                    valid: 1,
                    ..LlcStateValue::default()
                })
            })
            .collect()
    }

    fn planned_states(&self) -> Vec<PlannedDomainState> {
        self.domains
            .iter()
            .map(|domain| {
                let has_contributor =
                    domain.managed_task_count > 0 || domain.signature_df_centi_mib_per_s > 0;
                PlannedDomainState {
                    df_pressure_x100: domain.signature_df_centi_mib_per_s,
                    llc_pressure_x100: domain.signature_llc_pressure_basis_points,
                    contributor_count: u32::from(has_contributor),
                    stable_count: u32::from(has_contributor),
                    task_count: domain.managed_task_count,
                    migration_budget: 4,
                    ..PlannedDomainState::default()
                }
            })
            .collect()
    }

    fn cpu_states(&self) -> Vec<CpuStateValue> {
        let topo = self.topo();
        let mut planned_job_count_by_cpu = BTreeMap::<u32, usize>::new();
        for domain in &self.domains {
            for &(cpu, job_count) in &domain.planned_cpu_job_counts {
                planned_job_count_by_cpu.insert(cpu, job_count);
            }
        }
        let mut states = vec![CpuStateValue::default(); topo.nr_cpu_ids];
        for domain in &topo.domains {
            for &cpu in &domain.cpus {
                let planned_job_count = planned_job_count_by_cpu.get(&cpu).copied().unwrap_or(0);
                states[cpu as usize] = CpuStateValue {
                    domain_id: domain.domain_id,
                    idle: u32::from(planned_job_count == 0),
                    cpu_dsq_depth: planned_job_count as u32,
                    current_tid: if planned_job_count == 0 {
                        0
                    } else {
                        20_000 + cpu
                    },
                    last_update_ns: 1_000_000,
                };
            }
        }
        states
    }

    fn planned_cpu_states(&self) -> Vec<PlannedCpuState> {
        let topo = self.topo();
        let mut states = vec![PlannedCpuState::default(); topo.nr_cpu_ids];
        for domain in &self.domains {
            for &(cpu, job_count) in &domain.planned_cpu_job_counts {
                if let Some(state) = states.get_mut(cpu as usize) {
                    state.jobs = (0..job_count as u32)
                        .map(|idx| 30_000 + cpu.saturating_mul(10) + idx)
                        .collect();
                }
            }
        }
        states
    }

    fn cpu_util_basis_points(&self) -> Vec<u32> {
        vec![10_00; self.topo().nr_cpu_ids]
    }

    fn allowed_cpus(&self, labels: &[&str]) -> BTreeSet<u32> {
        labels
            .iter()
            .flat_map(|label| {
                let domain_idx = self.domain_id(label);
                self.domains[domain_idx as usize].cpus.iter().copied()
            })
            .collect()
    }
}

fn eval_policy_task(cpu: u32, domain: u32) -> QueuedTask {
    // Raw mock task data: L2 bandwidth estimate = 9,000 MiB/s.
    let mut task = task_with_bw(cpu as i32, domain as i32, 9_000_00);
    task.tid = 42_001;
    task.tgid = 42_000;
    task
}

// Verifies the eval-style asymmetric IO map where the source CCD is hot and all
// CCDs are legal. Expected: scheduler leaves the overloaded source and selects
// the lowest-pressure legal CCD with an idle/free CPU.
#[test]
fn eval_policy_io_load_map_moves_from_overloaded_source_to_coolest_legal_domain() {
    let map = MockIoLoadMap::eval_asymmetric();
    let topo = map.topo();
    let mapping = map.mapping();
    let planned_states = map.planned_states();
    let llc_states = map.llc_states();
    let df_states = map.df_states();
    let cpu_states = map.cpu_states();
    let cpu_util_basis_points = map.cpu_util_basis_points();
    let planned_cpu_states = map.planned_cpu_states();
    let allowed_domain_labels = ["ccd0", "ccd1", "ccd3", "ccd4"];
    let allowed_cpus = map.allowed_cpus(&allowed_domain_labels);
    let source_cpu = 0;
    let source_domain = map.domain_id("ccd0");
    let expected_domain = map.domain_id("ccd3");
    let expected_cpu = 21;
    // Mock task signature: projected DF = 9,000 MiB/s, projected LLC = 20%.
    let task_df_centi_mib_per_s = 9_000_00;
    let task_llc_pressure_basis_points = 20_00;
    let scheduler_now_ns = 10_000_000;
    let cfg = aggressive_policy();
    let task = eval_policy_task(source_cpu, source_domain);
    let meta = stable_meta(task_df_centi_mib_per_s, task_llc_pressure_basis_points);

    let decision = choose_placement_with_planned_cpus(
        &task,
        Some(&meta),
        &topo,
        &mapping,
        &planned_states,
        &llc_states,
        &df_states,
        &cpu_states,
        &cpu_util_basis_points,
        &planned_cpu_states,
        &allowed_cpus,
        scheduler_now_ns,
        cfg,
    );

    assert_eq!(decision.selected_domain, Some(expected_domain));
    assert_eq!(decision.selected_cpu, Some(expected_cpu));
    assert_eq!(decision.reason, DecisionReason::MoveCoolerDomain);
}

// Verifies that affinity constraints override the globally coolest CCD.
// Expected: scheduler skips the excluded coolest CCD and chooses the next best
// legal CCD without placing the task outside its allowed CPU mask.
#[test]
fn eval_policy_io_load_map_skips_coolest_domain_when_affinity_excludes_it() {
    let map = MockIoLoadMap::eval_asymmetric();
    let topo = map.topo();
    let mapping = map.mapping();
    let planned_states = map.planned_states();
    let llc_states = map.llc_states();
    let df_states = map.df_states();
    let cpu_states = map.cpu_states();
    let cpu_util_basis_points = map.cpu_util_basis_points();
    let planned_cpu_states = map.planned_cpu_states();
    let allowed_domain_labels = ["ccd0", "ccd1", "ccd4"];
    let allowed_cpus = map.allowed_cpus(&allowed_domain_labels);
    let source_cpu = 0;
    let source_domain = map.domain_id("ccd0");
    let excluded_coolest_domain = map.domain_id("ccd3");
    let expected_domain = map.domain_id("ccd4");
    let expected_cpu = 28;
    // Mock task signature: projected DF = 9,000 MiB/s, projected LLC = 20%.
    let task_df_centi_mib_per_s = 9_000_00;
    let task_llc_pressure_basis_points = 20_00;
    let scheduler_now_ns = 10_000_000;
    let cfg = aggressive_policy();
    let task = eval_policy_task(source_cpu, source_domain);
    let meta = stable_meta(task_df_centi_mib_per_s, task_llc_pressure_basis_points);

    let decision = choose_placement_with_planned_cpus(
        &task,
        Some(&meta),
        &topo,
        &mapping,
        &planned_states,
        &llc_states,
        &df_states,
        &cpu_states,
        &cpu_util_basis_points,
        &planned_cpu_states,
        &allowed_cpus,
        scheduler_now_ns,
        cfg,
    );

    assert_ne!(decision.selected_domain, Some(excluded_coolest_domain));
    assert_eq!(decision.selected_domain, Some(expected_domain));
    assert_eq!(decision.selected_cpu, Some(expected_cpu));
    assert_eq!(decision.reason, DecisionReason::MoveCoolerDomain);
}

// Verifies that placement does not chase a cooler CCD when that CCD has no free
// CPU slot for the task. Expected: scheduler chooses a slightly warmer legal
// CCD that still has available CPU capacity.
#[test]
fn eval_policy_io_load_map_prefers_free_slot_domain_over_cooler_full_domain() {
    let mut map = MockIoLoadMap::eval_asymmetric();
    {
        let full_cool_domain = map.domain_mut("ccd1");
        full_cool_domain.live_df_read_centi_mib_per_s = 2_000_00;
        full_cool_domain.signature_df_centi_mib_per_s = 2_000_00;
        full_cool_domain.llc_pressure_basis_points = 8_00;
        full_cool_domain.signature_llc_pressure_basis_points = 8_00;
        full_cool_domain.llc_state = HotCoolState::Cool;
        full_cool_domain.managed_task_count = 2;
        full_cool_domain.planned_cpu_job_counts = vec![(7, 1), (8, 1)];
    }
    {
        let free_domain = map.domain_mut("ccd3");
        free_domain.live_df_read_centi_mib_per_s = 8_000_00;
        free_domain.signature_df_centi_mib_per_s = 8_000_00;
        free_domain.llc_pressure_basis_points = 20_00;
        free_domain.signature_llc_pressure_basis_points = 20_00;
        free_domain.llc_state = HotCoolState::Cool;
        free_domain.managed_task_count = 0;
        free_domain.planned_cpu_job_counts.clear();
    }
    let topo = map.topo();
    let mapping = map.mapping();
    let planned_states = map.planned_states();
    let llc_states = map.llc_states();
    let df_states = map.df_states();
    let cpu_states = map.cpu_states();
    let cpu_util_basis_points = map.cpu_util_basis_points();
    let planned_cpu_states = map.planned_cpu_states();
    let allowed_domain_labels = ["ccd0", "ccd1", "ccd3", "ccd4"];
    let allowed_cpus = map.allowed_cpus(&allowed_domain_labels);
    let source_cpu = 0;
    let source_domain = map.domain_id("ccd0");
    let full_domain = map.domain_id("ccd1");
    let expected_domain = map.domain_id("ccd3");
    let expected_cpu = 21;
    // Mock task signature: projected DF = 9,000 MiB/s, projected LLC = 20%.
    let task_df_centi_mib_per_s = 9_000_00;
    let task_llc_pressure_basis_points = 20_00;
    let scheduler_now_ns = 10_000_000;
    let cfg = aggressive_policy();
    let task = eval_policy_task(source_cpu, source_domain);
    let meta = stable_meta(task_df_centi_mib_per_s, task_llc_pressure_basis_points);

    let decision = choose_placement_with_planned_cpus(
        &task,
        Some(&meta),
        &topo,
        &mapping,
        &planned_states,
        &llc_states,
        &df_states,
        &cpu_states,
        &cpu_util_basis_points,
        &planned_cpu_states,
        &allowed_cpus,
        scheduler_now_ns,
        cfg,
    );

    assert_ne!(decision.selected_domain, Some(full_domain));
    assert_eq!(decision.selected_domain, Some(expected_domain));
    assert_eq!(decision.selected_cpu, Some(expected_cpu));
    assert_eq!(decision.reason, DecisionReason::MoveCoolerDomain);
}

// Verifies a skewed-noise map where one CCD has dominant IO pressure while
// other legal CCDs remain cool/free. Expected: scheduler migrates away from the
// high-pressure CCD and lands on the lowest-pressure available CCD.
#[test]
fn eval_policy_io_load_map_skewed_noise_moves_away_from_high_pressure_chiplet() {
    let map = MockIoLoadMap::eval_skewed();
    let topo = map.topo();
    let mapping = map.mapping();
    let planned_states = map.planned_states();
    let llc_states = map.llc_states();
    let df_states = map.df_states();
    let cpu_states = map.cpu_states();
    let cpu_util_basis_points = map.cpu_util_basis_points();
    let planned_cpu_states = map.planned_cpu_states();
    let allowed_domain_labels = ["ccd0", "ccd1", "ccd3", "ccd4"];
    let allowed_cpus = map.allowed_cpus(&allowed_domain_labels);
    let source_cpu = 28;
    let source_domain = map.domain_id("ccd4");
    let expected_domain = map.domain_id("ccd0");
    let expected_cpu = 0;
    // Mock task signature: projected DF = 9,000 MiB/s, projected LLC = 20%.
    let task_df_centi_mib_per_s = 9_000_00;
    let task_llc_pressure_basis_points = 20_00;
    let scheduler_now_ns = 10_000_000;
    let cfg = aggressive_policy();
    let task = eval_policy_task(source_cpu, source_domain);
    let meta = stable_meta(task_df_centi_mib_per_s, task_llc_pressure_basis_points);

    let decision = choose_placement_with_planned_cpus(
        &task,
        Some(&meta),
        &topo,
        &mapping,
        &planned_states,
        &llc_states,
        &df_states,
        &cpu_states,
        &cpu_util_basis_points,
        &planned_cpu_states,
        &allowed_cpus,
        scheduler_now_ns,
        cfg,
    );

    assert_eq!(decision.selected_domain, Some(expected_domain));
    assert_eq!(decision.selected_cpu, Some(expected_cpu));
    assert_eq!(decision.reason, DecisionReason::MoveCoolerDomain);
}

// Verifies a time series of aggregate IO maps built from individual mock
// threads. Expected: when every CCD starts already balanced, each thread keeps
// the same CCD and CPU across small per-thread bandwidth noise.
#[test]
fn eval_policy_io_load_map_series_keeps_already_balanced_placements_stable() {
    let series = vec![
        // Round 0 raw aggregate DF reads: ccd0 = 18,000 MiB/s, ccd1 = 18,000
        // MiB/s, ccd3 = 17,000 MiB/s, ccd4 = 17,000 MiB/s. All stay below
        // the 25,000 MiB/s fully-stressed reference.
        vec![
            MockThreadIoLoad {
                tid: 100,
                domain_label: "ccd0",
                cpu: 0,
                df_read_centi_mib_per_s: 10_000_00,
                llc_pressure_basis_points: 10_00,
            },
            MockThreadIoLoad {
                tid: 101,
                domain_label: "ccd0",
                cpu: 1,
                df_read_centi_mib_per_s: 8_000_00,
                llc_pressure_basis_points: 10_00,
            },
            MockThreadIoLoad {
                tid: 200,
                domain_label: "ccd1",
                cpu: 7,
                df_read_centi_mib_per_s: 9_500_00,
                llc_pressure_basis_points: 10_00,
            },
            MockThreadIoLoad {
                tid: 201,
                domain_label: "ccd1",
                cpu: 8,
                df_read_centi_mib_per_s: 8_500_00,
                llc_pressure_basis_points: 10_00,
            },
            MockThreadIoLoad {
                tid: 300,
                domain_label: "ccd3",
                cpu: 21,
                df_read_centi_mib_per_s: 9_000_00,
                llc_pressure_basis_points: 10_00,
            },
            MockThreadIoLoad {
                tid: 301,
                domain_label: "ccd3",
                cpu: 22,
                df_read_centi_mib_per_s: 8_000_00,
                llc_pressure_basis_points: 10_00,
            },
            MockThreadIoLoad {
                tid: 400,
                domain_label: "ccd4",
                cpu: 28,
                df_read_centi_mib_per_s: 9_500_00,
                llc_pressure_basis_points: 10_00,
            },
            MockThreadIoLoad {
                tid: 401,
                domain_label: "ccd4",
                cpu: 29,
                df_read_centi_mib_per_s: 7_500_00,
                llc_pressure_basis_points: 10_00,
            },
        ],
        // Round 1 raw aggregate shape is unchanged, with only small per-thread
        // shifts around the same balanced CCD totals.
        vec![
            MockThreadIoLoad {
                tid: 100,
                domain_label: "ccd0",
                cpu: 0,
                df_read_centi_mib_per_s: 9_800_00,
                llc_pressure_basis_points: 10_00,
            },
            MockThreadIoLoad {
                tid: 101,
                domain_label: "ccd0",
                cpu: 1,
                df_read_centi_mib_per_s: 8_200_00,
                llc_pressure_basis_points: 10_00,
            },
            MockThreadIoLoad {
                tid: 200,
                domain_label: "ccd1",
                cpu: 7,
                df_read_centi_mib_per_s: 9_600_00,
                llc_pressure_basis_points: 10_00,
            },
            MockThreadIoLoad {
                tid: 201,
                domain_label: "ccd1",
                cpu: 8,
                df_read_centi_mib_per_s: 8_400_00,
                llc_pressure_basis_points: 10_00,
            },
            MockThreadIoLoad {
                tid: 300,
                domain_label: "ccd3",
                cpu: 21,
                df_read_centi_mib_per_s: 9_100_00,
                llc_pressure_basis_points: 10_00,
            },
            MockThreadIoLoad {
                tid: 301,
                domain_label: "ccd3",
                cpu: 22,
                df_read_centi_mib_per_s: 7_900_00,
                llc_pressure_basis_points: 10_00,
            },
            MockThreadIoLoad {
                tid: 400,
                domain_label: "ccd4",
                cpu: 28,
                df_read_centi_mib_per_s: 9_400_00,
                llc_pressure_basis_points: 10_00,
            },
            MockThreadIoLoad {
                tid: 401,
                domain_label: "ccd4",
                cpu: 29,
                df_read_centi_mib_per_s: 7_600_00,
                llc_pressure_basis_points: 10_00,
            },
        ],
        // Round 2 keeps all per-thread loads at or below 10,000 MiB/s and all
        // CCD aggregate loads under the fully-stressed reference.
        vec![
            MockThreadIoLoad {
                tid: 100,
                domain_label: "ccd0",
                cpu: 0,
                df_read_centi_mib_per_s: 10_000_00,
                llc_pressure_basis_points: 10_00,
            },
            MockThreadIoLoad {
                tid: 101,
                domain_label: "ccd0",
                cpu: 1,
                df_read_centi_mib_per_s: 7_500_00,
                llc_pressure_basis_points: 10_00,
            },
            MockThreadIoLoad {
                tid: 200,
                domain_label: "ccd1",
                cpu: 7,
                df_read_centi_mib_per_s: 9_800_00,
                llc_pressure_basis_points: 10_00,
            },
            MockThreadIoLoad {
                tid: 201,
                domain_label: "ccd1",
                cpu: 8,
                df_read_centi_mib_per_s: 8_100_00,
                llc_pressure_basis_points: 10_00,
            },
            MockThreadIoLoad {
                tid: 300,
                domain_label: "ccd3",
                cpu: 21,
                df_read_centi_mib_per_s: 9_200_00,
                llc_pressure_basis_points: 10_00,
            },
            MockThreadIoLoad {
                tid: 301,
                domain_label: "ccd3",
                cpu: 22,
                df_read_centi_mib_per_s: 7_900_00,
                llc_pressure_basis_points: 10_00,
            },
            MockThreadIoLoad {
                tid: 400,
                domain_label: "ccd4",
                cpu: 28,
                df_read_centi_mib_per_s: 9_500_00,
                llc_pressure_basis_points: 10_00,
            },
            MockThreadIoLoad {
                tid: 401,
                domain_label: "ccd4",
                cpu: 29,
                df_read_centi_mib_per_s: 8_000_00,
                llc_pressure_basis_points: 10_00,
            },
        ],
    ];
    let allowed_domain_labels = ["ccd0", "ccd1", "ccd3", "ccd4"];
    let scheduler_now_ns = 10_000_000;
    let cfg = aggressive_policy();
    let mut placement_history = BTreeMap::<u32, BTreeSet<(u32, u32)>>::new();

    for threads in &series {
        let map = MockIoLoadMap::from_thread_loads(threads);
        let topo = map.topo();
        let mapping = map.mapping();
        let planned_states = map.planned_states();
        let llc_states = map.llc_states();
        let df_states = map.df_states();
        let cpu_states = map.cpu_states_for_thread_loads(threads);
        let cpu_util_basis_points = map.cpu_util_basis_points();
        let planned_cpu_states = map.planned_cpu_states_for_thread_loads(threads);
        let allowed_cpus = map.allowed_cpus(&allowed_domain_labels);

        for thread in threads {
            let current_domain = map.domain_id(thread.domain_label);
            let task = thread.task(&map);
            let meta = thread.meta();
            let decision = choose_placement_with_planned_cpus(
                &task,
                Some(&meta),
                &topo,
                &mapping,
                &planned_states,
                &llc_states,
                &df_states,
                &cpu_states,
                &cpu_util_basis_points,
                &planned_cpu_states,
                &allowed_cpus,
                scheduler_now_ns,
                cfg,
            );

            assert_eq!(decision.selected_domain, Some(current_domain));
            assert_eq!(decision.selected_cpu, Some(thread.cpu));
            assert!(
                matches!(
                    decision.reason,
                    DecisionReason::StayCurrentDomain | DecisionReason::DestinationGuard
                ),
                "mock thread {} unexpectedly moved or used an active move reason: {:?}",
                thread.tid,
                decision.reason
            );
            placement_history
                .entry(thread.tid)
                .or_default()
                .insert((current_domain, thread.cpu));
        }
    }

    for (tid, placements) in placement_history {
        assert_eq!(
            placements.len(),
            1,
            "mock thread {tid} changed placement across balanced samples: {placements:?}"
        );
    }
}

// Verifies that a one-time move out of an overloaded aggregate IO map becomes
// stable after the mock thread placement is updated and the aggregate map is
// rebuilt. Expected: the first pass migrates one thread to the cooler CCD; the
// next pass keeps every thread on its post-migration CCD and CPU.
#[test]
fn eval_policy_io_load_map_series_stabilizes_after_migration() {
    let mut threads = vec![
        // Initial ccd0 aggregate DF read is 25,000 MiB/s: 100% of the
        // fully-stressed reference, with one extra thread sharing CPU 0.
        MockThreadIoLoad {
            tid: 102,
            domain_label: "ccd0",
            cpu: 0,
            df_read_centi_mib_per_s: 7_000_00,
            llc_pressure_basis_points: 15_00,
        },
        MockThreadIoLoad {
            tid: 100,
            domain_label: "ccd0",
            cpu: 0,
            df_read_centi_mib_per_s: 10_000_00,
            llc_pressure_basis_points: 30_00,
        },
        MockThreadIoLoad {
            tid: 101,
            domain_label: "ccd0",
            cpu: 1,
            df_read_centi_mib_per_s: 8_000_00,
            llc_pressure_basis_points: 25_00,
        },
        MockThreadIoLoad {
            tid: 200,
            domain_label: "ccd1",
            cpu: 7,
            df_read_centi_mib_per_s: 9_000_00,
            llc_pressure_basis_points: 15_00,
        },
        MockThreadIoLoad {
            tid: 201,
            domain_label: "ccd1",
            cpu: 8,
            df_read_centi_mib_per_s: 8_000_00,
            llc_pressure_basis_points: 15_00,
        },
        // Initial ccd3 aggregate DF read is only 7,000 MiB/s, so CPU 22 is the
        // coolest legal free slot for the migration.
        MockThreadIoLoad {
            tid: 300,
            domain_label: "ccd3",
            cpu: 21,
            df_read_centi_mib_per_s: 7_000_00,
            llc_pressure_basis_points: 10_00,
        },
        MockThreadIoLoad {
            tid: 400,
            domain_label: "ccd4",
            cpu: 28,
            df_read_centi_mib_per_s: 9_000_00,
            llc_pressure_basis_points: 15_00,
        },
        MockThreadIoLoad {
            tid: 401,
            domain_label: "ccd4",
            cpu: 29,
            df_read_centi_mib_per_s: 8_000_00,
            llc_pressure_basis_points: 15_00,
        },
    ];
    let allowed_domain_labels = ["ccd0", "ccd1", "ccd3", "ccd4"];
    let scheduler_now_ns = 10_000_000;
    let cfg = aggressive_policy();
    let migrating_tid = 102;

    let initial_map = MockIoLoadMap::from_thread_loads(&threads);
    let topo = initial_map.topo();
    let mapping = initial_map.mapping();
    let planned_states = initial_map.planned_states();
    let llc_states = initial_map.llc_states();
    let df_states = initial_map.df_states();
    let cpu_states = initial_map.cpu_states_for_thread_loads(&threads);
    let cpu_util_basis_points = initial_map.cpu_util_basis_points();
    let planned_cpu_states = initial_map.planned_cpu_states_for_thread_loads(&threads);
    let allowed_cpus = initial_map.allowed_cpus(&allowed_domain_labels);
    let migrating_thread = threads
        .iter()
        .find(|thread| thread.tid == migrating_tid)
        .expect("missing migrating mock thread");
    let expected_target_domain = initial_map.domain_id("ccd3");
    let expected_target_cpu = 22;
    let task = migrating_thread.task(&initial_map);
    let meta = migrating_thread.meta();

    let first_decision = choose_placement_with_planned_cpus(
        &task,
        Some(&meta),
        &topo,
        &mapping,
        &planned_states,
        &llc_states,
        &df_states,
        &cpu_states,
        &cpu_util_basis_points,
        &planned_cpu_states,
        &allowed_cpus,
        scheduler_now_ns,
        cfg,
    );

    assert_eq!(first_decision.selected_domain, Some(expected_target_domain));
    assert_eq!(first_decision.selected_cpu, Some(expected_target_cpu));
    assert_eq!(first_decision.reason, DecisionReason::MoveCoolerDomain);

    let migrated_thread = threads
        .iter_mut()
        .find(|thread| thread.tid == migrating_tid)
        .expect("missing migrating mock thread");
    migrated_thread.domain_label = "ccd3";
    migrated_thread.cpu = expected_target_cpu;

    let stable_map = MockIoLoadMap::from_thread_loads(&threads);
    let topo = stable_map.topo();
    let mapping = stable_map.mapping();
    let planned_states = stable_map.planned_states();
    let llc_states = stable_map.llc_states();
    let df_states = stable_map.df_states();
    let cpu_states = stable_map.cpu_states_for_thread_loads(&threads);
    let cpu_util_basis_points = stable_map.cpu_util_basis_points();
    let planned_cpu_states = stable_map.planned_cpu_states_for_thread_loads(&threads);
    let allowed_cpus = stable_map.allowed_cpus(&allowed_domain_labels);

    for thread in &threads {
        let current_domain = stable_map.domain_id(thread.domain_label);
        let task = thread.task(&stable_map);
        let mut meta = thread.meta();
        meta.settle_until_ns = 0;
        let decision = choose_placement_with_planned_cpus(
            &task,
            Some(&meta),
            &topo,
            &mapping,
            &planned_states,
            &llc_states,
            &df_states,
            &cpu_states,
            &cpu_util_basis_points,
            &planned_cpu_states,
            &allowed_cpus,
            scheduler_now_ns,
            cfg,
        );

        assert_eq!(decision.selected_domain, Some(current_domain));
        assert_eq!(decision.selected_cpu, Some(thread.cpu));
        assert!(
            matches!(
                decision.reason,
                DecisionReason::StayCurrentDomain | DecisionReason::DestinationGuard
            ),
            "mock thread {} unexpectedly moved again or used an active move reason: {:?}",
            thread.tid,
            decision.reason
        );
    }
}

// Verifies a llama.cpp-like loaded map where all legal CCDs are already hot.
// Expected: no tick-driven cross-domain churn, because moving a thread only
// transfers overload from one hot CCD to another.
fn assert_eval_policy_all_hot_loaded_map_keeps_current_placements() {
    let threads = vec![
        MockThreadIoLoad {
            tid: 100,
            domain_label: "ccd0",
            cpu: 0,
            df_read_centi_mib_per_s: 5_000_00,
            llc_pressure_basis_points: 20_00,
        },
        MockThreadIoLoad {
            tid: 101,
            domain_label: "ccd0",
            cpu: 1,
            df_read_centi_mib_per_s: 5_000_00,
            llc_pressure_basis_points: 20_00,
        },
        MockThreadIoLoad {
            tid: 102,
            domain_label: "ccd0",
            cpu: 2,
            df_read_centi_mib_per_s: 5_000_00,
            llc_pressure_basis_points: 20_00,
        },
        MockThreadIoLoad {
            tid: 103,
            domain_label: "ccd0",
            cpu: 3,
            df_read_centi_mib_per_s: 5_000_00,
            llc_pressure_basis_points: 20_00,
        },
        MockThreadIoLoad {
            tid: 104,
            domain_label: "ccd0",
            cpu: 4,
            df_read_centi_mib_per_s: 5_000_00,
            llc_pressure_basis_points: 20_00,
        },
        MockThreadIoLoad {
            tid: 200,
            domain_label: "ccd1",
            cpu: 7,
            df_read_centi_mib_per_s: 5_000_00,
            llc_pressure_basis_points: 20_00,
        },
        MockThreadIoLoad {
            tid: 201,
            domain_label: "ccd1",
            cpu: 8,
            df_read_centi_mib_per_s: 5_000_00,
            llc_pressure_basis_points: 20_00,
        },
        MockThreadIoLoad {
            tid: 202,
            domain_label: "ccd1",
            cpu: 9,
            df_read_centi_mib_per_s: 5_000_00,
            llc_pressure_basis_points: 20_00,
        },
        MockThreadIoLoad {
            tid: 203,
            domain_label: "ccd1",
            cpu: 10,
            df_read_centi_mib_per_s: 5_000_00,
            llc_pressure_basis_points: 20_00,
        },
        MockThreadIoLoad {
            tid: 204,
            domain_label: "ccd1",
            cpu: 11,
            df_read_centi_mib_per_s: 5_000_00,
            llc_pressure_basis_points: 20_00,
        },
        MockThreadIoLoad {
            tid: 300,
            domain_label: "ccd3",
            cpu: 21,
            df_read_centi_mib_per_s: 5_000_00,
            llc_pressure_basis_points: 20_00,
        },
        MockThreadIoLoad {
            tid: 301,
            domain_label: "ccd3",
            cpu: 22,
            df_read_centi_mib_per_s: 5_000_00,
            llc_pressure_basis_points: 20_00,
        },
        MockThreadIoLoad {
            tid: 302,
            domain_label: "ccd3",
            cpu: 23,
            df_read_centi_mib_per_s: 5_000_00,
            llc_pressure_basis_points: 20_00,
        },
        MockThreadIoLoad {
            tid: 303,
            domain_label: "ccd3",
            cpu: 24,
            df_read_centi_mib_per_s: 5_000_00,
            llc_pressure_basis_points: 20_00,
        },
        MockThreadIoLoad {
            tid: 304,
            domain_label: "ccd3",
            cpu: 25,
            df_read_centi_mib_per_s: 5_000_00,
            llc_pressure_basis_points: 20_00,
        },
        MockThreadIoLoad {
            tid: 400,
            domain_label: "ccd4",
            cpu: 28,
            df_read_centi_mib_per_s: 5_000_00,
            llc_pressure_basis_points: 20_00,
        },
        MockThreadIoLoad {
            tid: 401,
            domain_label: "ccd4",
            cpu: 29,
            df_read_centi_mib_per_s: 5_000_00,
            llc_pressure_basis_points: 20_00,
        },
        MockThreadIoLoad {
            tid: 402,
            domain_label: "ccd4",
            cpu: 30,
            df_read_centi_mib_per_s: 5_000_00,
            llc_pressure_basis_points: 20_00,
        },
        MockThreadIoLoad {
            tid: 403,
            domain_label: "ccd4",
            cpu: 31,
            df_read_centi_mib_per_s: 5_000_00,
            llc_pressure_basis_points: 20_00,
        },
        MockThreadIoLoad {
            tid: 404,
            domain_label: "ccd4",
            cpu: 32,
            df_read_centi_mib_per_s: 5_000_00,
            llc_pressure_basis_points: 20_00,
        },
    ];
    let map = MockIoLoadMap::from_thread_loads_on_full_ccd_topology(&threads);
    let topo = map.topo();
    let mapping = map.mapping();
    let planned_states = map.planned_states();
    let llc_states = map.llc_states();
    let df_states = map.df_states();
    let cpu_states = map.cpu_states_for_thread_loads(&threads);
    let cpu_util_basis_points = map.cpu_util_basis_points();
    let planned_cpu_states = map.planned_cpu_states_for_thread_loads(&threads);
    let allowed_cpus = map.allowed_cpus(&["ccd0", "ccd1", "ccd3", "ccd4"]);
    let scheduler_now_ns = 10_000_000;
    let cfg = aggressive_policy();

    for thread in &threads {
        let current_domain = map.domain_id(thread.domain_label);
        let mut task = thread.task(&map);
        task.trigger = QueueTrigger::Tick as u32;
        task.tick_seq = u64::from(thread.tid);
        let meta = thread.meta();

        let decision = choose_placement_with_planned_cpus(
            &task,
            Some(&meta),
            &topo,
            &mapping,
            &planned_states,
            &llc_states,
            &df_states,
            &cpu_states,
            &cpu_util_basis_points,
            &planned_cpu_states,
            &allowed_cpus,
            scheduler_now_ns,
            cfg,
        );

        assert_eq!(
            decision.selected_domain,
            Some(current_domain),
            "hot loaded thread {} unexpectedly moved domains: {decision:?}",
            thread.tid
        );
        assert_eq!(
            decision.selected_cpu,
            Some(thread.cpu),
            "hot loaded thread {} unexpectedly changed CPUs: {decision:?}",
            thread.tid
        );
        assert_eq!(
            decision.tick_decision,
            TickDecision::Stay,
            "hot loaded thread {} should not request a tick move: {decision:?}",
            thread.tid
        );
        assert_ne!(
            decision.reason,
            DecisionReason::MoveCoolerDomain,
            "all legal domains are hot, so thread {} should not chase a cooler domain: {decision:?}",
            thread.tid
        );
    }
}

#[test]
fn eval_policy_all_hot_loaded_map_keeps_current_placements() {
    assert_eval_policy_all_hot_loaded_map_keeps_current_placements();
}

#[cfg(feature = "scheduler-paper-greedy")]
#[test]
fn paper_greedy_eval_policy_all_hot_loaded_map_keeps_current_placements() {
    assert_eval_policy_all_hot_loaded_map_keeps_current_placements();
}

// Verifies a 7-CPU CCD boundary condition. Expected: an additional unplaced job
// is not assigned to ccd0 after all 7 ccd0 CPUs already have jobs, even though
// ccd0 still has DF bandwidth headroom below the 25,000 MiB/s full-stress mark.
#[test]
fn eval_policy_io_load_map_full_ccd_rejects_additional_job_even_with_bw_headroom() {
    let threads = vec![
        // Raw mock aggregate: ccd0 has 7/7 CPUs allocated and 18,000 MiB/s DF
        // read, so CPU slots are full while bandwidth still has headroom.
        MockThreadIoLoad {
            tid: 100,
            domain_label: "ccd0",
            cpu: 0,
            df_read_centi_mib_per_s: 3_000_00,
            llc_pressure_basis_points: 5_00,
        },
        MockThreadIoLoad {
            tid: 101,
            domain_label: "ccd0",
            cpu: 1,
            df_read_centi_mib_per_s: 3_000_00,
            llc_pressure_basis_points: 5_00,
        },
        MockThreadIoLoad {
            tid: 102,
            domain_label: "ccd0",
            cpu: 2,
            df_read_centi_mib_per_s: 3_000_00,
            llc_pressure_basis_points: 5_00,
        },
        MockThreadIoLoad {
            tid: 103,
            domain_label: "ccd0",
            cpu: 3,
            df_read_centi_mib_per_s: 3_000_00,
            llc_pressure_basis_points: 5_00,
        },
        MockThreadIoLoad {
            tid: 104,
            domain_label: "ccd0",
            cpu: 4,
            df_read_centi_mib_per_s: 2_000_00,
            llc_pressure_basis_points: 5_00,
        },
        MockThreadIoLoad {
            tid: 105,
            domain_label: "ccd0",
            cpu: 5,
            df_read_centi_mib_per_s: 2_000_00,
            llc_pressure_basis_points: 5_00,
        },
        MockThreadIoLoad {
            tid: 106,
            domain_label: "ccd0",
            cpu: 6,
            df_read_centi_mib_per_s: 2_000_00,
            llc_pressure_basis_points: 5_00,
        },
    ];
    let map = MockIoLoadMap::from_thread_loads_on_full_ccd_topology(&threads);
    let topo = map.topo();
    let mapping = map.mapping();
    let planned_states = map.planned_states();
    let llc_states = map.llc_states();
    let df_states = map.df_states();
    let cpu_states = map.cpu_states_for_thread_loads(&threads);
    let cpu_util_basis_points = map.cpu_util_basis_points();
    let planned_cpu_states = map.planned_cpu_states_for_thread_loads(&threads);
    let allowed_domain_labels = ["ccd0", "ccd1", "ccd3", "ccd4"];
    let allowed_cpus = map.allowed_cpus(&allowed_domain_labels);
    let full_domain = map.domain_id("ccd0");
    let expected_domain = map.domain_id("ccd1");
    let expected_cpu = 7;
    let scheduler_now_ns = 10_000_000;
    let cfg = aggressive_policy();
    let incoming_task_df_centi_mib_per_s = 5_000_00;
    let incoming_task_llc_pressure_basis_points = 10_00;
    let mut incoming_task = task_with_bw(-1, -1, incoming_task_df_centi_mib_per_s);
    incoming_task.tid = 900;
    incoming_task.tgid = 900;
    let meta = stable_meta(
        incoming_task_df_centi_mib_per_s,
        incoming_task_llc_pressure_basis_points,
    );

    let decision = choose_placement_with_planned_cpus(
        &incoming_task,
        Some(&meta),
        &topo,
        &mapping,
        &planned_states,
        &llc_states,
        &df_states,
        &cpu_states,
        &cpu_util_basis_points,
        &planned_cpu_states,
        &allowed_cpus,
        scheduler_now_ns,
        cfg,
    );

    assert_ne!(decision.selected_domain, Some(full_domain));
    assert!(!topo.domains[full_domain as usize]
        .cpus
        .contains(&decision.selected_cpu.expect("selected cpu")));
    assert_eq!(decision.selected_domain, Some(expected_domain));
    assert_eq!(decision.selected_cpu, Some(expected_cpu));
}

// Verifies that a full source CCD does not block bandwidth rebalancing when a
// legal destination CCD has an idle CPU. Expected: the selected source thread
// migrates from the 7/7-full ccd0 to idle ccd1 CPU 9.
#[test]
fn eval_policy_io_load_map_full_ccd_does_not_block_bw_rebalance_to_idle_cpu() {
    let threads = vec![
        // Raw mock aggregate: ccd0 has 7/7 CPUs allocated and 25,000 MiB/s DF
        // read, exactly the fully-stressed reference.
        MockThreadIoLoad {
            tid: 100,
            domain_label: "ccd0",
            cpu: 0,
            df_read_centi_mib_per_s: 8_000_00,
            llc_pressure_basis_points: 20_00,
        },
        MockThreadIoLoad {
            tid: 101,
            domain_label: "ccd0",
            cpu: 1,
            df_read_centi_mib_per_s: 3_000_00,
            llc_pressure_basis_points: 10_00,
        },
        MockThreadIoLoad {
            tid: 102,
            domain_label: "ccd0",
            cpu: 2,
            df_read_centi_mib_per_s: 3_000_00,
            llc_pressure_basis_points: 10_00,
        },
        MockThreadIoLoad {
            tid: 103,
            domain_label: "ccd0",
            cpu: 3,
            df_read_centi_mib_per_s: 3_000_00,
            llc_pressure_basis_points: 10_00,
        },
        MockThreadIoLoad {
            tid: 104,
            domain_label: "ccd0",
            cpu: 4,
            df_read_centi_mib_per_s: 3_000_00,
            llc_pressure_basis_points: 10_00,
        },
        MockThreadIoLoad {
            tid: 105,
            domain_label: "ccd0",
            cpu: 5,
            df_read_centi_mib_per_s: 3_000_00,
            llc_pressure_basis_points: 10_00,
        },
        MockThreadIoLoad {
            tid: 106,
            domain_label: "ccd0",
            cpu: 6,
            df_read_centi_mib_per_s: 2_000_00,
            llc_pressure_basis_points: 10_00,
        },
        // Raw mock aggregate: ccd1 has 2/7 CPUs allocated and 7,000 MiB/s DF
        // read, leaving CPU 9 as the first legal idle target.
        MockThreadIoLoad {
            tid: 200,
            domain_label: "ccd1",
            cpu: 7,
            df_read_centi_mib_per_s: 4_000_00,
            llc_pressure_basis_points: 10_00,
        },
        MockThreadIoLoad {
            tid: 201,
            domain_label: "ccd1",
            cpu: 8,
            df_read_centi_mib_per_s: 3_000_00,
            llc_pressure_basis_points: 10_00,
        },
        MockThreadIoLoad {
            tid: 300,
            domain_label: "ccd3",
            cpu: 21,
            df_read_centi_mib_per_s: 9_000_00,
            llc_pressure_basis_points: 15_00,
        },
        MockThreadIoLoad {
            tid: 301,
            domain_label: "ccd3",
            cpu: 22,
            df_read_centi_mib_per_s: 9_000_00,
            llc_pressure_basis_points: 15_00,
        },
        MockThreadIoLoad {
            tid: 400,
            domain_label: "ccd4",
            cpu: 28,
            df_read_centi_mib_per_s: 9_000_00,
            llc_pressure_basis_points: 15_00,
        },
        MockThreadIoLoad {
            tid: 401,
            domain_label: "ccd4",
            cpu: 29,
            df_read_centi_mib_per_s: 8_000_00,
            llc_pressure_basis_points: 15_00,
        },
    ];
    let map = MockIoLoadMap::from_thread_loads_on_full_ccd_topology(&threads);
    let topo = map.topo();
    let mapping = map.mapping();
    let planned_states = map.planned_states();
    let llc_states = map.llc_states();
    let df_states = map.df_states();
    let cpu_states = map.cpu_states_for_thread_loads(&threads);
    let cpu_util_basis_points = map.cpu_util_basis_points();
    let planned_cpu_states = map.planned_cpu_states_for_thread_loads(&threads);
    let allowed_domain_labels = ["ccd0", "ccd1", "ccd3", "ccd4"];
    let allowed_cpus = map.allowed_cpus(&allowed_domain_labels);
    let source_thread = threads
        .iter()
        .find(|thread| thread.tid == 100)
        .expect("missing bandwidth-heavy source thread");
    let expected_domain = map.domain_id("ccd1");
    let expected_cpu = 9;
    let scheduler_now_ns = 10_000_000;
    let cfg = aggressive_policy();
    let task = source_thread.task(&map);
    let meta = source_thread.meta();

    let decision = choose_placement_with_planned_cpus(
        &task,
        Some(&meta),
        &topo,
        &mapping,
        &planned_states,
        &llc_states,
        &df_states,
        &cpu_states,
        &cpu_util_basis_points,
        &planned_cpu_states,
        &allowed_cpus,
        scheduler_now_ns,
        cfg,
    );

    assert_eq!(decision.selected_domain, Some(expected_domain));
    assert_eq!(decision.selected_cpu, Some(expected_cpu));
    assert_eq!(decision.reason, DecisionReason::MoveCoolerDomain);
    assert!(decision.predicted_source_df_x100 < decision.current_source_df_x100);
    assert!(decision.predicted_destination_df_x100 > decision.current_destination_df_x100);
}

#[test]
fn congested_task_moves_to_cooler_domain() {
    let decision = choose_placement(
        &task(0, 0),
        Some(&stable_meta_with_domain_df(900_000, 900_000, 2_000)),
        &topo(),
        &mapping(),
        &planned_states(&[2, 1], &[agg(4_200_000, 9_000), agg(1_000_000, 2_000)]),
        &[
            llc(9_000, HotCoolState::Hot),
            llc(2_000, HotCoolState::Cool),
        ],
        &[df(4_200_000), df(1_000_000)],
        &[
            CpuStateValue {
                domain_id: 0,
                idle: 1,
                cpu_dsq_depth: 0,
                current_tid: 0,
                last_update_ns: 0,
            },
            CpuStateValue {
                domain_id: 0,
                idle: 0,
                cpu_dsq_depth: 1,
                current_tid: 12,
                last_update_ns: 0,
            },
            CpuStateValue {
                domain_id: 1,
                idle: 1,
                cpu_dsq_depth: 0,
                current_tid: 0,
                last_update_ns: 0,
            },
            CpuStateValue {
                domain_id: 1,
                idle: 1,
                cpu_dsq_depth: 0,
                current_tid: 0,
                last_update_ns: 0,
            },
        ],
        &[1_000, 8_000, 1_000, 1_000],
        &BTreeSet::from([0, 1, 2, 3]),
        10_000_000,
        aggressive_policy(),
    );

    assert_eq!(decision.selected_domain, Some(1));
    assert_eq!(decision.selected_cpu, Some(2));
    assert_eq!(decision.reason, DecisionReason::MoveCoolerDomain);
}

#[test]
fn pinned_thread_never_escapes_single_allowed_cpu() {
    let mut pinned = task(0, 0);
    pinned.nr_cpus_allowed = 1;
    let allowed = BTreeSet::from([0]);

    let decision = choose_placement(
        &pinned,
        Some(&stable_meta_with_domain_df(900_000, 900_000, 2_000)),
        &topo(),
        &mapping(),
        &planned_states(&[2, 0], &[agg(4_200_000, 9_000), agg(1_000_000, 2_000)]),
        &[
            llc(9_000, HotCoolState::Hot),
            llc(1_000, HotCoolState::Cool),
        ],
        &[df(4_200_000), df(1_000_000)],
        &cpu_states(),
        &[9_000, 8_000, 1_000, 1_000],
        &allowed,
        10_000_000,
        aggressive_policy(),
    );

    assert_eq!(decision.selected_domain, Some(0));
    assert_eq!(decision.selected_cpu, Some(0));
    assert!(decision
        .selected_cpu
        .is_some_and(|cpu| allowed.contains(&cpu)));
}

#[test]
fn unpinned_thread_avoids_cpu_reserved_for_pinned_peer() {
    let mut unpinned = task(1, 0);
    unpinned.tid = 1002;
    unpinned.nr_cpus_allowed = 2;
    let mut planned_cpu_states = vec![PlannedCpuState::default(); 4];
    planned_cpu_states[1].jobs.push(1001);

    let decision = choose_placement_with_planned_cpus(
        &unpinned,
        Some(&stable_meta_with_domain_df(900_000, 900_000, 2_000)),
        &topo(),
        &mapping(),
        &planned_states(&[2, 0], &[agg(4_200_000, 9_000), agg(1_000_000, 2_000)]),
        &[
            llc(9_000, HotCoolState::Hot),
            llc(1_000, HotCoolState::Cool),
        ],
        &[df(4_200_000), df(1_000_000)],
        &[
            CpuStateValue {
                domain_id: 0,
                idle: 1,
                cpu_dsq_depth: 0,
                current_tid: 0,
                last_update_ns: 0,
            },
            CpuStateValue {
                domain_id: 0,
                idle: 1,
                cpu_dsq_depth: 0,
                current_tid: 0,
                last_update_ns: 0,
            },
            CpuStateValue::default(),
            CpuStateValue::default(),
        ],
        &[1_000, 1_000, 1_000, 1_000],
        &planned_cpu_states,
        &BTreeSet::from([0, 1]),
        10_000_000,
        aggressive_policy(),
    );

    assert_eq!(decision.selected_domain, Some(0));
    assert_eq!(decision.selected_cpu, Some(0));
}

#[test]
fn young_enqueue_task_spreads_to_idle_cpu_before_signature_learning() {
    let mut task = task_with_bw(0, 0, 10_000);
    task.enq_cnt = 1;
    let decision = choose_placement(
        &task,
        None,
        &topo(),
        &mapping(),
        &planned_states(&[5, 0], &[agg(0, 0), agg(0, 0)]),
        &[
            llc(1_000, HotCoolState::Cool),
            llc(1_000, HotCoolState::Cool),
        ],
        &[df(1_000), df(1_000)],
        &cpu_states(),
        &[9_000, 8_000, 1_000, 1_000],
        &BTreeSet::from([0, 1, 2, 3]),
        10_000_000,
        aggressive_policy(),
    );

    assert_eq!(decision.selected_domain, Some(1));
    assert_eq!(decision.selected_cpu, Some(2));
    assert_eq!(decision.reason, DecisionReason::IdleSpread);
}

#[test]
fn mature_cold_task_stays_on_current_domain() {
    let mut cold = task_with_bw(0, 0, 10_000);
    cold.enq_cnt = 6;
    let decision = choose_placement(
        &cold,
        None,
        &topo(),
        &mapping(),
        &planned_states(&[5, 0], &[agg(0, 0), agg(0, 0)]),
        &[
            llc(1_000, HotCoolState::Cool),
            llc(1_000, HotCoolState::Cool),
        ],
        &[df(1_000), df(1_000)],
        &cpu_states(),
        &[9_000, 8_000, 1_000, 1_000],
        &BTreeSet::from([0, 1, 2, 3]),
        10_000_000,
        aggressive_policy(),
    );

    assert_eq!(decision.selected_domain, Some(0));
    assert_eq!(decision.reason, DecisionReason::StayCurrentDomain);
}

#[test]
fn task_on_cooler_domain_stays_put() {
    let decision = choose_placement(
        &task(2, 1),
        None,
        &topo(),
        &mapping(),
        &planned_states(&domain_task_counts(), &domain_signature_sums()),
        &[
            llc(9_000, HotCoolState::Hot),
            llc(2_000, HotCoolState::Cool),
        ],
        &[df(8_000), df(2_000)],
        &cpu_states(),
        &[9_000, 8_000, 1_000, 1_000],
        &BTreeSet::from([0, 1, 2, 3]),
        10_000_000,
        aggressive_policy(),
    );

    assert_eq!(decision.selected_domain, Some(1));
    assert_eq!(decision.selected_cpu, Some(2));
    assert_eq!(decision.reason, DecisionReason::StayCurrentDomain);
}

#[test]
fn small_improvement_stays_on_current_domain_with_linear_allocation_score() {
    let decision = choose_placement(
        &task_with_bw(0, 0, 60_000),
        None,
        &topo(),
        &mapping(),
        &planned_states(
            &domain_task_counts(),
            &[agg(5_000, 5_000), agg(4_990, 4_990)],
        ),
        &[
            llc(5_000, HotCoolState::Hot),
            llc(4_990, HotCoolState::Cool),
        ],
        &[df(5_000), df(4_990)],
        &cpu_states(),
        &[9_000, 8_000, 1_000, 1_000],
        &BTreeSet::from([0, 1, 2, 3]),
        10_000_000,
        policy(),
    );

    assert_eq!(decision.selected_domain, Some(0));
    assert_eq!(decision.selected_cpu, Some(1));
    assert_eq!(decision.reason, DecisionReason::StayCurrentDomain);
}

#[test]
fn signature_sum_baseline_beats_live_df_top_up() {
    let meta = ManagedThreadState {
        signature: ThreadSignature {
            valid: true,
            stable: true,
            projected_df_pressure_x100: 1_000,
            projected_llc_pressure_x100: 500,
            ..ThreadSignature::default()
        },
        ..ManagedThreadState::default()
    };
    let decision = choose_placement(
        &task_with_bw(0, 0, 60_000),
        Some(&meta),
        &topo(),
        &mapping(),
        &planned_states(
            &domain_task_counts(),
            &[agg(6_000, 5_000), agg(1_000, 2_000)],
        ),
        &[
            llc(5_000, HotCoolState::Hot),
            llc(2_000, HotCoolState::Cool),
        ],
        &[df(9_500), df(1_000)],
        &cpu_states(),
        &[9_000, 8_000, 1_000, 1_000],
        &BTreeSet::from([0, 1, 2, 3]),
        10_000_000,
        aggressive_policy(),
    );

    assert_eq!(decision.current_source_df_x100, 6_000);
    assert_eq!(decision.predicted_source_df_x100, 5_000);
}

#[test]
fn unstable_valid_signature_still_drives_effects() {
    let meta = ManagedThreadState {
        signature: ThreadSignature {
            valid: true,
            stable: false,
            projected_df_pressure_x100: 500,
            projected_llc_pressure_x100: 300,
            ..ThreadSignature::default()
        },
        ..ManagedThreadState::default()
    };
    let task = task_with_bw(0, 0, 900_000);
    let effect = task_signature_effect(&task, Some(&meta), 2_000_000);

    assert!(effect.signature_valid);
    assert_eq!(effect.df_x100, 500);
    assert_eq!(effect.llc_x100, 300);
}

#[test]
fn overloaded_task_with_unstable_valid_signature_uses_signature_in_placement() {
    let meta = ManagedThreadState {
        signature: ThreadSignature {
            valid: true,
            stable: false,
            projected_df_pressure_x100: 900_000,
            projected_llc_pressure_x100: 2_000,
            ..ThreadSignature::default()
        },
        ..ManagedThreadState::default()
    };
    let mut planned = planned_states(&[2, 1], &[agg(0, 4_000), agg(0, 1_000)]);
    planned[0].contributor_count = 0;
    planned[1].contributor_count = 0;

    let decision = choose_placement(
        &task_with_bw(0, 0, 900_000),
        Some(&meta),
        &topo(),
        &mapping(),
        &planned,
        &[
            llc(6_000, HotCoolState::Hot),
            llc(1_000, HotCoolState::Cool),
        ],
        &[df(4_200_000), df(1_000_000)],
        &cpu_states(),
        &[9_000, 8_000, 1_000, 1_000],
        &BTreeSet::from([0, 1, 2, 3]),
        10_000_000,
        aggressive_policy(),
    );

    assert!(decision.signature_valid);
    assert_eq!(decision.current_source_df_x100, 900_000);
    assert_eq!(decision.predicted_source_df_x100, 0);
}

#[test]
fn tick_trigger_stay_marks_tick_decision() {
    let decision = choose_placement(
        &tick_task(2, 1, 900_000),
        None,
        &topo(),
        &mapping(),
        &planned_states(&domain_task_counts(), &domain_signature_sums()),
        &[
            llc(9_000, HotCoolState::Hot),
            llc(2_000, HotCoolState::Cool),
        ],
        &[df(8_000), df(2_000)],
        &cpu_states(),
        &[9_000, 8_000, 1_000, 1_000],
        &BTreeSet::from([0, 1, 2, 3]),
        10_000_000,
        aggressive_policy(),
    );

    assert_eq!(decision.trigger, QueueTrigger::Tick);
    assert_eq!(decision.tick_decision, TickDecision::Stay);
    assert_eq!(decision.reason, DecisionReason::StayCurrentDomain);
}

#[test]
fn tick_move_phase_gate_staggers_cross_domain_moves_by_tid() {
    let mut allowed = tick_task(0, 0, 900_000);
    allowed.tid = 1000;
    let mut blocked = tick_task(0, 0, 900_000);
    blocked.tid = 1001;
    let meta = stable_meta_with_domain_df(200_000, 200_000, 2_000);

    let allowed_decision = choose_placement(
        &allowed,
        Some(&meta),
        &topo(),
        &mapping(),
        &planned_states(&[2, 1], &[agg(3_550_000, 9_000), agg(1_000_000, 2_000)]),
        &[
            llc(9_000, HotCoolState::Hot),
            llc(2_000, HotCoolState::Cool),
        ],
        &[df(3_550_000), df(1_000_000)],
        &cpu_states(),
        &[9_000, 8_000, 1_000, 1_000],
        &BTreeSet::from([0, 1, 2, 3]),
        10_000_000,
        aggressive_policy(),
    );

    let blocked_decision = choose_placement(
        &blocked,
        Some(&meta),
        &topo(),
        &mapping(),
        &planned_states(&[2, 1], &[agg(3_550_000, 9_000), agg(1_000_000, 2_000)]),
        &[
            llc(9_000, HotCoolState::Hot),
            llc(2_000, HotCoolState::Cool),
        ],
        &[df(3_550_000), df(1_000_000)],
        &cpu_states(),
        &[9_000, 8_000, 1_000, 1_000],
        &BTreeSet::from([0, 1, 2, 3]),
        10_000_000,
        policy(),
    );

    assert_eq!(allowed_decision.reason, DecisionReason::MoveCoolerDomain);
    assert_eq!(allowed_decision.selected_domain, Some(1));
    assert_eq!(blocked_decision.reason, DecisionReason::MoveCoolerDomain);
    assert_eq!(blocked_decision.selected_domain, Some(1));
    assert_eq!(blocked_decision.tick_decision, TickDecision::Move);
}

#[test]
fn planned_state_commit_keeps_follow_on_move_out_of_full_domain() {
    let meta = stable_meta_with_domain_df(900_000, 900_000, 2_000);
    let mut planned = planned_states(&[2, 1], &[agg(4_200_000, 4_000), agg(1_000_000, 1_000)]);
    let first = choose_placement(
        &task_with_bw(0, 0, 900_000),
        Some(&meta),
        &topo(),
        &mapping(),
        &planned,
        &[
            llc(6_000, HotCoolState::Hot),
            llc(1_000, HotCoolState::Cool),
        ],
        &[df(4_200_000), df(1_000_000)],
        &cpu_states(),
        &[9_000, 8_000, 1_000, 1_000],
        &BTreeSet::from([0, 1, 2, 3]),
        10_000_000,
        policy(),
    );

    assert_eq!(first.selected_domain, Some(1));
    assert_eq!(first.reason, DecisionReason::MoveCoolerDomain);

    planned[0].df_pressure_x100 = first.predicted_source_df_x100;
    planned[0].llc_pressure_x100 = first.predicted_source_llc_x100;
    planned[0].task_count = planned[0].task_count.saturating_sub(1);
    planned[0].outgoing_migrations = planned[0].outgoing_migrations.saturating_add(1);
    planned[0].contributor_count = 1;
    planned[0].stable_count = 1;
    planned[1].df_pressure_x100 = first.predicted_destination_df_x100;
    planned[1].llc_pressure_x100 = first.predicted_destination_llc_x100;
    planned[1].task_count = planned[1].task_count.saturating_add(1);
    planned[1].incoming_migrations = planned[1].incoming_migrations.saturating_add(1);
    planned[1].contributor_count = 2;
    planned[1].stable_count = 2;

    let second = choose_placement(
        &task_with_bw(1, 0, 900_000),
        Some(&meta),
        &topo(),
        &mapping(),
        &planned,
        &[
            llc(6_000, HotCoolState::Hot),
            llc(1_000, HotCoolState::Cool),
        ],
        &[
            df(first.predicted_source_df_x100),
            df(first.predicted_destination_df_x100),
        ],
        &cpu_states(),
        &[9_000, 8_000, 1_000, 1_000],
        &BTreeSet::from([0, 1, 2, 3]),
        10_000_000,
        policy(),
    );

    assert_eq!(second.selected_domain, Some(0));
    assert_eq!(second.reason, DecisionReason::DestinationGuard);
}

#[test]
fn migration_budget_can_block_extra_same_round_move() {
    let meta = stable_meta(900_000, 2_000);
    let mut planned = planned_states(&[2, 1], &[agg(4_200_000, 4_000), agg(1_000_000, 1_000)]);
    planned[0].outgoing_migrations = planned[0].migration_budget;

    let decision = choose_placement(
        &task_with_bw(0, 0, 900_000),
        Some(&meta),
        &topo(),
        &mapping(),
        &planned,
        &[
            llc(6_000, HotCoolState::Hot),
            llc(1_000, HotCoolState::Cool),
        ],
        &[df(4_200_000), df(1_000_000)],
        &cpu_states(),
        &[9_000, 8_000, 1_000, 1_000],
        &BTreeSet::from([0, 1, 2, 3]),
        10_000_000,
        policy(),
    );

    assert_eq!(decision.selected_domain, Some(0));
    assert_eq!(decision.reason, DecisionReason::MigrationBudget);
}

#[test]
fn reverse_hysteresis_blocks_immediate_return_to_previous_domain() {
    let mut meta = stable_meta(200_000, 2_000);
    meta.last_migration_from_domain = Some(0);
    meta.last_migration_to_domain = Some(1);
    meta.reverse_protect_until_ns = 20_000_000;

    let decision = choose_placement(
        &task_with_bw(2, 1, 900_000),
        Some(&meta),
        &topo(),
        &mapping(),
        &planned_states(&[1, 1], &[agg(3_700_000, 1_000), agg(4_000_000, 6_000)]),
        &[
            llc(1_000, HotCoolState::Cool),
            llc(6_000, HotCoolState::Hot),
        ],
        &[df(3_700_000), df(4_000_000)],
        &cpu_states(),
        &[1_000, 1_000, 8_000, 8_000],
        &BTreeSet::from([0, 1, 2, 3]),
        10_000_000,
        policy(),
    );

    assert_eq!(decision.selected_domain, Some(1));
    assert_eq!(decision.reason, DecisionReason::ReverseHysteresis);
}

#[test]
fn destination_guard_blocks_move_that_overheats_target_domain() {
    let meta = stable_meta(500_000, 3_500);
    let decision = choose_placement(
        &task_with_bw(2, 1, 900_000),
        Some(&meta),
        &topo(),
        &mapping(),
        &planned_states(&[1, 1], &[agg(3_700_000, 9_400), agg(4_000_000, 9_500)]),
        &[
            llc(1_000, HotCoolState::Cool),
            llc(9_500, HotCoolState::Hot),
        ],
        &[df(3_700_000), df(4_000_000)],
        &cpu_states(),
        &[1_000, 1_000, 8_000, 8_000],
        &BTreeSet::from([0, 1, 2, 3]),
        10_000_000,
        policy(),
    );

    assert_eq!(decision.selected_domain, Some(1));
    assert_eq!(decision.reason, DecisionReason::DestinationGuard);
}

#[test]
fn same_domain_stay_keeps_current_cpu_if_there_is_no_overlap() {
    let decision = choose_placement(
        &task_with_bw(2, 1, 900_000),
        None,
        &topo(),
        &mapping(),
        &planned_states(&domain_task_counts(), &domain_signature_sums()),
        &[
            llc(9_000, HotCoolState::Hot),
            llc(2_000, HotCoolState::Cool),
        ],
        &[df(8_000), df(2_000)],
        &cpu_states(),
        &[9_000, 8_000, 9_000, 1_000],
        &BTreeSet::from([0, 1, 2, 3]),
        10_000_000,
        policy(),
    );

    assert_eq!(decision.selected_domain, Some(1));
    assert_eq!(decision.selected_cpu, Some(2));
    assert_eq!(decision.reason, DecisionReason::StayCurrentDomain);
}

#[test]
fn same_domain_stay_moves_off_current_cpu_when_peer_is_running_there() {
    let decision = choose_placement(
        &task_with_bw(2, 1, 900_000),
        None,
        &topo(),
        &mapping(),
        &planned_states(&domain_task_counts(), &domain_signature_sums()),
        &[
            llc(9_000, HotCoolState::Hot),
            llc(2_000, HotCoolState::Cool),
        ],
        &[df(8_000), df(2_000)],
        &cpu_states_with_busy_peer_on_current_cpu(),
        &[9_000, 8_000, 1_000, 1_000],
        &BTreeSet::from([0, 1, 2, 3]),
        10_000_000,
        policy(),
    );

    assert_eq!(decision.selected_domain, Some(1));
    assert_eq!(decision.selected_cpu, Some(3));
    assert_eq!(decision.reason, DecisionReason::StayCurrentDomain);
}

#[test]
fn tick_same_domain_stay_keeps_legal_current_cpu_for_seeded_cases() {
    let mut seed = 0xcafe_f00d_51ab_2026_u64;

    for case_idx in 0..256_u32 {
        let current_cpu = 2 + (next_seeded_u32(&mut seed) % 2) as i32;
        let current_cpu_u32 = current_cpu as u32;
        let mut task = tick_task(
            current_cpu,
            1,
            if next_seeded_u32(&mut seed) % 2 == 0 {
                900_000
            } else {
                10_000
            },
        );
        task.tid = 1_001 + case_idx as i32;
        task.tgid = 1_000;
        task.trigger = if next_seeded_u32(&mut seed) % 2 == 0 {
            QueueTrigger::Tick as u32
        } else {
            QueueTrigger::VillainReslice as u32
        };
        task.tick_seq = u64::from(next_seeded_u32(&mut seed));

        let cpu_states = (0..4_u32)
            .map(|cpu| {
                let owner_roll = next_seeded_u32(&mut seed) % 4;
                CpuStateValue {
                    domain_id: if cpu < 2 { 0 } else { 1 },
                    idle: next_seeded_u32(&mut seed) % 2,
                    cpu_dsq_depth: next_seeded_u32(&mut seed) % 4,
                    current_tid: match owner_roll {
                        0 => 0,
                        1 => task.tid as u32,
                        _ => 20_000 + case_idx * 8 + cpu,
                    },
                    last_update_ns: 0,
                }
            })
            .collect::<Vec<_>>();
        let cpu_util_basis_points = (0..4)
            .map(|_| next_seeded_u32(&mut seed) % 10_000)
            .collect::<Vec<_>>();
        let mut planned_cpu_states = vec![PlannedCpuState::default(); 4];
        for cpu in 0..4_u32 {
            let job_count = next_seeded_u32(&mut seed) % 3;
            planned_cpu_states[cpu as usize].jobs = (0..job_count)
                .map(|idx| 30_000 + case_idx * 8 + cpu * 3 + idx)
                .collect();
            if next_seeded_u32(&mut seed) % 5 == 0 {
                planned_cpu_states[cpu as usize].jobs.push(task.tid as u32);
            }
        }
        let allowed_cpus = match next_seeded_u32(&mut seed) % 3 {
            0 => BTreeSet::from([current_cpu_u32]),
            1 => BTreeSet::from([2, 3]),
            _ => BTreeSet::from([0, 1, 2, 3]),
        };

        let decision = choose_placement_with_planned_cpus(
            &task,
            None,
            &topo(),
            &mapping(),
            &planned_states(&domain_task_counts(), &domain_signature_sums()),
            &[
                llc(9_000, HotCoolState::Hot),
                llc(2_000, HotCoolState::Cool),
            ],
            &[df(8_000), df(2_000)],
            &cpu_states,
            &cpu_util_basis_points,
            &planned_cpu_states,
            &allowed_cpus,
            10_000_000,
            policy(),
        );

        assert_eq!(
            decision.selected_domain,
            Some(1),
            "case {case_idx}: {decision:?}"
        );
        assert_eq!(
            decision.selected_cpu,
            Some(current_cpu_u32),
            "case {case_idx} seed={seed:#x} task={task:?} cpu_states={cpu_states:?} cpu_utils={cpu_util_basis_points:?} planned_cpu_states={planned_cpu_states:?} allowed={allowed_cpus:?} decision={decision:?}"
        );
        assert_eq!(
            decision.tick_decision,
            TickDecision::Stay,
            "case {case_idx}: {decision:?}"
        );
        assert_eq!(
            decision.reason,
            DecisionReason::StayCurrentDomain,
            "case {case_idx}: {decision:?}"
        );
    }
}

#[test]
fn same_domain_stay_does_not_blindly_reuse_busy_current_cpu() {
    let decision = choose_placement(
        &task_with_bw(2, 1, 900_000),
        None,
        &topo(),
        &mapping(),
        &planned_states(&domain_task_counts(), &domain_signature_sums()),
        &[
            llc(9_000, HotCoolState::Hot),
            llc(2_000, HotCoolState::Cool),
        ],
        &[df(8_000), df(2_000)],
        &cpu_states_with_busy_current_and_no_idle_peer_on_current_domain(),
        &[9_000, 8_000, 1_000, 1_000],
        &BTreeSet::from([0, 1, 2, 3]),
        10_000_000,
        policy(),
    );

    assert_eq!(decision.selected_domain, Some(1));
    assert_eq!(decision.selected_cpu, Some(3));
    assert_eq!(decision.reason, DecisionReason::StayCurrentDomain);
}

#[test]
fn same_domain_stay_keeps_current_cpu_when_task_is_running_there() {
    let decision = choose_placement(
        &task_with_bw(2, 1, 900_000),
        None,
        &topo(),
        &mapping(),
        &planned_states(&domain_task_counts(), &domain_signature_sums()),
        &[
            llc(9_000, HotCoolState::Hot),
            llc(2_000, HotCoolState::Cool),
        ],
        &[df(8_000), df(2_000)],
        &cpu_states_with_idle_sibling_on_current_domain(),
        &[9_000, 8_000, 6_000, 1_000],
        &BTreeSet::from([0, 1, 2, 3]),
        10_000_000,
        policy(),
    );

    assert_eq!(decision.selected_domain, Some(1));
    assert_eq!(decision.selected_cpu, Some(2));
    assert_eq!(decision.reason, DecisionReason::StayCurrentDomain);
}

#[test]
fn free_cpu_domain_beats_cooler_but_spinning_domain() {
    let meta = stable_meta(400_000, 2_000);
    let decision = choose_placement(
        &task_with_bw(0, 0, 900_000),
        Some(&meta),
        &topo(),
        &mapping(),
        &planned_states(&[2, 1], &[agg(400_000, 2_000), agg(350_000, 2_000)]),
        &[
            llc(2_000, HotCoolState::Cool),
            llc(6_000, HotCoolState::Hot),
        ],
        &[df(1_000_000), df(900_000)],
        &cpu_states_domain0_spinning_domain1_free(),
        &[8_000, 8_000, 1_000, 7_000],
        &BTreeSet::from([0, 1, 2, 3]),
        10_000_000,
        aggressive_policy(),
    );

    assert_eq!(decision.selected_domain, Some(1));
    assert_eq!(decision.selected_cpu, Some(2));
    assert_eq!(decision.reason, DecisionReason::MoveCoolerDomain);
}

#[test]
fn same_domain_stay_ignores_planned_job_imbalance_without_overlap() {
    let decision = choose_placement_with_planned_cpus(
        &task_with_bw(2, 1, 900_000),
        None,
        &topo(),
        &mapping(),
        &planned_states(&domain_task_counts(), &domain_signature_sums()),
        &[
            llc(9_000, HotCoolState::Hot),
            llc(2_000, HotCoolState::Cool),
        ],
        &[df(8_000), df(2_000)],
        &cpu_states(),
        &[9_000, 8_000, 1_000, 1_000],
        &planned_cpu_states_with_counts(&[(2, 3)]),
        &BTreeSet::from([0, 1, 2, 3]),
        10_000_000,
        policy(),
    );

    assert_eq!(decision.selected_domain, Some(1));
    assert_eq!(decision.selected_cpu, Some(2));
    assert_eq!(decision.reason, DecisionReason::StayCurrentDomain);
}

#[test]
fn cross_domain_move_prefers_less_packed_idle_cpu_in_target_domain() {
    let decision = choose_placement_with_planned_cpus(
        &task(0, 0),
        Some(&stable_meta_with_domain_df(900_000, 900_000, 2_000)),
        &topo(),
        &mapping(),
        &planned_states(&[2, 1], &[agg(4_200_000, 9_000), agg(1_000_000, 2_000)]),
        &[
            llc(9_000, HotCoolState::Hot),
            llc(2_000, HotCoolState::Cool),
        ],
        &[df(4_200_000), df(1_000_000)],
        &cpu_states(),
        &[9_000, 8_000, 1_000, 1_000],
        &planned_cpu_states_with_counts(&[(2, 2)]),
        &BTreeSet::from([0, 1, 2, 3]),
        10_000_000,
        aggressive_policy(),
    );

    assert_eq!(decision.selected_domain, Some(1));
    assert_eq!(decision.selected_cpu, Some(3));
    assert_eq!(decision.reason, DecisionReason::MoveCoolerDomain);
}

#[test]
fn free_slot_domain_beats_better_scored_full_domain() {
    let decision = choose_placement_with_planned_cpus(
        &task_with_bw(2, 1, 900_000),
        Some(&stable_meta_with_domain_df(400_000, 400_000, 2_000)),
        &topo(),
        &mapping(),
        &planned_states(&[0, 2], &[agg(450_000, 2_500), agg(200_000, 2_000)]),
        &[
            llc(2_500, HotCoolState::Cool),
            llc(2_000, HotCoolState::Cool),
        ],
        &[df(450_000), df(200_000)],
        &cpu_states_domain0_free_domain1_full(),
        &[1_000, 1_000, 8_000, 8_000],
        &planned_cpu_states_with_counts(&[(2, 1), (3, 1)]),
        &BTreeSet::from([0, 1, 2, 3]),
        10_000_000,
        aggressive_policy(),
    );

    assert_eq!(decision.selected_domain, Some(0));
    assert_eq!(decision.selected_cpu, Some(0));
    assert_eq!(decision.reason, DecisionReason::MoveCoolerDomain);
}

#[test]
fn slot_penalty_uses_affinity_constrained_cpu_capacity() {
    let decision = choose_placement_with_planned_cpus(
        &task_with_bw(2, 1, 900_000),
        Some(&stable_meta_with_domain_df(400_000, 400_000, 2_000)),
        &topo(),
        &mapping(),
        &planned_states(&[0, 1], &[agg(450_000, 2_500), agg(200_000, 2_000)]),
        &[
            llc(2_500, HotCoolState::Cool),
            llc(2_000, HotCoolState::Cool),
        ],
        &[df(450_000), df(200_000)],
        &cpu_states_domain0_free_domain1_full(),
        &[1_000, 1_000, 8_000, 8_000],
        &planned_cpu_states_with_counts(&[(2, 1)]),
        &BTreeSet::from([0, 1, 2]),
        10_000_000,
        aggressive_policy(),
    );

    assert_eq!(decision.selected_domain, Some(0));
    assert_eq!(decision.selected_cpu, Some(0));
    assert_eq!(decision.reason, DecisionReason::MoveCoolerDomain);
}

// Verifies the paper-level behavior that an under-subscribed IO chiplet path
// should defer to the default/current placement instead of migrating only to
// smooth a learned traffic-signature imbalance. Expected: source and
// destination live DF are below capacity, so the task stays on its current CCD.
fn assert_eval_policy_pdf_under_subscribed_io_defer_keeps_current_domain() {
    let meta = stable_meta_with_domain_df(9_000_00, 0, 15_00);
    let task = task_with_bw(0, 0, 9_000_00);
    // Raw mock data:
    // domain0 live DF = 18,000 MiB/s, below the 20,000 MiB/s policy capacity.
    // domain1 live DF =  1,000 MiB/s, also under-subscribed.
    // The only imbalance is the learned per-thread/domain signature aggregate.
    let decision = choose_placement_with_planned_cpus(
        &task,
        Some(&meta),
        &topo(),
        &mapping(),
        &planned_states(&[2, 0], &[agg(18_000_00, 30_00), agg(1_000_00, 5_00)]),
        &[
            llc(30_00, HotCoolState::Cool),
            llc(5_00, HotCoolState::Cool),
        ],
        &[df(18_000_00), df(1_000_00)],
        &pdf_current_task_on_domain0_with_idle_destination(),
        &[10_00, 5_00, 1_00, 1_00],
        &[],
        &BTreeSet::from([0, 1, 2, 3]),
        10_000_000,
        aggressive_policy(),
    );

    assert_eq!(decision.selected_domain, Some(0));
    assert_eq!(decision.selected_cpu, Some(0));
    assert_eq!(decision.reason, DecisionReason::StayCurrentDomain);
}

#[test]
fn eval_policy_pdf_under_subscribed_io_defer_keeps_current_domain() {
    assert_eval_policy_pdf_under_subscribed_io_defer_keeps_current_domain();
}

#[test]
fn eval_policy_under_subscribed_gate_uses_mapping_df_capacity() {
    let meta = stable_meta_with_domain_df(9_000_00, 0, 15_00);
    let task = task_with_bw(0, 0, 9_000_00);
    let mut mapping = mapping();
    mapping.domain_to_df_capacity_mib_s_x100[0] = Some(15_000_00);

    let decision = choose_placement_with_planned_cpus(
        &task,
        Some(&meta),
        &topo(),
        &mapping,
        &planned_states(&[2, 0], &[agg(18_000_00, 30_00), agg(1_000_00, 5_00)]),
        &[
            llc(30_00, HotCoolState::Cool),
            llc(5_00, HotCoolState::Cool),
        ],
        &[df(18_000_00), df(1_000_00)],
        &pdf_current_task_on_domain0_with_idle_destination(),
        &[10_00, 5_00, 1_00, 1_00],
        &[],
        &BTreeSet::from([0, 1, 2, 3]),
        10_000_000,
        aggressive_policy(),
    );

    assert_eq!(decision.selected_domain, Some(1));
    assert_eq!(decision.reason, DecisionReason::MoveCoolerDomain);
}

#[cfg(feature = "scheduler-paper-greedy")]
#[test]
fn paper_greedy_eval_policy_pdf_under_subscribed_io_defer_keeps_current_domain() {
    assert_eval_policy_pdf_under_subscribed_io_defer_keeps_current_domain();
}

#[cfg(feature = "scheduler-paper-greedy")]
#[test]
fn paper_greedy_under_subscribed_io_can_move_when_current_domain_is_over_capacity() {
    let meta = stable_meta_with_domain_df(4_000_00, 0, 10_00);
    let task = task_with_bw(0, 0, 4_000_00);

    let decision = choose_placement_with_planned_cpus(
        &task,
        Some(&meta),
        &topo(),
        &mapping(),
        &planned_states(&[3, 0], &[agg(8_000_00, 10_00), agg(1_000_00, 5_00)]),
        &[
            llc(10_00, HotCoolState::Cool),
            llc(5_00, HotCoolState::Cool),
        ],
        &[df(8_000_00), df(1_000_00)],
        &pdf_current_task_on_domain0_with_idle_destination(),
        &[10_00, 10_00, 1_00, 1_00],
        &[],
        &BTreeSet::from([0, 1, 2, 3]),
        10_000_000,
        aggressive_policy(),
    );

    assert_eq!(decision.selected_domain, Some(1));
    assert_eq!(decision.selected_cpu, Some(2));
    assert_eq!(decision.reason, DecisionReason::MoveCoolerDomain);
}

// Verifies the paper-level behavior that when an egress path is congested but
// there is no legal cooler destination, cSwitch should fall back to rate
// control rather than merely returning a default-slice stay/no-candidate
// decision. Expected: a selected villain is resliced.
fn assert_eval_policy_pdf_congested_no_destination_uses_rate_control() {
    let meta = stable_meta_with_domain_df(9_000_00, 0, 90_00);
    let task = villain_reslice_task(0, 0, 9_000_00);
    // Raw mock data:
    // domain0 live DF = 25,000 MiB/s, above the 20,000 MiB/s policy capacity.
    // domain1 is cooler, but the task affinity allows only domain0 CPUs.
    let decision = choose_placement_with_planned_cpus(
        &task,
        Some(&meta),
        &topo(),
        &mapping(),
        &planned_states(&[2, 0], &[agg(25_000_00, 90_00), agg(1_000_00, 5_00)]),
        &[llc(90_00, HotCoolState::Hot), llc(5_00, HotCoolState::Cool)],
        &[df(25_000_00), df(1_000_00)],
        &pdf_current_task_on_domain0_with_idle_destination(),
        &[10_00, 5_00, 1_00, 1_00],
        &[],
        &BTreeSet::from([0, 1]),
        10_000_000,
        aggressive_policy(),
    );

    assert_eq!(decision.selected_domain, Some(0));
    assert_eq!(decision.selected_cpu, Some(0));
    assert_eq!(decision.tick_decision, TickDecision::Reslice);
    assert!(decision.slice_ns < DEFAULT_SLICE_US * 1_000);
}

#[test]
fn eval_policy_pdf_congested_no_destination_tick_waits_for_villain_selection() {
    let meta = stable_meta_with_domain_df(9_000_00, 0, 90_00);
    let task = tick_task(0, 0, 9_000_00);
    let decision = choose_placement_with_planned_cpus(
        &task,
        Some(&meta),
        &topo(),
        &mapping(),
        &planned_states(&[2, 0], &[agg(25_000_00, 90_00), agg(1_000_00, 5_00)]),
        &[llc(90_00, HotCoolState::Hot), llc(5_00, HotCoolState::Cool)],
        &[df(25_000_00), df(1_000_00)],
        &pdf_current_task_on_domain0_with_idle_destination(),
        &[10_00, 5_00, 1_00, 1_00],
        &[],
        &BTreeSet::from([0, 1]),
        10_000_000,
        aggressive_policy(),
    );

    assert_eq!(decision.selected_domain, Some(0));
    assert_eq!(decision.selected_cpu, Some(0));
    assert_eq!(decision.tick_decision, TickDecision::Stay);
    assert_eq!(decision.slice_ns, DEFAULT_SLICE_US * 1_000);
}

#[test]
fn cs_villain_rate_control_allows_process_leader_pressure_producer() {
    let leader_tid = 3773096;
    let mut meta = stable_meta_with_domain_df(9_000_00, 0, 90_00);
    meta.tid = leader_tid;
    meta.tgid = leader_tid;
    meta.comm = "mc_stride_probe".to_string();
    let mut task = villain_reslice_task(0, 0, 9_000_00);
    set_task_identity(&mut task, leader_tid, leader_tid, "mc_stride_probe");

    // A process leader can be the task that actually produces memory pressure.
    // The policy must not skip rate control merely because tid == tgid or the
    // comm is the probe process name.
    let decision = choose_placement_with_planned_cpus(
        &task,
        Some(&meta),
        &topo(),
        &mapping(),
        &planned_states(&[2, 0], &[agg(25_000_00, 90_00), agg(1_000_00, 5_00)]),
        &[llc(90_00, HotCoolState::Hot), llc(5_00, HotCoolState::Cool)],
        &[df(25_000_00), df(1_000_00)],
        &pdf_current_task_on_domain0_with_idle_destination(),
        &[10_00, 5_00, 1_00, 1_00],
        &[],
        &BTreeSet::from([0, 1]),
        10_000_000,
        aggressive_policy(),
    );

    assert_eq!(decision.selected_domain, Some(0));
    assert_eq!(
        decision.tick_decision,
        TickDecision::Reslice,
        "process leader that produces pressure must remain rate-control eligible: {decision:?}"
    );
    assert!(decision.slice_ns < DEFAULT_SLICE_US * 1_000);
}

#[test]
fn eval_policy_pdf_congested_no_destination_uses_rate_control() {
    assert_eval_policy_pdf_congested_no_destination_uses_rate_control();
}

#[cfg(feature = "scheduler-paper-greedy")]
#[test]
fn paper_greedy_eval_policy_pdf_congested_no_destination_uses_rate_control() {
    assert_eval_policy_pdf_congested_no_destination_uses_rate_control();
}
