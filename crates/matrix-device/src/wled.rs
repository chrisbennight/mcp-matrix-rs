//! WLED JSON API client.
//!
//! This is the configuration and state plane: identity, dimensions, framerate, power
//! headroom, brightness, and power. Frames never travel here — they go over DDP. The
//! split is deliberate, and mixing them would put a 12 KB payload through an HTTP
//! endpoint with a 24 KB command buffer behind it.

use serde::{Deserialize, Serialize};
use std::time::Duration;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum WledError {
    #[error("{operation} to {url} failed: {source}")]
    Transport {
        operation: &'static str,
        url: String,
        #[source]
        source: reqwest::Error,
    },

    #[error("{url} returned HTTP {status}")]
    Status { url: String, status: u16 },

    #[error("could not decode the response from {url}: {source}")]
    Decode {
        url: String,
        #[source]
        source: reqwest::Error,
    },

    #[error("base URL {0:?} is not usable")]
    InvalidBase(String),
}

impl WledError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Transport { .. } => "wled_transport_failed",
            Self::Status { .. } => "wled_http_status",
            Self::Decode { .. } => "wled_decode_failed",
            Self::InvalidBase(_) => "wled_invalid_base_url",
        }
    }
}

/// Physical matrix dimensions as the device reports them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct MatrixInfo {
    #[serde(rename = "w")]
    pub width: u16,
    #[serde(rename = "h")]
    pub height: u16,
}

/// LED subsystem figures.
///
/// Every field defaults, because the `info` object varies across WLED versions and
/// build flags and a missing field should not fail the whole read. A caller that needs
/// a figure checks it explicitly rather than trusting a zero.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct LedInfo {
    #[serde(default)]
    pub count: u32,

    /// Frames per second the device is actually achieving. This is the feedback signal
    /// the playout rate is derived from — never a constant chosen here.
    #[serde(default)]
    pub fps: u16,

    /// Current estimated draw in milliamps.
    #[serde(default, rename = "pwr")]
    pub power_ma: u32,

    /// Configured power ceiling in milliamps. Zero means automatic brightness limiting
    /// is switched off on the device, so there is no ceiling to clamp against.
    #[serde(default, rename = "maxpwr")]
    pub max_power_ma: u32,

    /// Present on 2D builds. Absent on a plain strip.
    #[serde(default)]
    pub matrix: Option<MatrixInfo>,
}

impl LedInfo {
    /// Whether the device is enforcing a power ceiling.
    pub fn has_power_ceiling(&self) -> bool {
        self.max_power_ma > 0
    }

    /// Remaining milliamps before the configured ceiling, if there is one.
    pub fn power_headroom_ma(&self) -> Option<u32> {
        self.has_power_ceiling()
            .then(|| self.max_power_ma.saturating_sub(self.power_ma))
    }
}

/// Response of `GET /json/info`. Read-only on the device by definition.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct DeviceInfo {
    #[serde(default)]
    pub name: String,

    #[serde(default, rename = "ver")]
    pub version: String,

    #[serde(default)]
    pub leds: LedInfo,

    #[serde(default)]
    pub arch: String,

    #[serde(default)]
    pub uptime: u64,
}

/// Partial update for `POST /json/state`.
///
/// Every field is optional and omitted when unset, because WLED merges a partial state
/// and sending a fully-populated object would clobber settings this server has no
/// opinion about.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct StateUpdate {
    #[serde(skip_serializing_if = "Option::is_none", rename = "on")]
    pub power: Option<bool>,

    #[serde(skip_serializing_if = "Option::is_none", rename = "bri")]
    pub brightness: Option<u8>,

    /// Crossfade duration in 100 ms units, i.e. tenths of a second. Zero makes a change
    /// immediate, which is what
    /// realtime playout wants — a crossfade fights the frame pump.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transition: Option<u16>,
}

impl StateUpdate {
    pub fn power(on: bool) -> Self {
        Self {
            power: Some(on),
            ..Self::default()
        }
    }

