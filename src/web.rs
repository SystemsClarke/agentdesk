//! The ops console's address: `web.port` and `web.key` in the data folder, written by the daemon while the console is serving.

use crate::paths;

/// `http://127.0.0.1:<port>/?k=<key>`, or None when no console is serving.
pub fn url() -> Option<String> {
    let port = std::fs::read_to_string(paths::data_dir().join("web.port")).ok()?.trim().parse::<u16>().ok()?;
    let key = std::fs::read_to_string(paths::data_dir().join("web.key")).ok()?.trim().to_string();
    if key.is_empty() || std::net::TcpStream::connect_timeout(&std::net::SocketAddr::from(([127, 0, 0, 1], port)), std::time::Duration::from_millis(200)).is_err() {
        return None;
    }
    Some(format!("http://127.0.0.1:{port}/?k={key}"))
}
