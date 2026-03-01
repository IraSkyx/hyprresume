use std::path::PathBuf;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;

/// A fake Hyprland socket1 server that responds to requests with canned JSON.
pub struct MockSocket1 {
    listener: UnixListener,
    clients_json: String,
}

impl MockSocket1 {
    pub fn new(path: &PathBuf, clients_json: String) -> Self {
        let listener = UnixListener::bind(path).expect("failed to bind mock socket1");
        Self {
            listener,
            clients_json,
        }
    }

    pub async fn serve(self) {
        loop {
            let Ok((stream, _)) = self.listener.accept().await else {
                break;
            };
            let json = self.clients_json.clone();
            tokio::spawn(async move {
                handle_socket1_request(stream, &json).await;
            });
        }
    }
}

async fn handle_socket1_request(mut stream: UnixStream, clients_json: &str) {
    let mut buf = String::new();
    let mut reader = BufReader::new(&mut stream);
    drop(reader.read_to_string(&mut buf).await);

    let response = if buf.contains("j/clients") {
        clients_json.to_string()
    } else if buf.contains("keyword") || buf.contains("dispatch") {
        "ok".to_string()
    } else {
        "unknown command".to_string()
    };

    drop(stream.write_all(response.as_bytes()).await);
}

/// A fake Hyprland socket2 that emits events.
pub struct MockSocket2;

impl MockSocket2 {
    pub async fn emit_events(path: PathBuf, events: Vec<String>) {
        let listener = UnixListener::bind(&path).expect("failed to bind mock socket2");

        let (mut stream, _) = listener.accept().await.expect("accept failed");

        for event in &events {
            let line = format!("{event}\n");
            if stream.write_all(line.as_bytes()).await.is_err() {
                break;
            }
        }

        drop(stream);
    }
}

pub fn fixture_clients_json() -> String {
    r#"[
    {
        "address": "0xaaa111",
        "class": "firefox",
        "title": "GitHub - Mozilla Firefox",
        "pid": 1001,
        "workspace": {"id": 1, "name": "1"},
        "at": [0, 0],
        "size": [1920, 1080],
        "floating": false,
        "fullscreen": 0,
        "initialClass": "firefox",
        "initialTitle": "Firefox"
    },
    {
        "address": "0xbbb222",
        "class": "com.mitchellh.ghostty",
        "title": "~",
        "pid": 1002,
        "workspace": {"id": 2, "name": "2"},
        "at": [0, 0],
        "size": [1920, 1080],
        "floating": false,
        "fullscreen": 0,
        "initialClass": "com.mitchellh.ghostty",
        "initialTitle": "ghostty"
    },
    {
        "address": "0xccc333",
        "class": "org.gnome.Nautilus",
        "title": "Home",
        "pid": 1003,
        "workspace": {"id": 1, "name": "1"},
        "at": [200, 150],
        "size": [900, 600],
        "floating": true,
        "fullscreen": 0,
        "initialClass": "org.gnome.Nautilus",
        "initialTitle": "Files"
    },
    {
        "address": "0xddd444",
        "class": "xdg-desktop-portal-gtk",
        "title": "",
        "pid": 1004,
        "workspace": {"id": -1, "name": "special"},
        "at": [0, 0],
        "size": [0, 0],
        "floating": false,
        "fullscreen": 0,
        "initialClass": "xdg-desktop-portal-gtk",
        "initialTitle": ""
    }
]"#
    .to_string()
}

