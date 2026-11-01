//! On-disk runtime state in `<project dir>/.ncompose/`.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Paths {
    pub root: PathBuf,
}

impl Paths {
    pub fn new(project_dir: &Path) -> Self {
        Paths { root: project_dir.join(".ncompose") }
    }
    pub fn state(&self) -> PathBuf {
        self.root.join("state.json")
    }
    pub fn logs(&self) -> PathBuf {
        self.root.join("logs")
    }
    pub fn log(&self, service: &str) -> PathBuf {
        self.logs().join(format!("{service}.log"))
    }
    pub fn supervisor_log(&self) -> PathBuf {
        self.root.join("supervisor.log")
    }
    pub fn restart_requests(&self) -> PathBuf {
        self.root.join("restart-requests")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct State {
    pub project: String,
    pub supervisor_pid: i32,
    /// When the supervisor process started (see [`process_start`]), so a reused PID
    /// is not mistaken for it. Absent in state files from older versions.
    #[serde(default)]
    pub supervisor_start: Option<u64>,
    pub detached: bool,
    /// Aliases `up` had to create with sudo.
    pub aliases_added: Vec<Ipv4Addr>,
    /// `up --remove-aliases`: remove `aliases_added` when the project stops.
    #[serde(default)]
    pub remove_aliases: bool,
    pub services: BTreeMap<String, ServiceState>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ServiceState {
    pub ip: Option<Ipv4Addr>,
    pub pid: Option<i32>,
    /// Start time of `pid` (see [`process_start`]).
    #[serde(default)]
    pub pid_start: Option<u64>,
    pub status: String,
    pub restarts: u32,
    pub ports: Vec<String>,
    pub last_exit: Option<String>,
}

impl State {
    pub fn load(paths: &Paths) -> Result<Option<State>> {
        match std::fs::read_to_string(paths.state()) {
            Ok(s) => Ok(Some(serde_json::from_str(&s).context("parsing state.json")?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save(&self, paths: &Paths) -> Result<()> {
        std::fs::create_dir_all(&paths.root)?;
        let tmp = paths.root.join("state.json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(tmp, paths.state())?;
        Ok(())
    }

    pub fn remove(paths: &Paths) {
        let _ = std::fs::remove_file(paths.state());
    }

    /// Whether the recorded supervisor is still the process running under its PID.
    pub fn supervisor_alive(&self) -> bool {
        same_process(self.supervisor_pid, self.supervisor_start)
    }
}

impl ServiceState {
    /// The service's PID, if that process is still the one we started.
    pub fn live_pid(&self) -> Option<i32> {
        self.pid.filter(|p| same_process(*p, self.pid_start))
    }
}

// --- restart requests --------------------------------------------------------
//
// `ncompose restart` appends a line ("*" or service names) and sends SIGUSR1; the
// supervisor takes every queued line. Both sides hold an exclusive flock, and the
// file is truncated rather than deleted, so a request appended concurrently with a
// take is either taken now or left for the next signal, never lost.

fn locked_requests(paths: &Paths) -> std::io::Result<std::fs::File> {
    std::fs::create_dir_all(&paths.root)?;
    let f = std::fs::OpenOptions::new().create(true).read(true).append(true).open(paths.restart_requests())?;
    if unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&f), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(f) // unlocked when closed
}

pub fn append_restart_request(paths: &Paths, line: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = locked_requests(paths)?;
    writeln!(f, "{line}")
}

/// What to restart: None for nothing queued, Some(empty) for every service.
pub fn take_restart_requests(paths: &Paths) -> std::io::Result<Option<Vec<String>>> {
    use std::io::Read;
    let mut f = locked_requests(paths)?;
    let mut text = String::new();
    f.read_to_string(&mut text)?;
    f.set_len(0)?;
    let words: Vec<String> = text.split_whitespace().map(str::to_string).collect();
    Ok(match words {
        w if w.is_empty() => None,
        w if w.iter().any(|n| n == "*") => Some(vec![]),
        w => Some(w),
    })
}

pub fn pid_alive(pid: i32) -> bool {
    pid > 0 && unsafe { libc::kill(pid, 0) } == 0
}

/// When process `pid` started, in microseconds since the epoch; None if there is no
/// such process (or we may not look at it).
pub fn process_start(pid: i32) -> Option<u64> {
    if pid <= 0 {
        return None;
    }
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let n = unsafe {
        libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, &mut info as *mut _ as *mut libc::c_void, size)
    };
    (n == size).then(|| info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec)
}

/// Whether `pid` is alive and is the process that started at `start`. PIDs are reused,
/// so after a crash or reboot a recorded PID may belong to an unrelated process.
/// Without a recorded start time (older state files) only liveness can be checked.
pub fn same_process(pid: i32, start: Option<u64>) -> bool {
    match start {
        Some(start) => pid_alive(pid) && process_start(pid) == Some(start),
        None => pid_alive(pid),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_project(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ncompose-state-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn state_round_trips_and_disappears() {
        let dir = temp_project("roundtrip");
        let paths = Paths::new(&dir);
        assert!(State::load(&paths).unwrap().is_none());

        let mut st = State { project: "p".into(), supervisor_pid: 42, detached: true, ..Default::default() };
        st.aliases_added.push(Ipv4Addr::new(127, 10, 0, 2));
        st.services.insert(
            "api".into(),
            ServiceState { ip: Some(Ipv4Addr::new(127, 10, 0, 3)), pid: Some(7), status: "healthy".into(), restarts: 2, ..Default::default() },
        );
        st.save(&paths).unwrap();
        let back = State::load(&paths).unwrap().unwrap();
        assert_eq!(back.supervisor_pid, 42);
        assert_eq!(back.aliases_added, [Ipv4Addr::new(127, 10, 0, 2)]);
        assert_eq!(back.services["api"].status, "healthy");
        assert_eq!(back.services["api"].restarts, 2);
        assert!(!paths.root.join("state.json.tmp").exists(), "written atomically via rename");

        State::remove(&paths);
        assert!(State::load(&paths).unwrap().is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_corrupt_state_file_is_an_error_not_a_blank() {
        let dir = temp_project("corrupt");
        let paths = Paths::new(&dir);
        std::fs::create_dir_all(&paths.root).unwrap();
        std::fs::write(paths.state(), "{ not json").unwrap();
        assert!(State::load(&paths).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn paths_live_under_dot_ncompose() {
        let paths = Paths::new(Path::new("/proj"));
        assert_eq!(paths.log("api"), PathBuf::from("/proj/.ncompose/logs/api.log"));
        assert_eq!(paths.state(), PathBuf::from("/proj/.ncompose/state.json"));
    }

    #[test]
    fn a_reused_pid_is_not_the_same_process() {
        let me = std::process::id() as i32;
        let start = process_start(me).expect("our own start time");
        assert!(same_process(me, Some(start)));
        assert!(!same_process(me, Some(start - 1)), "same PID, different start: another process");
        assert!(same_process(me, None), "older state files: liveness only");
        assert_eq!(process_start(0), None);

        let mut child = std::process::Command::new("/bin/sleep").arg("5").spawn().unwrap();
        let pid = child.id() as i32;
        let st = ServiceState { pid: Some(pid), pid_start: process_start(pid), ..Default::default() };
        assert_eq!(st.live_pid(), Some(pid));
        let reused = ServiceState { pid_start: st.pid_start.map(|s| s + 1), ..st.clone() };
        assert_eq!(reused.live_pid(), None);
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(st.live_pid(), None, "gone");
    }

    #[test]
    fn state_files_from_older_versions_still_load() {
        let old = r#"{"project":"p","supervisor_pid":1,"detached":true,"aliases_added":[],
            "services":{"api":{"ip":null,"pid":5,"status":"running","restarts":0,"ports":[],"last_exit":null}}}"#;
        let st: State = serde_json::from_str(old).unwrap();
        assert_eq!(st.supervisor_start, None);
        assert!(!st.remove_aliases);
        assert_eq!(st.services["api"].pid_start, None);
    }

    #[test]
    fn restart_requests_are_taken_once_and_never_mean_all_by_accident() {
        let dir = temp_project("restart");
        let paths = Paths::new(&dir);
        assert_eq!(take_restart_requests(&paths).unwrap(), None, "no file: nothing to do");
        append_restart_request(&paths, "api").unwrap();
        append_restart_request(&paths, "db worker").unwrap();
        assert_eq!(take_restart_requests(&paths).unwrap(), Some(vec!["api".into(), "db".into(), "worker".into()]));
        // A second signal for requests already taken finds an empty queue, not "restart all".
        assert_eq!(take_restart_requests(&paths).unwrap(), None);
        append_restart_request(&paths, "api").unwrap();
        append_restart_request(&paths, "*").unwrap();
        assert_eq!(take_restart_requests(&paths).unwrap(), Some(vec![]), "all");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn pid_liveness() {
        assert!(pid_alive(std::process::id() as i32));
        assert!(!pid_alive(0), "0 would mean our own process group");
        assert!(!pid_alive(-1), "negative pids are groups, never a service");
    }
}
