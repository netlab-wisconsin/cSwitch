/* Copyright (c) Andrea Righi <andrea.righi@linux.dev> */
/*
 * scx_rustland_core: BPF backend for schedulers running in user-space.
 *
 * This BPF backend implements the low level sched-ext functionalities for a
 * user-space counterpart, that implements the actual scheduling policy.
 *
 * The BPF part collects total cputime and weight from the tasks that need to
 * run, then it sends all details to the user-space scheduler that decides the
 * best order of execution of the tasks (based on the collected metrics).
 *
 * The user-space scheduler then returns to the BPF component the list of tasks
 * to be dispatched in the proper order.
 *
 * Messages between the BPF component and the user-space scheduler are passed
 * using BPF_MAP_TYPE_RINGBUF / BPF_MAP_TYPE_USER_RINGBUF maps: @queued for
 * the messages sent by the BPF dispatcher to the user-space scheduler and
 * @dispatched for the messages sent by the user-space scheduler to the BPF
 * dispatcher.
 *
 * The BPF dispatcher is completely agnostic of the particular scheduling
 * policy implemented in user-space. For this reason developers that are
 * willing to use this scheduler to experiment scheduling policies should be
 * able to simply modify the Rust component, without having to deal with any
 * internal kernel / BPF details.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */
#ifdef LSP
#define __bpf__
#include "../../../../scheds/include/scx/common.bpf.h"
#else
#include <scx/common.bpf.h>
#endif

#include <scx/percpu.bpf.h>
#include "intf.h"

char _license[] SEC("license") = "GPL";

UEI_DEFINE(uei);

/*
 * Introduce a custom DSQ shared across all the CPUs, where we can dispatch
 * tasks that will be executed on the first CPU available.
 *
 * Per-CPU DSQs are also provided, to allow the scheduler to run a task on a
 * specific CPU (see dsq_init()).
 */
#define SHARED_DSQ MAX_CPUS

/*
 * The user-space scheduler itself is dispatched using a separate DSQ, that
 * is consumed after all other DSQs.
 *
 * This ensures to work in bursts: tasks are queued, then the user-space
 * scheduler runs and dispatches them. Once all these tasks exhaust their
 * time slices, the scheduler is invoked again, repeating the cycle.
 */
#define SCHED_DSQ (MAX_CPUS + 1)

/*
 * Scheduler attributes and statistics.
 */
const volatile u32 usersched_pid; /* User-space scheduler PID */
const volatile u32 khugepaged_pid; /* khugepaged PID */
u64 usersched_last_run_at; /* Timestamp of the last user-space scheduler execution */
static u64 nr_cpu_ids; /* Maximum possible CPU number */

/*
 * Default task time slice.
 */
const volatile u64 slice_ns;
const volatile bool enable_tick_resched;
const volatile bool enable_light_compete;
const volatile bool enable_bpf_stay_fastpath;
const volatile u32 tick_reeval_every;
const volatile u32 l2_need_mib_s_x100;
const volatile u32 tick_defer_max;
const volatile u64 llc_stale_ns;
const volatile u64 df_stale_ns;
const volatile u64 helper_slice_ns;

/*
 * Number of tasks that are queued for scheduling.
 *
 * This number is incremented by the BPF component when a task is queued to the
 * user-space scheduler and it must be decremented by the user-space scheduler
 * when a task is consumed.
 */
volatile u64 nr_queued;

/*
 * Number of tasks that are waiting for scheduling.
 *
 * This number must be updated by the user-space scheduler to keep track if
 * there is still some scheduling work to do.
 */
volatile u64 nr_scheduled;

/*
 * Amount of currently running tasks.
 */
volatile u64 nr_running, nr_online_cpus;

/* Dispatch statistics */
volatile u64 nr_user_dispatches, nr_kernel_dispatches,
	     nr_cancel_dispatches, nr_bounce_dispatches;
volatile u64 nr_dispatched_ringbuf_drains, nr_dispatch_task_missing,
	     nr_dispatch_cpu_inserts, nr_dispatch_shared_inserts,
	     nr_dispatch_cpu_kicks, nr_dispatch_cpu_consumes,
	     nr_dispatch_shared_consumes, nr_dispatch_sched_consumes,
	     nr_stale_dispatch_rescues;

/* Failure statistics */
volatile u64 nr_failed_dispatches, nr_sched_congested;
volatile u64 nr_tick_events, nr_tick_to_userspace, nr_tick_backoff_skip, nr_tick_fastpath_stay,
	     nr_tick_reslice, nr_reenqueue_local_fail;

/* Report additional debugging information */
const volatile bool debug;

/* Rely on the in-kernel idle CPU selection policy */
const volatile bool builtin_idle;

#define STALL_IPC_CAP_X1000 4000U
#define STALL_COUNTER_VALID_INST  (1U << 0)
#define STALL_COUNTER_VALID_CYCLES (1U << 1)
#define STALL_COUNTER_VALID_MASK (STALL_COUNTER_VALID_INST | STALL_COUNTER_VALID_CYCLES)

/* Allow to use bpf_printk() only when @debug is set */
#define dbg_msg(_fmt, ...) do {						\
	if (debug)							\
		bpf_printk(_fmt, ##__VA_ARGS__);			\
} while(0)

/*
 * CPUs in the system have SMT is enabled.
 */
const volatile bool smt_enabled = true;

/*
 * Maximum amount of tasks queued between kernel and user-space at a certain
 * time.
 *
 * The @queued and @dispatched lists are used in a producer/consumer fashion
 * between the BPF part and the user-space part.
 */
#define MAX_ENQUEUED_TASKS 4096

/*
 * Maximum amount of slots reserved to the tasks dispatched via shared queue.
 */
#define MAX_DISPATCH_SLOT (MAX_ENQUEUED_TASKS / 8)

/*
 * The map containing tasks that are queued to user space from the kernel.
 *
 * This map is drained by the user-space scheduler.
 */
struct {
	__uint(type, BPF_MAP_TYPE_RINGBUF);
	__uint(max_entries, MAX_ENQUEUED_TASKS *
				sizeof(struct queued_task_ctx));
} queued SEC(".maps");

/*
 * The user ring buffer containing pids that are dispatched from user space to
 * the kernel.
 *
 * Drained by the kernel in .dispatch().
 */
struct {
	__uint(type, BPF_MAP_TYPE_USER_RINGBUF);
	__uint(max_entries, MAX_ENQUEUED_TASKS *
				sizeof(struct dispatched_task_ctx));
} dispatched SEC(".maps");

struct {
	__uint(type, BPF_MAP_TYPE_PERF_EVENT_ARRAY);
	__uint(max_entries, MAX_CPUS);
	__type(key, u32);
	__type(value, u32);
} perf_events_local_ccx SEC(".maps");

struct {
	__uint(type, BPF_MAP_TYPE_PERF_EVENT_ARRAY);
	__uint(max_entries, MAX_CPUS);
	__type(key, u32);
	__type(value, u32);
} perf_events_near_cache SEC(".maps");

struct {
	__uint(type, BPF_MAP_TYPE_PERF_EVENT_ARRAY);
	__uint(max_entries, MAX_CPUS);
	__type(key, u32);
	__type(value, u32);
} perf_events_dram_near SEC(".maps");

struct {
	__uint(type, BPF_MAP_TYPE_PERF_EVENT_ARRAY);
	__uint(max_entries, MAX_CPUS);
	__type(key, u32);
	__type(value, u32);
} perf_events_instructions SEC(".maps");

struct {
	__uint(type, BPF_MAP_TYPE_PERF_EVENT_ARRAY);
	__uint(max_entries, MAX_CPUS);
	__type(key, u32);
	__type(value, u32);
} perf_events_cpu_cycles SEC(".maps");

