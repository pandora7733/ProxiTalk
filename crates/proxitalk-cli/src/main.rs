mod app;
mod ui;

use std::fs::File;
use std::net::{IpAddr, UdpSocket};
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{Context, Result};
use proxitalk_core::{client, server};

const USAGE: &str = "\
사용법:
  proxitalk-cli host <닉네임> [포트]      방 열기 (기본 포트 9000)
  proxitalk-cli join <IP:포트> <닉네임>   방 접속";

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();

    // (접속할 주소, 닉네임, 화면 제목)을 정한다.
    let (addr, nickname, title) = match args.get(1).map(String::as_str) {
        Some("host") => {
            let nickname = args.get(2).context("닉네임이 필요합니다")?;
            let port: u16 = match args.get(3) {
                Some(p) => p.parse().context("포트는 0~65535 사이 숫자여야 합니다")?,
                None => 9000,
            };
            init_logging(nickname)?;
            server::start_server(port).await?;

            let ip = local_ip().map_or("<이 PC의 IP>".to_string(), |ip| ip.to_string());
            let title = format!("내 방 · 접속 주소 {ip}:{port}");
            // 방장도 일반 멤버처럼 자기 방에 접속한다.
            (format!("127.0.0.1:{port}"), nickname.clone(), title)
        }
        Some("join") => {
            let addr = args
                .get(2)
                .context("접속할 주소가 필요합니다 (예: 192.168.0.12:9000)")?;
            let nickname = args.get(3).context("닉네임이 필요합니다")?;
            init_logging(nickname)?;
            (addr.clone(), nickname.clone(), addr.clone())
        }
        _ => {
            println!("ProxiTalk v{}\n{USAGE}", proxitalk_core::version());
            return Ok(());
        }
    };

    // 입장(Hello/Welcome)은 TUI를 켜기 전에 한다. 거절되면 평범한 에러로 출력된다.
    let session = client::connect(&addr, &nickname).await?;

    let terminal = ratatui::init();
    let result = app::run(terminal, session, title).await;
    ratatui::restore(); // 에러가 났어도 터미널은 반드시 원래대로 돌려놓는다.

    println!("로그 파일: {}", log_path(&nickname).display());
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