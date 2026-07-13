use anyhow::{Context, Result};
use procfs::process::Process;
use std::collections::{BTreeSet, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

const CGROUP2_ROOT: &str = "/sys/fs/cgroup";

pub fn ensure_cgroup_dir(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path).with_context(|| format!("failed to create {}", path.display()))
}

pub fn restrict_cgroup_cpus(path: &Path, cpulist: &str) -> Result<()> {
    let mems_path = path.join("cpuset.mems");
    if mems_path.exists() {
        let current = std::fs::read_to_string(&mems_path).unwrap_or_default();
        if current.trim().is_empty() {
            let parent = path.parent().unwrap_or(path);
            let mems = std::fs::read_to_string(parent.join("cpuset.mems.effective"))
                .or_else(|_| std::fs::read_to_string(parent.join("cpuset.mems")))?;
            std::fs::write(&mems_path, mems.trim())?;
        }
    }
    let cpus_path = path.join("cpuset.cpus");
    if cpus_path.exists() {
        std::fs::write(&cpus_path, cpulist)?;
    }
    Ok(())
}

pub fn move_pid_to_cgroup(path: &Path, pid: u32) -> Result<()> {
    std::fs::write(path.join("cgroup.procs"), format!("{pid}\n"))
        .with_context(|| format!("failed to move pid {pid} into {}", path.display()))
}