struct {
	__uint(type, BPF_MAP_TYPE_ARRAY);
	__uint(max_entries, MAX_DOMAINS);
	__type(key, u32);
	__type(value, struct llc_state_ctx);
} llc_state_map SEC(".maps");

struct {
	__uint(type, BPF_MAP_TYPE_ARRAY);
	__uint(max_entries, MAX_DOMAINS);
	__type(key, u32);
	__type(value, struct df_state_ctx);
} df_state_map SEC(".maps");

struct {
	__uint(type, BPF_MAP_TYPE_HASH);
	__uint(max_entries, MAX_CPUS * 4);
	__type(key, u32);
	__type(value, u8);
} internal_tgids SEC(".maps");

struct {
	__uint(type, BPF_MAP_TYPE_ARRAY);
	__uint(max_entries, MAX_CPUS);
	__type(key, u32);
	__type(value, u32);
} cpu_domain_map SEC(".maps");

struct {
	__uint(type, BPF_MAP_TYPE_ARRAY);
	__uint(max_entries, MAX_CPUS);
	__type(key, u32);
	__type(value, struct cpu_state_ctx);
} cpu_state_map SEC(".maps");

/*
 * Per-task local storage.
 *
 * This contain all the per-task information used internally by the BPF code.
 */
struct task_ctx {
	u32 tid;
	u32 tgid;
	u64 start_ts;
	u64 stop_ts;
	u64 exec_runtime;
	u64 enq_cnt;
	u64 run_start_ns;
	u64 run_start_fill_ctr[MEM_SOURCE_COUNT];
	u32 run_start_fill_valid_mask;
	u64 run_start_inst_ctr;
	u64 run_start_cycle_ctr;
	u32 run_start_stall_valid_mask;
	u32 last_l2_bw_mib_s_x100;
	u32 ewma_l2_bw_mib_s_x100;
	u32 last_fill_bw_mib_s_x100[MEM_SOURCE_COUNT];
	u32 ewma_fill_bw_mib_s_x100[MEM_SOURCE_COUNT];
	u32 last_ipc_x1000;
	u32 ewma_ipc_x1000;
	u32 last_stall_pct_x100;
	u32 ewma_stall_pct_x100;
	u32 pending_trigger;
	u32 tick_holdoff;
	s32 current_cpu;
	s32 current_domain;
	u64 tick_seq;
	u64 last_tick_ns;
	u64 last_update_ns;
};

/* Map that contains task-local storage. */
struct {
	__uint(type, BPF_MAP_TYPE_TASK_STORAGE);
	__uint(map_flags, BPF_F_NO_PREALLOC);
	__type(key, int);
	__type(value, struct task_ctx);
} task_ctx_stor SEC(".maps");

/*
 * Return a local task context from a generic task or NULL if the context
 * doesn't exist.
 */
struct task_ctx *try_lookup_task_ctx(const struct task_struct *p)
{
	struct task_ctx *tctx = bpf_task_storage_get(&task_ctx_stor,
						(struct task_struct *)p, 0, 0);
	if (!tctx)
		dbg_msg("warning: failed to get task context for pid=%d (%s)",
			p->pid, p->comm);
	return tctx;
}

static __always_inline s32 cpu_to_domain(u32 cpu)
{
	u32 *domain;

	if (cpu >= MAX_CPUS)
		return -1;

	domain = bpf_map_lookup_elem(&cpu_domain_map, &cpu);
	if (!domain || *domain == (__u32)-1)
		return -1;

	return (s32)*domain;
}

static __always_inline bool is_internal_task(const struct task_struct *p)
{
	u32 tgid = p->tgid;
	u8 *present;

	present = bpf_map_lookup_elem(&internal_tgids, &tgid);
	return present && *present;
}

static __always_inline bool domain_llc_hot(s32 domain, u64 now)
{
	struct llc_state_ctx *state;
	u32 key;

	if (domain < 0 || domain >= MAX_DOMAINS)
		return false;

	key = (u32)domain;
	state = bpf_map_lookup_elem(&llc_state_map, &key);
	if (!state || !state->valid || time_delta(now, state->sample_ts_ns) > llc_stale_ns)
		return false;

	return state->state == 1;
}

static __always_inline bool domain_df_hot(s32 domain, u64 now)
{
	struct df_state_ctx *state;
	u32 key;

	if (domain < 0 || domain >= MAX_DOMAINS)
		return false;

	key = (u32)domain;
	state = bpf_map_lookup_elem(&df_state_map, &key);
	if (!state || !state->valid || time_delta(now, state->sample_ts_ns) > df_stale_ns)
		return false;

	return state->state == 1;
}

static __always_inline bool task_should_fastpath_stay(const struct task_struct *p,
						      const struct task_ctx *tctx,
						      u64 now)
{
	if (!enable_bpf_stay_fastpath || !tctx)
		return false;
	if (tctx->current_cpu < 0 || tctx->current_domain < 0)
		return false;
	if (!bpf_cpumask_test_cpu(tctx->current_cpu, p->cpus_ptr))
		return false;
	if (tctx->ewma_l2_bw_mib_s_x100 >= l2_need_mib_s_x100)
		return false;
	if (domain_llc_hot(tctx->current_domain, now) || domain_df_hot(tctx->current_domain, now))
		return false;

	return true;
}

static __always_inline int read_local_ccx_counter(u32 cpu, u64 *counter)
{
	struct bpf_perf_event_value value = {};
	int err;

	if (cpu >= MAX_CPUS)
		return -EINVAL;

	err = bpf_perf_event_read_value(&perf_events_local_ccx, cpu, &value, sizeof(value));
	if (err)
		return err;

	*counter = value.counter;
	return 0;
}

static __always_inline int read_near_cache_counter(u32 cpu, u64 *counter)
{
	struct bpf_perf_event_value value = {};
	int err;

	if (cpu >= MAX_CPUS)
		return -EINVAL;

	err = bpf_perf_event_read_value(&perf_events_near_cache, cpu, &value, sizeof(value));
	if (err)
		return err;

	*counter = value.counter;
	return 0;
}

static __always_inline int read_dram_near_counter(u32 cpu, u64 *counter)
{
	struct bpf_perf_event_value value = {};
	int err;

	if (cpu >= MAX_CPUS)
		return -EINVAL;

	err = bpf_perf_event_read_value(&perf_events_dram_near, cpu, &value, sizeof(value));
	if (err)
		return err;

	*counter = value.counter;
	return 0;
}

static __always_inline int read_instructions_counter(u32 cpu, u64 *counter)
{
	struct bpf_perf_event_value value = {};
	int err;

	if (cpu >= MAX_CPUS)
		return -EINVAL;

	err = bpf_perf_event_read_value(&perf_events_instructions, cpu, &value, sizeof(value));
	if (err)
		return err;

	*counter = value.counter;
	return 0;
}

static __always_inline int read_cpu_cycles_counter(u32 cpu, u64 *counter)
{
	struct bpf_perf_event_value value = {};
	int err;

	if (cpu >= MAX_CPUS)
		return -EINVAL;

	err = bpf_perf_event_read_value(&perf_events_cpu_cycles, cpu, &value, sizeof(value));
	if (err)
		return err;

	*counter = value.counter;
	return 0;
}

static __always_inline int read_fill_counter(u32 cpu, u32 source_idx, u64 *counter)
{
	switch (source_idx) {
	case MEM_SOURCE_LOCAL_CCX:
		return read_local_ccx_counter(cpu, counter);
	case MEM_SOURCE_NEAR_CACHE:
		return read_near_cache_counter(cpu, counter);
	case MEM_SOURCE_DRAM_NEAR:
		return read_dram_near_counter(cpu, counter);
	default:
		return -EINVAL;
	}
}

