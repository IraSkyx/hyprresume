use std::fs;

const KNOWN_SHELLS: &[&str] = &[
    "bash", "zsh", "fish", "sh", "dash", "ksh", "tcsh", "csh", "nu", "elvish", "ion", "xonsh",
];

/// Resolve the working directory of a child shell process.
///
/// Walks `/proc/<pid>/task/*/children` to find direct child processes,
/// checks if any is a known shell, and reads its CWD via `/proc/<child>/cwd`.
pub fn resolve_shell_cwd(pid: i64) -> Option<String> {
    if pid <= 0 {
        return None;
    }

    for child_pid in child_pids(pid) {
        if let Some(cwd) = try_read_shell_cwd(child_pid) {
            tracing::debug!("pid {pid} → child shell {child_pid} → cwd {cwd}");
            return Some(cwd);
        }

        // Check grandchildren (e.g. terminal → shell wrapper → actual shell)
        for grandchild in child_pids(child_pid) {
            if let Some(cwd) = try_read_shell_cwd(grandchild) {
                tracing::debug!("pid {pid} → grandchild shell {grandchild} → cwd {cwd}");
                return Some(cwd);
            }
        }
    }

    None
}

fn try_read_shell_cwd(pid: i64) -> Option<String> {
    if !is_shell(pid) {
        return None;
    }
    let cwd = fs::read_link(format!("/proc/{pid}/cwd")).ok()?;
    let cwd_str = cwd.to_string_lossy().to_string();
    if cwd_str.is_empty() || cwd_str == "/" {
        return None;
    }
    Some(cwd_str)
}

fn child_pids(pid: i64) -> Vec<i64> {
    let tasks_dir = format!("/proc/{pid}/task");
    let Ok(tasks) = fs::read_dir(&tasks_dir) else {
        return Vec::new();
    };

    let mut children = Vec::new();
    for task in tasks.flatten() {
        let children_path = task.path().join("children");
        if let Ok(content) = fs::read_to_string(&children_path) {
            for token in content.split_whitespace() {
                if let Ok(child) = token.parse::<i64>() {
                    children.push(child);
                }
            }
        }
    }
    children
}

fn is_shell(pid: i64) -> bool {
    comm_name(pid).is_some_and(|name| KNOWN_SHELLS.contains(&name.as_str()))
}

fn comm_name(pid: i64) -> Option<String> {
    fs::read_to_string(format!("/proc/{pid}/comm"))
        .ok()
        .map(|s| s.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_process_has_no_shell_child() {
        let pid = i64::from(std::process::id());
        assert!(resolve_shell_cwd(pid).is_none());
    }

    #[test]
    fn negative_pid_returns_none() {
        assert!(resolve_shell_cwd(-1).is_none());
    }

    #[test]
    fn zero_pid_returns_none() {
        assert!(resolve_shell_cwd(0).is_none());
    }

    #[test]
    fn nonexistent_pid_returns_none() {
        assert!(resolve_shell_cwd(999_999_999).is_none());
    }

    #[test]
    fn child_pids_of_init() {
        drop(child_pids(1));
    }

    #[test]
    fn known_shells_list_is_nonempty() {
        assert!(!KNOWN_SHELLS.is_empty());
    }

    #[test]
    fn comm_name_of_self() {
        let pid = i64::from(std::process::id());
        let name = comm_name(pid);
        assert!(name.is_some());
    }
}
