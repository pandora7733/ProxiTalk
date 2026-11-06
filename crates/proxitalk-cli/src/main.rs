use anyhow::{Context, Result};
use proxitalk_core::{client, server};

const USAGE: &str = "\
사용법:
  proxitalk-cli host <닉네임> [포트]      방 열기 (기본 포트 9000)
  proxitalk-cli join <IP:포트> <닉네임>   방 접속";

#[tokio::main]
async fn main() -> Result<()> {
    println!("ProxiTalk v{}", proxitalk_core::version());

    let args: Vec<String> = std::env::args().collect();

    match args.get(1).map(String::as_str) {
        Some("host") => {
            let nickname = args.get(2).context("닉네임이 필요합니다")?;
            let port: u16 = match args.get(3) {
                Some(p) => p.parse().context("포트는 0~65535 사이 숫자여야 합니다")?,
                None => 9000,
            };

            server::start_server(port).await?;
            println!(
                "* {port}번 포트에 방을 열었습니다. 다른 기기에서: join <이 PC의 IP>:{port} <닉네임>"
            );

            // 방장도 일반 멤버처럼 자기 방에 접속한다.
            client::run_client(&format!("127.0.0.1:{port}"), nickname).await
        }
        Some("join") => {
            let addr = args
                .get(2)
                .context("접속할 주소가 필요합니다 (예: 192.168.0.12:9000)")?;
            let nickname = args.get(3).context("닉네임이 필요합니다")?;
            client::run_client(addr, nickname).await
        }
        _ => {
            println!("{USAGE}");
            Ok(())
        }
    }
}