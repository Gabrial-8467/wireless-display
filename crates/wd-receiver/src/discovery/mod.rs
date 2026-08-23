use std::net::IpAddr;

use mdns_sd::{ServiceDaemon, ServiceInfo};

/// mDNS service type advertised by this receiver so companion phones (and the
/// mock-phone tool) can find it on the local network.
pub const SERVICE_TYPE: &str = "_wdlink._udp.local.";
pub const TXT_PROTO_KEY: &str = "proto";
pub const TXT_FP_KEY: &str = "fp";

pub struct Advertisement {
    instance_name: String,
    daemon: ServiceDaemon,
}

impl Advertisement {
    /// Registers the receiver on the local network. Phones browse for
    /// `_wdlink._udp` and connect to us; we never initiate connections.
    pub fn start(
        receiver_name: &str,
        host_name: &str,
        ip: IpAddr,
        port: u16,
        fingerprint_short: &str,
    ) -> anyhow::Result<Self> {
        let daemon = ServiceDaemon::new()
            .map_err(|e| anyhow::anyhow!("mDNS daemon failed to start: {e}"))?;
        let instance = sanitize_instance(receiver_name);
        let host = format!("{}-wd.local.", host_name.trim_end_matches('.'));
        let mut props = std::collections::HashMap::new();
        props.insert(TXT_PROTO_KEY.to_string(), format!("wdl/{}", wd_protocol::Version::CURRENT));
        props.insert(TXT_FP_KEY.to_string(), fingerprint_short.to_string());
        let service = ServiceInfo::new(SERVICE_TYPE, &instance, &host, ip, port, props)
            .map_err(|e| anyhow::anyhow!("invalid advertisement: {e}"))?;
        daemon
            .register(service)
            .map_err(|e| anyhow::anyhow!("mDNS registration failed: {e}"))?;
        tracing::info!(instance = %instance, %host, %ip, port, "advertising via mDNS");
        Ok(Self { instance_name: instance, daemon })
    }

    pub fn instance_name(&self) -> &str {
        &self.instance_name
    }

    pub fn stop(self) {
        if let Err(e) = self.daemon.unregister(&format!(
            "{}.{}",
            self.instance_name,
            SERVICE_TYPE
        )) {
            tracing::warn!(error = %e, "mDNS unregister failed");
        }
        let _ = self.daemon.shutdown();
    }
}

fn sanitize_instance(name: &str) -> String {
    let cleaned: String = name
        .trim()
        .chars()
        .take(48)
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == ' ' { c } else { '-' })
        .collect();
    if cleaned.is_empty() {
        "Wireless Display".to_string()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_names_are_sanitized() {
        assert_eq!(sanitize_instance("fedora box"), "fedora box");
        assert_eq!(sanitize_instance("  weird@name!  "), "weird-name-");
        assert_eq!(sanitize_instance(""), "Wireless Display");
        assert_eq!(sanitize_instance("x").len(), 1);
    }

    #[test]
    fn advertisement_registers_and_stops_cleanly() {
        // Loopback works for mdns-sd registration even without a LAN.
        let adv = Advertisement::start(
            "Test Receiver",
            "testhost",
            "127.0.0.1".parse().unwrap(),
            48_321,
            "abcdef123456",
        )
        .expect("advertisement should start on loopback");
        assert_eq!(adv.instance_name(), "Test Receiver");
        adv.stop();
    }
}
