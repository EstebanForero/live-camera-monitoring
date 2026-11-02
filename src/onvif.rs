//! Minimal ONVIF client for a Tapo C200.
//!
//! Tapo serves Profile S on port 2020 and expects WS-Security UsernameToken
//! with a password digest. The same camera account used for RTSP is the
//! ONVIF user. Pan/tilt velocity is normalized to -1..=1. Positive pan turns
//! the view to the right. Positive tilt turns it up. The C200 has no zoom.

use std::time::Duration;

use base64::Engine;
use chrono::{Duration as ChronoDuration, Utc};
use serde::Serialize;
use sha1::{Digest, Sha1};

use crate::config::{bracket_host, CameraConfig};

#[derive(Clone, Copy)]
enum AuthStyle {
    Digest,
    Text,
}

#[derive(Clone)]
pub struct OnvifEndpoint {
    client: reqwest::Client,
    ptz_url: String,
    pub profile: String,
    username: String,
    password: String,
    time_offset: ChronoDuration,
    auth: AuthStyle,
    pub manufacturer: String,
    pub model: String,
    pub firmware: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Preset {
    pub token: String,
    pub name: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionReport {
    pub rtsp_reachable: bool,
    pub onvif_ok: bool,
    pub manufacturer: String,
    pub model: String,
    pub firmware: String,
    pub profile_token: String,
    pub detail: String,
}

impl OnvifEndpoint {
    pub async fn connect(cfg: &CameraConfig) -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(8))
            .connect_timeout(Duration::from_secs(4))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent("camera-live")
            .build()
            .map_err(|e| e.to_string())?;

        let urls = candidate_urls(&cfg.host, cfg.onvif_port);
        let mut errors = Vec::new();
        for url in &urls {
            match try_url(&client, url, cfg).await {
                Ok(ep) => return Ok(ep),
                Err(err) => errors.push(err),
            }
        }
        Err(summarize_onvif_errors(&errors))
    }

    pub async fn continuous_move(&self, pan: f64, tilt: f64) -> Result<(), String> {
        let pan = pan.clamp(-1.0, 1.0);
        let tilt = tilt.clamp(-1.0, 1.0);
        let body = format!(
            r#"<tptz:ContinuousMove xmlns:tptz="http://www.onvif.org/ver20/ptz/wsdl"><tptz:ProfileToken>{}</tptz:ProfileToken><tptz:Velocity><tt:PanTilt x="{pan:.3}" y="{tilt:.3}"/></tptz:Velocity><tptz:Timeout>PT2S</tptz:Timeout></tptz:ContinuousMove>"#,
            xml_escape(&self.profile)
        );
        self.call(
            &self.ptz_url,
            "http://www.onvif.org/ver20/ptz/wsdl/ContinuousMove",
            &body,
        )
        .await
        .map(|_| ())
    }

    pub async fn stop(&self) -> Result<(), String> {
        let body = format!(
            r#"<tptz:Stop xmlns:tptz="http://www.onvif.org/ver20/ptz/wsdl"><tptz:ProfileToken>{}</tptz:ProfileToken><tptz:PanTilt>true</tptz:PanTilt><tptz:Zoom>true</tptz:Zoom></tptz:Stop>"#,
            xml_escape(&self.profile)
        );
        self.call(
            &self.ptz_url,
            "http://www.onvif.org/ver20/ptz/wsdl/Stop",
            &body,
        )
        .await
        .map(|_| ())
    }

    pub async fn home(&self) -> Result<(), String> {
        let body = format!(
            r#"<tptz:GotoHomePosition xmlns:tptz="http://www.onvif.org/ver20/ptz/wsdl"><tptz:ProfileToken>{}</tptz:ProfileToken></tptz:GotoHomePosition>"#,
            xml_escape(&self.profile)
        );
        self.call(
            &self.ptz_url,
            "http://www.onvif.org/ver20/ptz/wsdl/GotoHomePosition",
            &body,
        )
        .await
        .map(|_| ())
        .map_err(|err| {
            if err.to_lowercase().contains("home") {
                "This camera has no home position yet. Save a preset instead.".into()
            } else {
                err
            }
        })
    }

