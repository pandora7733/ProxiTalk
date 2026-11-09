mod app;
mod lobby;
mod ui;

use std::fs::File;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{Context, Result, bail};
use proxitalk_core::client::Session;
use proxitalk_core::invite::{self, Invite};
use proxitalk_core::{client, discovery, server};

const USAGE: &str = "\
사용법:
  proxitalk-cli host <닉네임> <방이름> [포트]   공개방 열기 (포트 생략 시 자동 선택)
  proxitalk-cli private <닉네임> <방이름> [포트] 비공개방 열기 (목록에 안 보이고 초대 링크로만 입장)
  proxitalk-cli browse <닉네임>                같은 네트워크의 공개방 목록에서 골라 입장
  proxitalk-cli join <IP:포트> <닉네임>         주소로 직접 입장 (mDNS가 막힌 네트워크용)
  proxitalk-cli join <초대 링크> <닉네임>       초대 링크로 비공개방 입장";

const MAX_ROOM_NAME_CHARS: usize = 20;

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();

        match args.get(1).map(String::as_str) {
        Some(command @ ("host" | "private")) => {
            let nickname = args.get(2).context("닉네임이 필요합니다")?;
            let room_name = args.get(3).context("방 이름이 필요합니다")?;
            let port: u16 = match args.get(4) {
                Some(p) => p.parse().context("포트는 0~65535 사이 숫자여야 합니다")?,
                None => 0, // 0 = OS가 빈 포트를 골라준다
            };
            run_host(nickname, room_name, port, command == "private").await
        }
        Some("browse") => {
            let nickname = args.get(2).context("닉네임이 필요합니다")?;
            run_browse(nickname).await
        }
        Some("join") => {
            let target = args
                .get(2)
                .context("접속할 주소나 초대 링크가 필요합니다 (예: 192.168.0.12:9000)")?;
            let nickname = args.get(3).context("닉네임이 필요합니다")?;
            init_logging(nickname)?;

            let (session, title) = if invite::is_invite_link(target) {
                let invite: Invite = target.parse()?;
                let session =
                    client::connect_any(&[invite.addr], nickname, Some(&invite.token)).await?;
                (session, invite.addr.to_string())
            } else {
                (
                    client::connect(target, nickname, None).await?,
                    target.clone(),
                )
            };

            run_chat(session, title, None).await?;
            println!("로그 파일: {}", log_path(nickname).display());
            Ok(())
        }
        _ => {
            println!("ProxiTalk v{}\n{USAGE}", proxitalk_core::version());
            Ok(())
        }
    }
}

async fn run_host(nickname: &str, room_name: &str, port: u16, private: bool) -> Result<()> {
    let room_name = room_name.trim();
    if room_name.is_empty() || room_name.chars().count() > MAX_ROOM_NAME_CHARS {
        bail!("방 이름은 1~{MAX_ROOM_NAME_CHARS}자여야 합니다");
    }
    init_logging(nickname)?;

    // 비공개방만 토큰을 만든다.
    let token = if private {
        Some(invite::generate_token()?)
    } else {
        None
    };
    let port = server::start_server(port, token.clone()).await?;

    // 비공개방은 광고하지 않는다. 그래서 방 목록에 나타나지 않는다.
    let advertiser = if private {
        None
    } else {
        // mDNS가 실패해도 방은 계속 연다. (IP로 직접 join은 가능하므로)
        match discovery::advertise(room_name, nickname, port) {
            Ok(advertiser) => Some(advertiser),
            Err(e) => {
                tracing::warn!("mDNS 광고 실패, IP 직접 접속만 가능: {e:#}");
                None
            }
        }
    };

    let (title, invite_link) = match &token {
        Some(token) => {
            let ip =
                local_ip().context("이 PC의 IP를 찾을 수 없어 초대 링크를 만들 수 없습니다")?;
            let invite = Invite {
                addr: SocketAddr::new(ip, port),
                token: token.clone(),
            };
            (format!("{room_name} · 비공개"), Some(invite.to_string()))
        }
        None => {
            let ip = local_ip().map_or("<이 PC의 IP>".to_string(), |ip| ip.to_string());
            (format!("{room_name} · 내 방 · 접속 주소 {ip}:{port}"), None)
        }
    };

    // 방장도 일반 멤버처럼 자기 방에 접속한다. (비공개방이면 토큰도 똑같이 제시한다)
    let session = client::connect(&format!("127.0.0.1:{port}"), nickname, token.as_deref()).await?;
    let result = run_chat(session, title, invite_link).await;

    if let Some(advertiser) = advertiser {
        advertiser.stop();
    }
    println!("로그 파일: {}", log_path(nickname).display());
    result
}

async fn run_browse(nickname: &str) -> Result<()> {
    init_logging(nickname)?;
    let rooms = discovery::browse()?;

    let mut terminal = ratatui::init();
    let result = async {
        match lobby::run(&mut terminal, rooms, nickname).await? {
            Some((session, title)) => app::run(&mut terminal, session, title, None).await,
            None => Ok(()),
        }
    }
    .await;
    ratatui::restore(); // 에러가 났어도 터미널은 반드시 원래대로 돌려놓는다.

    println!("로그 파일: {}", log_path(nickname).display());
    result
}

/// 채팅 화면을 띄우고, 끝나면 터미널을 원래대로 돌려놓는다.
async fn run_chat(session: Session, title: String, invite: Option<String>) -> Result<()> {
    let mut terminal = ratatui::init();
    let result = app::run(&mut terminal, session, title, invite).await;
    ratatui::restore();
    result
}

/// TUI가 화면 전체를 쓰므로, 로그는 화면 대신 임시 폴더의 파일로 보낸다.
fn init_logging(nickname: &str) -> Result<()> {
    let path = log_path(nickname);
    let file = File::create(&path)
        .with_context(|| format!("로그 파일을 만들 수 없습니다: {}", path.display()))?;
    tracing_subscriber::fmt()
        .with_writer(Mutex::new(file))
        .with_ansi(false)
        .init();
    Ok(())
}

fn log_path(nickname: &str) -> PathBuf {
    std::env::temp_dir().join(format!("proxitalk-{nickname}.log"))
}

/// 이 PC의 LAN IP를 알아낸다.
/// UDP는 connect해도 패킷을 보내지 않는다. OS가 "이 목적지로 가려면 어느 IP를 쓸지"만 정해준다.
fn local_ip() -> Option<IpAddr> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    Some(socket.local_addr().ok()?.ip())
}