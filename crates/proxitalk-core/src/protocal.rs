//! 방장 ↔ 멤버 사이에 주고받는 메시지 정의와 프레이밍

use anyhow::{Result, bail};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// 서로 다른 버전의 앱끼리 접속하는 것을 막기 위한 번호
pub const PROTOCOL_VERSION: u32 = 1;

/// 프레임 하나의 최대 크기 (악의적이거나 망가진 길이 값 방어)
const MAX_FRAME_LEN: usize = 64 * 1024;

pub type MemberId = u32;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemberInfo {
    pub id: MemberId,
    pub nickname: String,
}

/// 멤버 → 방장
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ClientMsg {
    /// 접속 직후 반드시 가장 먼저 보내야 하는 입장 요청
    Hello {
        version: u32,
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
}

/// [길이 4바이트][JSON 본문] 형태로 메시지 하나를 보낸다.
pub async fn write_frame<W, T>(writer: &mut W, msg: &T) -> Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let body = serde_json::to_vec(msg)?;
    if body.len() > MAX_FRAME_LEN {
        bail!("메시지가 너무 큽니다: {} 바이트", body.len());
    }

    // 길이와 본문을 한 버퍼에 담아 한 번에 쓴다.
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
    frame.extend_from_slice(&body);
    writer.write_all(&frame).await?;
    Ok(())
}

/// 메시지 하나를 읽는다. 상대가 연결을 정상 종료했으면 `Ok(None)`.
pub async fn read_frame<R, T>(reader: &mut R) -> Result<Option<T>>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
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
    Ok(Some(serde_json::from_slice(&body)?))
}