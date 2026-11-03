//! 멤버 쪽: 방장에 접속해서 채팅한다. (방장 자신도 이 코드로 자기 방에 접속한다)

use anyhow::{Context, Result, bail};
use tokio::net::TcpStream;
use tokio::net::tcp::OwnedReadHalf;
use tokio::sync::mpsc;

use crate::protocol::{self, ClientMsg, MemberId, PROTOCOL_VERSION, ServerMsg};

pub async fn run_client(addr: &str, nickname: &str) -> Result<()> {
    let stream = TcpStream::connect(addr)
        .await
        .with_context(|| format!("{addr} 에 연결할 수 없습니다"))?;
    let (mut reader, mut writer) = stream.into_split();

    // 입장 요청
    let hello = ClientMsg::Hello {
        version: PROTOCOL_VERSION,
        nickname: nickname.to_string(),
    };
    protocol::write_frame(&mut writer, &hello).await?;

    // 첫 응답은 반드시 Welcome 또는 Reject
    let my_id = match protocol::read_frame(&mut reader).await? {
        Some(ServerMsg::Welcome { your_id, members }) => {
            let names: Vec<&str> = members.iter().map(|m| m.nickname.as_str()).collect();
            println!(
                "* 입장했습니다. 현재 멤버 {}명: {}",
                members.len(),
                names.join(", ")
            );
            your_id
        }
        Some(ServerMsg::Reject { reason }) => bail!("입장 거절: {reason}"),
        Some(other) => bail!("예상하지 못한 응답: {other:?}"),
        None => bail!("방장이 응답 없이 연결을 끊었습니다"),
    };

    println!("메시지를 입력하고 Enter. 나가기는 Ctrl+D");

    let mut keyboard = spawn_stdin_reader();
    let mut incoming = spawn_server_reader(reader);

    loop {
        tokio::select! {
            input = keyboard.recv() => {
                let Some(text) = input else {
                    println!("* 방을 나갑니다.");
                    break;
                };
                if text.trim().is_empty() {
                    continue;
                }
                protocol::write_frame(&mut writer, &ClientMsg::Chat { text })
                    .await
                    .context("메시지를 보낼 수 없습니다")?;
            }
            msg = incoming.recv() => {
                let Some(msg) = msg else {
                    println!("* 방과의 연결이 끊어졌습니다.");
                    break;
                };
                print_server_msg(&msg, my_id);
            }
        }
    }
    Ok(())
}

fn print_server_msg(msg: &ServerMsg, my_id: MemberId) {
    match msg {
        ServerMsg::Chat { from, text } => {
            let who: &str = if from.id == my_id {
                "나"
            } else {
                &from.nickname
            };
            println!("[{who}] {text}");
        }
        ServerMsg::MemberJoined { member } => println!("* {} 님이 입장했습니다.", member.nickname),
        ServerMsg::MemberLeft { member } => println!("* {} 님이 나갔습니다.", member.nickname),
        ServerMsg::Welcome { .. } | ServerMsg::Reject { .. } => {}
    }
}

/// 소켓에서 프레임을 읽는 일만 하는 전용 태스크. 읽은 메시지는 채널로 넘긴다.
fn spawn_server_reader(mut reader: OwnedReadHalf) -> mpsc::Receiver<ServerMsg> {
    let (tx, rx) = mpsc::channel(64);
    tokio::spawn(async move {
        loop {
            match protocol::read_frame(&mut reader).await {
                Ok(Some(msg)) => {
                    if tx.send(msg).await.is_err() {
                        break;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    eprintln!("* 연결 오류: {e}");
                    break;
                }
            }
        }
        // tx가 drop되면 incoming.recv()가 None을 받는다.
    });
    rx
}

/// 키보드 입력은 별도의 OS 스레드에서 읽고, 채널로 async 쪽에 넘겨준다.
fn spawn_stdin_reader() -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel(32);
    std::thread::spawn(move || {
        for line in std::io::stdin().lines() {
            let Ok(line) = line else { break };
            if tx.blocking_send(line).is_err() {
                break;
            }
        }
    });
    rx
}