    pub async fn presets(&self) -> Result<Vec<Preset>, String> {
        let body = format!(
            r#"<tptz:GetPresets xmlns:tptz="http://www.onvif.org/ver20/ptz/wsdl"><tptz:ProfileToken>{}</tptz:ProfileToken></tptz:GetPresets>"#,
            xml_escape(&self.profile)
        );
        let xml = self
            .call(
                &self.ptz_url,
                "http://www.onvif.org/ver20/ptz/wsdl/GetPresets",
                &body,
            )
            .await?;
        Ok(parse_presets(&xml))
    }

    pub async fn goto_preset(&self, token: &str) -> Result<(), String> {
        let body = format!(
            r#"<tptz:GotoPreset xmlns:tptz="http://www.onvif.org/ver20/ptz/wsdl"><tptz:ProfileToken>{}</tptz:ProfileToken><tptz:PresetToken>{}</tptz:PresetToken></tptz:GotoPreset>"#,
            xml_escape(&self.profile),
            xml_escape(token)
        );
        self.call(
            &self.ptz_url,
            "http://www.onvif.org/ver20/ptz/wsdl/GotoPreset",
            &body,
        )
        .await
        .map(|_| ())
    }

    pub async fn set_preset(&self, name: &str) -> Result<(), String> {
        let name = name.trim();
        if name.is_empty() {
            return Err("Name the preset before saving it.".into());
        }
        let body = format!(
            r#"<tptz:SetPreset xmlns:tptz="http://www.onvif.org/ver20/ptz/wsdl"><tptz:ProfileToken>{}</tptz:ProfileToken><tptz:PresetName>{}</tptz:PresetName></tptz:SetPreset>"#,
            xml_escape(&self.profile),
            xml_escape(name)
        );
        self.call(
            &self.ptz_url,
            "http://www.onvif.org/ver20/ptz/wsdl/SetPreset",
            &body,
        )
        .await
        .map(|_| ())
    }

    async fn call(&self, url: &str, action: &str, body: &str) -> Result<String, String> {
        let xml = envelope(
            &security_header(&self.username, &self.password, self.created(), self.auth),
            body,
        );
        let (status, text) = post(&self.client, url, action, xml).await?;
        if let Some(fault) = fault_message(&text) {
            if is_auth_fault(&text) {
                return Err(
                    "The camera rejected the username or password. Use the Camera Account from the Tapo app, not your TP-Link login.".into(),
                );
            }
            return Err(fault);
        }
        if !(200..300).contains(&status) && !text.contains("Envelope") {
            return Err(format!("The camera returned HTTP {status}."));
        }
        Ok(text)
    }

    fn created(&self) -> String {
        (Utc::now() + self.time_offset)
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string()
    }
}

pub async fn test_connection(cfg: &CameraConfig) -> ConnectionReport {
    let rtsp_reachable = tcp_open(&cfg.host, cfg.rtsp_port).await;
    match OnvifEndpoint::connect(cfg).await {
        Ok(ep) => {
            let mut detail = if rtsp_reachable {
                format!(
                    "RTSP port {} is open. ONVIF reports {} {} (firmware {}). Pan and tilt use profile {}.",
                    cfg.rtsp_port,
                    display_or(&ep.manufacturer, "the camera"),
                    display_or(&ep.model, ""),
                    display_or(&ep.firmware, "unknown"),
                    ep.profile
                )
            } else {
                format!(
                    "ONVIF reports {} {}, but nothing accepted a connection on RTSP port {}. Video needs that port.",
                    display_or(&ep.manufacturer, "the camera"),
                    display_or(&ep.model, ""),
                    cfg.rtsp_port
                )
            };
            detail = detail.replace("  ", " ");
            ConnectionReport {
                rtsp_reachable,
                onvif_ok: true,
                manufacturer: ep.manufacturer,
                model: ep.model,
                firmware: ep.firmware,
                profile_token: ep.profile,
                detail,
            }
        }
        Err(err) => {
            let detail = if rtsp_reachable {
                format!(
                    "RTSP port {} accepted a connection, so video can start. Pan and tilt did not: {err}",
                    cfg.rtsp_port
                )
            } else {
                format!(
                    "Could not open RTSP port {} and pan/tilt failed: {err}",
                    cfg.rtsp_port
                )
            };
            ConnectionReport {
                rtsp_reachable,
                onvif_ok: false,
                manufacturer: String::new(),
                model: String::new(),
                firmware: String::new(),
                profile_token: String::new(),
                detail,
            }
        }
    }
}

