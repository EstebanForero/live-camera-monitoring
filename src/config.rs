use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CameraConfig {
    pub name: String,
    pub host: String,
    pub rtsp_port: u16,
    pub onvif_port: u16,
    pub username: String,
    pub password: String,
    /// `main` is Tapo `/stream1` (1080p). `sub` is `/stream2` (360p).
    pub stream: String,
    /// Desktop alert when the picture itself changes.
    #[serde(default = "default_motion")]
    pub motion: bool,
    /// Red outlines in the popped-out view. Independent of desktop alerts.
    #[serde(default)]
    pub motion_borders: bool,
    /// 1 is the least sensitive, 10 is the most. Shared by alerts and outlines.
    #[serde(default = "default_sensitivity")]
    pub motion_sensitivity: u8,
}

fn default_motion() -> bool {
    true
}

fn default_sensitivity() -> u8 {
    5
}

impl Default for CameraConfig {
    fn default() -> Self {
        Self {
            name: "Tapo C200".into(),
            host: String::new(),
            rtsp_port: 554,
            onvif_port: 2020,
            username: String::new(),
            password: String::new(),
            stream: "sub".into(),
            motion: true,
            motion_borders: false,
            motion_sensitivity: 5,
        }
    }
}

pub fn config_path() -> Result<PathBuf, String> {
    let dir = dirs::config_dir()
        .ok_or("Could not find a config directory.")?
        .join("camera-live");
    fs::create_dir_all(&dir).map_err(|e| format!("Could not create {dir:?}: {e}"))?;
    Ok(dir.join("config.json"))
}

pub fn load() -> Result<Option<CameraConfig>, String> {
    let path = config_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&path).map_err(|e| format!("Could not read {path:?}: {e}"))?;
    if raw.trim().is_empty() {
        return Ok(None);
    }
    let cfg: CameraConfig =
        serde_json::from_str(&raw).map_err(|e| format!("Camera settings file is invalid: {e}"))?;
    Ok(Some(normalize(cfg)?))
}

pub fn save(cfg: &CameraConfig) -> Result<(), String> {
    let path = config_path()?;
    let data = serde_json::to_vec_pretty(cfg).map_err(|e| e.to_string())?;
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)
        .map_err(|e| format!("Could not write {path:?}: {e}"))?;
    file.write_all(&data).map_err(|e| e.to_string())?;
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
    Ok(())
}

pub fn normalize(mut cfg: CameraConfig) -> Result<CameraConfig, String> {
    cfg.name = cfg.name.trim().to_string();
    cfg.username = cfg.username.trim().to_string();
    cfg.password = cfg.password.trim().to_string();
    cfg.host = cfg.host.trim().to_string();
    cfg.stream = cfg.stream.trim().to_ascii_lowercase();

    if let Some(parsed) = parse_endpoint(&cfg.host) {
        if cfg.username.is_empty() {
            cfg.username = parsed.username;
        }
        if cfg.password.is_empty() {
            cfg.password = parsed.password;
        }
        if parsed.port != 0 {
            cfg.rtsp_port = parsed.port;
        }
        if let Some(stream) = parsed.stream {
            cfg.stream = stream;
        }
        cfg.host = parsed.host;
    }

    cfg.host = cfg
        .host
        .trim_start_matches("rtsp://")
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    if let Some((host, port)) = split_host_port(&cfg.host) {
        cfg.host = host;
        if cfg.rtsp_port == 554 || cfg.rtsp_port == 0 {
            cfg.rtsp_port = port;
        }
    }

    if cfg.motion_sensitivity == 0 {
        cfg.motion_sensitivity = 5;
    }
    cfg.motion_sensitivity = cfg.motion_sensitivity.clamp(1, 10);
    if cfg.name.is_empty() {
        cfg.name = "Tapo C200".into();
    }
    if cfg.host.is_empty() {
        return Err("Enter the camera's IP address.".into());
    }
    if cfg.username.is_empty() || cfg.password.is_empty() {
        return Err(
            "Enter the camera account username and password from the Tapo app. That account is separate from your TP-Link login.".into(),
        );
    }
    if cfg.rtsp_port == 0 {
        cfg.rtsp_port = 554;
    }
    if cfg.onvif_port == 0 {
        cfg.onvif_port = 2020;
    }
    if cfg.stream != "main" && cfg.stream != "sub" {
        return Err("Choose Smooth or HD for the stream.".into());
    }
    Ok(cfg)
}

