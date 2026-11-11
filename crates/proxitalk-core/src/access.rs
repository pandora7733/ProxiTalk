//! 비공개방의 입장 규칙: 코드 교체와 실패 횟수 제한.
//! 시간을 인자로 받기 때문에 테스트에서 실제로 기다릴 필요가 없다.

use std::collections::HashMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::invite::{self, Invite};

/// 코드를 정기적으로 바꾸는 주기
pub const ROTATE_EVERY: Duration = Duration::from_secs(5 * 60);
/// 정기 교체 뒤 이전 코드도 인정하는 시간 (전달하는 사이에 만료되지 않도록)
const GRACE: Duration = Duration::from_secs(30);
/// 같은 IP가 IP_WINDOW 안에 이만큼 실패하면 코드를 바꾸고 IP_BLOCK 동안 막는다.
pub const MAX_IP_FAILURES: u32 = 5;
const IP_WINDOW: Duration = Duration::from_secs(10 * 60);
pub const IP_BLOCK: Duration = Duration::from_secs(10 * 60);
/// IP를 바꿔가며 공격해도 전체 시도 속도는 이 값을 넘지 못한다.
const MAX_TOTAL_FAILURES: u32 = 30;
const TOTAL_WINDOW: Duration = Duration::from_secs(60);
/// 공격자가 코드를 계속 바꿔서 정상 참가자를 방해하지 못하게 한다.
const MIN_FORCED_ROTATION_GAP: Duration = Duration::from_secs(60);

struct IpRecord {
    failures: u32,
    window_start: Instant,
    blocked_until: Option<Instant>,
}

pub struct FailureOutcome {
    /// 이번 실패로 이 IP가 차단되었다.
    pub ip_blocked: bool,
    /// 이번 실패로 전체 시도 제한에 걸렸다.
    pub total_limit_reached: bool,
}

pub struct Access {
    port: u16,
    generation: u8,
    secret: u64,
    expires_at: Instant,
    /// 정기 교체 직후의 이전 코드: (세대, 비밀값, 인정 기한)
    previous: Option<(u8, u64, Instant)>,
    ip_records: HashMap<IpAddr, IpRecord>,
    total_failures: u32,
    total_window_start: Instant,
    last_forced_rotation: Option<Instant>,
}

impl Access {
    pub fn new(port: u16, now: Instant) -> Result<Self> {
        Ok(Self {
            port,
            generation: 0,
            secret: invite::generate_secret()?,
            expires_at: now + ROTATE_EVERY,
            previous: None,
            ip_records: HashMap::new(),
            total_failures: 0,
            total_window_start: now,
            last_forced_rotation: None,
        })
    }

    pub fn invite(&self) -> Invite {
        Invite {
            port: self.port,
            generation: self.generation,
            secret: self.secret,
        }
    }

    pub fn expires_at(&self) -> Instant {
        self.expires_at
    }

    pub fn rotation_due(&self, now: Instant) -> bool {
        now >= self.expires_at
    }

    /// 정기 교체: 이전 코드를 잠시 더 인정한다.
    pub fn rotate_on_schedule(&mut self, now: Instant) -> Result<()> {
        let old = (self.generation, self.secret, now + GRACE);
        self.rotate(now)?;
        self.previous = Some(old);
        self.prune(now);
        Ok(())
    }

    /// 즉시 교체: 이전 코드는 바로 무효가 된다.
    pub fn rotate_now(&mut self, now: Instant) -> Result<()> {
        self.rotate(now)?;
        self.previous = None;
        self.last_forced_rotation = Some(now);
        Ok(())
    }

    fn rotate(&mut self, now: Instant) -> Result<()> {
        self.secret = invite::generate_secret()?;
        self.generation = invite::next_generation(self.generation);
        self.expires_at = now + ROTATE_EVERY;
        Ok(())
    }

    pub fn can_force_rotation(&self, now: Instant) -> bool {
        self.last_forced_rotation
            .is_none_or(|last| now.duration_since(last) >= MIN_FORCED_ROTATION_GAP)
    }

    /// 이 세대의 비밀값을 돌려준다. 만료된 코드면 None.
    pub fn secret_for(&self, generation: u8, now: Instant) -> Option<u64> {
        if generation == self.generation {
            return Some(self.secret);
        }
        match self.previous {
            Some((old_generation, secret, until))
                if old_generation == generation && now < until =>
            {
                Some(secret)
            }
            _ => None,
        }
    }

    pub fn is_blocked(&self, ip: IpAddr, now: Instant) -> bool {
        let total_blocked = now.duration_since(self.total_window_start) < TOTAL_WINDOW
            && self.total_failures >= MAX_TOTAL_FAILURES;
        let ip_blocked = self
            .ip_records
            .get(&ip)
            .and_then(|record| record.blocked_until)
            .is_some_and(|until| now < until);
        total_blocked || ip_blocked
    }

