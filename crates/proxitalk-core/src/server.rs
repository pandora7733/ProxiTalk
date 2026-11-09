//! 방장 쪽: 접속을 받고, 룸 액터가 멤버 목록과 브로드캐스트를 담당한다.

use std::collections::HashMap;
use std::time::Duration;
use std::sync::Arc;
use subtle::ConstantTimeEq;

use anyhow::{Context, Result, bail};
use tokio::net::tcp::OwnedReadHalf;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{mpsc, oneshot};

use crate::protocol::{self, ClientMsg, MemberId, MemberInfo, PROTOCOL_VERSION, ServerMsg};

const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_NICKNAME_CHARS: usize = 20;
/// 멤버 한 명에게 보낼 메시지를 쌓아둘 수 있는 개수
const OUTBOX_SIZE: usize = 64;

type JoinReply = Result<(MemberId, Vec<MemberInfo>), String>;

/// 연결 태스크 → 룸 액터로 보내는 명령
enum RoomCmd {
    Join {
        nickname: String,
        outbox: mpsc::Sender<ServerMsg>,
        reply: oneshot::Sender<JoinReply>,
    },
    Chat {
        from: MemberId,
        text: String,
    },
    Leave {
        id: MemberId,
    },
}

struct Member {
    info: MemberInfo,
    outbox: mpsc::Sender<ServerMsg>,
}

/// 포트를 열고 룸 액터와 접속 대기 루프를 백그라운드로 띄운다.
/// port가 0이면 OS가 비어 있는 포트를 골라준다. 실제로 열린 포트 번호를 돌려준다.
/// access_token이 Some이면 비공개방: 같은 토큰을 제시한 사람만 입장할 수 있다.
pub async fn start_server(port: u16, access_token: Option<String>) -> Result<u16> {
    let listener = TcpListener::bind(("0.0.0.0", port))
        .await
        .with_context(|| format!("{port}번 포트를 열 수 없습니다"))?;
    let port = listener.local_addr()?.port();

    let (room_tx, room_rx) = mpsc::channel(256);
    tokio::spawn(room_task(room_rx));
    tokio::spawn(accept_loop(listener, room_tx, access_token.map(Arc::from)));
    Ok(port)
}

async fn accept_loop(
    listener: TcpListener,
    room: mpsc::Sender<RoomCmd>,
    access_token: Option<Arc<str>>,
) {
    loop {
        match listener.accept().await {
            Ok((stream, addr)) => {
                // 접속자마다 독립된 태스크를 하나씩 만든다.
                let room = room.clone();
                let access_token = access_token.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_connection(stream, room, access_token).await {
                        tracing::warn!("{addr}: {e:#}");
                    }
                });
            }
            Err(e) => tracing::warn!("accept 실패: {e}"),
        }
    }
}

/// 접속자 한 명을 담당한다: 입장 처리 → 쓰기 태스크 생성 → 읽기 루프
async fn handle_connection(
    stream: TcpStream,
    room: mpsc::Sender<RoomCmd>,
    access_token: Option<Arc<str>>,
) -> Result<()> {
    let (mut reader, mut writer) = stream.into_split();

    // ── 1단계: 입장 전. Hello 외에는 아무것도 받지 않는다 ──
    let first = tokio::time::timeout(HELLO_TIMEOUT, protocol::read_frame(&mut reader))
        .await
        .context("Hello를 기다리다 시간 초과")??;

    let (nickname, token) = match first {
        Some(ClientMsg::Hello {
            version,
            nickname,
            token,
        }) if version == PROTOCOL_VERSION => (nickname, token),
        Some(ClientMsg::Hello { version, .. }) => {
            let reason = format!("버전이 다릅니다 (방장 {PROTOCOL_VERSION}, 내 앱 {version})");
            protocol::write_frame(&mut writer, &ServerMsg::Reject { reason }).await?;
            bail!("버전 불일치로 거절");
        }
        Some(_) => bail!("Hello 전에 다른 메시지를 보냄"),
        None => return Ok(()), // 아무 말 없이 끊음
    };

    // 룸에 들어가기 전에 초대 토큰부터 검사한다.
    if !token_matches(access_token.as_deref(), token.as_deref()) {
        let reason = "초대 링크가 없거나 올바르지 않습니다".to_string();
        protocol::write_frame(&mut writer, &ServerMsg::Reject { reason }).await?;
        bail!("초대 토큰이 틀려 거절");
    }


    // 룸 액터에게 입장을 요청하고, oneshot 채널로 결과를 기다린다.
    let (outbox_tx, mut outbox_rx) = mpsc::channel(OUTBOX_SIZE);
    let (reply_tx, reply_rx) = oneshot::channel();
    room.send(RoomCmd::Join {
        nickname,
        outbox: outbox_tx,
        reply: reply_tx,
    })
    .await?;

    let (my_id, members) = match reply_rx.await? {
        Ok(joined) => joined,
        Err(reason) => {
            protocol::write_frame(&mut writer, &ServerMsg::Reject { reason }).await?;
            return Ok(());
        }
    };
    protocol::write_frame(
        &mut writer,
        &ServerMsg::Welcome {
            your_id: my_id,
            members,
        },
    )
    .await?;

    // ── 2단계: 입장 후 ──
    // 쓰기 담당: 룸이 outbox에 넣어준 메시지를 소켓으로 보낸다.
    tokio::spawn(async move {
        while let Some(msg) = outbox_rx.recv().await {
            if protocol::write_frame(&mut writer, &msg).await.is_err() {
                break;
            }
        }
    });

    // 읽기 담당: 이 태스크가 직접 한다. 끝나면 이유와 상관없이 퇴장 처리.
    let result = read_loop(&mut reader, my_id, &room).await;
    let _ = room.send(RoomCmd::Leave { id: my_id }).await;
    result
}