static __always_inline struct cpu_state_ctx *lookup_cpu_state(u32 cpu)
{
	if (cpu >= MAX_CPUS)
		return NULL;

	return bpf_map_lookup_elem(&cpu_state_map, &cpu);
}

static __always_inline void set_cpu_state_tid(u32 cpu, u32 tid)
{
	struct cpu_state_ctx *state;

	state = lookup_cpu_state(cpu);
	if (!state)
		return;

	state->current_tid = tid;
	state->last_update_ns = scx_bpf_now();
}

static __always_inline void set_cpu_state_idle(u32 cpu, bool idle)
{
	struct cpu_state_ctx *state;

	state = lookup_cpu_state(cpu);
	if (!state)
		return;

	state->idle = idle;
	state->last_update_ns = scx_bpf_now();
}

static __always_inline void adjust_cpu_dsq_depth(u32 cpu, s32 delta)
{
	struct cpu_state_ctx *state;

	state = lookup_cpu_state(cpu);
	if (!state)
		return;

	if (delta >= 0)
		state->cpu_dsq_depth += (u32)delta;
	else
		state->cpu_dsq_depth -= MIN(state->cpu_dsq_depth, (u32)(-delta));
	state->last_update_ns = scx_bpf_now();
}

static __always_inline bool sample_slice_metrics(struct task_ctx *tctx, u64 now)
{
	u64 counter, delta_ctr, delta_ns;
	u64 total_last = 0, total_ewma = 0;
	u32 valid_mask = (1U << MEM_SOURCE_COUNT) - 1;
	bool first_sample;
	int err, idx;
	bool updated = false;

	if (!tctx || !tctx->run_start_ns || tctx->current_cpu < 0)
		return false;

	delta_ns = now - tctx->run_start_ns;
	if (delta_ns < MIN_VALID_RUN_NS)
		return false;

	first_sample = !tctx->last_update_ns;

	if (tctx->run_start_fill_valid_mask == valid_mask) {
#pragma unroll
		for (idx = 0; idx < MEM_SOURCE_COUNT; idx++) {
			err = read_fill_counter((u32)tctx->current_cpu, idx, &counter);
			if (err || counter < tctx->run_start_fill_ctr[idx])
				return false;

			delta_ctr = counter - tctx->run_start_fill_ctr[idx];
			tctx->last_fill_bw_mib_s_x100[idx] =
				(u32)((delta_ctr * L2_MIB_PER_SEC_X100_SCALE) / delta_ns);
			if (first_sample)
				tctx->ewma_fill_bw_mib_s_x100[idx] =
					tctx->last_fill_bw_mib_s_x100[idx];
			else
				tctx->ewma_fill_bw_mib_s_x100[idx] =
					(tctx->ewma_fill_bw_mib_s_x100[idx] * 7 +
					 tctx->last_fill_bw_mib_s_x100[idx]) / 8;
			total_last += tctx->last_fill_bw_mib_s_x100[idx];
			total_ewma += tctx->ewma_fill_bw_mib_s_x100[idx];
		}
		tctx->last_l2_bw_mib_s_x100 =
			total_last > (u64)((u32)-1) ? (u32)-1 : (u32)total_last;
		tctx->ewma_l2_bw_mib_s_x100 =
			total_ewma > (u64)((u32)-1) ? (u32)-1 : (u32)total_ewma;
		updated = true;
	}

	if (tctx->run_start_stall_valid_mask == STALL_COUNTER_VALID_MASK) {
		u64 inst_ctr = 0, cycle_ctr = 0;
		u64 delta_inst, delta_cycles, ipc_x1000, clamped_ipc_x1000;
		u64 stall_pct_x100;

		err = read_instructions_counter((u32)tctx->current_cpu, &inst_ctr);
		if (err || inst_ctr < tctx->run_start_inst_ctr)
			goto out;
		err = read_cpu_cycles_counter((u32)tctx->current_cpu, &cycle_ctr);
		if (err || cycle_ctr < tctx->run_start_cycle_ctr)
			goto out;

		delta_inst = inst_ctr - tctx->run_start_inst_ctr;
		delta_cycles = cycle_ctr - tctx->run_start_cycle_ctr;
		if (!delta_cycles)
			goto out;

		ipc_x1000 = (delta_inst * 1000) / delta_cycles;
		clamped_ipc_x1000 = MIN(ipc_x1000, (u64)STALL_IPC_CAP_X1000);
		stall_pct_x100 = ((u64)STALL_IPC_CAP_X1000 - clamped_ipc_x1000) * 10000 /
				 (u64)STALL_IPC_CAP_X1000;

		tctx->last_ipc_x1000 =
			ipc_x1000 > (u64)((u32)-1) ? (u32)-1 : (u32)ipc_x1000;
		tctx->last_stall_pct_x100 = (u32)stall_pct_x100;
		if (first_sample) {
			tctx->ewma_ipc_x1000 = tctx->last_ipc_x1000;
			tctx->ewma_stall_pct_x100 = tctx->last_stall_pct_x100;
		} else {
			tctx->ewma_ipc_x1000 =
				(tctx->ewma_ipc_x1000 * 7 + tctx->last_ipc_x1000) / 8;
			tctx->ewma_stall_pct_x100 =
				(tctx->ewma_stall_pct_x100 * 7 + tctx->last_stall_pct_x100) / 8;
		}
		updated = true;
	}

out:
	if (updated)
		tctx->last_update_ns = now;
	return updated;
}

/*
 * Heartbeat timer used to periodically trigger the check to run the user-space
 * scheduler.
 *
 * Without this timer we may starve the scheduler if the system is completely
 * idle and hit the watchdog that would auto-kill this scheduler.
 */
struct usersched_timer {
	struct bpf_timer timer;
};

struct {
	__uint(type, BPF_MAP_TYPE_ARRAY);
	__uint(max_entries, 1);
	__type(key, u32);
	__type(value, struct usersched_timer);
} usersched_timer SEC(".maps");

/*
 * Time period of the scheduler heartbeat, used to periodically kick the
 * user-space scheduler and check if there is any pending activity.
 */
#define USERSCHED_TIMER_NS	NSEC_PER_SEC

/*
 * Return true if the target task @p is the user-space scheduler.
 */
static inline bool is_usersched_task(const struct task_struct *p)
{
	/*
	 * usersched_pid is populated from std::process::id(), which is the
	 * scheduler process TGID. Match against TGID rather than the single thread
	 * PID so helper threads from the scheduler process can never be fed back
	 * into the managed workload path if they enter sched_ext in the future.
	 */
	return usersched_pid && p->tgid == usersched_pid;
}

/*
 * Return true if the target task @p is kswapd.
 */
static inline bool is_kswapd(const struct task_struct *p)
{
        return p->flags & (PF_KSWAPD | PF_KCOMPACTD);
}

/*
 * Return true if the target task @p is khugepaged, false otherwise.
 */
static inline bool is_khugepaged(const struct task_struct *p)
{
	return khugepaged_pid && p->pid == khugepaged_pid;
}

/*
 * Return true if @p still wants to run, false otherwise.
 */
static bool is_queued(const struct task_struct *p)
{
	return p->scx.flags & SCX_TASK_QUEUED;
}

/*
 * Flag used to wake-up the user-space scheduler.
 */
static volatile u32 usersched_needed;

/*
 * Set user-space scheduler wake-up flag (equivalent to an atomic release
 * operation).
 */
static void set_usersched_needed(void)
{
	__sync_fetch_and_or(&usersched_needed, 1);
}

/*
 * Check and clear user-space scheduler wake-up flag (equivalent to an atomic
 * acquire operation).
 */
