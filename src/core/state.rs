use std::collections::HashMap;

use regex::Regex;

use crate::config::Config;
use crate::models::TrackedWindow;

pub struct StateManager {
    windows: HashMap<String, TrackedWindow>,
    exclude_patterns: Vec<Regex>,
    include_patterns: Vec<Regex>,
}

fn normalize_address(addr: &str) -> String {
    addr.trim_start_matches("0x").to_lowercase()
}

impl StateManager {
    pub fn new(config: &Config) -> Self {
        let exclude_patterns = config
            .rules
            .exclude
            .iter()
            .filter_map(|p| match Regex::new(p) {
                Ok(r) => Some(r),
                Err(e) => {
                    tracing::warn!("invalid exclude pattern '{p}': {e}");
                    None
                }
            })
            .collect();

        let include_patterns = config
            .rules
            .include
            .iter()
            .filter_map(|p| match Regex::new(p) {
                Ok(r) => Some(r),
                Err(e) => {
                    tracing::warn!("invalid include pattern '{p}': {e}");
                    None
                }
            })
            .collect();

        Self {
            windows: HashMap::new(),
            exclude_patterns,
            include_patterns,
        }
    }

    pub fn should_track(&self, class: &str) -> bool {
        if class.is_empty() {
            return false;
        }

        for pattern in &self.exclude_patterns {
            if pattern.is_match(class) {
                return false;
            }
        }

        if !self.include_patterns.is_empty() {
            return self.include_patterns.iter().any(|p| p.is_match(class));
        }

        true
    }

    pub fn add(&mut self, window: TrackedWindow) {
        tracing::debug!(
            "tracking: {} ({}) on workspace {}",
            window.app_id,
            window.address,
            window.workspace
        );
        let key = normalize_address(&window.address);
        self.windows.insert(key, window);
    }

    pub fn remove(&mut self, address: &str) -> Option<TrackedWindow> {
        let key = normalize_address(address);
        let w = self.windows.remove(&key);
        if let Some(ref w) = w {
            tracing::debug!("untracked: {} ({})", w.app_id, address);
        }
        w
    }

    pub fn update_workspace(&mut self, address: &str, workspace: &str) {
        let key = normalize_address(address);
        if let Some(w) = self.windows.get_mut(&key) {
            w.workspace = workspace.to_string();
            tracing::debug!("updated workspace: {} → {workspace}", w.app_id);
        }
    }

    pub fn update_floating(&mut self, address: &str, floating: bool) {
        let key = normalize_address(address);
        if let Some(w) = self.windows.get_mut(&key) {
            w.floating = floating;
        }
    }

    pub fn windows(&self) -> Vec<&TrackedWindow> {
        self.windows.values().collect()
    }

    pub fn window_count(&self) -> usize {
        self.windows.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, GeneralConfig, RulesConfig};
    use std::collections::HashMap;

    fn test_config(exclude: Vec<&str>, include: Vec<&str>) -> Config {
        Config {
            general: GeneralConfig {
                save_interval: 60,
                session_dir: "/tmp/hyprresume-test".into(),
                restore_on_start: false,
                per_window_launch: false,
                restore_geometry: false,
            },
            rules: RulesConfig {
                exclude: exclude.into_iter().map(String::from).collect(),
                include: include.into_iter().map(String::from).collect(),
            },
            overrides: HashMap::new(),
            experimental: crate::config::ExperimentalConfig::default(),
        }
    }

    fn make_window(address: &str, app_id: &str, workspace: &str) -> TrackedWindow {
        TrackedWindow {
            address: address.to_string(),
            app_id: app_id.to_string(),
            launch_cmd: format!("{app_id}-cmd"),
            workspace: workspace.to_string(),
            position: (0, 0),
            size: (800, 600),
            floating: false,
            fullscreen: false,
        }
    }

    // --- should_track ---

    #[test]
    fn track_empty_class_returns_false() {
        let state = StateManager::new(&test_config(vec![], vec![]));
        assert!(!state.should_track(""));
    }

    #[test]
    fn track_normal_class() {
        let state = StateManager::new(&test_config(vec![], vec![]));
        assert!(state.should_track("firefox"));
    }

    #[test]
    fn track_excluded_exact() {
        let state = StateManager::new(&test_config(vec![r"^firefox$"], vec![]));
        assert!(!state.should_track("firefox"));
        assert!(state.should_track("firefox-nightly"));
    }

    #[test]
    fn track_excluded_prefix() {
        let state = StateManager::new(&test_config(vec![r"^xdg-desktop-portal.*"], vec![]));
        assert!(!state.should_track("xdg-desktop-portal-gtk"));
        assert!(!state.should_track("xdg-desktop-portal-hyprland"));
        assert!(state.should_track("firefox"));
    }

