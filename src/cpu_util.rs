use anyhow::{Context, Result};

pub struct CpuUtilTracker {
    prev_total: Vec<u64>,
    prev_idle: Vec<u64>,
    util_x100: Vec<u32>,
    initialized: bool,
}

impl CpuUtilTracker {
    pub fn new(nr_cpu_ids: usize) -> Self {
        Self {
            prev_total: vec![0; nr_cpu_ids],
            prev_idle: vec![0; nr_cpu_ids],
            util_x100: vec![0; nr_cpu_ids],
            initialized: false,
        }
    }

    pub fn values(&self) -> &[u32] {
        &self.util_x100
    }

    pub fn sample(&mut self, nr_cpu_ids: usize) -> Result<()> {
        let text = std::fs::read_to_string("/proc/stat")
            .context("failed to read /proc/stat for cpu util sampling")?;
        for line in text.lines() {
            if !line.starts_with("cpu") {
                continue;
            }
            let bytes = line.as_bytes();
            if bytes.len() < 4 || bytes[3] == b' ' {
                continue;
            }
            let mut parts = line.split_whitespace();
            let Some(cpu_label) = parts.next() else {
                continue;
            };
            let Ok(cpu_idx) = cpu_label[3..].parse::<usize>() else {
                continue;
            };
            if cpu_idx >= nr_cpu_ids
                || cpu_idx >= self.prev_total.len()
                || cpu_idx >= self.prev_idle.len()
                || cpu_idx >= self.util_x100.len()
            {
                continue;
            }

            let mut fields = [0u64; 10];
            for slot in &mut fields {
                let Some(v) = parts.next() else {
                    break;
                };
                *slot = v.parse::<u64>().unwrap_or(0);
            }
            let idle = fields[3].saturating_add(fields[4]);
            let total: u64 = fields.iter().sum();

            if self.initialized {
                let prev_total = self.prev_total[cpu_idx];
                let prev_idle = self.prev_idle[cpu_idx];
                let delta_total = total.saturating_sub(prev_total);
                let delta_idle = idle.saturating_sub(prev_idle);
                if delta_total > 0 {
                    let busy = delta_total.saturating_sub(delta_idle);
                    self.util_x100[cpu_idx] =
                        ((busy.saturating_mul(10_000)) / delta_total).min(10_000) as u32;
                }
            }

            self.prev_total[cpu_idx] = total;
            self.prev_idle[cpu_idx] = idle;
        }
        self.initialized = true;
        Ok(())
    }
}