static bool test_and_clear_usersched_needed(void)
{
	return __sync_fetch_and_and(&usersched_needed, 0) == 1;
}

/*
 * Return true if there's any pending activity to do for the scheduler, false
 * otherwise.
 *
 * NOTE: a task is sent to the user-space scheduler using the "queued"
 * ringbuffer, then the scheduler drains the queued tasks and adds them to
 * its internal data structures / state; at this point tasks become
 * "scheduled" and the user-space scheduler will take care of updating
 * nr_scheduled accordingly; lastly tasks will be dispatched and the
 * user-space scheduler will update nr_scheduled again.
 *
 * Checking nr_scheduled and the available data in the ringbuffer allows to
 * determine if there is still some pending work to do for the scheduler:
 * new tasks have been queued since last check, or there are still tasks
 * "queued" or "scheduled" since the previous user-space scheduler run.
 *
 * If there's no pending action, it is pointless to wake-up the scheduler
 * (even if a CPU becomes idle), because there is nothing to do.
 *
 * Also keep in mind that we don't need any protection here since this code
 * doesn't run concurrently with the user-space scheduler (that is single
 * threaded), therefore this check is also safe from a concurrency perspective.
 */
static bool usersched_has_pending_tasks(void)
{
	if (test_and_clear_usersched_needed())
		return true;

	if (nr_scheduled)
		return true;

	return bpf_ringbuf_query(&queued, BPF_RB_AVAIL_DATA) > 0;
}

/*
 * Return the DSQ ID associated to a CPU, or SHARED_DSQ if the CPU is not
 * valid.
 */
static u64 cpu_to_dsq(s32 cpu)
{
	if (cpu < 0 || cpu >= MAX_CPUS) {
		scx_bpf_error("Invalid cpu: %d", cpu);
		return SHARED_DSQ;
	}
	return (u64)cpu;
}

/*
 * Return true if @this_cpu and @that_cpu are in the same LLC, false
 * otherwise.
 */
static inline bool cpus_share_cache(s32 this_cpu, s32 that_cpu)
{
        if (this_cpu == that_cpu)
                return true;

	return cpu_llc_id(this_cpu) == cpu_llc_id(that_cpu);
}

/*
 * Return true if @this_cpu is faster than @that_cpu, false otherwise.
 */
static inline bool is_cpu_faster(s32 this_cpu, s32 that_cpu)
{
        if (this_cpu == that_cpu)
                return false;

	return cpu_priority(this_cpu) > cpu_priority(that_cpu);
}

/*
 * Return true if @cpu is a fully-idle SMT core, false otherwise.
 */
static inline bool is_smt_idle(s32 cpu)
{
	const struct cpumask *idle_smtmask;
        bool is_idle;

	if (!smt_enabled)
		return true;

	idle_smtmask = scx_bpf_get_idle_smtmask();
        is_idle = bpf_cpumask_test_cpu(cpu, idle_smtmask);
        scx_bpf_put_cpumask(idle_smtmask);

	return is_idle;
}

/*
 * Return true on a wake-up event, false otherwise.
 */
static inline bool is_wakeup(u64 wake_flags)
{
	return wake_flags & SCX_WAKE_TTWU;
}

/*
 * Find an idle CPU in the system for the task.
 *
 * NOTE: the idle CPU selection doesn't need to be formally perfect, it is
 * totally fine to accept racy conditions and potentially make mistakes, by
 * picking CPUs that are not idle or even offline, the logic has been designed
 * to handle these mistakes in favor of a more efficient response and a reduced
 * scheduling overhead.
 */
static s32 pick_idle_cpu(struct task_struct *p, s32 prev_cpu, u64 wake_flags)
{
	s32 cpu, this_cpu = bpf_get_smp_processor_id();
	bool is_this_cpu_allowed = bpf_cpumask_test_cpu(this_cpu, p->cpus_ptr);

	/*
	 * For tasks that can run only on a single CPU, we can simply verify if
	 * their only allowed CPU is still idle.
	 */
	if (p->nr_cpus_allowed == 1) {
		if (scx_bpf_test_and_clear_cpu_idle(prev_cpu))
			return prev_cpu;

		return -EBUSY;
	}

	/*
	 * On wakeup if the waker's CPU is faster than the wakee's CPU, try
	 * to move the wakee closer to the waker.
	 *
	 * In presence of hybrid cores this helps to naturally migrate
	 * tasks over to the faster cores.
	 */
	if (is_wakeup(wake_flags) &&
	    is_cpu_faster(this_cpu, prev_cpu) && is_this_cpu_allowed) {
		/*
		 * If both the waker's CPU and the wakee's CPU are in the
		 * same LLC and the wakee's CPU is a fully idle SMT core,
		 * don't migrate.
		 */
		if (cpus_share_cache(this_cpu, prev_cpu) &&
		    is_smt_idle(prev_cpu) &&
		    scx_bpf_test_and_clear_cpu_idle(prev_cpu))
			return prev_cpu;

		prev_cpu = this_cpu;
	}

	/*
	 * Fallback to the old API if the kernel doesn't support
	 * scx_bpf_select_cpu_and().
	 *
	 * This is required to support kernels <= 6.16.
	 */
	if (!bpf_ksym_exists(scx_bpf_select_cpu_and)) {
		bool is_idle = false;

		if (!wake_flags)
			return -EBUSY;

		cpu = scx_bpf_select_cpu_dfl(p, prev_cpu, wake_flags, &is_idle);

		return is_idle ? cpu : -EBUSY;
	}

	/*
	 * Pick any idle CPU usable by the task.
	 */
	return scx_bpf_select_cpu_and(p, prev_cpu, wake_flags, p->cpus_ptr, 0);
}

/*
 * Wake-up a target @cpu for the dispatched task @p. If @cpu can't be used
 * wakeup another valid CPU.
 */
static void kick_task_cpu(const struct task_struct *p, s32 cpu)
{
	if (!bpf_cpumask_test_cpu(cpu, p->cpus_ptr)) {
		/*
		 * Kick the target CPU anyway, since it may be locked and
		 * needs to go back to idle to reset its state.
		 */
		__sync_fetch_and_add(&nr_dispatch_cpu_kicks, 1);
		scx_bpf_kick_cpu(cpu, SCX_KICK_IDLE);

		/*
		 * Pick any other idle CPU that the task can use.
		 */
		cpu = scx_bpf_pick_idle_cpu(p->cpus_ptr, 0);
		if (cpu < 0)
			return;
	}
	__sync_fetch_and_add(&nr_dispatch_cpu_kicks, 1);
	scx_bpf_kick_cpu(cpu, SCX_KICK_IDLE);
}

/*
 * Dispatch a task to a target per-CPU DSQ, waking up the corresponding CPU, if
 * needed.
 */
