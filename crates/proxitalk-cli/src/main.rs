use anyhow::{Context, Result};
use proxitalk_core::chat;

const USAGE: &str = "\
사용법:
  proxitalk-cli host [포트]        방 열기 (기본 포트 9000)
  proxitalk-cli join <IP:포트>     방 접속";

#[tokio::main]
async fn main() -> Result<()> {
    println!("ProxiTalk v{}", proxitalk_core::version());

    let args: Vec<String> = std::env::args().collect();

    match args.get(1).map(String::as_str) {
        Some("host") => {
            let port: u16 = match args.get(2) {
                Some(p) => p.parse().context("포트는 0~65535 사이 숫자여야 합니다")?,
                None => 9000,
            };
            chat::run_host(port).await
        }
        Some("join") => {
            let addr = args
                .get(2)
                .context("접속할 주소가 필요합니다 (예: 192.168.0.12:9000)")?;
            chat::run_client(addr).await
        }
        _ => {
            println!("{USAGE}");
            Ok(())
        }
    }
}