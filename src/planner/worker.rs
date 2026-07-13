use super::*;

pub fn start_worker(
    config: PlannerConfig,
    stop: Arc<AtomicBool>,
    pin_cpu: Option<u32>,
) -> anyhow::Result<(
    Sender<PlannerRequest>,
    Receiver<PlannerOutput>,
    thread::JoinHandle<()>,
)> {
    let (request_tx, request_rx) = bounded::<PlannerRequest>(1);
    let (result_tx, result_rx) = unbounded::<PlannerOutput>();
    let mut move_trace = PlannerMoveTraceLogger::new(config.move_trace_path.as_deref())?;
    let mut plan_debug = PlannerMoveTraceLogger::new(config.plan_debug_path.as_deref())?;
    let handle = thread::spawn(move || {
        if let Err(err) = maybe_pin_current_thread(pin_cpu) {
            log::warn!("planner failed to pin control thread: {err}");
        }
        while !stop.load(Ordering::Relaxed) {
            let Ok(first) = request_rx.recv_timeout(Duration::from_millis(10)) else {
                continue;
            };
            let profiler = crate::overhead_profile::global();
            let e2e_start = profiler.as_ref().map(|_| ThreadClockSample::capture());
            let mut snapshot = first.snapshot;
            let mut triggers = first.triggers;
            let start = Instant::now();
            while start.elapsed() < config.debounce {
                let remaining = config.debounce.saturating_sub(start.elapsed());
                match request_rx.recv_timeout(remaining) {
                    Ok(next) => {
                        snapshot = next.snapshot;
                        triggers.extend(next.triggers);
                    }
                    Err(_) => break,
                }
            }
            let output = compute_plan_with_debug(
                &config,
                snapshot,
                &triggers,
                &mut move_trace,
                &mut plan_debug,
            );
            let _ = result_tx.send(output);
            if let (Some(profiler), Some(start)) = (profiler, e2e_start) {
                profiler.record_scope("planner_e2e", start, true);
            }
        }
    });
    Ok((request_tx, result_rx, handle))
}

fn maybe_pin_current_thread(cpu: Option<u32>) -> std::io::Result<()> {
    let Some(cpu) = cpu else {
        return Ok(());
    };
    // SAFETY: `cpu_set_t` is a plain C bitset, and Linux affinity APIs expect it
    // to be zero-initialized before CPU_SET mutates the selected bit.
    let mut cpuset = unsafe { std::mem::zeroed::<libc::cpu_set_t>() };
    // SAFETY: `cpuset` is a valid mutable cpu_set_t. The caller supplies a kernel
    // CPU id, which libc's CPU_SET accepts as a bit index for this bitset.
    unsafe {
        libc::CPU_ZERO(&mut cpuset);
        libc::CPU_SET(cpu as usize, &mut cpuset);
    }
    // SAFETY: Passing pid 0 scopes the call to the current thread. `cpuset`
    // lives for the duration of the call and the size matches cpu_set_t.
    let ret =
        unsafe { libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &cpuset) };
    if ret != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

pub fn maybe_dedicated_control_plane_cpu(
    policy: ControlPlaneCpuPolicy,
    topo: &TopologyLayout,
    mapping: &MappingInfo,
) -> anyhow::Result<Option<u32>> {
    let candidate = mapping
        .excluded_domains
        .iter()
        .copied()
        .min()
        .and_then(|domain| topo.domains.get(domain as usize).map(|info| info.rep_cpu));
    match policy {
        ControlPlaneCpuPolicy::Mixed => Ok(None),
        ControlPlaneCpuPolicy::AutoDedicated => Ok(candidate),
        ControlPlaneCpuPolicy::Dedicated => candidate.map(Some).ok_or_else(|| {
            anyhow::anyhow!("no excluded domain available for dedicated control-plane CPU")
        }),
    }
}

pub fn sync_qualified_for_planner(
    thread: &ManagedThreadState,
    disable_auto_sync_hints: bool,
    sync_tgid_overrides: &BTreeSet<u32>,
) -> bool {
    sync_tgid_overrides.contains(&thread.tgid)
        || (!disable_auto_sync_hints && thread_sync_qualified(thread))
}