async fn tcp_open(host: &str, port: u16) -> bool {
    let host = host.to_string();
    tokio::task::spawn_blocking(move || {
        use std::net::{TcpStream, ToSocketAddrs};
        let addr = format!("{host}:{port}")
            .to_socket_addrs()
            .ok()
            .and_then(|mut it| it.next())?;
        TcpStream::connect_timeout(&addr, Duration::from_secs(3)).ok()
    })
    .await
    .ok()
    .flatten()
    .is_some()
}

fn candidate_urls(host: &str, port: u16) -> Vec<String> {
    let host = bracket_host(host);
    [
        format!("http://{host}:{port}/onvif/device_service"),
        format!("http://{host}:{port}/onvif/service"),
    ]
    .into_iter()
    .collect()
}

async fn try_url(
    client: &reqwest::Client,
    device_url: &str,
    cfg: &CameraConfig,
) -> Result<OnvifEndpoint, String> {
    let time_offset = read_time_offset(client, device_url).await.unwrap_or(ChronoDuration::zero());
    let info_body =
        r#"<tds:GetDeviceInformation xmlns:tds="http://www.onvif.org/ver10/device/wsdl"/>"#;
    let (auth, info_xml) = authed_call(
        client,
        device_url,
        "http://www.onvif.org/ver10/device/wsdl/GetDeviceInformation",
        info_body,
        &cfg.username,
        &cfg.password,
        time_offset,
    )
    .await?;

    let caps_body = r#"<tds:GetCapabilities xmlns:tds="http://www.onvif.org/ver10/device/wsdl"><tds:Category>All</tds:Category></tds:GetCapabilities>"#;
    let caps_xml = soap(
        client,
        device_url,
        "http://www.onvif.org/ver10/device/wsdl/GetCapabilities",
        caps_body,
        &cfg.username,
        &cfg.password,
        time_offset,
        auth,
    )
    .await
    .unwrap_or_default();

    let media_url = element_inner(&caps_xml, "Media")
        .and_then(|section| xml_text(section, "XAddr"))
        .filter(|url| url.starts_with("http"))
        .unwrap_or_else(|| device_url.to_string());
    let ptz_url = element_inner(&caps_xml, "PTZ")
        .and_then(|section| xml_text(section, "XAddr"))
        .filter(|url| url.starts_with("http"))
        .unwrap_or_else(|| device_url.to_string());

    let profiles_body = r#"<trt:GetProfiles xmlns:trt="http://www.onvif.org/ver10/media/wsdl"/>"#;
    let profiles_xml = soap(
        client,
        &media_url,
        "http://www.onvif.org/ver10/media/wsdl/GetProfiles",
        profiles_body,
        &cfg.username,
        &cfg.password,
        time_offset,
        auth,
    )
    .await
    .unwrap_or_default();
    let tokens = attr_values(&profiles_xml, "Profiles", "token");
    let profile = tokens
        .into_iter()
        .find(|token| token.to_ascii_lowercase().contains('1'))
        .unwrap_or_else(|| "profile_1".into());

    Ok(OnvifEndpoint {
        client: client.clone(),
        ptz_url,
        profile,
        username: cfg.username.clone(),
        password: cfg.password.clone(),
        time_offset,
        auth,
        manufacturer: xml_text(&info_xml, "Manufacturer").unwrap_or_default(),
        model: xml_text(&info_xml, "Model").unwrap_or_default(),
        firmware: xml_text(&info_xml, "FirmwareVersion").unwrap_or_default(),
    })
}

async fn read_time_offset(
    client: &reqwest::Client,
    url: &str,
) -> Result<ChronoDuration, String> {
    let body =
        r#"<tds:GetSystemDateAndTime xmlns:tds="http://www.onvif.org/ver10/device/wsdl"/>"#;
    let xml = envelope("", body);
    let (_status, text) = post(
        client,
        url,
        "http://www.onvif.org/ver10/device/wsdl/GetSystemDateAndTime",
        xml,
    )
    .await?;
    let year = xml_text(&text, "Year").and_then(|v| v.parse().ok()).ok_or("no time")?;
    let month = xml_text(&text, "Month").and_then(|v| v.parse().ok()).ok_or("no time")?;
    let day = xml_text(&text, "Day").and_then(|v| v.parse().ok()).ok_or("no time")?;
    let hour = xml_text(&text, "Hour").and_then(|v| v.parse().ok()).ok_or("no time")?;
    let minute = xml_text(&text, "Minute").and_then(|v| v.parse().ok()).ok_or("no time")?;
    let second = xml_text(&text, "Second").and_then(|v| v.parse().ok()).unwrap_or(0);
    let camera = chrono::NaiveDate::from_ymd_opt(year, month, day)
        .and_then(|d| d.and_hms_opt(hour, minute, second))
        .ok_or("bad time")?;
    let camera = camera.and_utc();
    Ok(camera.signed_duration_since(Utc::now()))
}