pub fn set_sched_ext(pid: u32) -> Result<()> {
    const SCHED_EXT: i32 = 7;
    let param = libc::sched_param { sched_priority: 0 };
    let ret = unsafe { libc::sched_setscheduler(pid as i32, SCHED_EXT, &param) };
    if ret != 0 {
        anyhow::bail!(
            "sched_setscheduler(SCHED_EXT) failed: {}",
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}

pub fn ensure_current_thread_sched_other() -> Result<()> {
    let param = libc::sched_param { sched_priority: 0 };
    let ret = unsafe { libc::sched_setscheduler(0, libc::SCHED_OTHER, &param) };
    if ret != 0 {
        anyhow::bail!(
            "sched_setscheduler(SCHED_OTHER) failed: {}",
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}

fn current_exe_path() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|path| std::fs::canonicalize(path).ok())
}

fn proc_exe_path(pid: u32) -> Option<PathBuf> {
    std::fs::canonicalize(format!("/proc/{pid}/exe")).ok()
}

fn cgroup2_relative_path(path: &Path) -> Option<String> {
    let relative = path.strip_prefix(CGROUP2_ROOT).ok()?;
    let value = relative.to_string_lossy();
    if value.is_empty() {
        Some("/".to_string())
    } else {
        Some(format!("/{}", value.trim_start_matches('/')))
    }
}

fn cgroup_text_contains_path(text: &str, target: &str) -> bool {
    text.lines().any(|line| {
        let mut fields = line.splitn(3, ':');
        matches!(
            (fields.next(), fields.next(), fields.next()),
            (Some("0"), Some(""), Some(path)) if path == target
        )
    })
}

fn pid_in_cgroup(path: &Path, pid: u32) -> bool {
    let Some(target) = cgroup2_relative_path(path) else {
        return false;
    };
    let Ok(text) = std::fs::read_to_string(format!("/proc/{pid}/cgroup")) else {
        return false;
    };
    cgroup_text_contains_path(&text, &target)
}

#[cfg(feature = "diagnostics")]
fn proc_comm(pid: u32) -> Option<String> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    Some(text.trim().to_string())
}

fn wait_for_exe_change(pid: u32, from_exe: &Path, verbose: bool) -> Result<()> {
    let deadline = Instant::now() + Duration::from_millis(50);

    loop {
        let Some(exe) = proc_exe_path(pid) else {
            anyhow::bail!("failed to resolve /proc/{pid}/exe while waiting for workload exec");
        };
        if exe != from_exe {
            if verbose {
                crate::diag_line!(
                    "launch_exec_diag pid={} exe={} comm={}",
                    pid,
                    exe.display(),
                    proc_comm(pid).unwrap_or_else(|| "?".to_string()),
                );
            }
            return Ok(());
        }
        if Instant::now() >= deadline {
            anyhow::bail!(
                "workload pid {pid} did not exec away from scheduler image before sched_ext switch"
            );
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn wait_for_workload_exec(pid: u32, verbose: bool) -> Result<()> {
    let Some(usersched_exe) = current_exe_path() else {
        return Ok(());
    };
    wait_for_exe_change(pid, &usersched_exe, verbose)
}

fn is_perf_sched_record_command(cmd: &[String]) -> bool {
    if cmd.len() < 3 {
        return false;
    }
    let Some(prog) = Path::new(&cmd[0]).file_name().and_then(|s| s.to_str()) else {
        return false;
    };
    prog == "perf" && cmd[1] == "sched" && cmd[2] == "record"
}

fn is_taskset_command(cmd: &[String]) -> bool {
    let Some(prog) = cmd
        .first()
        .and_then(|arg| Path::new(arg).file_name())
        .and_then(|name| name.to_str())
    else {
        return false;
    };
    prog == "taskset"
}

fn launch_command(
    command: &mut Command,
    command_view: &[String],
    cgroup: &Path,
    verbose: bool,
) -> Result<Child> {
    let Some(_program) = command_view.first() else {
        anyhow::bail!("missing workload command");
    };
    let root_is_perf_sched_record = is_perf_sched_record_command(command_view);
    let root_is_taskset = is_taskset_command(command_view);
    ensure_current_thread_sched_other()?;
    let mut child = command.spawn().context("failed to spawn workload")?;
    let pid = child.id();

    let initial_exe = proc_exe_path(pid);
    if root_is_taskset {
        if let Some(exe) = initial_exe.as_ref() {
            wait_for_exe_change(pid, exe, verbose)?;
        }
    } else if verbose {
        wait_for_workload_exec(pid, true)?;
    }

    if !root_is_perf_sched_record {
        let _ = unsafe { libc::kill(pid as i32, libc::SIGSTOP) };

        if let Err(err) = move_pid_to_cgroup(cgroup, pid) {
            let _ = unsafe { libc::kill(pid as i32, libc::SIGKILL) };
            let _ = child.wait();
            return Err(err);
        }

        if let Err(err) = set_sched_ext(pid) {
            let _ = unsafe { libc::kill(pid as i32, libc::SIGKILL) };
            let _ = child.wait();
            return Err(err);
        }

        let _ = unsafe { libc::kill(pid as i32, libc::SIGCONT) };
    }

    Ok(child)
}

pub fn launch_workload(cmd: &[String], cgroup: &Path, verbose: bool) -> Result<Child> {
    let Some(program) = cmd.first() else {
        anyhow::bail!("missing workload command");
    };
    let mut command = Command::new(program);
    command.args(&cmd[1..]);
    launch_command(&mut command, cmd, cgroup, verbose)
}

pub fn launch_shell_workload(command_text: &str, cgroup: &Path, verbose: bool) -> Result<Child> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    let mut command = Command::new(&shell);
    let shell_name = Path::new(&shell)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    let mut command_view = vec![shell.clone()];
    match shell_name {
        "bash" => {
            command.arg("--noprofile").arg("--norc").arg("-c");
            command_view.push("--noprofile".to_string());
            command_view.push("--norc".to_string());
            command_view.push("-c".to_string());
        }
        "zsh" => {
            command.arg("-f").arg("-c");
            command_view.push("-f".to_string());
            command_view.push("-c".to_string());
        }
        _ => {
            command.arg("-c");
            command_view.push("-c".to_string());
        }
    }
    command.arg(command_text);
    command_view.push(command_text.to_string());
    launch_command(&mut command, &command_view, cgroup, verbose)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CgroupMemberInfo {
    pub pid: u32,
    pub ppid: Option<u32>,
    pub comm: String,
}

fn proc_ppid(pid: u32) -> Option<u32> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("PPid:\t") {
            return value.trim().parse().ok();
        }
    }
    None
}

fn cgroup_member_info(path: &Path) -> Result<Vec<CgroupMemberInfo>> {
    let text = std::fs::read_to_string(path.join("cgroup.procs"))
        .with_context(|| format!("failed to read {}", path.join("cgroup.procs").display()))?;
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let pid: u32 = line
            .parse()
            .with_context(|| format!("invalid cgroup pid: {line}"))?;
        let comm = std::fs::read_to_string(format!("/proc/{pid}/comm"))
            .ok()
            .map(|value| value.trim().to_string())
            .unwrap_or_else(|| "?".to_string());
        out.push(CgroupMemberInfo {
            pid,
            ppid: proc_ppid(pid),
            comm,
        });
    }
    out.sort_by_key(|member| member.pid);
    Ok(out)
}

pub fn is_descendant_pid(
    pid: u32,
    allowed_roots: &BTreeSet<u32>,
    parent_lookup: &impl Fn(u32) -> Option<u32>,
) -> bool {
    let mut current = Some(pid);
    let mut depth = 0usize;
    while let Some(value) = current {
        if allowed_roots.contains(&value) {
            return true;
        }
        current = parent_lookup(value);
        depth += 1;
        if depth >= 256 {
            break;
        }
    }
    false
}

pub fn is_managed_cgroup_member(
    pid: u32,
    allowed_roots: &BTreeSet<u32>,
    cgroup_members: &BTreeSet<u32>,
    parent_lookup: &impl Fn(u32) -> Option<u32>,
) -> bool {
    let mut current = Some(pid);
    let mut depth = 0usize;
    while let Some(value) = current {
        if allowed_roots.contains(&value) {
            return true;
        }
        if value != pid && !cgroup_members.contains(&value) {
            return false;
        }
        current = parent_lookup(value);
        depth += 1;
        if depth >= 256 {
            break;
        }
    }
    false
}

pub struct CgroupGuard {
    cgroup_path: PathBuf,
    allowed_roots: BTreeSet<u32>,
    verbose: bool,
}

impl CgroupGuard {
    pub fn new(cgroup_path: PathBuf, verbose: bool) -> Self {
        Self {
            cgroup_path,
            allowed_roots: BTreeSet::new(),
            verbose,
        }
    }

    pub fn allow_root(&mut self, pid: u32) {
        self.allowed_roots.insert(pid);
    }

    pub fn set_allowed_anchors(&mut self, pids: BTreeSet<u32>) {
        self.allowed_roots = pids;
    }

    pub fn assert_clean_start(&self) -> Result<()> {
        let members = cgroup_member_info(&self.cgroup_path)?;
        let foreign: Vec<_> = members
            .into_iter()
            .filter(|member| !self.allowed_roots.contains(&member.pid))
            .collect();
        if foreign.is_empty() {
            return Ok(());
        }
        anyhow::bail!(
            "managed cgroup {} is not clean before launch: {}",
            self.cgroup_path.display(),
            Self::format_members(&foreign)
        );
    }

    pub fn assert_runtime_membership(&self) -> Result<()> {
        if self.allowed_roots.is_empty() {
            return Ok(());
        }
        let members = cgroup_member_info(&self.cgroup_path)?;
        let member_pids = members
            .iter()
            .map(|member| member.pid)
            .collect::<BTreeSet<_>>();
        let foreign: Vec<_> = members
            .into_iter()
            .filter(|member| {
                !is_managed_cgroup_member(member.pid, &self.allowed_roots, &member_pids, &proc_ppid)
            })
            .collect();
        if foreign.is_empty() {
            return Ok(());
        }
        if self.verbose {
            crate::diag_line!(
                "cgroup_guard_violation cgroup={} members={}",
                self.cgroup_path.display(),
                Self::format_members(&foreign),
            );
        }
        anyhow::bail!(
            "foreign processes entered managed cgroup {}: {}. launch concurrent apps via --spawn-shell / managed workload launch instead of from a managed shell",
            self.cgroup_path.display(),
            Self::format_members(&foreign)
        );
    }

    fn format_members(members: &[CgroupMemberInfo]) -> String {
        members
            .iter()
            .map(|member| match member.ppid {
                Some(ppid) => format!("{}:{}(ppid={})", member.pid, member.comm, ppid),
                None => format!("{}:{}", member.pid, member.comm),
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[cfg(feature = "diagnostics")]
#[derive(Default)]
struct WorkloadAdoptDiag {
    scans: u64,
    descendants_last: u64,
    pids_moved: u64,
    tids_switched: u64,
    pre_exec_skipped: u64,
}

#[cfg(not(feature = "diagnostics"))]
#[derive(Default)]
struct WorkloadAdoptDiag;

impl WorkloadAdoptDiag {
    fn on_scan(&mut self, descendants: usize) {
        #[cfg(feature = "diagnostics")]
        {
            self.scans = self.scans.saturating_add(1);
            self.descendants_last = descendants as u64;
        }
        #[cfg(not(feature = "diagnostics"))]
        let _ = descendants;
    }

    fn on_pid_moved(&mut self) {
        #[cfg(feature = "diagnostics")]
        {
            self.pids_moved = self.pids_moved.saturating_add(1);
        }
    }

    fn on_tid_switched(&mut self) {
        #[cfg(feature = "diagnostics")]
        {
            self.tids_switched = self.tids_switched.saturating_add(1);
        }
    }

    fn on_pre_exec_skipped(&mut self) {
        #[cfg(feature = "diagnostics")]
        {
            self.pre_exec_skipped = self.pre_exec_skipped.saturating_add(1);
        }
    }

    #[cfg(feature = "diagnostics")]
    fn emit(&self, root_pid: u32, verbose: bool) {
        if verbose {
            crate::diag_line!(
                "workload_adopt_diag root_pid={} scans={} descendants_last={} pids_moved={} tids_switched={} pre_exec_skipped={}",
                root_pid,
                self.scans,
                self.descendants_last,
                self.pids_moved,
                self.tids_switched,
                self.pre_exec_skipped,
            );
        }
    }

    #[cfg(not(feature = "diagnostics"))]
    fn emit(&self, _root_pid: u32, _verbose: bool) {}
}

pub struct WorkloadAdopter {
    root_pid: u32,
    cgroup_path: PathBuf,
    usersched_exe: Option<PathBuf>,
    known_pids: HashSet<u32>,
    known_tids: HashSet<u32>,
    last_scan: Instant,
    scan_every: Duration,
    verbose: bool,
    diag: WorkloadAdoptDiag,
}

impl WorkloadAdopter {
    pub fn new(root_pid: u32, cgroup_path: PathBuf, verbose: bool) -> Self {
        Self {
            root_pid,
            cgroup_path,
            usersched_exe: std::env::current_exe()
                .ok()
                .and_then(|path| std::fs::canonicalize(path).ok()),
            known_pids: HashSet::from([root_pid]),
            known_tids: HashSet::new(),
            last_scan: Instant::now() - Duration::from_millis(100),
            scan_every: Duration::from_millis(10),
            verbose,
            diag: WorkloadAdoptDiag::default(),
        }
    }

    pub fn maybe_sync(&mut self) {
        if self.last_scan.elapsed() < self.scan_every {
            return;
        }
        let profiler = crate::overhead_profile::global();
        let start = profiler
            .as_ref()
            .map(|_| crate::overhead_profile::ThreadClockSample::capture());
        self.last_scan = Instant::now();
        let result = self.sync_now();
        if let (Some(profiler), Some(start)) = (&profiler, start) {
            profiler.record_scope("workload_adopter_sync", start, result.is_ok());
        }
        let _ = result;
    }

    fn sync_now(&mut self) -> Result<()> {
        let profiler = crate::overhead_profile::global();
        let collect_start = profiler
            .as_ref()
            .map(|_| crate::overhead_profile::ThreadClockSample::capture());
        let descendants = collect_descendant_processes(self.root_pid)?;
        if let (Some(profiler), Some(start)) = (&profiler, collect_start) {
            profiler.record_scope("workload_adopter_collect_descendants", start, true);
        }
        self.diag.on_scan(descendants.len());

        for process in descendants {
            let pid = process.pid as u32;
            if !self.known_pids.contains(&pid) {
                if pid_in_cgroup(&self.cgroup_path, pid) {
                    self.known_pids.insert(pid);
                } else {
                    let move_start = profiler
                        .as_ref()
                        .map(|_| crate::overhead_profile::ThreadClockSample::capture());
                    let moved = move_pid_to_cgroup(&self.cgroup_path, pid).is_ok();
                    if let (Some(profiler), Some(start)) = (&profiler, move_start) {
                        profiler.record_scope("workload_adopter_move_pid", start, moved);
                    }
                    if moved {
                        self.known_pids.insert(pid);
                        self.diag.on_pid_moved();
                    }
                }
            }

            if self.is_pre_exec_stub(&process) {
                self.diag.on_pre_exec_skipped();
                continue;
            }

            let tasks_start = profiler
                .as_ref()
                .map(|_| crate::overhead_profile::ThreadClockSample::capture());
            let tasks_result = process.tasks();
            if let (Some(profiler), Some(start)) = (&profiler, tasks_start) {
                profiler.record_scope(
                    "workload_adopter_process_tasks",
                    start,
                    tasks_result.is_ok(),
                );
            }
            let tasks = match tasks_result {
                Ok(tasks) => tasks,
                Err(_) => continue,
            };
            for task in tasks.flatten() {
                let tid = task.tid as u32;
                if self.known_tids.contains(&tid) {
                    continue;
                }
                let switch_start = profiler
                    .as_ref()
                    .map(|_| crate::overhead_profile::ThreadClockSample::capture());
                let switched = set_sched_ext(tid).is_ok();
                if let (Some(profiler), Some(start)) = (&profiler, switch_start) {
                    profiler.record_scope("workload_adopter_set_sched_ext", start, switched);
                }
                if switched {
                    self.known_tids.insert(tid);
                    self.diag.on_tid_switched();
                }
            }
        }

        self.diag.emit(self.root_pid, self.verbose);

        Ok(())
    }

    fn is_pre_exec_stub(&self, process: &Process) -> bool {
        let Some(usersched_exe) = self.usersched_exe.as_ref() else {
            return false;
        };
        let Ok(exe) = process.exe() else {
            return false;
        };
        let Ok(exe) = std::fs::canonicalize(exe) else {
            return false;
        };
        exe == *usersched_exe
    }

    pub fn tracked_pids(&self) -> BTreeSet<u32> {
        let mut out = BTreeSet::from([self.root_pid]);
        out.extend(self.known_pids.iter().copied());
        out
    }
}

fn collect_descendant_processes(root_pid: u32) -> Result<Vec<Process>> {
    let mut out = Vec::new();
    let mut queue = VecDeque::from([root_pid as i32]);
    let mut visited = HashSet::new();

    while let Some(pid) = queue.pop_front() {
        if !visited.insert(pid) {
            continue;
        }
        let process = match Process::new(pid) {
            Ok(process) => process,
            Err(_) => continue,
        };
        let children = match process.task_main_thread().and_then(|task| task.children()) {
            Ok(children) => children,
            Err(_) => Vec::new(),
        };
        out.push(process);
        for child in children {
            queue.push_back(child as i32);
        }
    }

    Ok(out)
}

pub fn shutdown_child_gracefully(child: &mut Child) {
    let pid = child.id() as i32;
    if let Ok(Some(_)) = child.try_wait() {
        let _ = child.wait();
        return;
    }

    let _ = unsafe { libc::kill(pid, libc::SIGINT) };
    for _ in 0..100 {
        std::thread::sleep(Duration::from_millis(50));
        if let Ok(Some(_)) = child.try_wait() {
            let _ = child.wait();
            return;
        }
    }

    let _ = unsafe { libc::kill(pid, libc::SIGTERM) };
    for _ in 0..40 {
        std::thread::sleep(Duration::from_millis(50));
        if let Ok(Some(_)) = child.try_wait() {
            let _ = child.wait();
            return;
        }
    }

    let _ = unsafe { libc::kill(pid, libc::SIGKILL) };
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::{
        cgroup2_relative_path, cgroup_text_contains_path, is_descendant_pid,
        is_managed_cgroup_member,
    };
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::Path;

    #[test]
    fn descendant_detection_walks_parent_chain() {
        let allowed = BTreeSet::from([100u32]);
        let parents = BTreeMap::from([(101u32, Some(100u32)), (102u32, Some(101u32))]);
        assert!(is_descendant_pid(102, &allowed, &|pid| {
            parents.get(&pid).copied().flatten()
        }));
    }

    #[test]
    fn descendant_detection_rejects_foreign_pid() {
        let allowed = BTreeSet::from([100u32]);
        let parents = BTreeMap::from([(201u32, Some(200u32)), (200u32, Some(1u32))]);
        assert!(!is_descendant_pid(201, &allowed, &|pid| {
            parents.get(&pid).copied().flatten()
        }));
    }

    #[test]
    fn managed_member_accepts_same_cgroup_parent_chain() {
        let allowed = BTreeSet::from([100u32]);
        let members = BTreeSet::from([100u32, 101u32, 102u32]);
        let parents = BTreeMap::from([(101u32, Some(100u32)), (102u32, Some(101u32))]);

        assert!(is_managed_cgroup_member(102, &allowed, &members, &|pid| {
            parents.get(&pid).copied().flatten()
        }));
    }

    #[test]
    fn managed_member_rejects_chain_that_leaves_cgroup() {
        let allowed = BTreeSet::from([100u32]);
        let members = BTreeSet::from([201u32]);
        let parents = BTreeMap::from([(201u32, Some(200u32)), (200u32, Some(1u32))]);

        assert!(!is_managed_cgroup_member(201, &allowed, &members, &|pid| {
            parents.get(&pid).copied().flatten()
        }));
    }

    #[test]
    fn cgroup2_relative_path_matches_proc_format() {
        assert_eq!(
            cgroup2_relative_path(Path::new("/sys/fs/cgroup/scx-test")).as_deref(),
            Some("/scx-test")
        );
    }

    #[test]
    fn cgroup_text_path_match_is_exact() {
        assert!(cgroup_text_contains_path("0::/scx-test\n", "/scx-test"));
        assert!(!cgroup_text_contains_path(
            "0::/scx-test-child\n",
            "/scx-test"
        ));
    }
}
