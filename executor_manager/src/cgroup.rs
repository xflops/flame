/*
Copyright 2026 The Flame Authors.
Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at
    http://www.apache.org/licenses/LICENSE-2.0
Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
*/

//! Per-executor resource groups on cgroup v2.
//!
//! The host shim runs the service of an executor in `<root>/flame/<executor-id>`
//! with the memory and CPU limits of the executor's resource requirement. The
//! limits bind every process the service starts, and removing the group kills
//! them all, also the ones that left the service's process group or session.
//!
//! cgroupfs is a file system, so everything here is plain file I/O, and the
//! tests run against a fake tree in a temporary directory. The host shim uses
//! it on Linux only.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::apis::ResourceRequirement;
use common::FlameError;

/// Where the cgroup v2 hierarchy is mounted.
pub const SYSTEM_ROOT: &str = "/sys/fs/cgroup";
/// Directory of the Flame groups below the cgroup root.
const GROUP_DIR: &str = "flame";
/// Controllers the limits need.
const CONTROLLERS: [&str; 2] = ["cpu", "memory"];
/// `cpu.max` period; the quota is `cores * PERIOD`.
const CPU_PERIOD_US: u64 = 100_000;
/// How long a group may take to become empty after it was killed.
const EMPTY_TIMEOUT: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(20);
/// `EBUSY`: a group with processes cannot enable controllers for its children.
const EBUSY: i32 = 16;

fn internal(msg: impl Into<String>) -> FlameError {
    FlameError::Internal(msg.into())
}

/// The effects on the system, replaceable so tests need no real processes or cgroupfs.
struct Ops<'a> {
    /// Sends SIGKILL to a process.
    kill: &'a (dyn Fn(i32) + Sync),
    /// Removes an empty group directory.
    rmdir: &'a (dyn Fn(&Path) -> io::Result<()> + Sync),
}

fn sigkill(pid: i32) {
    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;
    // A process that is already gone is what we want.
    let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
}

/// The `memory.max` and `cpu.max` values of a resource requirement. A zero
/// requirement means no limit. The CPU unit is whole cores.
pub fn limits(resreq: &ResourceRequirement) -> (String, String) {
    let memory = match resreq.memory {
        0 => "max".to_string(),
        bytes => bytes.to_string(),
    };
    let cpu = match resreq.cpu {
        0 => format!("max {CPU_PERIOD_US}"),
        cores => format!("{} {CPU_PERIOD_US}", cores.saturating_mul(CPU_PERIOD_US)),
    };
    (memory, cpu)
}

/// An executor id is a directory name: no separators, no dots at the start.
fn validate_id(id: &str) -> Result<(), FlameError> {
    let ok = !id.is_empty()
        && id.len() <= 128
        && !id.starts_with('.')
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c));
    if ok {
        Ok(())
    } else {
        Err(FlameError::InvalidConfig(format!(
            "invalid executor id <{id}> for a cgroup"
        )))
    }
}

/// The cgroup v2 hierarchy (or a delegated subtree of it) Flame creates groups in.
#[derive(Debug, Clone)]
pub struct CgroupRoot {
    root: PathBuf,
}

impl CgroupRoot {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The hierarchy of this node, at `/sys/fs/cgroup`.
    pub fn system() -> Self {
        Self::new(SYSTEM_ROOT)
    }

    fn base(&self) -> PathBuf {
        self.root.join(GROUP_DIR)
    }

    /// Checks for cgroup v2 and a writable subtree, and prepares `<root>/flame`
    /// so that the groups below it carry CPU and memory limits. Idempotent.
    pub fn prepare(&self) -> Result<(), FlameError> {
        let unavailable = |why: String| {
            FlameError::InvalidConfig(format!(
                "cgroup v2 resource groups are not available at <{}>: {why} \
                 (they need cgroup v2 and a writable, delegated subtree)",
                self.root.display()
            ))
        };
        let controllers = fs::read_to_string(self.root.join("cgroup.controllers"))
            .map_err(|e| unavailable(format!("cannot read cgroup.controllers: {e}")))?;
        for controller in CONTROLLERS {
            if !controllers.split_whitespace().any(|c| c == controller) {
                return Err(unavailable(format!(
                    "the {controller} controller is not available"
                )));
            }
        }
        let base = self.base();
        fs::create_dir_all(&base)
            .map_err(|e| unavailable(format!("cannot create {}: {e}", base.display())))?;
        enable_controllers(&self.root)?;
        enable_controllers(&base)
    }

