use crate::agent::Agent;
use std::fs;
use std::path::PathBuf;

/// Resolves the path to the on-disk store. Pure: it does not touch the
/// filesystem — `save` is responsible for creating the parent directory.
fn data_path() -> PathBuf {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::current_dir().unwrap_or_default());
    home.join(".warden").join("agents.json")
}

pub fn save(agents: &[Agent]) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let path = data_path();
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let json = serde_json::to_string_pretty(agents)?;
    fs::write(&path, json)?;
    Ok(path)
}

pub fn load() -> Result<Vec<Agent>, Box<dyn std::error::Error>> {
    let path = data_path();
    if !path.exists() {
        return Err("No data file found".into());
    }
    let content = fs::read_to_string(path)?;
    let agents: Vec<Agent> = serde_json::from_str(&content)?;
    Ok(agents)
}

/// Test-only helpers shared across the crate's test modules.
#[cfg(test)]
pub(crate) mod test_support {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Mutex, MutexGuard};

    static ENV_LOCK: Mutex<()> = Mutex::new(());
    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// RAII guard that repoints `$HOME` at a fresh temp dir for the duration of a
    /// test, then restores it and cleans up on drop. Holding the global lock
    /// serializes every disk-touching test so they don't race on the env var.
    pub struct TempHome {
        _guard: MutexGuard<'static, ()>,
        prev: Option<String>,
        dir: PathBuf,
    }

    impl TempHome {
        pub fn new() -> Self {
            let guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let n = COUNTER.fetch_add(1, Ordering::SeqCst);
            let dir =
                std::env::temp_dir().join(format!("warden-test-{}-{}", std::process::id(), n));
            std::fs::create_dir_all(&dir).unwrap();
            let prev = std::env::var("HOME").ok();
            std::env::set_var("HOME", &dir);
            TempHome {
                _guard: guard,
                prev,
                dir,
            }
        }
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::TempHome;
    use super::{data_path, load, save};
    use crate::agent::Agent;

    #[test]
    fn save_returns_path_and_writes_file() {
        let _h = TempHome::new();
        let agents = vec![Agent::new("n", "m", "t")];
        let path = save(&agents).unwrap();
        assert!(path.ends_with("agents.json"));
        assert!(path.exists());
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("\"name\""));
    }

    #[test]
    fn save_then_load_roundtrips() {
        let _h = TempHome::new();
        let agents = vec![
            Agent::new("alpha", "m", "t"),
            Agent::new("beta", "m2", "t2"),
        ];
        save(&agents).unwrap();
        let loaded = load().unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].name, "alpha");
        assert_eq!(loaded[1].model, "m2");
    }

    #[test]
    fn load_without_file_errors() {
        let _h = TempHome::new();
        assert!(load().is_err());
    }

    #[test]
    fn save_empty_slice_roundtrips() {
        let _h = TempHome::new();
        save(&[]).unwrap();
        assert!(load().unwrap().is_empty());
    }

    #[test]
    fn data_path_points_into_warden_dir_without_side_effects() {
        let _h = TempHome::new();
        let p = data_path();
        assert!(p.ends_with("agents.json"));
        assert!(p.parent().unwrap().ends_with(".warden"));
        // data_path is pure — resolving it must not create anything on disk.
        assert!(!p.parent().unwrap().exists());
    }

    #[test]
    fn save_creates_warden_dir() {
        let _h = TempHome::new();
        save(&[]).unwrap();
        assert!(data_path().parent().unwrap().is_dir());
    }
}
