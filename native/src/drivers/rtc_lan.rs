//! Socket ownership and LAN enforcement shared by the two actual WebRTC
//! protocols. No STUN/TURN, DNS candidates, ambient proxies, or hidden I/O.
use crate::{GatewayError, Result, network::is_local_address, transport::Socket};
use futures_util::{StreamExt, future::BoxFuture, stream::FuturesUnordered};
use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::Instant};
use str0m::{
    Candidate, Input, Rtc,
    net::{Protocol, Receive, Transmit},
};
use tokio::net::UdpSocket;
use tokio_tungstenite::MaybeTlsStream;
type Read = (
    Arc<UdpSocket>,
    Vec<u8>,
    std::io::Result<(usize, SocketAddr)>,
);
pub(super) struct RtcLan {
    sockets: HashMap<SocketAddr, Arc<UdpSocket>>,
    readers: FuturesUnordered<BoxFuture<'static, Read>>,
}
impl RtcLan {
    pub async fn new(ws: &Socket) -> Result<Self> {
        let mut primary = match ws.get_ref() {
            MaybeTlsStream::Plain(tcp) => tcp.local_addr(),
            MaybeTlsStream::Rustls(tls) => tls.get_ref().0.local_addr(),
            _ => return Err(GatewayError::new("DEVICE_OFFLINE")),
        }
        .map_err(|_| GatewayError::new("DEVICE_OFFLINE"))?;
        if !is_local_address(&primary.ip().to_string()) {
            return Err(GatewayError::new("INVALID_ADDRESS"));
        }
        primary.set_port(0);
        let mut addresses = vec![primary];
        let interfaces = tokio::task::spawn_blocking(if_addrs::get_if_addrs)
            .await
            .map_err(|_| GatewayError::new("DEVICE_OFFLINE"))?
            .map_err(|_| GatewayError::new("DEVICE_OFFLINE"))?;
        for interface in interfaces {
            let ip = interface.ip();
            if ip.is_ipv4()
                && is_local_address(&ip.to_string())
                && !addresses.iter().any(|a| a.ip() == ip)
                && !matches!(
                    interface.oper_status,
                    if_addrs::IfOperStatus::Down
                        | if_addrs::IfOperStatus::NotPresent
                        | if_addrs::IfOperStatus::LowerLayerDown
                )
            {
                addresses.push(SocketAddr::new(ip, 0));
            }
        }
        if addresses.len() > 16 {
            return Err(GatewayError::message(
                "RESOURCE_LIMIT",
                "At most sixteen local ICE interfaces are supported",
            ));
        }
        let mut sockets = HashMap::new();
        for address in addresses {
            match UdpSocket::bind(address).await {
                Ok(socket) => {
                    sockets.insert(
                        socket
                            .local_addr()
                            .map_err(|_| GatewayError::new("DEVICE_OFFLINE"))?,
                        Arc::new(socket),
                    );
                }
                Err(_) if address.ip() != primary.ip() => (),
                Err(_) => return Err(GatewayError::new("DEVICE_OFFLINE")),
            }
        }
        let readers = FuturesUnordered::new();
        for socket in sockets.values() {
            readers.push(read(socket.clone(), vec![0; 65536]));
        }
        Ok(Self { sockets, readers })
    }
    pub fn add_candidates(&self, rtc: &mut Rtc) -> Result<()> {
        for local in self.sockets.keys() {
            rtc.add_local_candidate(
                Candidate::host(*local, "udp").map_err(|_| GatewayError::new("INVALID_ADDRESS"))?,
            )
            .ok_or_else(|| GatewayError::new("INVALID_ADDRESS"))?;
        }
        Ok(())
    }
    pub fn transmit(&self, packet: &Transmit) -> Result<()> {
        if !is_local_address(&packet.destination.ip().to_string()) {
            return Err(GatewayError::new("INVALID_ADDRESS"));
        }
        let socket = self
            .sockets
            .get(&packet.source)
            .ok_or_else(|| GatewayError::new("DEVICE_OFFLINE"))?;
        // Datagram buffers are never queued here. Dropping a congested audio
        // datagram is preferable to stale playback; ICE/DTLS/SCTP themselves
        // own their protocol retransmission, never application HID replay.
        let _ = socket.try_send_to(&packet.contents, packet.destination);
        Ok(())
    }
    pub async fn receive(&mut self, rtc: &mut Rtc) -> Result<()> {
        let (socket, buffer, result) = self
            .readers
            .next()
            .await
            .ok_or_else(|| GatewayError::new("DEVICE_OFFLINE"))?;
        match result {
            Ok((length, source)) if is_local_address(&source.ip().to_string()) => {
                let destination = socket
                    .local_addr()
                    .map_err(|_| GatewayError::new("DEVICE_OFFLINE"))?;
                if let Ok(contents) = buffer[..length].try_into() {
                    let _ = rtc.handle_input(Input::Receive(
                        Instant::now(),
                        Receive {
                            proto: Protocol::Udp,
                            source,
                            destination,
                            contents,
                        },
                    ));
                }
            }
            Ok(_) => (),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::ConnectionRefused
                        | std::io::ErrorKind::NetworkUnreachable
                        | std::io::ErrorKind::HostUnreachable
                        | std::io::ErrorKind::Interrupted
                ) => {}
            Err(_) => return Err(GatewayError::new("DEVICE_OFFLINE")),
        }
        self.readers.push(read(socket, buffer));
        Ok(())
    }
}
fn read(socket: Arc<UdpSocket>, mut buffer: Vec<u8>) -> BoxFuture<'static, Read> {
    Box::pin(async move {
        let received = socket.recv_from(&mut buffer).await;
        (socket, buffer, received)
    })
}