    /// Creates the group of executor `id` with the limits of `resreq`.
    pub fn create(&self, id: &str, resreq: &ResourceRequirement) -> Result<Cgroup, FlameError> {
        validate_id(id)?;
        self.prepare()?;
        let path = self.base().join(id);
        fs::create_dir(&path)
            .map_err(|e| internal(format!("create cgroup {}: {e}", path.display())))?;
        let group = Cgroup { path };
        let (memory, cpu) = limits(resreq);
        let result = group
            .write("memory.max", &memory)
            .and_then(|()| group.write("memory.swap.max", "0"))
            .and_then(|()| group.write("cpu.max", &cpu));
        if let Err(e) = result {
            let _ = fs::remove_dir(&group.path);
            return Err(e);
        }
        Ok(group)
    }

    /// Kills and removes the groups of executors that are not in `live`, such
    /// as the ones a crashed executor manager left. Returns the ids removed.
    /// A group that cannot be removed is logged and skipped.
    pub async fn cleanup_stale(&self, live: &HashSet<String>) -> Result<Vec<String>, FlameError> {
        self.cleanup_stale_with(
            live,
            &Ops {
                kill: &sigkill,
                rmdir: &|p| fs::remove_dir(p),
            },
            EMPTY_TIMEOUT,
        )
        .await
    }

    async fn cleanup_stale_with(
        &self,
        live: &HashSet<String>,
        ops: &Ops<'_>,
        timeout: Duration,
    ) -> Result<Vec<String>, FlameError> {
        let base = self.base();
        let entries = match fs::read_dir(&base) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(internal(format!("read {}: {e}", base.display()))),
        };
        let mut removed = Vec::new();
        for entry in entries.flatten() {
            let Ok(id) = entry.file_name().into_string() else {
                continue;
            };
            if !entry.path().is_dir() || live.contains(&id) {
                continue;
            }
            let group = Cgroup { path: entry.path() };
            match group.remove_with(ops, timeout).await {
                Ok(()) => {
                    tracing::info!("removed stale cgroup of executor <{id}>");
                    removed.push(id);
                }
                Err(e) => tracing::warn!("failed to remove stale cgroup of executor <{id}>: {e}"),
            }
        }
        Ok(removed)
    }
}

/// Enables the CPU and memory controllers for the children of `dir`.
fn enable_controllers(dir: &Path) -> Result<(), FlameError> {
    let file = dir.join("cgroup.subtree_control");
    let enabled =
        fs::read_to_string(&file).map_err(|e| internal(format!("read {}: {e}", file.display())))?;
    let missing: Vec<String> = CONTROLLERS
        .iter()
        .filter(|c| !enabled.split_whitespace().any(|e| e == **c))
        .map(|c| format!("+{c}"))
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    fs::write(&file, missing.join(" ")).map_err(|e| {
        if e.raw_os_error() == Some(EBUSY) {
            FlameError::InvalidConfig(format!(
                "cannot enable the cpu and memory controllers in <{}>: it has processes of its \
                 own; set cluster.executors.cgroup.root to an empty delegated cgroup",
                dir.display()
            ))
        } else {
            internal(format!("enable controllers in {}: {e}", dir.display()))
        }
    })
}

/// The resource group of one executor.
#[derive(Debug, Clone)]
pub struct Cgroup {
    path: PathBuf,
}

impl Cgroup {
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn write(&self, file: &str, value: &str) -> Result<(), FlameError> {
        let path = self.path.join(file);
        fs::write(&path, value).map_err(|e| internal(format!("write {}: {e}", path.display())))
    }

    /// Moves a process into the group.
    pub fn add_process(&self, pid: u32) -> Result<(), FlameError> {
        self.write("cgroup.procs", &pid.to_string())
    }

    /// Moves a process and its descendants into the group. A child the process
    /// started before it was moved would otherwise stay outside the limits.
    pub fn add_process_tree(&self, pid: u32, proc_root: &Path) -> Result<(), FlameError> {
        self.add_process(pid)?;
        for child in children_of(proc_root, pid) {
            self.add_process_tree(child, proc_root)?;
        }
        Ok(())
    }

    /// Every process in the group and in the groups below it.
    pub fn pids(&self) -> Vec<i32> {
        let mut pids = Vec::new();
        collect_pids(&self.path, &mut pids);
        pids
    }

