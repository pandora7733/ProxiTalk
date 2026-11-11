//! 방장 ↔ 멤버 사이에 주고받는 메시지 정의와 프레이밍

use anyhow::{Result, bail};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// 서로 다른 버전의 앱끼리 접속하는 것을 막기 위한 번호
pub const PROTOCOL_VERSION: u32 = 3;

/// 프레임 하나의 최대 크기 (악의적이거나 망가진 길이 값 방어)
const MAX_FRAME_LEN: usize = 64 * 1024;

/// 채팅 한 건의 최대 글자 수. (JSON과 암호화를 더해도 프레임 크기를 넘지 않는다)
pub const MAX_CHAT_CHARS: usize = 2000;

pub type MemberId = u32;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemberInfo {
    pub id: MemberId,
    pub nickname: String,
}

/// 암호화 전, 인증 단계에서만 평문으로 주고받는 메시지. 비밀값은 들어가지 않는다.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum HandshakeMsg {
    /// 멤버 → 방장: 접속 직후 가장 먼저 보낸다.
    Start {
        version: u32,
        /// 초대 코드의 세대 번호. 방장이 어떤 비밀값과 비교할지 고르는 데 쓴다.
        generation: u8,
        pake: Vec<u8>,
    },
    /// 방장 → 멤버
    Reply { pake: Vec<u8> },
    /// 방장 → 멤버: 암호화된 Hello를 풀 수 있었다. (= 같은 코드를 알고 있다)
    Accepted,
    /// 방장 → 멤버: 인증 실패. 키가 다르면 암호문을 읽을 수 없어서 평문으로 보낸다.
    Rejected { reason: String },
}

/// 멤버 → 방장 (암호화됨)
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ClientMsg {
    /// 인증 직후 반드시 가장 먼저 보내야 하는 입장 요청
    Hello {
        nickname: String,
    },
    Chat {
        text: String,
    },
}

/// 방장 → 멤버
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ServerMsg {
    Welcome {
        your_id: MemberId,
        members: Vec<MemberInfo>,
    },
    Reject {
        reason: String,
    },
    Chat {
        from: MemberInfo,
        text: String,
    },
    MemberJoined {
        member: MemberInfo,
    },
    MemberLeft {
        member: MemberInfo,
    },
    Kicked,
}

/// [길이 4바이트][JSON 본문] 형태로 메시지 하나를 보낸다.
pub async fn write_frame<W, T>(writer: &mut W, msg: &T) -> Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    write_bytes(writer, &serde_json::to_vec(msg)?).await
}

/// 메시지 하나를 읽는다. 상대가 연결을 정상 종료했으면 `Ok(None)`.
pub async fn read_frame<R, T>(reader: &mut R) -> Result<Option<T>>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    match read_bytes(reader).await? {
        Some(body) => Ok(Some(serde_json::from_slice(&body)?)),
        None => Ok(None),
    }
}

/// [길이 4바이트][본문]을 한 버퍼에 담아 한 번에 쓴다.
pub async fn write_bytes<W>(writer: &mut W, body: &[u8]) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    if body.len() > MAX_FRAME_LEN {
        bail!("메시지가 너무 큽니다: {} 바이트", body.len());
    }

    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
    frame.extend_from_slice(body);
    writer.write_all(&frame).await?;
    Ok(())
}

/// 프레임 하나의 본문을 읽는다. 상대가 연결을 정상 종료했으면 `Ok(None)`.
pub async fn read_bytes<R>(reader: &mut R) -> Result<Option<Vec<u8>>>
where
    R: AsyncRead + Unpin,
{
    let len = match reader.read_u32().await {
        Ok(len) => len as usize,
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if len > MAX_FRAME_LEN {
        bail!("프레임이 너무 큽니다: {len} 바이트");
    }

    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).await?;
    Ok(Some(body))
}
