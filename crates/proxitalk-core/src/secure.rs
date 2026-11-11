//! SPAKE2(PAKE)로 비밀값을 보내지 않고 키를 합의하고, 그 키로 프레임을 암호화한다.

use anyhow::{Context, Result, anyhow};
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use serde::Serialize;
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};
use spake2::{Ed25519Group, Identity, Password, Spake2};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::protocol;

const MEMBER_ID: &[u8] = b"proxitalk-member";
const HOST_ID: &[u8] = b"proxitalk-host";

pub type PakeState = Spake2<Ed25519Group>;

/// 양쪽이 같은 비밀값을 넣었을 때만 같은 키가 나온다.
pub struct SessionKeys {
    to_host: [u8; 32],
    to_member: [u8; 32],
}

pub fn start_as_member(password: &[u8]) -> (PakeState, Vec<u8>) {
    Spake2::<Ed25519Group>::start_a(
        &Password::new(password),
        &Identity::new(MEMBER_ID),
        &Identity::new(HOST_ID),
    )
}

pub fn start_as_host(password: &[u8]) -> (PakeState, Vec<u8>) {
    Spake2::<Ed25519Group>::start_b(
        &Password::new(password),
        &Identity::new(MEMBER_ID),
        &Identity::new(HOST_ID),
    )
}

/// 상대의 PAKE 메시지로 키를 완성한다. 방향마다 다른 키를 쓴다.
pub fn finish(state: PakeState, peer_message: &[u8]) -> Result<SessionKeys> {
    let shared = state
        .finish(peer_message)
        .map_err(|e| anyhow!("PAKE 메시지가 올바르지 않습니다: {e:?}"))?;
    Ok(SessionKeys {
        to_host: derive_key(&shared, b"to-host"),
        to_member: derive_key(&shared, b"to-member"),
    })
}

fn derive_key(shared: &[u8], label: &[u8]) -> [u8; 32] {
    Sha256::new()
        .chain_update(label)
        .chain_update(shared)
        .finalize()
        .into()
}

impl SessionKeys {
    /// 멤버 쪽의 (보내기, 받기)
    pub fn for_member(&self) -> (Sealer, Opener) {
        (Sealer::new(&self.to_host), Opener::new(&self.to_member))
    }

    /// 방장 쪽의 (보내기, 받기)
    pub fn for_host(&self) -> (Sealer, Opener) {
        (Sealer::new(&self.to_member), Opener::new(&self.to_host))
    }
}

/// 같은 키로 같은 nonce를 두 번 쓰면 안 되므로, 프레임마다 번호를 하나씩 올린다.
fn nonce(counter: u64) -> [u8; 12] {
    let mut bytes = [0u8; 12];
    bytes[4..].copy_from_slice(&counter.to_be_bytes());
    bytes
}

/// 보내는 쪽 암호화
pub struct Sealer {
    cipher: ChaCha20Poly1305,
    counter: u64,
}

impl Sealer {
    fn new(key: &[u8; 32]) -> Self {
        Self {
            cipher: ChaCha20Poly1305::new(Key::from_slice(key)),
            counter: 0,
        }
    }

    fn seal<T: Serialize>(&mut self, msg: &T) -> Result<Vec<u8>> {
        let plain = serde_json::to_vec(msg)?;
        let sealed = self
            .cipher
            .encrypt(Nonce::from_slice(&nonce(self.counter)), plain.as_slice())
            .map_err(|_| anyhow!("암호화에 실패했습니다"))?;
        self.counter += 1;
        Ok(sealed)
    }

    pub async fn write<W, T>(&mut self, writer: &mut W, msg: &T) -> Result<()>
    where
        W: AsyncWrite + Unpin,
        T: Serialize,
    {
        let sealed = self.seal(msg)?;
        protocol::write_bytes(writer, &sealed).await
    }
}

/// 받는 쪽 복호화. 키가 다르거나 내용이 바뀌었거나 순서가 다르면 실패한다.
pub struct Opener {
    cipher: ChaCha20Poly1305,
    counter: u64,
}

impl Opener {
    fn new(key: &[u8; 32]) -> Self {
        Self {
            cipher: ChaCha20Poly1305::new(Key::from_slice(key)),
            counter: 0,
        }
    }

    pub fn open<T: DeserializeOwned>(&mut self, sealed: &[u8]) -> Result<T> {
        let plain = self
            .cipher
            .decrypt(Nonce::from_slice(&nonce(self.counter)), sealed)
            .map_err(|_| anyhow!("복호화에 실패했습니다"))?;
        self.counter += 1;
        serde_json::from_slice(&plain).context("메시지 형식이 올바르지 않습니다")
    }

    pub async fn read<R, T>(&mut self, reader: &mut R) -> Result<Option<T>>
    where
        R: AsyncRead + Unpin,
        T: DeserializeOwned,
    {
        match protocol::read_bytes(reader).await? {
            Some(sealed) => Ok(Some(self.open(&sealed)?)),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ClientMsg;

    fn channels(member_password: &[u8], host_password: &[u8]) -> (Sealer, Opener) {
        let (member, member_msg) = start_as_member(member_password);
        let (host, host_msg) = start_as_host(host_password);
        let member_keys = finish(member, &host_msg).unwrap();
        let host_keys = finish(host, &member_msg).unwrap();
        (member_keys.for_member().0, host_keys.for_host().1)
    }

    fn chat() -> ClientMsg {
        ClientMsg::Chat {
            text: "안녕".into(),
        }
    }

    #[test]
    fn same_password_can_talk() {
        let (mut sealer, mut opener) = channels(b"same", b"same");
        let sealed = sealer.seal(&chat()).unwrap();
        let ClientMsg::Chat { text } = opener.open(&sealed).unwrap() else {
            panic!("Chat이어야 함");
        };
        assert_eq!(text, "안녕");
    }

    #[test]
    fn different_password_cannot_open() {
        let (mut sealer, mut opener) = channels(b"right", b"wrong");
        let sealed = sealer.seal(&chat()).unwrap();
        assert!(opener.open::<ClientMsg>(&sealed).is_err());
    }

    #[test]
    fn tampered_frame_is_rejected() {
        let (mut sealer, mut opener) = channels(b"same", b"same");
        let mut sealed = sealer.seal(&chat()).unwrap();
        sealed[0] ^= 1;
        assert!(opener.open::<ClientMsg>(&sealed).is_err());
    }

    #[test]
    fn replayed_frame_is_rejected() {
        let (mut sealer, mut opener) = channels(b"same", b"same");
        let sealed = sealer.seal(&chat()).unwrap();
        opener.open::<ClientMsg>(&sealed).unwrap();
        assert!(opener.open::<ClientMsg>(&sealed).is_err());
    }

    #[test]
    fn secret_and_text_are_not_on_the_wire() {
        let (mut sealer, _) = channels(b"same", b"same");
        let sealed = sealer.seal(&chat()).unwrap();
        let needle = "안녕".as_bytes();
        assert!(!sealed.windows(needle.len()).any(|w| w == needle));
    }
}
