//! ProxiTalk 핵심 로직 (네트워크, 프로토콜, 룸)

pub mod client;
pub mod protocol;
pub mod server;

/// Cargo.toml의 version 값을 컴파일 시점에 가져온다.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
