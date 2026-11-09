//! 같은 네트워크의 방을 찾는다.
//! 공개방은 mDNS로 광고하고, 비공개방은 코드를 가진 사람이 UDP 브로드캐스트로 물어본다.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::time::timeout;

use crate::protocol::PROTOCOL_VERSION;

/// "ProxiTalk 방"이라는 서비스 종류. 같은 종류끼리만 서로 보인다.
const SERVICE_TYPE: &str = "_proxitalk._tcp.local.";

/// 비공개방 찾기: 참가자의 질문과 방장의 대답
const PROBE: &[u8] = b"PROXITALK-WHERE";
const PROBE_REPLY: &[u8] = b"PROXITALK-HERE";
/// 와이파이 브로드캐스트는 잘 유실되므로 여러 번 물어본다. (최대 3초)
const PROBE_ATTEMPTS: usize = 6;
const PROBE_INTERVAL: Duration = Duration::from_millis(500);

/// 방 목록 화면에 보여줄 방 하나
#[derive(Debug, Clone)]
pub struct RoomInfo {
    /// mDNS 전체 이름. 방마다 고유하므로 식별자로 쓴다.
    pub id: String,
    pub name: String,
    pub host: String,
    /// 방장에게 접속할 수 있는 주소 후보들
    pub addrs: Vec<SocketAddr>,
}

pub enum RoomEvent {
    Found(RoomInfo),
    Lost { id: String },
}

/// 광고 중인 방. stop()을 부르면 광고를 내린다.
pub struct Advertiser {
    daemon: ServiceDaemon,
    fullname: String,
}

/// 방장: "여기 이런 방이 있어요"를 네트워크에 알린다.
pub fn advertise(room_name: &str, host_nickname: &str, port: u16) -> Result<Advertiser> {
    let daemon = ServiceDaemon::new().context("mDNS를 시작할 수 없습니다")?;

    let id = unique_id();
    let host_name = format!("proxitalk-{id}.local.");
    let version = PROTOCOL_VERSION.to_string();
    // TXT 레코드: 방에 대한 부가 정보 (키=값 목록)
    let properties = [
        ("name", room_name),
        ("host", host_nickname),
        ("ver", version.as_str()),
    ];

    // IP는 비워두고 enable_addr_auto(): 모든 네트워크 카드의 IP를 자동으로 채워 광고한다.
    let info = ServiceInfo::new(SERVICE_TYPE, &id, &host_name, (), port, &properties[..])?
        .enable_addr_auto();
    let fullname = info.get_fullname().to_string();
    daemon
        .register(info)
        .context("mDNS 광고를 등록할 수 없습니다")?;

    Ok(Advertiser { daemon, fullname })
}

impl Advertiser {
    /// 광고를 내린다. "goodbye" 패킷을 보내서 다른 기기의 방 목록에서 바로 사라지게 한다.
    pub fn stop(self) {
        if let Ok(status) = self.daemon.unregister(&self.fullname) {
            let _ = status.recv_timeout(Duration::from_secs(1));
        }
        let _ = self.daemon.shutdown();
    }
}

/// 멤버: 네트워크의 공개방을 계속 찾는다. 찾거나 사라질 때마다 채널로 알려준다.
pub fn browse() -> Result<mpsc::Receiver<RoomEvent>> {
    let daemon = ServiceDaemon::new().context("mDNS를 시작할 수 없습니다")?;
    let events = daemon.browse(SERVICE_TYPE)?;
    let (tx, rx) = mpsc::channel(32);
    let my_version = PROTOCOL_VERSION.to_string();

    tokio::spawn(async move {
        loop {
            let event = tokio::select! {
                event = events.recv_async() => match event {
                    Ok(event) => event,
                    Err(_) => break,
                },
                // 방 목록 화면이 닫혀서 받는 쪽이 사라지면 검색을 끝낸다.
                _ = tx.closed() => break,
            };

            let room_event = match event {
                ServiceEvent::ServiceResolved(service) => {
                    if service.get_property_val_str("ver") != Some(my_version.as_str()) {
                        continue; // 버전이 다른 앱의 방은 보여주지 않는다.
                    }
                    let port = service.get_port();
                    let addrs = service
                        .get_addresses_v4()
                        .into_iter()
                        // 127.x(자기 자신)와 169.254.x(자동 할당)는 다른 기기에서 쓸 수 없다.
                        .filter(|ip| !ip.is_loopback() && !ip.is_link_local())
                        .map(|ip| SocketAddr::new(IpAddr::V4(ip), port))
                        .collect::<Vec<_>>();
                    if addrs.is_empty() {
                        continue; // 쓸 수 있는 주소가 아직 없다. 다음 갱신을 기다린다.
                    }
                    RoomEvent::Found(RoomInfo {
                        id: service.get_fullname().to_string(),
                        name: service
                            .get_property_val_str("name")
                            .unwrap_or("?")
                            .to_string(),
                        host: service
                            .get_property_val_str("host")
                            .unwrap_or("?")
                            .to_string(),
                        addrs,
                    })
                }
                ServiceEvent::ServiceRemoved(_, fullname) => RoomEvent::Lost { id: fullname },
                _ => continue,
            };

            if tx.send(room_event).await.is_err() {
                break;
            }
        }
        let _ = daemon.shutdown();
    });

    Ok(rx)
}

/// 방장(비공개방): 방 포트로 들어오는 질문에 대답한다. 먼저 나서서 알리지는 않는다.
pub async fn answer_probes(port: u16) -> Result<()> {
    let socket = UdpSocket::bind(("0.0.0.0", port))
        .await
        .with_context(|| format!("UDP {port}번 포트를 열 수 없습니다"))?;

    tokio::spawn(async move {
        let mut buf = [0u8; 32];
        loop {
            match socket.recv_from(&mut buf).await {
                Ok((len, from)) if &buf[..len] == PROBE => {
                    if let Err(e) = socket.send_to(PROBE_REPLY, from).await {
                        tracing::warn!("{from}: 비공개방 찾기 응답 실패: {e}");
                    }
                }
                Ok(_) => {} // 모르는 패킷은 무시한다.
                Err(e) => tracing::warn!("비공개방 찾기 수신 실패: {e}"),
            }
        }
    });
    Ok(())
}

/// 참가자: 같은 네트워크 전체에 물어보고, 대답한 기기의 주소를 돌려준다.
pub async fn find_private_room(port: u16) -> Result<SocketAddr> {
    let socket = UdpSocket::bind(("0.0.0.0", 0)).await?;
    socket.set_broadcast(true)?;

    let mut buf = [0u8; 32];
    for _ in 0..PROBE_ATTEMPTS {
        socket.send_to(PROBE, (Ipv4Addr::BROADCAST, port)).await?;
        // 받은 패킷의 "보낸 사람 주소"가 곧 방장의 IP다.
        match timeout(PROBE_INTERVAL, socket.recv_from(&mut buf)).await {
            Ok(Ok((len, from))) if &buf[..len] == PROBE_REPLY => {
                return Ok(SocketAddr::new(from.ip(), port));
            }
            _ => {} // 시간 초과나 모르는 패킷이면 다시 물어본다.
        }
    }
    bail!(
        "이 네트워크에서 방을 찾지 못했습니다. 코드와 와이파이를 확인하거나, 방장 화면의 IP로 join <IP>/<코드> 를 시도해 보세요"
    )
}

/// 방마다 겹치지 않는 짧은 ID (현재 시각 나노초와 프로세스 번호를 섞는다)
fn unique_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("{:08x}", nanos ^ std::process::id().rotate_left(16))
}