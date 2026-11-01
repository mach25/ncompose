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
    pub detached: bool,
    /// Aliases this project created (removed again on `down`).
    pub aliases_added: Vec<Ipv4Addr>,
    pub services: BTreeMap<String, ServiceState>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ServiceState {
    pub ip: Option<Ipv4Addr>,
    pub pid: Option<i32>,
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
}

pub fn pid_alive(pid: i32) -> bool {
    pid > 0 && unsafe { libc::kill(pid, 0) } == 0
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
    fn pid_liveness() {
        assert!(pid_alive(std::process::id() as i32));
        assert!(!pid_alive(0), "0 would mean our own process group");
        assert!(!pid_alive(-1), "negative pids are groups, never a service");
    }
}
