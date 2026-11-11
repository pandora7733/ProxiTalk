//! 멤버 쪽: 방장에 접속하고, 화면(UI)과는 채널로만 대화한다.
//! (방장 자신도 이 코드로 자기 방에 접속한다)

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::mpsc;
use tokio::time::timeout;

use crate::invite::{self, Invite};
use crate::protocol::{
    self, ClientMsg, HandshakeMsg, MemberId, MemberInfo, PROTOCOL_VERSION, ServerMsg,
};
use crate::secure::{self, Opener, Sealer};

/// 주소 하나에 TCP 연결을 시도하는 최대 시간
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
/// 인증을 끝내는 최대 시간
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
/// 인증 뒤 Welcome/Reject를 기다리는 최대 시간. 방장이 수락하는 시간(60초)보다 길어야 한다.
const WELCOME_TIMEOUT: Duration = Duration::from_secs(70);

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

/// "IP:포트" 문자열로 접속한다. (join 명령, 방장 자신)
pub async fn connect(addr: &str, nickname: &str, invite: Option<&Invite>) -> Result<Session> {
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host(addr)
        .await
        .with_context(|| format!("주소를 해석할 수 없습니다: {addr}"))?
        .collect();
    connect_any(&addrs, nickname, invite).await
}

/// 주소 후보들을 차례로 시도해서, 처음 연결된 곳으로 입장한다. (방 목록, 초대 코드)
pub async fn connect_any(
    addrs: &[SocketAddr],
    nickname: &str,
    invite: Option<&Invite>,
) -> Result<Session> {
    let stream = connect_first(addrs).await?;
    handshake(stream, nickname, invite).await
}

async fn connect_first(addrs: &[SocketAddr]) -> Result<TcpStream> {
    let mut last_error = anyhow!("접속할 주소가 없습니다");
    for addr in addrs {
        match timeout(CONNECT_TIMEOUT, TcpStream::connect(addr)).await {
            Ok(Ok(stream)) => return Ok(stream),
            Ok(Err(e)) => last_error = anyhow!("{addr} 에 연결할 수 없습니다: {e}"),
            Err(_) => last_error = anyhow!("{addr} 연결 시간 초과"),
        }
    }
    Err(last_error)
}

/// 인증 → Hello → Welcome/Reject 까지 처리하고, 성공하면 Session을 돌려준다.
async fn handshake(stream: TcpStream, nickname: &str, invite: Option<&Invite>) -> Result<Session> {
    let (mut reader, mut writer) = stream.into_split();

    let (sealer, mut opener) = timeout(
        HANDSHAKE_TIMEOUT,
        authenticate(&mut reader, &mut writer, nickname, invite),
    )
    .await
    .context("방장이 응답하지 않습니다")??;

    let welcome = timeout(WELCOME_TIMEOUT, opener.read(&mut reader))
        .await
        .context("방장이 수락하지 않았습니다")?
        // 코드를 모르는 가짜 방장은 올바른 암호문을 만들 수 없다.
        .context("방장을 확인할 수 없습니다")?;
    let (my_id, members) = match welcome {
        Some(ServerMsg::Welcome { your_id, members }) => (your_id, members),
        Some(ServerMsg::Reject { reason }) => bail!("입장 거절: {reason}"),
        Some(other) => bail!("예상하지 못한 응답: {other:?}"),
        None => bail!("방장이 응답 없이 연결을 끊었습니다"),
    };

    Ok(Session {
        my_id,
        members,
        outgoing: spawn_writer(writer, sealer),
        incoming: spawn_reader(reader, opener),
    })
}

/// PAKE로 키를 합의하고, 암호화된 Hello를 보낸다.
async fn authenticate(
    reader: &mut OwnedReadHalf,
    writer: &mut OwnedWriteHalf,
    nickname: &str,
    invite: Option<&Invite>,
) -> Result<(Sealer, Opener)> {
    let (password, generation) = match invite {
        Some(invite) => (
            invite::password_bytes(invite.secret).to_vec(),
            invite.generation,
        ),
        None => (invite::PUBLIC_PASSWORD.to_vec(), 0),
    };

    let (state, member_pake) = secure::start_as_member(&password);
    let start = HandshakeMsg::Start {
        version: PROTOCOL_VERSION,
        generation,
        pake: member_pake,
    };
    protocol::write_frame(writer, &start).await?;

    let host_pake = match protocol::read_frame(reader).await? {
        Some(HandshakeMsg::Reply { pake }) => pake,
        Some(HandshakeMsg::Rejected { reason }) => bail!("입장 거절: {reason}"),
        Some(other) => bail!("예상하지 못한 응답: {other:?}"),
        None => bail!("방장이 응답 없이 연결을 끊었습니다"),
    };
    let keys = secure::finish(state, &host_pake)?;
    let (mut sealer, opener) = keys.for_member();

    let hello = ClientMsg::Hello {
        nickname: nickname.to_string(),
    };
    sealer.write(writer, &hello).await?;

    match protocol::read_frame(reader).await? {
        Some(HandshakeMsg::Accepted) => Ok((sealer, opener)),
        Some(HandshakeMsg::Rejected { reason }) => bail!("입장 거절: {reason}"),
        Some(other) => bail!("예상하지 못한 응답: {other:?}"),
        None => bail!("방장이 응답 없이 연결을 끊었습니다"),
    }
}

/// 채널로 받은 메시지를 암호화해서 소켓에 쓰는 전용 태스크
fn spawn_writer(mut writer: OwnedWriteHalf, mut sealer: Sealer) -> mpsc::Sender<ClientMsg> {
    let (tx, mut rx) = mpsc::channel::<ClientMsg>(64);
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if let Err(e) = sealer.write(&mut writer, &msg).await {
                tracing::warn!("전송 실패: {e}");
                break;
            }
        }
        // 여기서 writer가 drop되면 TCP 연결의 쓰기 방향이 닫힌다 (= 방장에게 퇴장 신호)
    });
    tx
}

/// 소켓에서 프레임을 읽어 복호화한 뒤 채널로 넘기는 전용 태스크
fn spawn_reader(mut reader: OwnedReadHalf, mut opener: Opener) -> mpsc::Receiver<ServerMsg> {
    let (tx, rx) = mpsc::channel(64);
    tokio::spawn(async move {
        loop {
            match opener.read(&mut reader).await {
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