    fn is_empty(&self) -> bool {
        match fs::read_to_string(self.path.join("cgroup.events")) {
            Ok(events) => !events.lines().any(|l| l.trim() == "populated 1"),
            Err(_) => self.pids().is_empty(),
        }
    }

    /// Kills every process of the group now, without waiting. Best effort, for
    /// cleanup paths that cannot wait (`Drop`); `remove` does the rest.
    pub fn kill(&self) {
        self.kill_all(&Ops {
            kill: &sigkill,
            rmdir: &|p| fs::remove_dir(p),
        });
    }

    /// Kills every process of the group and removes the group and the groups
    /// below it. The group is gone when this returns `Ok`.
    pub async fn remove(&self) -> Result<(), FlameError> {
        self.remove_with(
            &Ops {
                kill: &sigkill,
                rmdir: &|p| fs::remove_dir(p),
            },
            EMPTY_TIMEOUT,
        )
        .await
    }

    async fn remove_with(&self, ops: &Ops<'_>, timeout: Duration) -> Result<(), FlameError> {
        if !self.path.exists() {
            return Ok(());
        }
        let deadline = Instant::now() + timeout;
        loop {
            self.kill_all(ops);
            if self.is_empty() {
                break;
            }
            if Instant::now() >= deadline {
                return Err(internal(format!(
                    "cgroup {} still has processes after SIGKILL",
                    self.path.display()
                )));
            }
            tokio::time::sleep(POLL).await;
        }
        remove_tree(&self.path, ops, deadline).await
    }

    /// `cgroup.kill` kills the whole subtree at once (Linux 5.14); without it
    /// every process is killed one by one.
    fn kill_all(&self, ops: &Ops<'_>) {
        let kill_file = self.path.join("cgroup.kill");
        if kill_file.exists() && fs::write(&kill_file, "1").is_ok() {
            return;
        }
        for pid in self.pids() {
            (ops.kill)(pid);
        }
    }
}

fn collect_pids(dir: &Path, pids: &mut Vec<i32>) {
    if let Ok(procs) = fs::read_to_string(dir.join("cgroup.procs")) {
        pids.extend(procs.lines().filter_map(|l| l.trim().parse::<i32>().ok()));
    }
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                collect_pids(&entry.path(), pids);
            }
        }
    }
}

