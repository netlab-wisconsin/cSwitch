use clap::{Parser, Subcommand};
use std::hint::black_box;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Parser)]
#[command(name = "synth_load")]
#[command(about = "Synthetic CPU and memory-touch workloads for sched_ext bring-up")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Spin(SpinArgs),
    Touch(TouchArgs),
}

#[derive(Debug, Clone, Parser)]
struct SpinArgs {
    #[arg(long, default_value_t = 1)]
    threads: usize,
    #[arg(long)]
    cpus: Option<String>,
    #[arg(long, default_value_t = 1_000)]
    duration_ms: u64,
    #[arg(long, default_value_t = 16_384)]
    yield_every: u64,
}

#[derive(Debug, Clone, Parser)]
struct TouchArgs {
    #[arg(long, default_value_t = 1)]
    threads: usize,
    #[arg(long)]
    cpus: Option<String>,
    #[arg(long, default_value_t = 1_000)]
    duration_ms: u64,
    #[arg(long, default_value_t = 64)]
    size_mib: usize,
    #[arg(long, default_value_t = 64)]
    stride_bytes: usize,
    #[arg(long, default_value_t = 4_096)]
    yield_every: u64,
}

fn deadline(duration_ms: u64) -> Option<Instant> {
    (duration_ms > 0).then(|| Instant::now() + Duration::from_millis(duration_ms))
}

fn parse_cpulist(spec: Option<&str>) -> anyhow::Result<Vec<usize>> {
    let Some(spec) = spec.filter(|value| !value.trim().is_empty()) else {
        return Ok(Vec::new());
    };

    let mut cpus = Vec::new();
    for part in spec.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some((start, end)) = part.split_once('-') {
            let start: usize = start.parse()?;
            let end: usize = end.parse()?;
            for cpu in start..=end {
                cpus.push(cpu);
            }
        } else {
            cpus.push(part.parse()?);
        }
    }
    Ok(cpus)
}

fn pin_current_thread(cpu: usize) -> std::io::Result<()> {
    let mut cpuset = unsafe { std::mem::zeroed::<libc::cpu_set_t>() };
    unsafe {
        libc::CPU_ZERO(&mut cpuset);
        libc::CPU_SET(cpu, &mut cpuset);
    }
    let ret =
        unsafe { libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &cpuset) };
    if ret != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_for_handler = Arc::clone(&stop);
    ctrlc::set_handler(move || {
        stop_for_handler.store(true, Ordering::Relaxed);
    })?;

    match cli.command {
        Command::Spin(args) => run_spin(args, stop),
        Command::Touch(args) => run_touch(args, stop),
    }
}

fn run_spin(args: SpinArgs, stop: Arc<AtomicBool>) -> anyhow::Result<()> {
    let end_at = deadline(args.duration_ms);
    let cpus = parse_cpulist(args.cpus.as_deref())?;
    let mut threads = Vec::with_capacity(args.threads);
    for thread_idx in 0..args.threads {
        let stop = Arc::clone(&stop);
        let end_at = end_at;
        let yield_every = args.yield_every.max(1);
        let assigned_cpu = cpus.get(thread_idx % cpus.len().max(1)).copied();
        threads.push(thread::spawn(move || {
            if let Some(cpu) = assigned_cpu {
                pin_current_thread(cpu).expect("failed to pin spin worker");
            }
            let mut counter = thread_idx as u64 + 1;
            while !stop.load(Ordering::Relaxed)
                && end_at
                    .map(|deadline| Instant::now() < deadline)
                    .unwrap_or(true)
            {
                counter = counter
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1);
                black_box(counter);
                if counter % yield_every == 0 {
                    unsafe {
                        libc::sched_yield();
                    }
                }
            }
            counter
        }));
    }

    for handle in threads {
        handle
            .join()
            .map_err(|_| anyhow::anyhow!("spin worker thread panicked"))?;
    }
    Ok(())
}

fn run_touch(args: TouchArgs, stop: Arc<AtomicBool>) -> anyhow::Result<()> {
    let end_at = deadline(args.duration_ms);
    let cpus = parse_cpulist(args.cpus.as_deref())?;
    let mut threads = Vec::with_capacity(args.threads);
    let stride = args.stride_bytes.max(1);
    let size_bytes = args.size_mib.max(1) * 1024 * 1024;

    for thread_idx in 0..args.threads {
        let stop = Arc::clone(&stop);
        let end_at = end_at;
        let yield_every = args.yield_every.max(1);
        let assigned_cpu = cpus.get(thread_idx % cpus.len().max(1)).copied();
        threads.push(thread::spawn(move || {
            if let Some(cpu) = assigned_cpu {
                pin_current_thread(cpu).expect("failed to pin touch worker");
            }
            let mut buf = vec![0u8; size_bytes];
            let mut iterations = 0u64;
            while !stop.load(Ordering::Relaxed)
                && end_at
                    .map(|deadline| Instant::now() < deadline)
                    .unwrap_or(true)
            {
                let add = (thread_idx as u8).wrapping_add(1);
                for offset in (0..buf.len()).step_by(stride) {
                    buf[offset] = buf[offset].wrapping_add(add);
                }
                black_box(&buf);
                iterations = iterations.wrapping_add(1);
                if iterations % yield_every == 0 {
                    unsafe {
                        libc::sched_yield();
                    }
                }
            }
            buf.len()
        }));
    }

    for handle in threads {
        handle
            .join()
            .map_err(|_| anyhow::anyhow!("touch worker thread panicked"))?;
    }
    Ok(())
}