    #[test]
    fn track_include_allowlist() {
        let state = StateManager::new(&test_config(vec![], vec![r"^firefox$", r"^code$"]));
        assert!(state.should_track("firefox"));
        assert!(state.should_track("code"));
        assert!(!state.should_track("discord"));
    }

    #[test]
    fn track_exclude_takes_precedence_over_include() {
        let state = StateManager::new(&test_config(
            vec![r"^firefox$"],
            vec![r"^firefox$", r"^code$"],
        ));
        assert!(!state.should_track("firefox"));
        assert!(state.should_track("code"));
    }

    #[test]
    fn track_default_excludes() {
        let state = StateManager::new(&Config::default());
        assert!(!state.should_track("xdg-desktop-portal-gtk"));
        assert!(!state.should_track("org.kde.polkitagent"));
        assert!(state.should_track("firefox"));
    }

    #[test]
    fn track_invalid_regex_ignored() {
        let state = StateManager::new(&test_config(vec![r"[invalid"], vec![]));
        assert!(state.should_track("firefox"));
    }

    // --- add / remove ---

    #[test]
    fn add_and_count() {
        let mut state = StateManager::new(&test_config(vec![], vec![]));
        assert_eq!(state.window_count(), 0);

        state.add(make_window("0xabc", "firefox", "1"));
        assert_eq!(state.window_count(), 1);

        state.add(make_window("0xdef", "code", "2"));
        assert_eq!(state.window_count(), 2);
    }

    #[test]
    fn add_same_address_overwrites() {
        let mut state = StateManager::new(&test_config(vec![], vec![]));

        state.add(make_window("0xabc", "firefox", "1"));
        state.add(make_window("0xabc", "firefox", "3"));
        assert_eq!(state.window_count(), 1);

        let windows = state.windows();
        assert_eq!(windows[0].workspace, "3");
    }

    #[test]
    fn remove_existing() {
        let mut state = StateManager::new(&test_config(vec![], vec![]));
        state.add(make_window("0xabc", "firefox", "1"));

        let removed = state.remove("0xabc");
        assert!(removed.is_some());
        assert_eq!(removed.unwrap().app_id, "firefox");
        assert_eq!(state.window_count(), 0);
    }

    #[test]
    fn remove_nonexistent() {
        let mut state = StateManager::new(&test_config(vec![], vec![]));
        let removed = state.remove("0xnonexistent");
        assert!(removed.is_none());
    }

    #[test]
    fn remove_normalizes_0x_prefix() {
        let mut state = StateManager::new(&test_config(vec![], vec![]));
        state.add(make_window("0xabc", "firefox", "1"));

        let removed = state.remove("abc");
        assert!(removed.is_some());
    }

    #[test]
    fn remove_address_stored_without_prefix() {
        let mut state = StateManager::new(&test_config(vec![], vec![]));
        state.add(make_window("abc", "firefox", "1"));

        let removed = state.remove("0xabc");
        assert!(removed.is_some());
    }

    // --- update_workspace ---

    #[test]
    fn update_workspace_existing() {
        let mut state = StateManager::new(&test_config(vec![], vec![]));
        state.add(make_window("0xabc", "firefox", "1"));

        state.update_workspace("0xabc", "5");

        let windows = state.windows();
        assert_eq!(windows[0].workspace, "5");
    }

    #[test]
    fn update_workspace_nonexistent_is_noop() {
        let mut state = StateManager::new(&test_config(vec![], vec![]));
        state.add(make_window("0xabc", "firefox", "1"));
        state.update_workspace("0xnonexistent", "5");

        let windows = state.windows();
        assert_eq!(windows[0].workspace, "1");
    }

    // --- update_floating ---

    #[test]
    fn update_floating_toggle() {
        let mut state = StateManager::new(&test_config(vec![], vec![]));
        state.add(make_window("0xabc", "firefox", "1"));

        assert!(!state.windows()[0].floating);

        state.update_floating("0xabc", true);
        assert!(state.windows()[0].floating);

        state.update_floating("0xabc", false);
        assert!(!state.windows()[0].floating);
    }

    // --- windows() ---

    #[test]
    fn windows_returns_all() {
        let mut state = StateManager::new(&test_config(vec![], vec![]));
        state.add(make_window("0xa", "firefox", "1"));
        state.add(make_window("0xb", "code", "2"));

        let windows = state.windows();
        assert_eq!(windows.len(), 2);

        let app_ids: Vec<&str> = windows.iter().map(|w| w.app_id.as_str()).collect();
        assert!(app_ids.contains(&"firefox"));
        assert!(app_ids.contains(&"code"));
    }
}