/// The child pids of `pid` from `<proc_root>/<pid>/task/*/children`.
fn children_of(proc_root: &Path, pid: u32) -> Vec<u32> {
    let tasks = proc_root.join(pid.to_string()).join("task");
    let Ok(entries) = fs::read_dir(tasks) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|task| fs::read_to_string(task.path().join("children")).ok())
        .flat_map(|children| {
            children
                .split_whitespace()
                .filter_map(|c| c.parse::<u32>().ok())
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Removes `dir` and the groups below it, deepest first. A just-emptied group
/// can still be busy for a moment, so `rmdir` is retried until `deadline`.
async fn remove_tree(dir: &Path, ops: &Ops<'_>, deadline: Instant) -> Result<(), FlameError> {
    let mut stack = vec![dir.to_path_buf()];
    let mut order = Vec::new();
    while let Some(d) = stack.pop() {
        if let Ok(entries) = fs::read_dir(&d) {
            stack.extend(entries.flatten().map(|e| e.path()).filter(|p| p.is_dir()));
        }
        order.push(d);
    }
    for d in order.iter().rev() {
        loop {
            match (ops.rmdir)(d) {
                Ok(()) => break,
                Err(e) if e.kind() == io::ErrorKind::NotFound => break,
                Err(e) if Instant::now() < deadline => {
                    tracing::debug!("cgroup {} is busy: {e}", d.display());
                    tokio::time::sleep(POLL).await;
                }
                Err(e) => return Err(internal(format!("remove cgroup {}: {e}", d.display()))),
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    use super::*;

    /// A fake cgroup v2 root: the files of the hierarchy, in a temporary directory.
    fn fake_root(controllers: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("cgroup.controllers"), controllers).unwrap();
        fs::write(dir.path().join("cgroup.subtree_control"), "").unwrap();
        // The kernel creates the control files of a new group; the fake has
        // them for `flame/`, which `prepare` creates.
        let flame = dir.path().join(GROUP_DIR);
        fs::create_dir_all(&flame).unwrap();
        fs::write(flame.join("cgroup.subtree_control"), "").unwrap();
        dir
    }

    fn resreq(cpu: u64, memory: u64) -> ResourceRequirement {
        ResourceRequirement {
            cpu,
            memory,
            gpu: 0,
        }
    }

    /// Group files as the kernel has them; the directory is removable once
    /// the fake `kill` deleted them.
    fn fake_group(root: &Path, id: &str, pids: &[i32]) -> PathBuf {
        let dir = root.join(GROUP_DIR).join(id);
        fs::create_dir_all(&dir).unwrap();
        let procs: String = pids.iter().map(|p| format!("{p}\n")).collect();
        fs::write(dir.join("cgroup.procs"), procs).unwrap();
        dir
    }

    /// Stands in for the kernel: a killed process leaves its group's files.
    fn ops_that_remove<'a>(
        root: &'a Path,
        killed: &'a Mutex<Vec<i32>>,
    ) -> (
        impl Fn(i32) + Sync + 'a,
        impl Fn(&Path) -> io::Result<()> + Sync + 'a,
    ) {
        let kill = move |pid: i32| {
            killed.lock().unwrap().push(pid);
            // The process is gone: empty every cgroup.procs below the root.
            for path in walk(root) {
                if path.file_name().is_some_and(|n| n == "cgroup.procs") {
                    fs::write(path, "").unwrap();
                }
            }
        };
        let rmdir = |p: &Path| fs::remove_dir_all(p);
        (kill, rmdir)
    }

    fn walk(dir: &Path) -> Vec<PathBuf> {
        let mut all = Vec::new();
        if let Ok(entries) = fs::read_dir(dir) {
            for e in entries.flatten() {
                all.push(e.path());
                if e.path().is_dir() {
                    all.extend(walk(&e.path()));
                }
            }
        }
        all
    }

    #[test]
    fn limits_follow_the_requirement() {
        assert_eq!(
            limits(&resreq(2, 4 << 30)),
            ("4294967296".to_string(), "200000 100000".to_string())
        );
        assert_eq!(limits(&resreq(1, 1 << 20)).1, "100000 100000");
        // Zero is no limit.
        assert_eq!(
            limits(&resreq(0, 0)),
            ("max".to_string(), "max 100000".to_string())
        );
    }

    #[test]
    fn prepare_enables_the_controllers_of_root_and_flame() {
        let root = fake_root("cpuset cpu io memory pids");
        CgroupRoot::new(root.path()).prepare().unwrap();
        for dir in [root.path().to_path_buf(), root.path().join(GROUP_DIR)] {
            let enabled = fs::read_to_string(dir.join("cgroup.subtree_control")).unwrap();
            assert_eq!(enabled, "+cpu +memory", "{}", dir.display());
        }
    }

    #[test]
    fn prepare_leaves_enabled_controllers_alone() {
        let root = fake_root("cpu memory");
        fs::write(root.path().join("cgroup.subtree_control"), "cpu memory\n").unwrap();
        fs::write(
            root.path().join(GROUP_DIR).join("cgroup.subtree_control"),
            "memory cpu",
        )
        .unwrap();
        CgroupRoot::new(root.path()).prepare().unwrap();
        let after = fs::read_to_string(root.path().join("cgroup.subtree_control")).unwrap();
        assert_eq!(after, "cpu memory\n");
    }

    #[test]
    fn prepare_fails_clearly_without_cgroup_v2_or_a_controller() {
        let empty = tempfile::tempdir().unwrap();
        let err = CgroupRoot::new(empty.path()).prepare().unwrap_err();
        assert!(matches!(err, FlameError::InvalidConfig(_)));
        assert!(err.to_string().contains("cgroup v2"), "{err}");

        let no_cpu = fake_root("memory io");
        let err = CgroupRoot::new(no_cpu.path()).prepare().unwrap_err();
        assert!(err.to_string().contains("cpu controller"), "{err}");

        let missing = CgroupRoot::new("/definitely/not/a/cgroup/root");
        assert!(missing.prepare().is_err());
    }

    #[test]
    fn create_sets_the_limits_of_the_executor() {
        let root = fake_root("cpu memory");
        let cg = CgroupRoot::new(root.path());
        let group = cg.create("exec-1", &resreq(2, 1 << 30)).unwrap();
        assert_eq!(group.path(), root.path().join(GROUP_DIR).join("exec-1"));
        let read = |f: &str| fs::read_to_string(group.path().join(f)).unwrap();
        assert_eq!(read("memory.max"), "1073741824");
        assert_eq!(read("memory.swap.max"), "0");
        assert_eq!(read("cpu.max"), "200000 100000");
        // A second group does not touch the first.
        let other = cg.create("exec-2", &resreq(1, 1 << 20)).unwrap();
        assert_eq!(read("memory.max"), "1073741824");
        assert_ne!(other.path(), group.path());
    }

    #[test]
    fn create_rejects_bad_ids_and_duplicates() {
        let root = fake_root("cpu memory");
        let cg = CgroupRoot::new(root.path());
        for bad in ["", ".hidden", "..", "a/b", "../escape", "a b"] {
            assert!(
                matches!(
                    cg.create(bad, &resreq(1, 1)),
                    Err(FlameError::InvalidConfig(_))
                ),
                "{bad}"
            );
        }
        cg.create("exec-1", &resreq(1, 1)).unwrap();
        assert!(cg.create("exec-1", &resreq(1, 1)).is_err());
        assert!(!root.path().join("escape").exists());
    }

    #[test]
    fn processes_are_added_with_their_descendants() {
        let root = fake_root("cpu memory");
        let group = CgroupRoot::new(root.path())
            .create("exec-1", &resreq(1, 1))
            .unwrap();
        group.add_process(42).unwrap();
        assert_eq!(
            fs::read_to_string(group.path().join("cgroup.procs")).unwrap(),
            "42"
        );

        // 100 has the children 101 and 102 (in two threads); 101 has 103.
        let proc = tempfile::tempdir().unwrap();
        for (pid, tid, children) in [(100, 100, "101"), (100, 105, "102"), (101, 101, "103")] {
            let task = proc
                .path()
                .join(pid.to_string())
                .join("task")
                .join(tid.to_string());
            fs::create_dir_all(&task).unwrap();
            fs::write(task.join("children"), children).unwrap();
        }
        // The fake cgroup.procs keeps only the last write; record them through
        // a directory per write instead: check the walk order by the files left.
        let seen = children_of(proc.path(), 100);
        assert_eq!(seen.len(), 2);
        assert!(seen.contains(&101) && seen.contains(&102));
        assert_eq!(children_of(proc.path(), 101), vec![103]);
        assert!(children_of(proc.path(), 999).is_empty());
        group.add_process_tree(100, proc.path()).unwrap();
    }

    #[test]
    fn pids_include_the_groups_below() {
        let root = fake_root("cpu memory");
        let dir = fake_group(root.path(), "exec-1", &[10, 11]);
        fs::create_dir_all(dir.join("sandbox-a")).unwrap();
        fs::write(dir.join("sandbox-a/cgroup.procs"), "20\n").unwrap();
        fs::create_dir_all(dir.join("sandbox-a/nested")).unwrap();
        fs::write(dir.join("sandbox-a/nested/cgroup.procs"), "30\n").unwrap();
        let group = Cgroup { path: dir };
        let mut pids = group.pids();
        pids.sort_unstable();
        assert_eq!(pids, [10, 11, 20, 30]);
    }

    #[tokio::test]
    async fn remove_kills_every_process_one_by_one_without_cgroup_kill() {
        let root = fake_root("cpu memory");
        let dir = fake_group(root.path(), "exec-1", &[10, 11]);
        fs::create_dir_all(dir.join("child")).unwrap();
        fs::write(dir.join("child/cgroup.procs"), "20\n").unwrap();
        let killed = Mutex::new(Vec::new());
        let (kill, rmdir) = ops_that_remove(root.path(), &killed);
        let group = Cgroup { path: dir.clone() };

        group
            .remove_with(
                &Ops {
                    kill: &kill,
                    rmdir: &rmdir,
                },
                Duration::from_secs(2),
            )
            .await
            .unwrap();

        let mut killed = killed.lock().unwrap().clone();
        killed.sort_unstable();
        killed.dedup();
        assert_eq!(killed, [10, 11, 20]);
        assert!(!dir.exists());
    }

    #[tokio::test]
    async fn remove_uses_cgroup_kill_when_the_kernel_has_it() {
        let root = fake_root("cpu memory");
        let dir = fake_group(root.path(), "exec-1", &[]);
        fs::write(dir.join("cgroup.kill"), "").unwrap();
        let killed = Mutex::new(Vec::new());
        let (kill, rmdir) = ops_that_remove(root.path(), &killed);
        Cgroup { path: dir.clone() }
            .remove_with(
                &Ops {
                    kill: &kill,
                    rmdir: &rmdir,
                },
                Duration::from_secs(2),
            )
            .await
            .unwrap();
        // Nothing was killed one by one, and the file was written before removal.
        assert!(killed.lock().unwrap().is_empty());
        assert!(!dir.exists());

        // Written once, with "1", while the group still existed.
        let dir = fake_group(root.path(), "exec-2", &[]);
        fs::write(dir.join("cgroup.kill"), "").unwrap();
        let group = Cgroup { path: dir.clone() };
        group.kill_all(&Ops {
            kill: &|_| {},
            rmdir: &|_| Ok(()),
        });
        assert_eq!(fs::read_to_string(dir.join("cgroup.kill")).unwrap(), "1");
    }

    #[tokio::test]
    async fn remove_waits_for_populated_to_clear_and_fails_when_it_does_not() {
        let root = fake_root("cpu memory");
        let dir = fake_group(root.path(), "exec-1", &[]);
        fs::write(dir.join("cgroup.events"), "populated 1\nfrozen 0\n").unwrap();
        let killed = Mutex::new(Vec::new());
        let (kill, rmdir) = ops_that_remove(root.path(), &killed);
        let err = Cgroup { path: dir.clone() }
            .remove_with(
                &Ops {
                    kill: &kill,
                    rmdir: &rmdir,
                },
                Duration::from_millis(150),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("still has processes"), "{err}");
        assert!(dir.exists());

        fs::write(dir.join("cgroup.events"), "populated 0\nfrozen 0\n").unwrap();
        Cgroup { path: dir.clone() }
            .remove_with(
                &Ops {
                    kill: &kill,
                    rmdir: &rmdir,
                },
                Duration::from_secs(2),
            )
            .await
            .unwrap();
        assert!(!dir.exists());
    }

    #[tokio::test]
    async fn remove_retries_a_busy_directory_and_ignores_a_missing_one() {
        let root = fake_root("cpu memory");
        let dir = fake_group(root.path(), "exec-1", &[]);
        let attempts = AtomicUsize::new(0);
        let rmdir = |p: &Path| {
            if attempts.fetch_add(1, Ordering::SeqCst) + 1 < 3 {
                Err(io::Error::from_raw_os_error(EBUSY))
            } else {
                fs::remove_dir_all(p)
            }
        };
        Cgroup { path: dir.clone() }
            .remove_with(
                &Ops {
                    kill: &|_| {},
                    rmdir: &rmdir,
                },
                Duration::from_secs(2),
            )
            .await
            .unwrap();
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
        assert!(!dir.exists());

        // Already gone is success.
        Cgroup { path: dir }.remove().await.unwrap();
    }

    #[tokio::test]
    async fn stale_cleanup_removes_only_groups_that_are_not_live() {
        let root = fake_root("cpu memory");
        let live_dir = fake_group(root.path(), "live-1", &[7]);
        let stale_a = fake_group(root.path(), "stale-a", &[8]);
        let stale_b = fake_group(root.path(), "stale-b", &[]);
        let killed = Mutex::new(Vec::new());
        let stale_a_dir = root.path().join(GROUP_DIR).join("stale-a");
        let (kill, rmdir) = ops_that_remove(&stale_a_dir, &killed);
        let live: HashSet<String> = ["live-1".to_string()].into();

        let mut removed = CgroupRoot::new(root.path())
            .cleanup_stale_with(
                &live,
                &Ops {
                    kill: &kill,
                    rmdir: &rmdir,
                },
                Duration::from_secs(2),
            )
            .await
            .unwrap();
        removed.sort();

        assert_eq!(removed, ["stale-a", "stale-b"]);
        assert!(live_dir.exists());
        assert_eq!(
            fs::read_to_string(live_dir.join("cgroup.procs")).unwrap(),
            "7\n"
        );
        assert!(!stale_a.exists() && !stale_b.exists());
        assert_eq!(*killed.lock().unwrap(), [8]);
    }

    #[tokio::test]
    async fn stale_cleanup_without_a_flame_directory_is_a_no_op() {
        let root = fake_root("cpu memory");
        let removed = CgroupRoot::new(root.path())
            .cleanup_stale(&HashSet::new())
            .await
            .unwrap();
        assert!(removed.is_empty());
    }
}
