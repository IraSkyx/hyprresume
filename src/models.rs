use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackedWindow {
    pub address: String,
    pub app_id: String,
    pub launch_cmd: String,
    pub workspace: String,
    #[serde(default)]
    pub position: (i32, i32),
    #[serde(default)]
    pub size: (i32, i32),
    #[serde(default)]
    pub floating: bool,
    #[serde(default)]
    pub fullscreen: bool,
    pub pid: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SessionFile {
    pub session: SessionMeta,
    #[serde(default, rename = "window")]
    pub windows: Vec<WindowEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SessionMeta {
    pub name: String,
    pub timestamp: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowEntry {
    pub app_id: String,
    pub launch_cmd: String,
    pub workspace: String,
    #[serde(default)]
    pub floating: bool,
    #[serde(default)]
    pub fullscreen: bool,
    #[serde(default)]
    pub position: Option<(i32, i32)>,
    #[serde(default)]
    pub size: Option<(i32, i32)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

/// Raw client data from `hyprctl clients -j`.
#[derive(Debug, Clone, Deserialize)]
pub struct HyprClient {
    pub address: String,
    pub class: String,
    pub pid: i64,
    pub workspace: HyprWorkspace,
    pub at: (i32, i32),
    pub size: (i32, i32),
    #[serde(default)]
    pub floating: bool,
    #[serde(default, rename = "fullscreen")]
    pub fullscreen_mode: i32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HyprWorkspace {
    pub name: String,
}

/// Resolved desktop entry info from XDG `.desktop` files.
#[derive(Debug, Clone)]
pub struct DesktopEntry {
    pub exec: String,
    pub startup_wm_class: Option<String>,
    pub desktop_id: String,
}

/// Events from Hyprland socket2.
#[derive(Debug, Clone)]
pub enum HyprEvent {
    OpenWindow {
        address: String,
        workspace: String,
        class: String,
    },
    CloseWindow {
        address: String,
    },
    MoveWindow {
        address: String,
        workspace: String,
    },
    ChangeFloatingMode {
        address: String,
        floating: bool,
    },
    Fullscreen {
        state: bool,
    },
    Unknown(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_file_roundtrip() {
        let session = SessionFile {
            session: SessionMeta {
                name: "test".to_string(),
                timestamp: 1_700_000_000,
            },
            windows: vec![
                WindowEntry {
                    app_id: "firefox".to_string(),
                    launch_cmd: "firefox".to_string(),
                    workspace: "1".to_string(),
                    floating: false,
                    fullscreen: false,
                    position: None,
                    size: None,
                    cwd: None,
                },
                WindowEntry {
                    app_id: "nautilus".to_string(),
                    launch_cmd: "nautilus".to_string(),
                    workspace: "2".to_string(),
                    floating: true,
                    fullscreen: false,
                    position: Some((200, 150)),
                    size: Some((900, 600)),
                    cwd: None,
                },
            ],
        };

        let toml_str = toml::to_string_pretty(&session).unwrap();
        let deserialized: SessionFile = toml::from_str(&toml_str).unwrap();

        assert_eq!(deserialized.session.name, "test");
        assert_eq!(deserialized.session.timestamp, 1_700_000_000);
        assert_eq!(deserialized.windows.len(), 2);

        assert_eq!(deserialized.windows[0].app_id, "firefox");
        assert!(!deserialized.windows[0].floating);
        assert!(deserialized.windows[0].position.is_none());

        assert_eq!(deserialized.windows[1].app_id, "nautilus");
        assert!(deserialized.windows[1].floating);
        assert_eq!(deserialized.windows[1].position, Some((200, 150)));
        assert_eq!(deserialized.windows[1].size, Some((900, 600)));
    }

    #[test]
    fn session_file_empty_windows() {
        let session = SessionFile {
            session: SessionMeta {
                name: "empty".to_string(),
                timestamp: 0,
            },
            windows: vec![],
        };

        let toml_str = toml::to_string_pretty(&session).unwrap();
        let deserialized: SessionFile = toml::from_str(&toml_str).unwrap();

        assert_eq!(deserialized.windows.len(), 0);
    }

    #[test]
    fn session_file_deserialize_minimal() {
        let toml_str = r#"
[session]
name = "min"
timestamp = 123

[[window]]
app_id = "firefox"
launch_cmd = "firefox"
workspace = "1"
"#;
        let session: SessionFile = toml::from_str(toml_str).unwrap();
        assert_eq!(session.windows.len(), 1);
        assert!(!session.windows[0].floating);
        assert!(!session.windows[0].fullscreen);
        assert!(session.windows[0].position.is_none());
    }

    #[test]
    fn hypr_client_deserialize() {
        let json = r#"{
            "address": "0x55bee6666ea0",
            "class": "firefox",
            "title": "Mozilla Firefox",
            "pid": 12345,
            "workspace": {"id": 1, "name": "1"},
            "at": [100, 200],
            "size": [1920, 1080],
            "floating": false,
            "fullscreen": 0,
            "initialClass": "firefox",
            "initialTitle": "Firefox"
        }"#;

        let client: HyprClient = serde_json::from_str(json).unwrap();
        assert_eq!(client.class, "firefox");
        assert_eq!(client.pid, 12345);
        assert_eq!(client.workspace.name, "1");
        assert_eq!(client.at, (100, 200));
        assert_eq!(client.size, (1920, 1080));
        assert!(!client.floating);
        assert_eq!(client.fullscreen_mode, 0);
    }

    #[test]
    fn hypr_client_deserialize_with_defaults() {
        let json = r#"{
            "address": "0xabc",
            "class": "myapp",
            "title": "",
            "pid": 1,
            "workspace": {"id": 2, "name": "2"},
            "at": [0, 0],
            "size": [800, 600]
        }"#;

        let client: HyprClient = serde_json::from_str(json).unwrap();
        assert!(!client.floating);
        assert_eq!(client.fullscreen_mode, 0);
    }

    #[test]
    fn hypr_clients_array_deserialize() {
        let json = r#"[
            {
                "address": "0x1",
                "class": "firefox",
                "title": "Tab 1",
                "pid": 100,
                "workspace": {"id": 1, "name": "1"},
                "at": [0, 0],
                "size": [800, 600]
            },
            {
                "address": "0x2",
                "class": "code",
                "title": "main.rs",
                "pid": 200,
                "workspace": {"id": 2, "name": "2"},
                "at": [100, 100],
                "size": [1200, 800],
                "floating": true,
                "fullscreen": 1
            }
        ]"#;

        let clients: Vec<HyprClient> = serde_json::from_str(json).unwrap();
        assert_eq!(clients.len(), 2);
        assert_eq!(clients[0].class, "firefox");
        assert!(!clients[0].floating);
        assert_eq!(clients[1].class, "code");
        assert!(clients[1].floating);
        assert_eq!(clients[1].fullscreen_mode, 1);
    }

    #[test]
    fn tracked_window_roundtrip() {
        let window = TrackedWindow {
            address: "0xabc".to_string(),
            app_id: "firefox".to_string(),
            launch_cmd: "firefox".to_string(),
            workspace: "1".to_string(),
            position: (100, 200),
            size: (1920, 1080),
            floating: true,
            fullscreen: false,
            pid: 12345,
        };

        let json = serde_json::to_string(&window).unwrap();
        let deserialized: TrackedWindow = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.address, "0xabc");
        assert_eq!(deserialized.position, (100, 200));
        assert!(deserialized.floating);
    }
}