    pub fn brightness(level: u8) -> Self {
        Self {
            brightness: Some(level),
            ..Self::default()
        }
    }

    pub fn with_transition(mut self, tenths_of_a_second: u16) -> Self {
        self.transition = Some(tenths_of_a_second);
        self
    }
}

/// HTTP client for one panel.
#[derive(Debug, Clone)]
pub struct WledClient {
    base: String,
    http: reqwest::Client,
}

impl WledClient {
    /// `base` is the device origin, e.g. `http://192.0.2.10`.
    pub fn new(base: impl Into<String>, timeout: Duration) -> Result<Self, WledError> {
        let base = base.into();
        let trimmed = base.trim_end_matches('/').to_string();
        if trimmed.is_empty() || !trimmed.contains("://") {
            return Err(WledError::InvalidBase(base));
        }
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|source| WledError::Transport {
                operation: "client build",
                url: trimmed.clone(),
                source,
            })?;
        Ok(Self {
            base: trimmed,
            http,
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    pub async fn info(&self) -> Result<DeviceInfo, WledError> {
        let url = self.url("/json/info");
        let response = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|source| WledError::Transport {
                operation: "GET",
                url: url.clone(),
                source,
            })?;

        let status = response.status();
        if !status.is_success() {
            return Err(WledError::Status {
                url,
                status: status.as_u16(),
            });
        }

        response
            .json::<DeviceInfo>()
            .await
            .map_err(|source| WledError::Decode { url, source })
    }

    /// Apply a partial state update.
    ///
    /// The response body is deliberately not parsed: without the `v` flag WLED returns
    /// a minimal acknowledgement, and a caller that needs the resulting state should
    /// read it back rather than trust an echo.
    pub async fn apply(&self, update: &StateUpdate) -> Result<(), WledError> {
        let url = self.url("/json/state");
        let response = self
            .http
            .post(&url)
            .json(update)
            .send()
            .await
            .map_err(|source| WledError::Transport {
                operation: "POST",
                url: url.clone(),
                source,
            })?;

        let status = response.status();
        if !status.is_success() {
            return Err(WledError::Status {
                url,
                status: status.as_u16(),
            });
        }
        Ok(())
    }

    pub async fn set_power(&self, on: bool) -> Result<(), WledError> {
        self.apply(&StateUpdate::power(on)).await
    }

    pub async fn set_brightness(&self, level: u8) -> Result<(), WledError> {
        self.apply(&StateUpdate::brightness(level)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A loopback HTTP responder for the GET and POST surface used by the client.
    struct FakePanel {
        addr: std::net::SocketAddr,
        captured: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl FakePanel {
        fn start(info_body: &'static str, status_line: &'static str) -> Self {
            let listener =
                std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback listener");
            let addr = listener.local_addr().expect("listener addr");
            let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let sink = std::sync::Arc::clone(&captured);

            std::thread::spawn(move || {
                use std::io::{Read, Write};
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { break };
                    let mut raw = vec![0u8; 8192];
                    let read = stream.read(&mut raw).unwrap_or(0);
                    let request = String::from_utf8_lossy(&raw[..read]).to_string();

                    let body = if request.starts_with("POST") {
                        sink.lock()
                            .expect("capture lock")
                            .push(request.rsplit("\r\n\r\n").next().unwrap_or("").to_string());
                        "{\"success\":true}"
                    } else {
                        info_body
                    };

                    let response = format!(
                        "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes());
                    let _ = stream.flush();
                }
            });

            Self { addr, captured }
        }

        fn base(&self) -> String {
            format!("http://{}", self.addr)
        }

        fn captured_bodies(&self) -> Vec<String> {
            self.captured.lock().expect("capture lock").clone()
        }
    }

    fn client_for(panel: &FakePanel) -> WledClient {
        WledClient::new(panel.base(), Duration::from_secs(5)).expect("valid base")
    }

    const M1_INFO: &str = r#"{
        "ver": "0.16.0",
        "name": "Apollo LED Matrix",
        "arch": "esp32",
        "uptime": 4211,
        "leds": {"count": 4096, "fps": 23, "pwr": 1850, "maxpwr": 3000, "matrix": {"w": 64, "h": 64}}
    }"#;

    #[test]
    fn a_base_url_without_a_scheme_is_rejected() {
        let err = WledClient::new("192.0.2.10", Duration::from_secs(1))
            .expect_err("a bare host is not a base URL");
        assert_eq!(err.code(), "wled_invalid_base_url");
    }

    #[test]
    fn a_trailing_slash_does_not_double_up_in_paths() {
        let client =
            WledClient::new("http://panel.local/", Duration::from_secs(1)).expect("valid base");
        assert_eq!(client.url("/json/info"), "http://panel.local/json/info");
    }

    #[tokio::test]
    async fn info_parses_the_figures_playout_depends_on() {
        let panel = FakePanel::start(M1_INFO, "200 OK");
        let info = client_for(&panel).info().await.expect("info");

        assert_eq!(info.name, "Apollo LED Matrix");
        assert_eq!(info.version, "0.16.0");
        assert_eq!(info.leds.count, 4096);
        assert_eq!(info.leds.fps, 23);
        assert_eq!(
            info.leds.matrix,
            Some(MatrixInfo {
                width: 64,
                height: 64
            })
        );
    }

    #[tokio::test]
    async fn info_tolerates_a_build_that_omits_fields() {
        let panel = FakePanel::start(r#"{"ver":"0.14.0"}"#, "200 OK");
        let info = client_for(&panel).info().await.expect("sparse info");

        assert_eq!(info.version, "0.14.0");
        assert_eq!(info.leds.count, 0);
        assert_eq!(info.leds.matrix, None);
        assert!(!info.leds.has_power_ceiling());
    }

    #[tokio::test]
    async fn an_http_error_is_reported_as_a_status_not_a_decode_failure() {
        let panel = FakePanel::start("{}", "500 Internal Server Error");
        let err = client_for(&panel)
            .info()
            .await
            .expect_err("a 500 must not be parsed as info");
        assert_eq!(err.code(), "wled_http_status");
    }

    #[tokio::test]
    async fn set_power_sends_only_the_power_field() {
        let panel = FakePanel::start(M1_INFO, "200 OK");
        client_for(&panel).set_power(true).await.expect("set power");

        let bodies = panel.captured_bodies();
        assert_eq!(bodies.len(), 1);
        assert_eq!(bodies[0], r#"{"on":true}"#);
    }

    #[tokio::test]
    async fn set_brightness_sends_only_the_brightness_field() {
        let panel = FakePanel::start(M1_INFO, "200 OK");
        client_for(&panel)
            .set_brightness(128)
            .await
            .expect("set brightness");

        let bodies = panel.captured_bodies();
        assert_eq!(bodies[0], r#"{"bri":128}"#);
    }

    #[test]
    fn a_state_update_omits_every_field_it_does_not_set() {
        let json = serde_json::to_string(&StateUpdate::default()).expect("serialize");
        assert_eq!(json, "{}");

        let json = serde_json::to_string(&StateUpdate::brightness(10).with_transition(0))
            .expect("serialize");
        assert_eq!(json, r#"{"bri":10,"transition":0}"#);
    }

    #[test]
    fn power_headroom_is_absent_when_the_device_enforces_no_ceiling() {
        let unlimited = LedInfo {
            power_ma: 1850,
            max_power_ma: 0,
            ..LedInfo::default()
        };
        assert!(!unlimited.has_power_ceiling());
        assert_eq!(unlimited.power_headroom_ma(), None);
    }

    #[test]
    fn power_headroom_saturates_rather_than_underflowing_when_over_budget() {
        let over = LedInfo {
            power_ma: 4000,
            max_power_ma: 3000,
            ..LedInfo::default()
        };
        assert_eq!(over.power_headroom_ma(), Some(0));

        let under = LedInfo {
            power_ma: 1850,
            max_power_ma: 3000,
            ..LedInfo::default()
        };
        assert_eq!(under.power_headroom_ma(), Some(1150));
    }
}