async fn authed_call(
    client: &reqwest::Client,
    url: &str,
    action: &str,
    body: &str,
    username: &str,
    password: &str,
    offset: ChronoDuration,
) -> Result<(AuthStyle, String), String> {
    let digest = soap(
        client, url, action, body, username, password, offset, AuthStyle::Digest,
    )
    .await;
    match digest {
        Ok(xml) => Ok((AuthStyle::Digest, xml)),
        Err(err) if err.contains("rejected the username") => {
            let text = soap(
                client, url, action, body, username, password, offset, AuthStyle::Text,
            )
            .await?;
            Ok((AuthStyle::Text, text))
        }
        Err(err) => Err(err),
    }
}

async fn soap(
    client: &reqwest::Client,
    url: &str,
    action: &str,
    body: &str,
    username: &str,
    password: &str,
    offset: ChronoDuration,
    auth: AuthStyle,
) -> Result<String, String> {
    let created = (Utc::now() + offset)
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    let xml = envelope(
        &security_header(username, password, created, auth),
        body,
    );
    let (status, text) = post(client, url, action, xml).await?;
    if status == 404 || text.to_ascii_lowercase().contains("not found") && !text.contains("Envelope")
    {
        return Err(format!("ONVIF was not found at {url}."));
    }
    if let Some(fault) = fault_message(&text) {
        if is_auth_fault(&text) {
            return Err(
                "The camera rejected the username or password. Use the Camera Account from the Tapo app, not your TP-Link login.".into(),
            );
        }
        return Err(fault);
    }
    if !text.contains("Envelope") {
        return Err(format!("Unexpected reply from the camera (HTTP {status})."));
    }
    Ok(text)
}

async fn post(
    client: &reqwest::Client,
    url: &str,
    action: &str,
    xml: String,
) -> Result<(u16, String), String> {
    let response = client
        .post(url)
        .header(
            "Content-Type",
            format!("application/soap+xml; charset=utf-8; action=\"{action}\""),
        )
        .body(xml)
        .send()
        .await
        .map_err(|err| map_reqwest(err, url))?;
    let status = response.status().as_u16();
    let text = response.text().await.map_err(|err| err.to_string())?;
    Ok((status, text))
}

fn map_reqwest(err: reqwest::Error, url: &str) -> String {
    if err.is_timeout() {
        format!("Timed out talking to {url}.")
    } else if err.is_connect() {
        format!("Could not connect to {url}. Check the address, the ONVIF port, and that this computer is on the same network.")
    } else {
        err.to_string()
    }
}

fn summarize_onvif_errors(errors: &[String]) -> String {
    if errors.iter().any(|err| err.contains("rejected the username")) {
        return "The camera rejected the username or password. Use the Camera Account from the Tapo app, not your TP-Link login.".into();
    }
    if errors.iter().all(|err| {
        let lower = err.to_lowercase();
        lower.contains("could not connect")
            || lower.contains("timed out")
            || lower.contains("not found")
            || lower.contains("connection refused")
    }) {
        return "Could not reach ONVIF on this camera. Tapo serves it on port 2020. Video can still work over RTSP.".into();
    }
    errors
        .last()
        .cloned()
        .unwrap_or_else(|| "ONVIF failed.".into())
}