static void dispatch_task(const struct dispatched_task_ctx *task)
{
	struct task_ctx *tctx;
	struct task_struct *p;
	s32 prev_cpu, cpu = task->cpu;

	__sync_fetch_and_add(&nr_dispatched_ringbuf_drains, 1);

	/* Ignore entry if the task doesn't exist anymore */
	p = bpf_task_from_pid(task->tid);
	if (!p) {
		__sync_fetch_and_add(&nr_dispatch_task_missing, 1);
		return;
	}
	prev_cpu = scx_bpf_task_cpu(p);

	/*
	 * Dispatch task to the shared DSQ if the user-space scheduler
	 * didn't select any specific target CPU.
	 */
	if (task->cpu == RL_CPU_ANY) {
		scx_bpf_dsq_insert_vtime(p, SHARED_DSQ,
					 task->slice_ns, task->vtime, task->flags);
		__sync_fetch_and_add(&nr_dispatch_shared_inserts, 1);
		kick_task_cpu(p, prev_cpu);
		goto out_release;
	}

	/*
	 * Dispatch the task to the target CPU selected by the
	 * user-space scheduler.
	 *
	 * However, if the target CPU is not valid (due to affinity
	 * constraints), keep the task on the previously used CPU,
	 * overriding the user-space scheduler decision.
	 */
	if (!bpf_cpumask_test_cpu(task->cpu, p->cpus_ptr)) {
		cpu = prev_cpu;
		__sync_fetch_and_add(&nr_bounce_dispatches, 1);
	} else {
		__sync_fetch_and_add(&nr_user_dispatches, 1);
	}
	scx_bpf_dsq_insert_vtime(p, cpu_to_dsq(cpu),
				 task->slice_ns, task->vtime, task->flags);
	__sync_fetch_and_add(&nr_dispatch_cpu_inserts, 1);

	tctx = try_lookup_task_ctx(p);
	if (!tctx) {
		scx_bpf_dispatch_cancel();
		__sync_fetch_and_add(&nr_cancel_dispatches, 1);
		goto out_release;
	}
	if (tctx->enq_cnt > task->enq_cnt)
		__sync_fetch_and_add(&nr_stale_dispatch_rescues, 1);
	tctx->tick_holdoff = task->tick_holdoff;
	if (cpu >= 0)
		adjust_cpu_dsq_depth((u32)cpu, 1);

	/*
	 * CPU selected by the user-space scheduler is valid, kick it.
	 */
	__sync_fetch_and_add(&nr_dispatch_cpu_kicks, 1);
	scx_bpf_kick_cpu(cpu, SCX_KICK_IDLE);

out_release:
	bpf_task_release(p);
}

/*
 * Return true it's safe to dispatch directly on @cpu, false otherwise.
 */
static bool can_direct_dispatch(s32 cpu)
{
	return !scx_bpf_dsq_nr_queued(SHARED_DSQ) &&
	       !scx_bpf_dsq_nr_queued(cpu_to_dsq(cpu));
}

s32 BPF_STRUCT_OPS(rustland_select_cpu, struct task_struct *p, s32 prev_cpu,
		   u64 wake_flags)
{
	s32 cpu, this_cpu = bpf_get_smp_processor_id();
	bool is_this_cpu_allowed = bpf_cpumask_test_cpu(this_cpu, p->cpus_ptr);

	/*
	 * Make sure @prev_cpu is usable, otherwise try to move close to
	 * the waker's CPU. If the waker's CPU is also not usable, then
	 * pick the first usable CPU.
	 */
	if (!bpf_cpumask_test_cpu(prev_cpu, p->cpus_ptr))
		prev_cpu = is_this_cpu_allowed ? this_cpu : bpf_cpumask_first(p->cpus_ptr);

	/*
	 * Scheduler is dispatched directly in .dispatch() when needed, so
	 * we can skip it here.
	 */
	if (is_usersched_task(p))
		return prev_cpu;

	/*
	 * If built-in idle CPU policy is not enabled, completely delegate
	 * the idle selection policy to user-space and keep reusing the
	 * same CPU here.
	 */
	if (!builtin_idle)
		return prev_cpu;

	/*
	 * Pick the idle CPU closest to @prev_cpu usable by the task.
	 */
	cpu = pick_idle_cpu(p, prev_cpu, wake_flags);
	if (cpu >= 0) {
		if (can_direct_dispatch(cpu)) {
			scx_bpf_dsq_insert_vtime(p, cpu_to_dsq(cpu),
						 slice_ns, p->scx.dsq_vtime, 0);
			__sync_fetch_and_add(&nr_kernel_dispatches, 1);
		}
		return cpu;
	}

	/*
	 * If we couldn't find an idle CPU, in case of a sync wakeup
	 * prioritize the waker's CPU.
	 */
	return prev_cpu;
}

/*
 * Select and wake-up an idle CPU for a specific task from the user-space
 * scheduler.
 */
SEC("syscall")
int rs_select_cpu(struct task_cpu_arg *input)
{
	struct task_struct *p;
	int cpu = input->cpu;

	p = bpf_task_from_pid(input->tid);
	if (!p)
		return -EINVAL;

	bpf_rcu_read_lock();
	/*
	 * Kernels that don't provide scx_bpf_select_cpu_and() only allow
	 * to use the built-in idle CPU selection policy only from
	 * ops.select_cpu() and opt.enqueue(), return any idle CPU usable
	 * by the task in this case.
	 */
	if (!bpf_ksym_exists(scx_bpf_select_cpu_and)) {
		if (!scx_bpf_test_and_clear_cpu_idle(cpu))
			cpu = scx_bpf_pick_idle_cpu(p->cpus_ptr, 0);
	} else {
		/*
		 * Set SCX_WAKE_TTWU, pretending to be a wakeup, to prioritize
		 * faster CPU selection (we probably want to add an option to allow
		 * the user-space scheduler to use this logic or not).
		 */
		cpu = pick_idle_cpu(p, cpu, SCX_WAKE_TTWU);
	}
	bpf_rcu_read_unlock();

	bpf_task_release(p);

	return cpu;
}

/*
 * Fill @task with all the information that need to be sent to the user-space
 * scheduler.
 */
static void get_task_info(struct queued_task_ctx *task,
			  const struct task_struct *p,
			  struct task_ctx *tctx, u64 enq_flags, s32 prev_cpu)
{
	if (tctx->current_cpu < 0)
		tctx->current_cpu = prev_cpu;
	if (tctx->current_domain < 0 && prev_cpu >= 0)
		tctx->current_domain = cpu_to_domain((u32)prev_cpu);

	task->tid = p->pid;
	task->tgid = p->tgid;
	task->current_cpu = tctx->current_cpu;
	task->current_domain = tctx->current_domain;
	task->nr_cpus_allowed = p->nr_cpus_allowed;
	task->flags = enq_flags;
	task->start_ts = tctx->start_ts;
	task->stop_ts = tctx->stop_ts;
	task->exec_runtime = tctx->exec_runtime;
	task->weight = p->scx.weight;
	task->vtime = p->scx.dsq_vtime;
	task->enq_cnt = ++tctx->enq_cnt;
	task->last_l2_bw_mib_s_x100 = tctx->last_l2_bw_mib_s_x100;
	task->ewma_l2_bw_mib_s_x100 = tctx->ewma_l2_bw_mib_s_x100;
	#pragma unroll
	for (int idx = 0; idx < MEM_SOURCE_COUNT; idx++) {
		task->last_fill_bw_mib_s_x100[idx] = tctx->last_fill_bw_mib_s_x100[idx];
		task->ewma_fill_bw_mib_s_x100[idx] = tctx->ewma_fill_bw_mib_s_x100[idx];
	}
	task->last_ipc_x1000 = tctx->last_ipc_x1000;
	task->ewma_ipc_x1000 = tctx->ewma_ipc_x1000;
	task->last_stall_pct_x100 = tctx->last_stall_pct_x100;
	task->ewma_stall_pct_x100 = tctx->ewma_stall_pct_x100;
	task->trigger = tctx->pending_trigger;
	task->tick_seq = tctx->tick_seq;
	task->last_update_ns = tctx->last_update_ns;
	tctx->pending_trigger = QUEUE_TRIGGER_ENQUEUE;

	bpf_core_read(&task->comm, sizeof(task->comm), &p->comm);
}

/*
 * User-space scheduler is congested: log that and increment congested counter.
 */
static void sched_congested(struct task_struct *p)
{
	dbg_msg("congested: pid=%d (%s)", p->pid, p->comm);
	__sync_fetch_and_add(&nr_sched_congested, 1);
}