    pub fn record_failure(&mut self, ip: IpAddr, now: Instant) -> FailureOutcome {
        if now.duration_since(self.total_window_start) >= TOTAL_WINDOW {
            self.total_window_start = now;
            self.total_failures = 0;
        }
        self.total_failures += 1;

        let record = self.ip_records.entry(ip).or_insert(IpRecord {
            failures: 0,
            window_start: now,
            blocked_until: None,
        });
        if now.duration_since(record.window_start) >= IP_WINDOW {
            record.failures = 0;
            record.window_start = now;
        }
        record.failures += 1;

        let ip_blocked = record.failures == MAX_IP_FAILURES;
        if ip_blocked {
            record.blocked_until = Some(now + IP_BLOCK);
        }
        FailureOutcome {
            ip_blocked,
            total_limit_reached: self.total_failures == MAX_TOTAL_FAILURES,
        }
    }

    /// 인증에 성공하면 그 IP의 실패 기록을 지운다.
    pub fn record_success(&mut self, ip: IpAddr) {
        self.ip_records.remove(&ip);
    }

    /// 다 지난 기록을 지워서 메모리가 계속 늘지 않게 한다.
    fn prune(&mut self, now: Instant) {
        self.ip_records.retain(|_, record| {
            let blocked = record.blocked_until.is_some_and(|until| now < until);
            blocked || now.duration_since(record.window_start) < IP_WINDOW
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IP: IpAddr = IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, 1));

    fn ip(last: u8) -> IpAddr {
        IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, last))
    }

    #[test]
    fn scheduled_rotation_keeps_old_code_during_grace() {
        let start = Instant::now();
        let mut access = Access::new(9000, start).unwrap();
        let old = access.invite();

        let later = start + ROTATE_EVERY;
        assert!(access.rotation_due(later));
        access.rotate_on_schedule(later).unwrap();

        assert_ne!(access.invite().generation, old.generation);
        assert_eq!(access.secret_for(old.generation, later), Some(old.secret));
        let after_grace = later + GRACE;
        assert_eq!(access.secret_for(old.generation, after_grace), None);
    }

    #[test]
    fn forced_rotation_kills_old_code_immediately() {
        let start = Instant::now();
        let mut access = Access::new(9000, start).unwrap();
        let old = access.invite();

        access.rotate_now(start).unwrap();
        assert_eq!(access.secret_for(old.generation, start), None);
        assert!(!access.can_force_rotation(start));
        assert!(access.can_force_rotation(start + MIN_FORCED_ROTATION_GAP));
    }

    #[test]
    fn fifth_failure_blocks_only_that_ip() {
        let now = Instant::now();
        let mut access = Access::new(9000, now).unwrap();

        for _ in 1..MAX_IP_FAILURES {
            assert!(!access.record_failure(IP, now).ip_blocked);
        }
        assert!(!access.is_blocked(IP, now));
        assert!(access.record_failure(IP, now).ip_blocked);

        assert!(access.is_blocked(IP, now));
        assert!(!access.is_blocked(ip(2), now));
        assert!(!access.is_blocked(IP, now + IP_BLOCK));
    }

    #[test]
    fn failures_outside_the_window_do_not_add_up() {
        let now = Instant::now();
        let mut access = Access::new(9000, now).unwrap();
        for _ in 1..MAX_IP_FAILURES {
            access.record_failure(IP, now);
        }
        assert!(!access.record_failure(IP, now + IP_WINDOW).ip_blocked);
    }

    #[test]
    fn success_clears_failures() {
        let now = Instant::now();
        let mut access = Access::new(9000, now).unwrap();
        for _ in 1..MAX_IP_FAILURES {
            access.record_failure(IP, now);
        }
        access.record_success(IP);
        assert!(!access.record_failure(IP, now).ip_blocked);
    }

    #[test]
    fn total_limit_stops_attackers_who_change_ip() {
        let now = Instant::now();
        let mut access = Access::new(9000, now).unwrap();
        let mut reached = false;
        for attempt in 0..MAX_TOTAL_FAILURES {
            reached = access
                .record_failure(ip(attempt as u8), now)
                .total_limit_reached;
        }
        assert!(reached);
        assert!(access.is_blocked(ip(200), now));
        assert!(!access.is_blocked(ip(200), now + TOTAL_WINDOW));
    }

    #[test]
    fn prune_forgets_old_records_but_keeps_blocks() {
        let now = Instant::now();
        let mut access = Access::new(9000, now).unwrap();
        access.record_failure(ip(1), now);
        for _ in 0..MAX_IP_FAILURES {
            access.record_failure(ip(2), now + IP_WINDOW - Duration::from_secs(1));
        }
        access.prune(now + IP_WINDOW);
        assert!(!access.ip_records.contains_key(&ip(1)));
        assert!(access.ip_records.contains_key(&ip(2)));
    }
}
