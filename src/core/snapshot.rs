use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::core::state::StateManager;
use crate::models::{SessionFile, SessionMeta, TrackedWindow, WindowEntry};

pub struct SnapshotEngine {
    session_dir: PathBuf,
    per_window_launch: bool,
}

fn sanitize_session_name(name: &str) -> Result<&str> {
    if name.is_empty() {
        bail!("session name cannot be empty");
    }
    if name.contains('/')
        || name.contains('\\')
        || name.contains('\0')
        || name.starts_with('.')
        || name.contains("..")
    {
        bail!("invalid session name '{name}': must not contain path separators or start with '.'");
    }
    Ok(name)
}

fn window_to_entry(w: &TrackedWindow) -> WindowEntry {
    WindowEntry {
        app_id: w.app_id.clone(),
        launch_cmd: w.launch_cmd.clone(),
        workspace: w.workspace.clone(),
        floating: w.floating,
        fullscreen: w.fullscreen,
        position: if w.floating { Some(w.position) } else { None },
        size: if w.floating { Some(w.size) } else { None },
    }
}

impl SnapshotEngine {
    pub fn new(config: &Config) -> Result<Self> {
        let session_dir = config.session_dir();
        std::fs::create_dir_all(&session_dir)
            .with_context(|| format!("creating session dir {}", session_dir.display()))?;

        Ok(Self {
            session_dir,
            per_window_launch: config.general.per_window_launch,
        })
    }

    pub fn save(&self, state: &StateManager, name: &str) -> Result<PathBuf> {
        let name = sanitize_session_name(name)?;
        let windows = state.windows();

        let entries: Vec<WindowEntry> = if self.per_window_launch {
            windows
                .iter()
                .filter(|w| !w.launch_cmd.is_empty())
                .map(|w| window_to_entry(w))
                .collect()
        } else {
            let mut seen = std::collections::HashSet::new();
            windows
                .iter()
                .filter(|w| !w.launch_cmd.is_empty() && seen.insert(w.app_id.clone()))
                .map(|w| window_to_entry(w))
                .collect()
        };

        let session_file = SessionFile {
            session: SessionMeta {
                name: name.to_string(),
                timestamp: chrono::Utc::now().timestamp(),
            },
            windows: entries,
        };

        let content =
            toml::to_string_pretty(&session_file).context("serializing session to TOML")?;

        let final_path = self.session_dir.join(format!("{name}.toml"));
        let tmp_path = self.session_dir.join(format!(".{name}.toml.tmp"));

        std::fs::write(&tmp_path, &content)
            .with_context(|| format!("writing tmp session file {}", tmp_path.display()))?;
        std::fs::rename(&tmp_path, &final_path)
            .with_context(|| format!("renaming session file to {}", final_path.display()))?;

        tracing::info!(
            "saved session '{name}' ({} apps) to {}",
            session_file.windows.len(),
            final_path.display()
        );

        Ok(final_path)
    }