pub fn stream_path(stream: &str) -> &'static str {
    if stream == "main" {
        "stream1"
    } else {
        "stream2"
    }
}

pub fn rtsp_url(cfg: &CameraConfig) -> String {
    let host = bracket_host(&cfg.host);
    format!(
        "rtsp://{}:{}@{}:{}/{}",
        userinfo(&cfg.username),
        userinfo(&cfg.password),
        host,
        cfg.rtsp_port,
        stream_path(&cfg.stream)
    )
}

pub fn redact(message: &str, cfg: &CameraConfig) -> String {
    let mut out = message.to_string();
    if !cfg.password.is_empty() {
        out = out.replace(&cfg.password, "••••");
        out = out.replace(&userinfo(&cfg.password), "••••");
    }
    out
}

fn userinfo(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

pub fn bracket_host(host: &str) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    }
}

struct ParsedEndpoint {
    username: String,
    password: String,
    host: String,
    port: u16,
    stream: Option<String>,
}

fn parse_endpoint(raw: &str) -> Option<ParsedEndpoint> {
    let trimmed = raw.trim();
    let rest = trimmed
        .strip_prefix("rtsp://")
        .or_else(|| trimmed.strip_prefix("rtsps://"))?;
    let (ui, after_at) = rest.rsplit_once('@')?;
    let (user, pass) = ui.split_once(':')?;
    let (hostport, path) = match after_at.split_once('/') {
        Some((h, p)) => (h, Some(p)),
        None => (after_at, None),
    };
    let (host, port) = split_host_port(hostport).unwrap_or_else(|| (hostport.to_string(), 554));
    let stream = path.and_then(|p| {
        if p.contains("stream1") {
            Some("main".into())
        } else if p.contains("stream2") {
            Some("sub".into())
        } else {
            None
        }
    });
    Some(ParsedEndpoint {
        username: percent_decode(user),
        password: percent_decode(pass),
        host,
        port,
        stream,
    })
}

fn split_host_port(hostport: &str) -> Option<(String, u16)> {
    if hostport.starts_with('[') {
        let end = hostport.find(']')?;
        let host = hostport[1..end].to_string();
        let rest = &hostport[end + 1..];
        let port = rest.strip_prefix(':')?.parse().ok()?;
        return Some((host, port));
    }
    let (host, port) = hostport.rsplit_once(':')?;
    if host.is_empty() || port.is_empty() || host.contains(':') {
        return None;
    }
    Some((host.to_string(), port.parse().ok()?))
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&value[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pasted_rtsp_url() {
        let cfg = normalize(CameraConfig {
            host: "rtsp://cam%20user:p%40ss@192.168.1.20:8554/stream1".into(),
            username: String::new(),
            password: String::new(),
            ..CameraConfig::default()
        })
        .unwrap();
        assert_eq!(cfg.host, "192.168.1.20");
        assert_eq!(cfg.rtsp_port, 8554);
        assert_eq!(cfg.username, "cam user");
        assert_eq!(cfg.password, "p@ss");
        assert_eq!(cfg.stream, "main");
        assert_eq!(
            rtsp_url(&cfg),
            "rtsp://cam%20user:p%40ss@192.168.1.20:8554/stream1"
        );
    }

    #[test]
    fn rejects_missing_password() {
        let err = normalize(CameraConfig {
            host: "192.168.1.20".into(),
            username: "cam".into(),
            ..CameraConfig::default()
        })
        .unwrap_err();
        assert!(err.contains("camera account"));
    }
}