/// 공개방(expected가 None)은 누구나 통과. 비공개방은 토큰이 같아야 통과.
fn token_matches(expected: Option<&str>, given: Option<&str>) -> bool {
    let Some(expected) = expected else {
        return true;
    };
    // ==는 다른 글자를 만나는 순간 멈춰서, 걸린 시간으로 토큰을 추측할 여지가 생긴다.
    // ct_eq는 내용과 상관없이 항상 끝까지 비교한다.
    given.is_some_and(|given| bool::from(given.as_bytes().ct_eq(expected.as_bytes())))
}

async fn read_loop(
    reader: &mut OwnedReadHalf,
    my_id: MemberId,
    room: &mpsc::Sender<RoomCmd>,
) -> Result<()> {
    loop {
        match protocol::read_frame(reader).await? {
            Some(ClientMsg::Chat { text }) => {
                room.send(RoomCmd::Chat { from: my_id, text }).await?;
            }
            Some(ClientMsg::Hello { .. }) => {} // 이미 입장했으므로 무시
            None => return Ok(()),
        }
    }
}

/// 룸 액터: 방 상태를 "혼자" 소유하고, 명령을 하나씩 순서대로 처리한다.
async fn room_task(mut commands: mpsc::Receiver<RoomCmd>) {
    let mut members: HashMap<MemberId, Member> = HashMap::new();
    let mut next_id: MemberId = 1;

    while let Some(cmd) = commands.recv().await {
        match cmd {
            RoomCmd::Join {
                nickname,
                outbox,
                reply,
            } => {
                let nickname = nickname.trim().to_string();
                if let Err(reason) = validate_nickname(&nickname, &members) {
                    let _ = reply.send(Err(reason));
                    continue;
                }

                let info = MemberInfo {
                    id: next_id,
                    nickname,
                };
                next_id += 1;

                // 기존 멤버들에게 먼저 알리고, 그다음 목록에 추가한다.
                broadcast(
                    &members,
                    ServerMsg::MemberJoined {
                        member: info.clone(),
                    },
                );
                members.insert(
                    info.id,
                    Member {
                        info: info.clone(),
                        outbox,
                    },
                );

                let mut list: Vec<MemberInfo> = members.values().map(|m| m.info.clone()).collect();
                list.sort_by_key(|m| m.id);
                let _ = reply.send(Ok((info.id, list)));
            }
            RoomCmd::Chat { from, text } => {
                let Some(sender) = members.get(&from) else {
                    continue;
                };
                let msg = ServerMsg::Chat {
                    from: sender.info.clone(),
                    text,
                };
                broadcast(&members, msg);
            }
            RoomCmd::Leave { id } => {
                if let Some(left) = members.remove(&id) {
                    broadcast(&members, ServerMsg::MemberLeft { member: left.info });
                }
            }
        }
    }
}

fn validate_nickname(nickname: &str, members: &HashMap<MemberId, Member>) -> Result<(), String> {
    if nickname.is_empty() {
        return Err("닉네임이 비어 있습니다".into());
    }
    if nickname.chars().count() > MAX_NICKNAME_CHARS {
        return Err(format!("닉네임은 {MAX_NICKNAME_CHARS}자 이하여야 합니다"));
    }
    if members.values().any(|m| m.info.nickname == nickname) {
        return Err(format!("'{nickname}' 닉네임은 이미 사용 중입니다"));
    }
    Ok(())
}

/// 모든 멤버의 outbox에 메시지를 넣는다. 절대 기다리지 않는다.
fn broadcast(members: &HashMap<MemberId, Member>, msg: ServerMsg) {
    for member in members.values() {
        if let Err(TrySendError::Full(_)) = member.outbox.try_send(msg.clone()) {
            tracing::warn!(
                "{} 님의 수신이 밀려 메시지를 버렸습니다",
                member.info.nickname
            );
        }
    }
}
