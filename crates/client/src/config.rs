//! Client configuration for Meta Quest OpenXR streaming runtime.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use serde::{Deserialize, Serialize};

use crate::error::{ClientError, ClientResult};

/// Supported transport protocol modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransportMode {
    /// Paced UDP datagrams with Reed-Solomon/XOR FEC.
    Udp,
    /// USB ADB reverse-tethering TCP tunnel (`adb reverse tcp:42069 tcp:42069`).
    AdbReverseTcp,
}

/// Preferred video compression codec.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientVideoCodec {
    /// AV1 hardware decoding (primary for Quest 3 / Snapdragon XR2 Gen 2).
    Av1,
    /// HEVC / H.265 hardware decoding (fallback).
    Hevc,
}

impl ClientVideoCodec {
    /// Returns the Android MIME type for this codec.
    pub fn mime_type(&self) -> &'static str {
        match self {
            Self::Av1 => "video/av01",
            Self::Hevc => "video/hevc",
        }
    }

    /// Returns the Qualcomm Snapdragon XR2 Gen 2 low-latency decoder name.
    pub fn qti_low_latency_name(&self) -> &'static str {
        match self {
            Self::Av1 => "c2.qti.av1.decoder.low_latency",
            Self::Hevc => "c2.qti.hevc.decoder.low_latency",
        }
    }
}

/// Virtual display projection mode in OpenXR space.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DisplayMode {
    /// Flat virtual monitor quad (`XrCompositionLayerQuad`).
    FlatQuad,
    /// Curved virtual monitor wrapping user field of view (`XrCompositionLayerCylinderKHR`).
    CurvedCylinder,
}

/// Configuration settings for the Quest streaming client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientConfig {
    /// Remote host address (default `127.0.0.1:42069` for ADB reverse tunnel).
    pub host_addr: SocketAddr,

    /// Transport mode (UDP or ADB Reverse TCP).
    pub transport_mode: TransportMode,

    /// Preferred video codec.
    pub codec: ClientVideoCodec,

    /// Virtual monitor display mode.
    pub display_mode: DisplayMode,

    /// Target display refresh rate in Hz (72.0, 90.0, or 120.0).
    pub target_refresh_rate: f32,

    /// Flat quad monitor physical width and height in meters (width, height).
    pub quad_size: (f32, f32),

    /// Flat quad distance from user head in meters.
    pub quad_distance: f32,

    /// Curved cylinder radius in meters.
    pub cylinder_radius: f32,

    /// Curved cylinder central angle in radians (e.g. 1.2 rad ≈ 68.75°).
    pub cylinder_central_angle: f32,

    /// Curved cylinder aspect ratio (e.g. 16.0 / 9.0).
    pub cylinder_aspect_ratio: f32,

    /// Whether to render the head-locked performance telemetry HUD.
    pub enable_hud: bool,

    /// Initial playout delay for the jitter buffer in milliseconds.
    pub jitter_buffer_initial_depth_ms: u64,

    /// Maximum capacity of the jitter buffer in packets.
    pub jitter_buffer_max_depth: usize,

    /// Virtual monitor horizontal stream resolution in pixels (default 1920).
    pub stream_width: u32,

    /// Virtual monitor vertical stream resolution in pixels (default 1080).
    pub stream_height: u32,

    /// Force using mock decoder for testing or simulation.
    pub force_mock_decoder: bool,

    /// Whether to enable Quest Passthrough (`XR_FB_passthrough`) to view real-world environment.
    pub enable_passthrough: bool,

    /// Optional file path to persist spatial screen layout across sessions.
    pub spatial_layout_path: Option<std::path::PathBuf>,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            host_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 42069),
            transport_mode: TransportMode::AdbReverseTcp,
            codec: ClientVideoCodec::Av1,
            display_mode: DisplayMode::CurvedCylinder,
            target_refresh_rate: 90.0,
            quad_size: (1.6, 0.9),
            quad_distance: 1.2,
            cylinder_radius: 1.5,
            cylinder_central_angle: 1.2,
            cylinder_aspect_ratio: 16.0 / 9.0,
            enable_hud: true,
            jitter_buffer_initial_depth_ms: 4,
            jitter_buffer_max_depth: 512,
            stream_width: 1920,
            stream_height: 1080,
            #[cfg(target_os = "android")]
            force_mock_decoder: false,
            #[cfg(not(target_os = "android"))]
            force_mock_decoder: true,
            enable_passthrough: true,
            spatial_layout_path: None,
        }
    }
}

impl ClientConfig {
    /// Validates the configuration values.
    pub fn validate(&self) -> ClientResult<()> {
        if self.target_refresh_rate < 30.0 || self.target_refresh_rate > 144.0 {
            return Err(ClientError::Config(format!(
                "Invalid target refresh rate: {} Hz (expected 30-144 Hz)",
                self.target_refresh_rate
            )));
        }

        if self.quad_size.0 <= 0.0 || self.quad_size.1 <= 0.0 {
            return Err(ClientError::Config(format!(
                "Invalid quad size: {}x{} m (must be positive)",
                self.quad_size.0, self.quad_size.1
            )));
        }

        if self.quad_distance <= 0.1 || self.quad_distance > 10.0 {
            return Err(ClientError::Config(format!(
                "Invalid quad distance: {} m (expected 0.1-10.0 m)",
                self.quad_distance
            )));
        }

        if self.cylinder_radius <= 0.1 || self.cylinder_radius > 10.0 {
            return Err(ClientError::Config(format!(
                "Invalid cylinder radius: {} m (expected 0.1-10.0 m)",
                self.cylinder_radius
            )));
        }

        if self.cylinder_central_angle <= 0.1 || self.cylinder_central_angle > std::f32::consts::PI
        {
            return Err(ClientError::Config(format!(
                "Invalid cylinder central angle: {} rad (expected 0.1-PI rad)",
                self.cylinder_central_angle
            )));
        }

        if self.cylinder_aspect_ratio <= 0.1 || self.cylinder_aspect_ratio > 10.0 {
            return Err(ClientError::Config(format!(
                "Invalid cylinder aspect ratio: {} (expected 0.1-10.0)",
                self.cylinder_aspect_ratio
            )));
        }

        if self.jitter_buffer_max_depth < 16 || self.jitter_buffer_max_depth > 4096 {
            return Err(ClientError::Config(format!(
                "Invalid jitter buffer max depth: {} (expected 16-4096)",
                self.jitter_buffer_max_depth
            )));
        }

        if self.stream_width == 0 || self.stream_height == 0 {
            return Err(ClientError::Config(
                "Stream resolution width and height must be non-zero".into(),
            ));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config_validates() {
        let config = ClientConfig::default();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_invalid_refresh_rate_rejected() {
        let config = ClientConfig {
            target_refresh_rate: 200.0,
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_invalid_cylinder_geometry_rejected() {
        let config_neg_radius = ClientConfig {
            cylinder_radius: -1.0,
            ..Default::default()
        };
        assert!(config_neg_radius.validate().is_err());

        let config_large_angle = ClientConfig {
            cylinder_radius: 1.5,
            cylinder_central_angle: 5.0, // > PI
            ..Default::default()
        };
        assert!(config_large_angle.validate().is_err());
    }
}