/*
 * Return true if a task has been enqueued as a remote wakeup, false
 * otherwise.
 */
static bool is_queued_wakeup(const struct task_struct *p, u64 enq_flags)
{
	return !__COMPAT_is_enq_cpu_selected(enq_flags) && !scx_bpf_task_running(p);
}

/*
 * Queue a task to the user-space scheduler.
 */
static void queue_task_to_userspace(struct task_struct *p, s32 prev_cpu, u64 enq_flags,
				       u32 trigger)
{
	struct task_struct *usersched_task;
	struct queued_task_ctx *task;
	struct task_ctx *tctx;
	u32 effective_trigger;

	tctx = try_lookup_task_ctx(p);
	if (!tctx)
		return;
	effective_trigger = tctx->pending_trigger;
	if (trigger != QUEUE_TRIGGER_ENQUEUE ||
	    effective_trigger == QUEUE_TRIGGER_ENQUEUE)
		effective_trigger = trigger;
	tctx->pending_trigger = effective_trigger;

	/*
	 * Allocate a new entry in the ring buffer.
	 *
	 * If ring buffer is full, the user-space scheduler is congested,
	 * so dispatch the task directly using the shared DSQ (the task
	 * will be consumed by the first CPU available).
	 */
	task = bpf_ringbuf_reserve(&queued, sizeof(*task), 0);
	if (!task) {
		sched_congested(p);
		scx_bpf_dsq_insert_vtime(p, SHARED_DSQ,
					 slice_ns, p->scx.dsq_vtime, enq_flags);
		__sync_fetch_and_add(&nr_kernel_dispatches, 1);
		return;
	}

	/*
	 * Collect task information and store them in the ring buffer that
	 * will be consumed by the user-space scheduler.
	 */
	dbg_msg("enqueue: pid=%d (%s)", p->pid, p->comm);
	get_task_info(task, p, tctx, enq_flags, prev_cpu);
	bpf_ringbuf_submit(task, 0);
	if (effective_trigger == QUEUE_TRIGGER_TICK ||
	    effective_trigger == QUEUE_TRIGGER_VILLAIN_RESLICE)
		__sync_fetch_and_add(&nr_tick_to_userspace, 1);
	__sync_fetch_and_add(&nr_queued, 1);
	set_usersched_needed();

	bpf_rcu_read_lock();
	usersched_task = usersched_pid ? bpf_task_from_pid(usersched_pid) : NULL;
	if (usersched_task) {
		scx_bpf_kick_cpu(scx_bpf_task_cpu(usersched_task), SCX_KICK_IDLE);
		bpf_task_release(usersched_task);
	} else {
		scx_bpf_kick_cpu(prev_cpu, SCX_KICK_IDLE);
	}
	bpf_rcu_read_unlock();
}

/*
 * Task @p becomes ready to run. We can dispatch the task directly here if the
 * user-space scheduler is not required, or enqueue it to be processed by the
 * scheduler.
 */
void BPF_STRUCT_OPS(rustland_enqueue, struct task_struct *p, u64 enq_flags)
{
	s32 prev_cpu = scx_bpf_task_cpu(p), cpu;
	bool is_wakeup = is_queued_wakeup(p, enq_flags);

	/*
	 * Insert the user-space scheduler to its dedicated DSQ, it will be
	 * consumed from ops.dispatch() only when there's any pending
	 * scheduling action to do.
	 */
	if (is_usersched_task(p)) {
		scx_bpf_dsq_insert(p, SCHED_DSQ, slice_ns, enq_flags);
		goto out_kick;
	}

	if (is_internal_task(p)) {
		if (prev_cpu < 0)
			prev_cpu = bpf_cpumask_first(p->cpus_ptr);
		scx_bpf_dsq_insert_vtime(p, cpu_to_dsq(prev_cpu),
					 helper_slice_ns ? helper_slice_ns :
					 (slice_ns ? slice_ns / 8 : slice_ns),
					 p->scx.dsq_vtime, enq_flags);
		__sync_fetch_and_add(&nr_kernel_dispatches, 1);
		goto out_kick;
	}

	/*
	 * Tasks constrained to one CPU have no userspace placement choice. Dispatch
	 * them directly and kick the CPU so short-lived pinned launcher/helper tasks
	 * cannot be stranded behind the userspace handoff path.
	 */
	if (p->nr_cpus_allowed == 1) {
		prev_cpu = bpf_cpumask_first(p->cpus_ptr);
		scx_bpf_dsq_insert_vtime(p, cpu_to_dsq(prev_cpu),
					 slice_ns, p->scx.dsq_vtime, enq_flags);
		__sync_fetch_and_add(&nr_kernel_dispatches, 1);
		is_wakeup = true;
		goto out_kick;
	}

	/*
	 * Always dispatch per-CPU kthreads directly on their target CPU.
	 *
	 * This allows to prioritize critical kernel threads that may
	 * potentially stall the entire system if they are blocked for too long
	 * (i.e., ksoftirqd/N, rcuop/N, etc.).
	 */
	if (is_kswapd(p) || is_khugepaged(p)) {
		scx_bpf_dsq_insert_vtime(p, cpu_to_dsq(prev_cpu),
					 slice_ns, p->scx.dsq_vtime, enq_flags);
		__sync_fetch_and_add(&nr_kernel_dispatches, 1);
		goto out_kick;
	}

	/*
	 * If @builtin_idle is enabled, give the task a chance to be
	 * directly dispatched only on a wakeup and only if
	 * ops.select_cpu() was skipped, otherwise the task is always
	 * queued to the user-space scheduler.
	 */
	if (!(builtin_idle && is_wakeup)) {
		queue_task_to_userspace(p, prev_cpu, enq_flags, QUEUE_TRIGGER_ENQUEUE);
		goto out_kick;
	}

	/*
	 * Try to find an idle CPU in the system, if all CPUs are busy
	 * queue the task to the user-space scheduler.
	 */
	cpu = pick_idle_cpu(p, prev_cpu, 0);
	if (cpu < 0) {
		queue_task_to_userspace(p, prev_cpu, enq_flags, QUEUE_TRIGGER_ENQUEUE);
		goto out_kick;
	}

	/*
	 * Always force a CPU wakeup, so that the allocated CPU can be
	 * released and go back idle even if the task isn't directly
	 * dispatched.
	 */
	prev_cpu = cpu;
	is_wakeup = true;

	/*
	 * Bounce the task to the user-space scheduler if we can't directly
	 * dispatch to the selected CPU.
	 */
	if (!can_direct_dispatch(cpu)) {
		queue_task_to_userspace(p, prev_cpu, enq_flags, QUEUE_TRIGGER_ENQUEUE);
		goto out_kick;
	}

	/*
	 * We can race with a dequeue here and the selected idle CPU might
	 * be not valid anymore, if the task affinity has changed.
	 *
	 * In this case just wakeup the picked CPU and ignore the enqueue,
	 * another enqueue event for the same task will be received later.
	 */
	if (!bpf_cpumask_test_cpu(cpu, p->cpus_ptr))
		goto out_kick;

	/*
	 * Directly dispatch the task to selected idle CPU (queued wakeup).
	 */
	scx_bpf_dsq_insert_vtime(p, cpu_to_dsq(cpu),
				 slice_ns, p->scx.dsq_vtime, enq_flags);
	__sync_fetch_and_add(&nr_kernel_dispatches, 1);

out_kick:
	/*
	 * Wakeup the task's CPU if needed.
	 */
	if (is_wakeup)
		scx_bpf_kick_cpu(prev_cpu, SCX_KICK_IDLE);
}

/*
 * Handle a task dispatched from user-space, performing the actual low-level
 * BPF dispatch.
 */
