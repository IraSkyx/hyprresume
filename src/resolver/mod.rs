pub mod cwd;
pub mod desktop;
pub mod flatpak;
pub mod proc;
pub mod profile;

use std::collections::HashMap;
use std::sync::Mutex;

use regex::Regex;

use crate::config::Config;
use crate::resolver::desktop::DesktopIndex;

struct CompiledOverride {
    pattern: Regex,
    command: String,
}

pub struct AppResolver {
    desktop_index: DesktopIndex,
    cache: Mutex<HashMap<String, Option<String>>>,
    exact_overrides: HashMap<String, String>,
    pattern_overrides: Vec<CompiledOverride>,
}

impl AppResolver {
    pub fn new(config: &Config) -> Self {
        let desktop_index = DesktopIndex::build();
        tracing::info!("indexed {} .desktop entries", desktop_index.len());

        let mut exact_overrides = HashMap::new();
        let mut pattern_overrides = Vec::new();

        for (key, cmd) in &config.overrides {
            if key.contains('*') || key.contains('?') {
                let regex_str = key.replace('*', ".*").replace('?', ".");
                match Regex::new(&regex_str) {
                    Ok(re) => pattern_overrides.push(CompiledOverride {
                        pattern: re,
                        command: cmd.clone(),
                    }),
                    Err(e) => tracing::warn!("invalid override pattern '{key}': {e}"),
                }
            } else {
                exact_overrides.insert(key.clone(), cmd.clone());
            }
        }

        Self {
            desktop_index,
            cache: Mutex::new(HashMap::new()),
            exact_overrides,
            pattern_overrides,
        }
    }

    /// Resolve a window class + pid to a launch command.
    /// Returns `None` if the app cannot be resolved.
    pub fn resolve(&self, class: &str, pid: i64) -> Option<String> {
        if class.is_empty() {
            return None;
        }

        {
            let cache = self.cache.lock().unwrap();
            if let Some(cached) = cache.get(class) {
                return cached.clone();
            }
        }

        let result = self.resolve_uncached(class, pid);

        {
            let mut cache = self.cache.lock().unwrap();
            cache.insert(class.to_string(), result.clone());
        }

        result
    }

    fn resolve_uncached(&self, class: &str, pid: i64) -> Option<String> {
        // 1. Exact overrides
        if let Some(cmd) = self.exact_overrides.get(class) {
            if cmd.is_empty() {
                tracing::debug!("{class}: skipped (empty override)");
                return None;
            }
            tracing::debug!("{class}: resolved via override → {cmd}");
            return Some(cmd.clone());
        }

        // 2. Pattern overrides (pre-compiled)
        for ov in &self.pattern_overrides {
            if ov.pattern.is_match(class) {
                if ov.command.is_empty() {
                    tracing::debug!("{class}: skipped (pattern override)");
                    return None;
                }
                tracing::debug!("{class}: resolved via pattern override → {}", ov.command);
                return Some(ov.command.clone());
            }
        }

        // 3. XDG .desktop file lookup
        if let Some(entry) = self.desktop_index.lookup(class) {
            tracing::debug!("{class}: resolved via .desktop → {}", entry.exec);
            return Some(entry.exec.clone());
        }

        // 4. Flatpak detection via cgroup
        if pid > 0 {
            if let Some(app_id) = flatpak::detect_flatpak_app(pid) {
                let cmd = format!("flatpak run {app_id}");
                tracing::debug!("{class}: resolved via flatpak cgroup → {cmd}");
                return Some(cmd);
            }

            // 5. /proc/pid/cmdline fallback
            if let Some(cmd) = proc::resolve_from_proc(pid) {
                tracing::debug!("{class}: resolved via /proc → {cmd}");
                return Some(cmd);
            }
        }

        tracing::warn!("{class}: could not resolve launch command");
        None
    }
}