    pub fn load(&self, name: &str) -> Result<SessionFile> {
        let name = sanitize_session_name(name)?;
        let path = self.session_dir.join(format!("{name}.toml"));
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("reading session file {}", path.display()))?;
        let session: SessionFile =
            toml::from_str(&content).with_context(|| format!("parsing {}", path.display()))?;
        Ok(session)
    }

    pub fn list(&self) -> Result<Vec<(String, i64)>> {
        let mut sessions = Vec::new();

        let entries = std::fs::read_dir(&self.session_dir)
            .with_context(|| format!("reading session dir {}", self.session_dir.display()))?;

        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "toml")
                && !path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .starts_with('.')
                && let Ok(content) = std::fs::read_to_string(&path)
                && let Ok(session) = toml::from_str::<SessionFile>(&content)
            {
                sessions.push((session.session.name, session.session.timestamp));
            }
        }

        sessions.sort_by(|a, b| b.1.cmp(&a.1));
        Ok(sessions)
    }

    pub fn delete(&self, name: &str) -> Result<()> {
        let name = sanitize_session_name(name)?;
        let path = self.session_dir.join(format!("{name}.toml"));
        if path.exists() {
            std::fs::remove_file(&path)
                .with_context(|| format!("deleting session {}", path.display()))?;
            tracing::info!("deleted session '{name}'");
        } else {
            tracing::warn!("session '{name}' not found at {}", path.display());
        }
        Ok(())
    }

    pub fn exists(&self, name: &str) -> bool {
        sanitize_session_name(name).is_ok()
            && self.session_dir.join(format!("{name}.toml")).exists()
    }

    pub fn session_dir(&self) -> &Path {
        &self.session_dir
    }

    #[cfg(test)]
    pub(crate) fn new_with_dir(session_dir: PathBuf, per_window_launch: bool) -> Result<Self> {
        std::fs::create_dir_all(&session_dir)
            .with_context(|| format!("creating session dir {}", session_dir.display()))?;
        Ok(Self {
            session_dir,
            per_window_launch,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn make_state_with_windows(windows: Vec<(&str, &str, &str, bool)>) -> StateManager {
        let mut config = Config::default();
        config.general.session_dir = "/tmp/unused".to_string();
        let mut state = StateManager::new(&config);

        for (addr, app_id, workspace, floating) in windows {
            state.add(TrackedWindow {
                address: addr.to_string(),
                app_id: app_id.to_string(),
                launch_cmd: format!("{app_id}-cmd"),
                workspace: workspace.to_string(),
                position: (100, 200),
                size: (800, 600),
                floating,
                fullscreen: false,
            });
        }

        state
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let engine = SnapshotEngine::new_with_dir(dir.path().to_path_buf(), false).unwrap();

        let state = make_state_with_windows(vec![
            ("0xa", "firefox", "1", false),
            ("0xb", "code", "2", false),
        ]);

        let path = engine.save(&state, "test").unwrap();
        assert!(path.exists());

        let loaded = engine.load("test").unwrap();
        assert_eq!(loaded.session.name, "test");
        assert!(loaded.session.timestamp > 0);
        assert_eq!(loaded.windows.len(), 2);
    }

    #[test]
    fn save_deduplicates_by_app_id() {
        let dir = tempfile::tempdir().unwrap();
        let engine = SnapshotEngine::new_with_dir(dir.path().to_path_buf(), false).unwrap();

        let state = make_state_with_windows(vec![
            ("0xa", "firefox", "1", false),
            ("0xb", "firefox", "2", false),
            ("0xc", "code", "3", false),
        ]);

        engine.save(&state, "dedup").unwrap();
        let loaded = engine.load("dedup").unwrap();

        assert_eq!(loaded.windows.len(), 2);
        let app_ids: Vec<&str> = loaded.windows.iter().map(|w| w.app_id.as_str()).collect();
        assert!(app_ids.contains(&"firefox"));
        assert!(app_ids.contains(&"code"));
    }

    #[test]
    fn save_per_window_launch_keeps_all() {
        let dir = tempfile::tempdir().unwrap();
        let engine = SnapshotEngine::new_with_dir(dir.path().to_path_buf(), true).unwrap();

        let state = make_state_with_windows(vec![
            ("0xa", "firefox", "1", false),
            ("0xb", "firefox", "2", false),
        ]);

        engine.save(&state, "perwin").unwrap();
        let loaded = engine.load("perwin").unwrap();
        assert_eq!(loaded.windows.len(), 2);
    }

    #[test]
    fn save_skips_empty_launch_cmd() {
        let dir = tempfile::tempdir().unwrap();
        let engine = SnapshotEngine::new_with_dir(dir.path().to_path_buf(), false).unwrap();

        let mut config = Config::default();
        config.general.session_dir = "/tmp/unused".to_string();
        let mut state = StateManager::new(&config);
        state.add(TrackedWindow {
            address: "0xa".to_string(),
            app_id: "unknown".to_string(),
            launch_cmd: String::new(),
            workspace: "1".to_string(),
            position: (0, 0),
            size: (0, 0),
            floating: false,
            fullscreen: false,
        });

        engine.save(&state, "empty").unwrap();
        let loaded = engine.load("empty").unwrap();
        assert_eq!(loaded.windows.len(), 0);
    }

    #[test]
    fn save_floating_includes_geometry() {
        let dir = tempfile::tempdir().unwrap();
        let engine = SnapshotEngine::new_with_dir(dir.path().to_path_buf(), false).unwrap();

        let state = make_state_with_windows(vec![("0xa", "nautilus", "1", true)]);

        engine.save(&state, "float").unwrap();
        let loaded = engine.load("float").unwrap();

        let win = &loaded.windows[0];
        assert!(win.floating);
        assert_eq!(win.position, Some((100, 200)));
        assert_eq!(win.size, Some((800, 600)));
    }

    #[test]
    fn save_non_floating_excludes_geometry() {
        let dir = tempfile::tempdir().unwrap();
        let engine = SnapshotEngine::new_with_dir(dir.path().to_path_buf(), false).unwrap();

        let state = make_state_with_windows(vec![("0xa", "firefox", "1", false)]);

        engine.save(&state, "nonfloat").unwrap();
        let loaded = engine.load("nonfloat").unwrap();

        let win = &loaded.windows[0];
        assert!(!win.floating);
        assert!(win.position.is_none());
        assert!(win.size.is_none());
    }

    #[test]
    fn exists_true_after_save() {
        let dir = tempfile::tempdir().unwrap();
        let engine = SnapshotEngine::new_with_dir(dir.path().to_path_buf(), false).unwrap();

        assert!(!engine.exists("test"));

        let state = make_state_with_windows(vec![("0xa", "firefox", "1", false)]);
        engine.save(&state, "test").unwrap();

        assert!(engine.exists("test"));
    }

    #[test]
    fn delete_removes_session() {
        let dir = tempfile::tempdir().unwrap();
        let engine = SnapshotEngine::new_with_dir(dir.path().to_path_buf(), false).unwrap();

        let state = make_state_with_windows(vec![("0xa", "firefox", "1", false)]);
        engine.save(&state, "deleteme").unwrap();

        assert!(engine.exists("deleteme"));
        engine.delete("deleteme").unwrap();
        assert!(!engine.exists("deleteme"));
    }

    #[test]
    fn delete_nonexistent_no_error() {
        let dir = tempfile::tempdir().unwrap();
        let engine = SnapshotEngine::new_with_dir(dir.path().to_path_buf(), false).unwrap();
        engine.delete("nonexistent").unwrap();
    }

    #[test]
    fn list_returns_sessions_sorted_by_timestamp() {
        let dir = tempfile::tempdir().unwrap();
        let engine = SnapshotEngine::new_with_dir(dir.path().to_path_buf(), false).unwrap();

        std::fs::write(
            dir.path().join("first.toml"),
            "[session]\nname = \"first\"\ntimestamp = 1000\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("second.toml"),
            "[session]\nname = \"second\"\ntimestamp = 2000\n",
        )
        .unwrap();

        let sessions = engine.list().unwrap();
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].0, "second");
        assert_eq!(sessions[1].0, "first");
    }

    #[test]
    fn list_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let engine = SnapshotEngine::new_with_dir(dir.path().to_path_buf(), false).unwrap();

        let sessions = engine.list().unwrap();
        assert!(sessions.is_empty());
    }

    #[test]
    fn list_ignores_hidden_files() {
        let dir = tempfile::tempdir().unwrap();
        let engine = SnapshotEngine::new_with_dir(dir.path().to_path_buf(), false).unwrap();

        let state = make_state_with_windows(vec![("0xa", "firefox", "1", false)]);
        engine.save(&state, "visible").unwrap();

        std::fs::write(dir.path().join(".interrupted.toml.tmp"), "garbage").unwrap();

        let sessions = engine.list().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].0, "visible");
    }

    #[test]
    fn load_nonexistent_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let engine = SnapshotEngine::new_with_dir(dir.path().to_path_buf(), false).unwrap();

        let result = engine.load("nonexistent");
        assert!(result.is_err());
    }

    #[test]
    fn save_is_atomic() {
        let dir = tempfile::tempdir().unwrap();
        let engine = SnapshotEngine::new_with_dir(dir.path().to_path_buf(), false).unwrap();

        let state = make_state_with_windows(vec![("0xa", "firefox", "1", false)]);
        engine.save(&state, "atomic").unwrap();

        let tmp_path = dir.path().join(".atomic.toml.tmp");
        assert!(!tmp_path.exists());

        let final_path = dir.path().join("atomic.toml");
        assert!(final_path.exists());
    }

    #[test]
    fn rejects_path_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let engine = SnapshotEngine::new_with_dir(dir.path().to_path_buf(), false).unwrap();

        let state = make_state_with_windows(vec![("0xa", "firefox", "1", false)]);

        assert!(engine.save(&state, "../escape").is_err());
        assert!(engine.save(&state, "../../etc/cron.d/pwned").is_err());
        assert!(engine.save(&state, ".hidden").is_err());
        assert!(engine.save(&state, "foo/bar").is_err());
        assert!(engine.save(&state, "").is_err());
        assert!(engine.load("../etc/passwd").is_err());
        assert!(engine.delete("../../nope").is_err());
        assert!(!engine.exists("../escape"));
    }
}