static long handle_dispatched_task(struct bpf_dynptr *dynptr, void *context)
{
	const struct dispatched_task_ctx *task;

	task = bpf_dynptr_data(dynptr, 0, sizeof(*task));
	if (!task)
		return 0;

	dispatch_task(task);

	return !!scx_bpf_dispatch_nr_slots();
}

/*
 * Dispatch tasks that are ready to run.
 *
 * This function is called when a CPU's local DSQ is empty and ready to accept
 * new dispatched tasks.
 *
 * We may dispatch tasks also on other CPUs from here, if the scheduler decided
 * so (usually if other CPUs are idle we may want to send more tasks to their
 * local DSQ to optimize the scheduling pipeline).
 */
void BPF_STRUCT_OPS(rustland_dispatch, s32 cpu, struct task_struct *prev)
{
	struct task_ctx *tctx;

	/*
	 * Consume all tasks from the @dispatched list and immediately
	 * dispatch them on the target CPU decided by the user-space
	 * scheduler.
	 */
	s32 ret = bpf_user_ringbuf_drain(&dispatched,
					 handle_dispatched_task, NULL, BPF_RB_NO_WAKEUP);
	if (ret)
		dbg_msg("User ringbuf drain error: %d", ret);

	/*
	 * Dispatch the user-space scheduler if there's any pending action
	 * to do.
	 */
	if (usersched_pid && usersched_has_pending_tasks() &&
	    scx_bpf_dsq_move_to_local(SCHED_DSQ)) {
		__sync_fetch_and_add(&nr_dispatch_sched_consumes, 1);
		return;
	}

	/*
	 * Consume a task from the per-CPU DSQ.
	 */
	if (scx_bpf_dsq_move_to_local(cpu_to_dsq(cpu))) {
		adjust_cpu_dsq_depth((u32)cpu, -1);
		__sync_fetch_and_add(&nr_dispatch_cpu_consumes, 1);
		return;
	}

	/*
	 * Consume a task from the shared DSQ.
	 */
	if (scx_bpf_dsq_move_to_local(SHARED_DSQ)) {
		__sync_fetch_and_add(&nr_dispatch_shared_consumes, 1);
		return;
	}

	/*
	 * If the current task expired its time slice and no other task
	 * wants to run, simply replenish its time slice and let it run for
	 * another round on the same CPU.
	 *
	 * In case of the user-space scheduler task, replenish its time
	 * slice only if there're still pending scheduling actions to do.
	 */
	if (prev && is_queued(prev) &&
	    (!is_usersched_task(prev) || usersched_has_pending_tasks())) {
		tctx = try_lookup_task_ctx(prev);
		if (enable_tick_resched && tctx &&
		    (tctx->pending_trigger == QUEUE_TRIGGER_TICK ||
		     tctx->pending_trigger == QUEUE_TRIGGER_VILLAIN_RESLICE))
			return;
		prev->scx.slice = slice_ns;
	}
}

void BPF_STRUCT_OPS(rustland_runnable, struct task_struct *p, u64 enq_flags)
{
	struct task_ctx *tctx;

	if (is_usersched_task(p) || is_internal_task(p))
		return;

	tctx = try_lookup_task_ctx(p);
	if (!tctx)
		return;

	tctx->exec_runtime = 0;
	tctx->tick_holdoff = 0;
}

/*
 * Task @p starts on its selected CPU (update CPU ownership map).
 */
void BPF_STRUCT_OPS(rustland_running, struct task_struct *p)
{
	s32 cpu = scx_bpf_task_cpu(p);
	struct task_ctx *tctx;
	u64 now, counter;
	int idx;
	s32 domain;

	if (is_usersched_task(p)) {
		usersched_last_run_at = scx_bpf_now();
		return;
	}
	if (is_internal_task(p))
		return;

	dbg_msg("start: pid=%d (%s) cpu=%ld", p->pid, p->comm, cpu);

	/*
	 * Mark the CPU as busy by setting the pid as owner (ignoring the
	 * user-space scheduler).
	 */
	__sync_fetch_and_add(&nr_running, 1);

	tctx = try_lookup_task_ctx(p);
	if (!tctx)
		return;
	now = scx_bpf_now();
	tctx->tid = p->pid;
	tctx->tgid = p->tgid;
	tctx->start_ts = now;
	tctx->run_start_ns = now;
	tctx->run_start_fill_valid_mask = 0;
	tctx->run_start_stall_valid_mask = 0;
	tctx->current_cpu = cpu;
	domain = cpu >= 0 ? cpu_to_domain((u32)cpu) : -1;
	tctx->current_domain = domain;
	if (cpu >= 0) {
		#pragma unroll
		for (idx = 0; idx < MEM_SOURCE_COUNT; idx++) {
			if (!read_fill_counter((u32)cpu, idx, &counter)) {
				tctx->run_start_fill_ctr[idx] = counter;
				tctx->run_start_fill_valid_mask |= 1U << idx;
			}
		}
		if (!read_instructions_counter((u32)cpu, &counter)) {
			tctx->run_start_inst_ctr = counter;
			tctx->run_start_stall_valid_mask |= STALL_COUNTER_VALID_INST;
		}
		if (!read_cpu_cycles_counter((u32)cpu, &counter)) {
			tctx->run_start_cycle_ctr = counter;
			tctx->run_start_stall_valid_mask |= STALL_COUNTER_VALID_CYCLES;
		}
	}
	if (cpu >= 0) {
		set_cpu_state_tid((u32)cpu, p->pid);
		set_cpu_state_idle((u32)cpu, false);
	}
}

/*
 * Task @p stops running on its associated CPU (update CPU ownership map).
 */
void BPF_STRUCT_OPS(rustland_stopping, struct task_struct *p, bool runnable)
{
	u64 now = scx_bpf_now();
	s32 cpu = scx_bpf_task_cpu(p);
	struct task_ctx *tctx;

	if (is_usersched_task(p) || is_internal_task(p))
		return;

	dbg_msg("stop: pid=%d (%s) cpu=%ld", p->pid, p->comm, cpu);

	__sync_fetch_and_sub(&nr_running, 1);

	tctx = try_lookup_task_ctx(p);
	if (!tctx)
		return;
	tctx->stop_ts = now;

	/*
	 * Update the partial execution time since last sleep.
	 */
	tctx->exec_runtime += now - tctx->start_ts;
	tctx->current_cpu = cpu;
	tctx->current_domain = cpu >= 0 ? cpu_to_domain((u32)cpu) : -1;
	sample_slice_metrics(tctx, now);
	tctx->run_start_ns = 0;
	tctx->run_start_fill_valid_mask = 0;
	tctx->run_start_stall_valid_mask = 0;
	if (cpu >= 0)
		set_cpu_state_tid((u32)cpu, 0);
}

void BPF_STRUCT_OPS(rustland_tick, struct task_struct *p)
{
	struct task_ctx *tctx;
	s32 cpu;
	u64 now;

	if (!enable_tick_resched)
		return;
	if (is_usersched_task(p) || is_internal_task(p))
		return;

	tctx = try_lookup_task_ctx(p);
	if (!tctx)
		return;

	now = scx_bpf_now();
	tctx->tick_seq++;
	tctx->last_tick_ns = now;
	__sync_fetch_and_add(&nr_tick_events, 1);

	if (!tick_reeval_every || (tctx->tick_seq % tick_reeval_every))
		return;
	if (tctx->tick_holdoff) {
		tctx->tick_holdoff--;
		__sync_fetch_and_add(&nr_tick_backoff_skip, 1);
		return;
	}

	if (task_should_fastpath_stay(p, tctx, now)) {
		__sync_fetch_and_add(&nr_tick_fastpath_stay, 1);
		return;
	}

	/*
	 * Single-CPU-affinity tasks are direct-dispatched from enqueue because
	 * userspace has no legal placement choice. Keep their tick path direct as
	 * well; otherwise a pinned task can carry a QUEUE_TRIGGER_TICK without ever
	 * appearing in the userspace queue, leaving it runnable with an exhausted
	 * slice and no DSQ entry.
	 */
	if (p->nr_cpus_allowed == 1) {
		__sync_fetch_and_add(&nr_tick_fastpath_stay, 1);
		return;
	}

	tctx->pending_trigger = QUEUE_TRIGGER_TICK;
	set_usersched_needed();
	cpu = scx_bpf_task_cpu(p);
	if (cpu >= 0)
		scx_bpf_kick_cpu(cpu, SCX_KICK_PREEMPT);
}

