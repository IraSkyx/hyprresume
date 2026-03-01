/// Detect if a process is running inside a Flatpak sandbox by inspecting its cgroup.
/// Returns the Flatpak application ID if found.
pub fn detect_flatpak_app(pid: i64) -> Option<String> {
    let cgroup_path = format!("/proc/{pid}/cgroup");
    let content = std::fs::read_to_string(cgroup_path).ok()?;

    // Flatpak cgroup entries look like:
    // 0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-flatpak-org.mozilla.firefox-12345.scope
    for line in content.lines() {
        if let Some(app_id) = extract_flatpak_id(line) {
            return Some(app_id);
        }
    }

    // Also check the flatpak-info file which is mounted inside the sandbox
    let info_path = format!("/proc/{pid}/root/.flatpak-info");
    if let Ok(info) = std::fs::read_to_string(info_path) {
        for line in info.lines() {
            if let Some(name) = line.strip_prefix("name=") {
                let name = name.trim();
                if !name.is_empty() {
                    return Some(name.to_string());
                }
            }
        }
    }

    None
}

pub fn extract_flatpak_id(cgroup_line: &str) -> Option<String> {
    // Look for patterns like "app-flatpak-org.mozilla.firefox-12345.scope"
    let line = cgroup_line.split("::").last().unwrap_or(cgroup_line);

    let scope = line.rsplit('/').next()?;
    let stripped = scope.strip_prefix("app-flatpak-")?;
    let stripped = stripped.strip_suffix(".scope")?;

    // Remove the trailing PID: "org.mozilla.firefox-12345" → "org.mozilla.firefox"
    if let Some(last_dash) = stripped.rfind('-') {
        let maybe_pid = &stripped[last_dash + 1..];
        if maybe_pid.chars().all(|c| c.is_ascii_digit()) {
            return Some(stripped[..last_dash].to_string());
        }
    }

    Some(stripped.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_standard_firefox() {
        assert_eq!(
            extract_flatpak_id(
                "0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-flatpak-org.mozilla.firefox-12345.scope"
            ),
            Some("org.mozilla.firefox".to_string())
        );
    }

    #[test]
    fn extract_zen_browser() {
        assert_eq!(
            extract_flatpak_id(
                "0::/user.slice/app.slice/app-flatpak-app.zen_browser.zen-9999.scope"
            ),
            Some("app.zen_browser.zen".to_string())
        );
    }

    #[test]
    fn extract_no_flatpak_prefix() {
        assert_eq!(extract_flatpak_id("0::/init.scope"), None);
    }

    #[test]
    fn extract_non_flatpak_app_scope() {
        assert_eq!(
            extract_flatpak_id(
                "0::/user.slice/user-1000.slice/app.slice/app-gnome-firefox-1234.scope"
            ),
            None
        );
    }

    #[test]
    fn extract_no_trailing_pid() {
        // If there's no numeric suffix after the last dash, return the whole string
        assert_eq!(
            extract_flatpak_id("0::/app.slice/app-flatpak-com.example.app.scope"),
            Some("com.example.app".to_string())
        );
    }

    #[test]
    fn extract_with_underscores_in_id() {
        assert_eq!(
            extract_flatpak_id("0::/app.slice/app-flatpak-com.some_vendor.my_app-42.scope"),
            Some("com.some_vendor.my_app".to_string())
        );
    }

    #[test]
    fn extract_large_pid() {
        assert_eq!(
            extract_flatpak_id("0::/app.slice/app-flatpak-org.freedesktop.Platform-999999.scope"),
            Some("org.freedesktop.Platform".to_string())
        );
    }

    #[test]
    fn extract_empty_line() {
        assert_eq!(extract_flatpak_id(""), None);
    }

    #[test]
    fn extract_just_scope_no_path() {
        assert_eq!(
            extract_flatpak_id("app-flatpak-org.test.App-1.scope"),
            Some("org.test.App".to_string())
        );
    }

    #[test]
    fn extract_multiple_double_colons() {
        assert_eq!(
            extract_flatpak_id(
                "12:blkio:/user.slice::0::/user.slice/app.slice/app-flatpak-org.test.App-1.scope"
            ),
            Some("org.test.App".to_string())
        );
    }
}