fn security_header(username: &str, password: &str, created: String, auth: AuthStyle) -> String {
    let user = xml_escape(username);
    match auth {
        AuthStyle::Text => format!(
            r#"<wsse:Security s:mustUnderstand="1" xmlns:wsse="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-secext-1.0.xsd"><wsse:UsernameToken><wsse:Username>{user}</wsse:Username><wsse:Password Type="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-username-token-profile-1.0#PasswordText">{}</wsse:Password></wsse:UsernameToken></wsse:Security>"#,
            xml_escape(password)
        ),
        AuthStyle::Digest => {
            let mut nonce = [0u8; 16];
            if getrandom::getrandom(&mut nonce).is_err() {
                nonce = *b"camera-live-nonc";
            }
            let mut hasher = Sha1::new();
            hasher.update(nonce);
            hasher.update(created.as_bytes());
            hasher.update(password.as_bytes());
            let digest = base64::engine::general_purpose::STANDARD.encode(hasher.finalize());
            let nonce_b64 = base64::engine::general_purpose::STANDARD.encode(nonce);
            format!(
                r#"<wsse:Security s:mustUnderstand="1" xmlns:wsse="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-secext-1.0.xsd" xmlns:wsu="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-utility-1.0.xsd"><wsse:UsernameToken><wsse:Username>{user}</wsse:Username><wsse:Password Type="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-username-token-profile-1.0#PasswordDigest">{digest}</wsse:Password><wsse:Nonce EncodingType="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-soap-message-security-1.0#Base64Binary">{nonce_b64}</wsse:Nonce><wsu:Created>{created}</wsu:Created></wsse:UsernameToken></wsse:Security>"#
            )
        }
    }
}

fn envelope(header: &str, body: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:tt="http://www.onvif.org/ver10/schema"><s:Header>{header}</s:Header><s:Body>{body}</s:Body></s:Envelope>"#
    )
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn display_or<'a>(value: &'a str, fallback: &'a str) -> &'a str {
    if value.trim().is_empty() {
        fallback
    } else {
        value
    }
}

fn is_auth_fault(xml: &str) -> bool {
    let lower = xml.to_lowercase();
    lower.contains("notauthorized")
        || lower.contains("failedauthentication")
        || lower.contains("not authorized")
        || lower.contains("sender not authorized")
}

fn fault_message(xml: &str) -> Option<String> {
    if !xml.contains("Fault") {
        return None;
    }
    xml_text(xml, "Text")
        .or_else(|| xml_text(xml, "Subcode"))
        .or_else(|| Some("The camera returned an ONVIF fault.".into()))
}

fn xml_text(xml: &str, local: &str) -> Option<String> {
    let mut i = 0;
    let bytes = xml.as_bytes();
    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        if xml[i..].starts_with("</") || xml[i..].starts_with("<?") || xml[i..].starts_with("<!") {
            i += 1;
            continue;
        }
        let rest = &xml[i + 1..];
        let end = rest.find('>')?;
        let inside = &rest[..end];
        let self_closing = inside.ends_with('/');
        let head = inside.trim_end_matches('/').trim();
        let raw_name = head.split_whitespace().next().unwrap_or("");
        let name = raw_name.rsplit(':').next().unwrap_or(raw_name);
        let content_at = i + 1 + end + 1;
        if name == local && !self_closing {
            if let Some(close_rel) = xml[content_at..].find('<') {
                let content = xml[content_at..content_at + close_rel].trim();
                if !content.is_empty() {
                    return Some(content.to_string());
                }
            }
        }
        i = content_at.max(i + 1);
    }
    None
}

fn element_inner<'a>(xml: &'a str, local: &str) -> Option<&'a str> {
    let mut i = 0;
    let bytes = xml.as_bytes();
    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        if xml[i..].starts_with("</") || xml[i..].starts_with("<?") || xml[i..].starts_with("<!") {
            i += 1;
            continue;
        }
        let rest = &xml[i + 1..];
        let Some(end) = rest.find('>') else {
            return None;
        };
        let inside = &rest[..end];
        let self_closing = inside.ends_with('/');
        let head = inside.trim_end_matches('/').trim();
        let raw_name = head.split_whitespace().next().unwrap_or("");
        let name = raw_name.rsplit(':').next().unwrap_or(raw_name);
        let after = i + 1 + end + 1;
        if name == local && !self_closing {
            if let Some(inner_end) = find_close(xml, after, local) {
                return Some(&xml[after..inner_end]);
            }
        }
        i = after;
    }
    None
}