void BPF_STRUCT_OPS(rustland_update_idle, s32 cpu, bool idle)
{
	if (cpu < 0)
		return;

	set_cpu_state_idle((u32)cpu, idle);
}

/*
 * A task joins the sched_ext scheduler.
 */
void BPF_STRUCT_OPS(rustland_enable, struct task_struct *p)
{
	p->scx.dsq_vtime = 0;
	p->scx.slice = slice_ns;
}

/*
 * Heartbeat scheduler timer callback.
 *
 * If the system is completely idle the sched-ext watchdog may incorrectly
 * detect that as a stall and automatically disable the scheduler. So, use this
 * timer to periodically wake-up the scheduler and avoid long inactivity.
 *
 * This can also help to prevent real "stalling" conditions in the scheduler.
 */
static int usersched_timer_fn(void *map, int *key, struct bpf_timer *timer)
{
	struct task_struct *p;
	int err = 0;

	if (!usersched_pid)
		goto out_rearm;

	/*
	 * Trigger the user-space scheduler if it has been inactive for
	 * more than USERSCHED_TIMER_NS.
	 */
	if (time_delta(scx_bpf_now(), usersched_last_run_at) >= USERSCHED_TIMER_NS) {
		bpf_rcu_read_lock();
		p = bpf_task_from_pid(usersched_pid);
		if (p) {
			set_usersched_needed();
			scx_bpf_kick_cpu(scx_bpf_task_cpu(p), SCX_KICK_IDLE);
			bpf_task_release(p);
		}
		bpf_rcu_read_unlock();
	}

out_rearm:
	/* Re-arm the timer */
	err = bpf_timer_start(timer, USERSCHED_TIMER_NS, 0);
	if (err)
		scx_bpf_error("Failed to arm stats timer");

	return 0;
}

/*
 * Initialize the heartbeat scheduler timer.
 */
static int usersched_timer_init(void)
{
	struct bpf_timer *timer;
	u32 key = 0;
	int err;

	if (!usersched_pid)
		return 0;

	timer = bpf_map_lookup_elem(&usersched_timer, &key);
	if (!timer) {
		scx_bpf_error("Failed to lookup scheduler timer");
		return -ESRCH;
	}
	bpf_timer_init(timer, &usersched_timer, CLOCK_BOOTTIME);
	bpf_timer_set_callback(timer, usersched_timer_fn);
	err = bpf_timer_start(timer, USERSCHED_TIMER_NS, 0);
	if (err)
		scx_bpf_error("Failed to arm scheduler timer");

	return err;
}

/*
 * Evaluate the amount of online CPUs.
 */
static s32 get_nr_online_cpus(void)
{
	const struct cpumask *online_cpumask;
	int i, cpus = 0;

	online_cpumask = scx_bpf_get_online_cpumask();

	bpf_for(i, 0, nr_cpu_ids) {
		if (!bpf_cpumask_test_cpu(i, online_cpumask))
			continue;
		cpus++;
	}

	scx_bpf_put_cpumask(online_cpumask);

	return cpus;
}

/*
 * Create a DSQ for each CPU available in the system and a global shared DSQ.
 *
 * All the tasks processed by the user-space scheduler can be dispatched either
 * to a specific CPU/DSQ or to the first CPU available (SHARED_DSQ).
 *
 * Custom DSQs are then consumed from the .dispatch() callback, that will
 * transfer all the enqueued tasks to the consuming CPU's local DSQ.
 */
static int dsq_init(void)
{
	int err;
	s32 cpu;

	/* Initialize amount of online CPUs */
	nr_online_cpus = get_nr_online_cpus();

	/* Create per-CPU DSQs */
	bpf_for(cpu, 0, nr_cpu_ids) {
		err = scx_bpf_create_dsq(cpu_to_dsq(cpu), -1);
		if (err) {
			scx_bpf_error("failed to create pcpu DSQ %d: %d",
				      cpu, err);
			return err;
		}
	}

	/* Create the global shared DSQ */
	err = scx_bpf_create_dsq(SHARED_DSQ, -1);
	if (err) {
		scx_bpf_error("failed to create shared DSQ: %d", err);
		return err;
	}

	/* Create the scheduler's DSQ */
	err = scx_bpf_create_dsq(SCHED_DSQ, -1);
	if (err) {
		scx_bpf_error("failed to create scheduler DSQ: %d", err);
		return err;
	}

	return 0;
}

/*
 * A new task @p is being created.
 *
 * Allocate and initialize all the internal structures for the task (this
 * function is allowed to block, so it can be used to preallocate memory).
 */
s32 BPF_STRUCT_OPS(rustland_init_task, struct task_struct *p,
		   struct scx_init_task_args *args)
{
	struct task_ctx *tctx;
	struct task_ctx init = {
		.tid = p->pid,
		.tgid = p->tgid,
		.last_ipc_x1000 = STALL_IPC_CAP_X1000,
		.ewma_ipc_x1000 = STALL_IPC_CAP_X1000,
		.tick_holdoff = 0,
		.pending_trigger = QUEUE_TRIGGER_ENQUEUE,
		.current_cpu = -1,
		.current_domain = -1,
	};

	tctx = bpf_task_storage_get(&task_ctx_stor, p, &init,
				    BPF_LOCAL_STORAGE_GET_F_CREATE);
	if (!tctx)
		return -ENOMEM;

	return 0;
}

/*
 * Initialize the scheduling class.
 */
s32 BPF_STRUCT_OPS_SLEEPABLE(rustland_init)
{
	int err;

	/* Compile-time checks */
	BUILD_BUG_ON((MAX_CPUS % 2));

	/* Initialize maximum possible CPU number */
	nr_cpu_ids = scx_bpf_nr_cpu_ids();

	/* Initialize rustland core */
	err = dsq_init();
	if (err)
		return err;
	err = usersched_timer_init();
	if (err)
		return err;

	return 0;
}

/*
 * Unregister the scheduling class.
 */
void BPF_STRUCT_OPS(rustland_exit, struct scx_exit_info *ei)
{
	UEI_RECORD(uei, ei);
}

/*
 * Scheduling class declaration.
 */
SCX_OPS_DEFINE(rustland,
	       .select_cpu		= (void *)rustland_select_cpu,
	       .enqueue			= (void *)rustland_enqueue,
	       .dispatch		= (void *)rustland_dispatch,
	       .tick			= (void *)rustland_tick,
	       .runnable		= (void *)rustland_runnable,
	       .running			= (void *)rustland_running,
	       .stopping		= (void *)rustland_stopping,
	       .update_idle		= (void *)rustland_update_idle,
	       .enable			= (void *)rustland_enable,
	       .init_task		= (void *)rustland_init_task,
	       .init			= (void *)rustland_init,
	       .exit			= (void *)rustland_exit,
	       .timeout_ms		= 5000,
	       .dispatch_max_batch	= MAX_DISPATCH_SLOT,
	       .name			= "rustland");
