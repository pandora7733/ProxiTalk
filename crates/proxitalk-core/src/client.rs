//! 멤버 쪽: 방장에 접속하고, 화면(UI)과는 채널로만 대화한다.
//! (방장 자신도 이 코드로 자기 방에 접속한다)

use anyhow::{Context, Result, bail};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::mpsc;

use crate::protocol::{self, ClientMsg, MemberId, MemberInfo, PROTOCOL_VERSION, ServerMsg};

/// 입장에 성공한 연결. UI는 이 구조체만 알면 된다.
pub struct Session {
    pub my_id: MemberId,
    /// 입장 시점의 멤버 목록
    pub members: Vec<MemberInfo>,
    /// UI → 방장: 여기에 넣으면 쓰기 태스크가 소켓으로 보낸다.
    pub outgoing: mpsc::Sender<ClientMsg>,
    /// 방장 → UI: 연결이 끊기면 recv()가 None을 돌려준다.
    pub incoming: mpsc::Receiver<ServerMsg>,
}

/// 접속 → Hello → Welcome/Reject 까지 처리하고, 성공하면 Session을 돌려준다.
pub async fn connect(addr: &str, nickname: &str) -> Result<Session> {
    let stream = TcpStream::connect(addr)
        .await
        .with_context(|| format!("{addr} 에 연결할 수 없습니다"))?;
    let (mut reader, mut writer) = stream.into_split();

    let hello = ClientMsg::Hello {
        version: PROTOCOL_VERSION,
        nickname: nickname.to_string(),
    };
    protocol::write_frame(&mut writer, &hello).await?;

    let (my_id, members) = match protocol::read_frame(&mut reader).await? {
        Some(ServerMsg::Welcome { your_id, members }) => (your_id, members),
        Some(ServerMsg::Reject { reason }) => bail!("입장 거절: {reason}"),
        Some(other) => bail!("예상하지 못한 응답: {other:?}"),
        None => bail!("방장이 응답 없이 연결을 끊었습니다"),
    };

    Ok(Session {
        my_id,
        members,
        outgoing: spawn_writer(writer),
        incoming: spawn_reader(reader),
    })
}

/// 채널로 받은 메시지를 소켓에 쓰는 전용 태스크
fn spawn_writer(mut writer: OwnedWriteHalf) -> mpsc::Sender<ClientMsg> {
    let (tx, mut rx) = mpsc::channel::<ClientMsg>(64);
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if let Err(e) = protocol::write_frame(&mut writer, &msg).await {
                tracing::warn!("전송 실패: {e}");
                break;
            }
        }
        // 여기서 writer가 drop되면 TCP 연결의 쓰기 방향이 닫힌다 (= 방장에게 퇴장 신호)
    });
    tx
}

/// 소켓에서 프레임을 읽어 채널로 넘기는 전용 태스크
fn spawn_reader(mut reader: OwnedReadHalf) -> mpsc::Receiver<ServerMsg> {
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
                    tracing::warn!("수신 오류: {e}");
                    break;
                }
            }
        }
    });
    rx
}