pub fn fixture_events() -> Vec<String> {
    vec![
        "openwindow>>eee555,3,discord,Discord".to_string(),
        "movewindow>>eee555,4".to_string(),
        "changefloatingmode>>ccc333,0".to_string(),
        "closewindow>>ccc333".to_string(),
        "fullscreen>>1".to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::client::{HyprCtl, HyprSocketPaths};
    use crate::ipc::event_listener;
    use crate::models::HyprEvent;

    #[tokio::test]
    async fn mock_socket1_responds_to_clients() {
        let dir = tempfile::tempdir().unwrap();
        let sock1 = dir.path().join("socket1.sock");
        let sock2 = dir.path().join("socket2.sock");

        let json = fixture_clients_json();
        let mock = MockSocket1::new(&sock1, json);
        let server = tokio::spawn(mock.serve());

        let paths = HyprSocketPaths::new(sock1, sock2);
        let client = HyprCtl::new(paths);

        let clients = client.get_clients().await.unwrap();
        assert_eq!(clients.len(), 4);
        assert_eq!(clients[0].class, "firefox");
        assert_eq!(clients[1].class, "com.mitchellh.ghostty");
        assert_eq!(clients[2].class, "org.gnome.Nautilus");
        assert!(clients[2].floating);

        server.abort();
    }

    #[tokio::test]
    async fn mock_socket1_handles_dispatch() {
        let dir = tempfile::tempdir().unwrap();
        let sock1 = dir.path().join("socket1.sock");
        let sock2 = dir.path().join("socket2.sock");

        let mock = MockSocket1::new(&sock1, "[]".to_string());
        let server = tokio::spawn(mock.serve());

        let paths = HyprSocketPaths::new(sock1, sock2);
        let client = HyprCtl::new(paths);

        client.dispatch("exec firefox").await.unwrap();
        client
            .dispatch("movetoworkspacesilent 3,address:0xabc")
            .await
            .unwrap();
        client
            .dispatch("togglefloating address:0xabc")
            .await
            .unwrap();
        client
            .dispatch("resizewindowpixel exact 800 600,address:0xabc")
            .await
            .unwrap();
        client
            .dispatch("movewindowpixel exact 100 200,address:0xabc")
            .await
            .unwrap();

        server.abort();
    }

    #[tokio::test]
    async fn mock_socket2_emits_events() {
        let dir = tempfile::tempdir().unwrap();
        let sock2 = dir.path().join("socket2.sock");

        let events = fixture_events();
        let sock2_clone = sock2.clone();
        let emitter = tokio::spawn(MockSocket2::emit_events(sock2_clone, events));

        let (tx, mut rx) = mpsc::channel::<HyprEvent>(64);
        let sock2_for_listener = sock2.clone();
        let listener =
            tokio::spawn(async move { event_listener::listen_on(&sock2_for_listener, tx).await });

        let mut received = Vec::new();
        let timeout = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while let Some(event) = rx.recv().await {
                received.push(event);
            }
        });
        let _ = timeout.await;

        assert_eq!(received.len(), 5);

        assert!(matches!(&received[0], HyprEvent::OpenWindow { class, .. } if class == "discord"));
        assert!(
            matches!(&received[1], HyprEvent::MoveWindow { workspace, .. } if workspace == "4")
        );
        assert!(
            matches!(&received[2], HyprEvent::ChangeFloatingMode { floating, .. } if !floating)
        );
        assert!(matches!(&received[3], HyprEvent::CloseWindow { address } if address == "ccc333"));
        assert!(matches!(
            &received[4],
            HyprEvent::Fullscreen { state: true }
        ));

        emitter.abort();
        listener.abort();
    }

    #[tokio::test]
    async fn mock_socket1_get_client_by_address() {
        let dir = tempfile::tempdir().unwrap();
        let sock1 = dir.path().join("socket1.sock");
        let sock2 = dir.path().join("socket2.sock");

        let mock = MockSocket1::new(&sock1, fixture_clients_json());
        let server = tokio::spawn(mock.serve());

        let paths = HyprSocketPaths::new(sock1, sock2);
        let client = HyprCtl::new(paths);

        let found = client.get_client_by_address("0xbbb222").await.unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().class, "com.mitchellh.ghostty");

        let missing = client.get_client_by_address("0xnonexistent").await.unwrap();
        assert!(missing.is_none());

        server.abort();
    }
}
