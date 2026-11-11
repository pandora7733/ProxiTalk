//! 비공개방 초대 코드: 12자 안에 포트, 세대 번호, 비밀값을 함께 담는다.
//!
//! 12자 × 5비트 = 60비트 = [포트 16비트][세대 4비트][비밀값 40비트]

use std::fmt;
use std::str::FromStr;

use anyhow::{Result, anyhow, bail};

const CODE_CHARS: usize = 12;
const BITS_PER_CHAR: usize = 5;
const SECRET_BITS: usize = 40;
const GENERATION_BITS: usize = 4;
const PORT_SHIFT: usize = SECRET_BITS + GENERATION_BITS;
const SECRET_MASK: u64 = (1 << SECRET_BITS) - 1;
const GENERATION_MASK: u64 = (1 << GENERATION_BITS) - 1;
/// 32글자 = 글자 하나에 5비트. 헷갈리는 I, L, O, U는 뺐다.
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// 공개방의 PAKE 비밀번호. 누구나 아는 값이라 인증은 아니고, 도청만 막는다.
pub const PUBLIC_PASSWORD: &[u8] = b"proxitalk-public-room";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invite {
    pub port: u16,
    pub generation: u8,
    pub secret: u64,
}

/// OS의 암호학적 난수로 40비트 비밀값을 만든다.
pub fn generate_secret() -> Result<u64> {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).map_err(|e| anyhow!("난수를 만들 수 없습니다: {e}"))?;
    Ok(u64::from_be_bytes(bytes) & SECRET_MASK)
}

/// 세대 번호는 4비트이므로 15 다음은 0이다.
pub fn next_generation(generation: u8) -> u8 {
    (u64::from(generation).wrapping_add(1) & GENERATION_MASK) as u8
}

/// PAKE에 넣는 비밀번호. 방장과 참가자가 똑같이 만든다.
pub fn password_bytes(secret: u64) -> [u8; 8] {
    secret.to_be_bytes()
}

/// Invite → 12자 코드. `invite.to_string()`을 쓸 수 있게 된다.
impl fmt::Display for Invite {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = (u64::from(self.port) << PORT_SHIFT)
            | (u64::from(self.generation) << SECRET_BITS)
            | self.secret;
        for position in (0..CODE_CHARS).rev() {
            let index = (value >> (position * BITS_PER_CHAR)) & 0b11111;
            write!(f, "{}", ALPHABET[index as usize] as char)?;
        }
        Ok(())
    }
}

/// 12자 코드 → Invite. `code.parse::<Invite>()`를 쓸 수 있게 된다.
impl FromStr for Invite {
    type Err = anyhow::Error;

    fn from_str(code: &str) -> Result<Self> {
        let code = code.trim().to_ascii_uppercase();
        if code.len() != CODE_CHARS {
            bail!(
                "초대 코드는 {CODE_CHARS}자여야 하는데 {}자입니다",
                code.len()
            );
        }

        let mut value = 0u64;
        for byte in code.bytes() {
            let Some(index) = ALPHABET.iter().position(|&c| c == byte) else {
                bail!("초대 코드에 쓸 수 없는 문자가 있습니다: {}", byte as char);
            };
            value = (value << BITS_PER_CHAR) | index as u64;
        }

        let port = (value >> PORT_SHIFT) as u16;
        if port == 0 {
            bail!("초대 코드가 올바르지 않습니다");
        }
        Ok(Invite {
            port,
            generation: ((value >> SECRET_BITS) & GENERATION_MASK) as u8,
            secret: value & SECRET_MASK,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_round_trip() {
        for (port, generation) in [(1, 0), (9000, 3), (46819, 9), (u16::MAX, 15)] {
            let invite = Invite {
                port,
                generation,
                secret: generate_secret().unwrap(),
            };
            let code = invite.to_string();
            assert_eq!(code.len(), CODE_CHARS);
            assert_eq!(code.parse::<Invite>().unwrap(), invite);
        }
    }

    #[test]
    fn largest_values_survive() {
        let invite = Invite {
            port: u16::MAX,
            generation: 15,
            secret: SECRET_MASK,
        };
        assert_eq!(invite.to_string(), "ZZZZZZZZZZZZ");
        assert_eq!("ZZZZZZZZZZZZ".parse::<Invite>().unwrap(), invite);
    }

    #[test]
    fn accepts_lowercase() {
        let invite = Invite {
            port: 9000,
            generation: 1,
            secret: 12345,
        };
        let lower = invite.to_string().to_lowercase();
        assert_eq!(lower.parse::<Invite>().unwrap(), invite);
    }

    #[test]
    fn generation_wraps_after_15() {
        assert_eq!(next_generation(0), 1);
        assert_eq!(next_generation(15), 0);
    }

    #[test]
    fn secrets_fit_in_40_bits_and_differ() {
        let first = generate_secret().unwrap();
        let second = generate_secret().unwrap();
        assert!(first <= SECRET_MASK && second <= SECRET_MASK);
        assert_ne!(first, second);
    }

    #[test]
    fn rejects_malformed_codes() {
        let bad_codes = [
            "",              // 비어 있음
            "ABC",           // 짧음
            "ABCDEFGHJKMNP", // 13자
            "ABCDEFGHJKMU",  // U는 쓰지 않음
            "한글코드",      // 다른 문자
            "000000000000",  // 포트 0
        ];
        for code in bad_codes {
            assert!(code.parse::<Invite>().is_err(), "통과하면 안 됨: {code}");
        }
    }
}
