//! 비공개방 초대 링크: proxitalk://<IP:포트>/<토큰>

use std::fmt;
use std::net::SocketAddr;
use std::str::FromStr;

use anyhow::{Context, Result, anyhow, bail};

const SCHEME: &str = "proxitalk://";
/// 토큰의 크기: 16바이트 = 128비트 (16진수 문자열로는 32자)
const TOKEN_BYTES: usize = 16;

#[derive(Debug, PartialEq, Eq)]
pub struct Invite {
    pub addr: SocketAddr,
    pub token: String,
}

/// OS의 암호학적 난수로 추측할 수 없는 토큰을 만든다.
pub fn generate_token() -> Result<String> {
    let mut bytes = [0u8; TOKEN_BYTES];
    getrandom::fill(&mut bytes).map_err(|e| anyhow!("난수를 만들 수 없습니다: {e}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

pub fn is_invite_link(text: &str) -> bool {
    text.starts_with(SCHEME)
}

/// Invite → 문자열. `invite.to_string()`을 쓸 수 있게 된다.
impl fmt::Display for Invite {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{SCHEME}{}/{}", self.addr, self.token)
    }
}

/// 문자열 → Invite. `link.parse::<Invite>()`를 쓸 수 있게 된다.
impl FromStr for Invite {
    type Err = anyhow::Error;

    fn from_str(link: &str) -> Result<Self> {
        let rest = link
            .trim()
            .strip_prefix(SCHEME)
            .context("초대 링크는 proxitalk:// 로 시작해야 합니다")?;
        let (addr, token) = rest
            .split_once('/')
            .context("초대 링크 형식이 올바르지 않습니다")?;
        let addr = addr
            .parse()
            .with_context(|| format!("초대 링크의 주소가 올바르지 않습니다: {addr}"))?;

        let valid_token =
            token.len() == TOKEN_BYTES * 2 && token.chars().all(|c| c.is_ascii_hexdigit());
        if !valid_token {
            bail!("초대 링크의 토큰이 올바르지 않습니다");
        }

        Ok(Invite {
            addr,
            token: token.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_round_trip() {
        let invite = Invite {
            addr: "192.168.0.12:9000".parse().unwrap(),
            token: generate_token().unwrap(),
        };
        let parsed: Invite = invite.to_string().parse().unwrap();
        assert_eq!(parsed, invite);
    }

    #[test]
    fn tokens_are_32_hex_chars_and_differ() {
        let first = generate_token().unwrap();
        let second = generate_token().unwrap();
        assert_eq!(first.len(), 32);
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(first, second);
    }

    #[test]
    fn rejects_malformed_links() {
        let token = "0".repeat(32);
        let bad_links = [
            format!("http://1.2.3.4:9000/{token}"),     // 스킴이 다름
            format!("proxitalk://1.2.3.4/{token}"),     // 포트 없음
            "proxitalk://1.2.3.4:9000".to_string(),     // 토큰 없음
            "proxitalk://1.2.3.4:9000/abc".to_string(), // 토큰이 짧음
            format!("proxitalk://1.2.3.4:9000/{}", "z".repeat(32)), // 16진수가 아님
        ];
        for link in bad_links {
            assert!(link.parse::<Invite>().is_err(), "통과하면 안 됨: {link}");
        }
    }
}