use crate::model::{AppConfig, HdHomeRunDeviceConfig, HdHomeRunFlags};
use log::{error, info, trace};
use socket2::{Domain, Protocol, Socket, Type};
use std::{
    net::{Ipv4Addr, SocketAddr, UdpSocket as StdUdpSocket},
    sync::Arc,
    time::Duration,
};
use tokio::net::UdpSocket;
use tokio_util::sync::CancellationToken;

const SSDP_GROUP: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);
const SSDP_PORT: u16 = 1900;

fn create_ssdp_response(device: &HdHomeRunDeviceConfig, server_host: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\n\
        Cache-Control: max-age=1800\r\n\
        LOCATION: http://{}:{}/device.xml\r\n\
        SERVER: Tuliprox/1.0 UPnP/1.1 Tuliprox-HDHR/1.0\r\n\
        ST: urn:schemas-upnp-org:device:MediaServer:1\r\n\
        USN: uuid:{}\r\n\
        \r\n",
        server_host, device.port, device.device_udn
    )
}

async fn ssdp_task_loop(socket: UdpSocket, app_config: Arc<AppConfig>, server_host: String) {
    let mut buf = [0; 1024];
    loop {
        let (len, remote_addr) = match socket.recv_from(&mut buf).await {
            Ok(result) => result,
            Err(e) => {
                error!("HDHomeRun SSDP socket error: {e}");
                tokio::time::sleep(Duration::from_secs(1)).await; // Prevent spamming logs on error
                continue;
            }
        };

        let request = String::from_utf8_lossy(&buf[..len]);
        if !request.starts_with("M-SEARCH") {
            continue;
        }
        let req = request.to_ascii_lowercase();
        if !req.contains(r#"man: "ssdp:discover""#) {
            continue;
        }
        // Extract ST and MX (defaults)
        let st = req
            .lines()
            .find_map(|l| l.strip_prefix("st:").map(|v| v.trim().to_string()))
            .unwrap_or_else(|| "ssdp:all".to_string());
        let mx: u64 = req.lines().find_map(|l| l.strip_prefix("mx:").and_then(|v| v.trim().parse().ok())).unwrap_or(1);
        // Normalize to the set we support
        let supported = ["urn:schemas-upnp-org:device:mediaserver:1", "upnp:rootdevice", "ssdp:all"];
        if !supported.contains(&st.as_str()) {
            continue;
        }
        // Randomized delay per MX
        let delay_ms = (fastrand::u64(0..=mx * 1000)).min(2000);
        if delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        }

        trace!("Received HDHomeRun M-SEARCH from {remote_addr}");
        let hdhomerun_guard = app_config.hdhomerun.load();
        if let Some(hd_config) = &*hdhomerun_guard {
            if hd_config.flags.contains(HdHomeRunFlags::Enabled) {
                for device in &hd_config.devices {
                    if device.t_enabled {
                        let response = create_ssdp_response(device, &server_host);
                        if let Err(e) = socket.send_to(response.as_bytes(), remote_addr).await {
                            error!("Failed to send SSDP response to {remote_addr}: {e}");
                        } else {
                            trace!("Sent SSDP response for device '{}' to {remote_addr}", device.name);
                        }
                    }
                }
            }
        }
    }
}

pub fn spawn_ssdp_discover_task(app_config: Arc<AppConfig>, server_host: String, cancel_token: CancellationToken) {
    tokio::spawn(async move {
        let addr = SocketAddr::from((Ipv4Addr::UNSPECIFIED, SSDP_PORT));
        let std_socket = match Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)) {
            Ok(s) => s,
            Err(e) => {
                error!("Failed to create UDP socket for SSDP discovery: {e}");
                return;
            }
        };
        if let Err(e) = std_socket.set_reuse_address(true) {
            error!("Failed to set reuse_address on SSDP socket: {e}");
        }
        #[cfg(not(windows))]
        if let Err(e) = std_socket.set_reuse_port(true) {
            error!("Failed to set reuse_port on SSDP socket: {e}");
        }
        if let Err(e) = std_socket.bind(&addr.into()) {
            error!("Failed to bind SSDP socket to {addr}: {e}");
            return;
        }
        if let Err(e) = std_socket.join_multicast_v4(&SSDP_GROUP, &Ipv4Addr::UNSPECIFIED) {
            error!("Failed to join SSDP multicast group: {e}");
            return;
        }
        let std_udp_socket: StdUdpSocket = std_socket.into();
        if let Err(e) = std_udp_socket.set_nonblocking(true) {
            error!("Failed to set SSDP socket to non-blocking: {e}");
            return;
        }
        match UdpSocket::from_std(std_udp_socket) {
            Ok(socket) => {
                info!("HDHomeRun SSDP discovery listener started on {addr}");
                tokio::select! {
                    () = ssdp_task_loop(socket, app_config, server_host) => {},
                    () = cancel_token.cancelled() => {
                        info!("HDHomeRun SSDP discovery listener shutting down.");
                    }
                }
            }
            Err(e) => error!("Failed to create tokio UdpSocket for SSDP discovery: {e}"),
        }
    });
}
