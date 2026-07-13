// This software may be used and distributed according to the terms of the
// GNU General Public License version 2.

#ifndef __INTF_H
#define __INTF_H

#define MAX(x, y) ((x) > (y) ? (x) : (y))
#define MIN(x, y) ((x) < (y) ? (x) : (y))

#define NSEC_PER_SEC	1000000000L
#define CLOCK_BOOTTIME	7

#include <stdbool.h>
#ifndef __kptr
#ifdef __KERNEL__
#error "__kptr_ref not defined in the kernel"
#endif
#define __kptr
#endif

#ifndef __VMLINUX_H__
typedef unsigned char u8;
typedef unsigned short u16;
typedef unsigned int u32;
typedef unsigned long u64;

typedef signed char s8;
typedef signed short s16;
typedef signed int s32;
typedef signed long s64;

typedef int pid_t;
#endif /* __VMLINUX_H__ */

#define BUILD_BUG_ON(expr) \
	do { \
		extern char __build_assert__[(expr) ? -1 : 1] \
			__attribute__((unused)); \
	} while (0)

#define MAX_CPUS 1024
#define MAX_DOMAINS 16
#define MIN_VALID_RUN_NS 200000ULL
#define L2_MIB_PER_SEC_X100_SCALE 6103516ULL
#define MEM_SOURCE_COUNT 3

#ifndef TASK_COMM_LEN
#define TASK_COMM_LEN	16
#endif

enum {
	RL_CPU_ANY = 1 << 20,
};

enum mem_source_idx {
	MEM_SOURCE_LOCAL_CCX = 0,
	MEM_SOURCE_NEAR_CACHE = 1,
	MEM_SOURCE_DRAM_NEAR = 2,
};

enum queue_trigger_idx {
	QUEUE_TRIGGER_ENQUEUE = 0,
	QUEUE_TRIGGER_TICK = 1,
	QUEUE_TRIGGER_VILLAIN_RESLICE = 2,
	QUEUE_TRIGGER_HELPER = 3,
};

struct task_cpu_arg {
	pid_t tid;
	s32 cpu;
	u64 flags;
};

struct queued_task_ctx {
	s32 tid;
	s32 tgid;
	s32 current_cpu;
	s32 current_domain;
	u64 nr_cpus_allowed;
	u64 flags;
	u64 start_ts;
	u64 stop_ts;
	u64 exec_runtime;
	u64 weight;
	u64 vtime;
	u64 enq_cnt;
	u32 last_l2_bw_mib_s_x100;
	u32 ewma_l2_bw_mib_s_x100;
	u32 last_fill_bw_mib_s_x100[MEM_SOURCE_COUNT];
	u32 ewma_fill_bw_mib_s_x100[MEM_SOURCE_COUNT];
	u32 last_ipc_x1000;
	u32 ewma_ipc_x1000;
	u32 last_stall_pct_x100;
	u32 ewma_stall_pct_x100;
	u32 trigger;
	u32 __pad;
	u64 tick_seq;
	u64 last_update_ns;
	char comm[TASK_COMM_LEN];
};

struct dispatched_task_ctx {
	s32 tid;
	s32 cpu;
	u64 flags;
	u64 slice_ns;
	u64 vtime;
	u64 enq_cnt;
	u32 tick_holdoff;
	u32 __pad;
};

struct cpu_state_ctx {
	u32 domain_id;
	u32 idle;
	u32 cpu_dsq_depth;
	u32 current_tid;
	u64 last_update_ns;
};

struct llc_state_ctx {
	u64 sample_ts_ns;
	u32 raw_l3_to_l3_ns_x100;
	u32 raw_dram_lat_ns_x100;
	u32 raw_l3_bw_mib_s_x100;
	u32 raw_miss_ratio_ppm;
	u64 raw_l3_req_per_s;
	u64 raw_l3_miss_per_s;
	u32 raw_pressure_pct_x100;
	u32 ewma_pressure_pct_x100;
	u32 state;
	u32 hot_count;
	u32 cool_count;
	u32 valid;
};

struct df_state_ctx {
	u64 sample_ts_ns;
	u32 ccx_id;
	u32 ccm_id;
	u32 raw_read_bw_mib_s_x100;
	u32 raw_write_bw_mib_s_x100;
	u32 raw_pressure_pct_x100;
	u32 ewma_pressure_pct_x100;
	u32 state;
	u32 hot_count;
	u32 cool_count;
	u32 valid;
};

#endif /* __INTF_H */
