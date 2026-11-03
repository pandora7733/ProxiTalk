//! M1: 1:1 TCP 채팅 (줄 단위 텍스트)

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

/// 방장: 포트를 열고 클라이언트 한 명이 접속할 때까지 기다린다.
pub async fn run_host(port: u16) -> Result<()> {
    // 0.0.0.0 = 이 PC의 모든 네트워크 인터페이스에서 접속을 받는다.
    let listener = TcpListener::bind(("0.0.0.0", port))
        .await
        .with_context(|| format!("{port}번 포트를 열 수 없습니다"))?;
    println!("[host] {port}번 포트에서 접속 대기 중...");

    let (stream, addr) = listener.accept().await?;
    println!("[host] {addr} 님이 접속했습니다.");

    chat_session(stream).await
}

/// 클라이언트: 방장 주소(IP:포트)로 접속한다.
pub async fn run_client(addr: &str) -> Result<()> {
    let stream = TcpStream::connect(addr)
        .await
        .with_context(|| format!("{addr} 에 연결할 수 없습니다"))?;
    println!("[client] {addr} 에 연결되었습니다.");

    chat_session(stream).await
}

/// 연결이 만들어진 뒤에는 방장/클라이언트 모두 똑같이 동작한다.
async fn chat_session(stream: TcpStream) -> Result<()> {
    // 하나의 소켓을 읽기 전용 / 쓰기 전용 두 개로 나눈다.
    let (reader, mut writer) = stream.into_split();
    let mut net_lines = BufReader::new(reader).lines();
    let mut keyboard = spawn_stdin_reader();

    println!("메시지를 입력하고 Enter. 종료는 Ctrl+D");

    loop {
        // 두 이벤트 중 "먼저 일어난 것"을 처리한다.
        tokio::select! {
            // 1) 내가 키보드로 한 줄 입력했다
            input = keyboard.recv() => {
                let Some(text) = input else {
                    println!("[종료] 입력을 마쳤습니다.");
                    break;
                };
                let packet = format!("{text}\n");
                if writer.write_all(packet.as_bytes()).await.is_err() {
                    println!("[연결 끊김] 메시지를 보낼 수 없습니다.");
                    break;
                }
            }
            // 2) 상대방이 보낸 한 줄이 도착했다
            line = net_lines.next_line() => {
                match line {
                    Ok(Some(text)) => println!("상대: {text}"),
                    Ok(None) => {
                        println!("[연결 종료] 상대방이 나갔습니다.");
                        break;
                    }
                    Err(e) => {
                        println!("[연결 끊김] {e}");
                        break;
                    }
                }
            }
        }
    }
    Ok(())
}

/// 키보드 입력은 별도의 OS 스레드에서 읽고, 채널로 async 쪽에 넘겨준다.
fn spawn_stdin_reader() -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel(32);
    std::thread::spawn(move || {
        for line in std::io::stdin().lines() {
            let Ok(line) = line else { break };
            // 받는 쪽(rx)이 사라졌으면 더 읽을 이유가 없다.
            if tx.blocking_send(line).is_err() {
                break;
            }
        }
        // 여기서 tx가 drop되면 rx.recv()가 None을 돌려준다 (= Ctrl+D)
    });
    rx
}