fn find_close(xml: &str, from: usize, local: &str) -> Option<usize> {
    let mut depth = 1;
    let mut i = from;
    let bytes = xml.as_bytes();
    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        if xml[i..].starts_with("<?") || xml[i..].starts_with("<!") {
            i += 1;
            continue;
        }
        let closing = xml[i..].starts_with("</");
        let rest = if closing { &xml[i + 2..] } else { &xml[i + 1..] };
        let end = rest.find('>')?;
        let inside = &rest[..end];
        let self_closing = !closing && inside.ends_with('/');
        let head = inside.trim_end_matches('/').trim();
        let raw_name = head.split_whitespace().next().unwrap_or("");
        let name = raw_name.rsplit(':').next().unwrap_or(raw_name);
        let after = i + if closing { 2 } else { 1 } + end + 1;
        if name == local {
            if closing {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            } else if !self_closing {
                depth += 1;
            }
        }
        i = after;
    }
    None
}

fn attr_values(xml: &str, element: &str, attr: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut i = 0;
    let bytes = xml.as_bytes();
    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        if xml[i..].starts_with("</") || xml[i..].starts_with("<?") || xml[i..].starts_with("<!") {
            i += 1;
            continue;
        }
        let rest = &xml[i + 1..];
        let Some(end) = rest.find('>') else {
            break;
        };
        let inside = rest[..end].trim_end_matches('/').trim();
        let raw_name = inside.split_whitespace().next().unwrap_or("");
        let name = raw_name.rsplit(':').next().unwrap_or(raw_name);
        if name == element {
            if let Some(value) = get_attr(inside, attr) {
                values.push(value);
            }
        }
        i = i + 1 + end + 1;
    }
    values
}

fn get_attr(inside: &str, attr: &str) -> Option<String> {
    let key = format!("{attr}=");
    let idx = inside.find(&key)?;
    let rest = &inside[idx + key.len()..];
    let quote = rest.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let value = &rest[1..];
    let end = value.find(quote)?;
    Some(value[..end].to_string())
}

fn parse_presets(xml: &str) -> Vec<Preset> {
    let mut presets = Vec::new();
    let mut i = 0;
    let bytes = xml.as_bytes();
    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        if xml[i..].starts_with("</") || xml[i..].starts_with("<?") || xml[i..].starts_with("<!") {
            i += 1;
            continue;
        }
        let rest = &xml[i + 1..];
        let Some(end) = rest.find('>') else {
            break;
        };
        let inside_raw = &rest[..end];
        let self_closing = inside_raw.ends_with('/');
        let inside = inside_raw.trim_end_matches('/').trim();
        let raw_name = inside.split_whitespace().next().unwrap_or("");
        let name = raw_name.rsplit(':').next().unwrap_or(raw_name);
        let after = i + 1 + end + 1;
        if name == "Preset" && !self_closing {
            if let Some(token) = get_attr(inside, "token") {
                let inner_end = find_close(xml, after, "Preset").unwrap_or(after);
                let inner = &xml[after..inner_end];
                let label = xml_text(inner, "Name").unwrap_or_else(|| token.clone());
                presets.push(Preset {
                    token,
                    name: label,
                });
                i = inner_end;
                continue;
            }
        }
        i = after;
    }
    presets
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_profile_token_and_ptz_address() {
        let xml = r#"<tt:PTZ><tt:XAddr>http://192.168.1.5:2020/onvif/ptz</tt:XAddr></tt:PTZ><trt:Profiles token="profile_1" fixed="true"><tt:Name>Main</tt:Name></trt:Profiles>"#;
        let ptz = element_inner(xml, "PTZ").and_then(|s| xml_text(s, "XAddr"));
        assert_eq!(ptz.as_deref(), Some("http://192.168.1.5:2020/onvif/ptz"));
        assert_eq!(attr_values(xml, "Profiles", "token"), vec!["profile_1"]);
    }

    #[test]
    fn reads_presets() {
        let xml = r#"<tptz:Preset token="1"><tt:Name>Door</tt:Name></tptz:Preset><tptz:Preset token="2"><tt:Name>Window</tt:Name></tptz:Preset>"#;
        assert_eq!(
            parse_presets(xml),
            vec![
                Preset { token: "1".into(), name: "Door".into() },
                Preset { token: "2".into(), name: "Window".into() },
            ]
        );
    }

    #[test]
    fn digest_header_contains_nonce_and_created() {
        let header = security_header("cam", "secret", "2026-10-29T00:00:00Z".into(), AuthStyle::Digest);
        assert!(header.contains("PasswordDigest"));
        assert!(header.contains(">cam<"));
        assert!(header.contains("2026-10-29T00:00:00Z"));
        assert!(!header.contains("secret"));
    }
}
