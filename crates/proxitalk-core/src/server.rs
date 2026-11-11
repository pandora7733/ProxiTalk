//! 방장 쪽: 접속을 받고, 룸 액터가 멤버 목록, 입장 규칙, 브로드캐스트를 담당한다.

use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tokio::io::AsyncReadExt;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{mpsc, oneshot};
use tokio::time::{MissedTickBehavior, interval, timeout};

use crate::access::{Access, IP_BLOCK, MAX_IP_FAILURES};
use crate::invite::{self, Invite};
use crate::protocol::{
    self, ClientMsg, HandshakeMsg, MAX_CHAT_CHARS, MemberId, MemberInfo, PROTOCOL_VERSION,
    ServerMsg,
};
use crate::secure::{self, Opener, Sealer};

/// 접속 후 인증과 Hello까지 끝내야 하는 시간
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
/// 방장이 입장 요청에 답해야 하는 시간
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_PENDING_JOINS: usize = 8;
const MAX_NICKNAME_CHARS: usize = 20;
/// 멤버 한 명에게 보낼 메시지를 쌓아둘 수 있는 개수
const OUTBOX_SIZE: usize = 64;
const WRONG_CODE: &str = "초대 코드가 없거나 올바르지 않습니다";
const TOO_MANY_ATTEMPTS: &str = "입장 시도가 너무 많습니다. 잠시 후 다시 시도하세요";

type JoinReply = Result<(MemberId, Vec<MemberInfo>), String>;

/// 방장 화면에 표시할 초대 코드
#[derive(Debug, Clone)]
pub struct CodeInfo {
    pub code: String,
    pub expires_at: Instant,
}

/// 룸 → 방장 화면. 네트워크를 거치지 않으므로 다른 멤버는 절대 볼 수 없다.
#[derive(Debug)]
pub enum HostEvent {
    CodeChanged {
        info: CodeInfo,
        /// 경고할 일이 있어서 바뀌었으면 그 내용. 정기 교체면 None
        alert: Option<String>,
    },
    Alert(String),
    JoinRequest {
        id: u64,
        nickname: String,
        ip: IpAddr,
    },
    /// 시간이 지나거나 참가자가 떠나서 요청이 사라졌다.
    JoinRequestClosed {
        id: u64,
    },
}

/// 방장 화면 → 룸. 이것도 네트워크를 거치지 않으므로 멤버가 위조할 수 없다.
#[derive(Debug)]
pub enum HostCommand {
    Approve { id: u64 },
    Deny { id: u64 },
    Kick { nickname: String },
}

pub struct HostControl {
    pub events: mpsc::Receiver<HostEvent>,
    pub commands: mpsc::Sender<HostCommand>,
}

pub struct HostedRoom {
    pub port: u16,
    /// 비공개방일 때만 Some
    pub invite: Option<Invite>,
    pub code: Option<CodeInfo>,
    pub control: HostControl,
}

/// 인증을 시작해도 되는지에 대한 룸의 답
enum AuthDecision {
    Password(Vec<u8>),
    Blocked,
}

/// 연결 태스크 → 룸 액터로 보내는 명령
enum RoomCmd {
    BeginAuth {
        ip: IpAddr,
        generation: u8,
        reply: oneshot::Sender<AuthDecision>,
    },
    AuthFailed {
        ip: IpAddr,
    },
    AuthPassed {
        ip: IpAddr,
    },
    Join {
        nickname: String,
        ip: IpAddr,
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
    ip: IpAddr,
    outbox: mpsc::Sender<ServerMsg>,
}

/// 방장의 수락을 기다리는 입장 요청
struct PendingJoin {
    id: u64,
    nickname: String,
    ip: IpAddr,
    outbox: mpsc::Sender<ServerMsg>,
    reply: oneshot::Sender<JoinReply>,
    requested_at: Instant,
}

/// 포트를 열고 룸 액터와 접속 대기 루프를 백그라운드로 띄운다.
/// port가 0이면 OS가 비어 있는 포트를 골라준다.
/// private이면 초대 코드를 만들고, 코드를 아는 사람만 입장할 수 있다.
pub async fn start_server(port: u16, private: bool) -> Result<HostedRoom> {
    let listener = TcpListener::bind(("0.0.0.0", port))
        .await
        .with_context(|| format!("{port}번 포트를 열 수 없습니다"))?;
    let port = listener.local_addr()?.port();

    let access = if private {
        Some(Access::new(port, Instant::now())?)
    } else {
        None
    };
    let invite = access.as_ref().map(Access::invite);
    let code = access.as_ref().map(code_info);

    let (room_tx, room_rx) = mpsc::channel(256);
    let (event_tx, event_rx) = mpsc::channel(64);
    let (command_tx, command_rx) = mpsc::channel(16);
    let room = Room::new(access, event_tx);
    tokio::spawn(room_task(room, room_rx, command_rx));
    tokio::spawn(accept_loop(listener, room_tx));

    Ok(HostedRoom {
        port,
        invite,
        code,
        control: HostControl {
            events: event_rx,
            commands: command_tx,
        },
    })
}

fn code_info(access: &Access) -> CodeInfo {
    CodeInfo {
        code: access.invite().to_string(),
        expires_at: access.expires_at(),
    }
}

async fn accept_loop(listener: TcpListener, room: mpsc::Sender<RoomCmd>) {
    loop {
        match listener.accept().await {
            Ok((stream, addr)) => {
                // 접속자마다 독립된 태스크를 하나씩 만든다.
                let room = room.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_connection(stream, addr.ip(), room).await {
                        tracing::warn!("{addr}: {e:#}");
                    }
                });
            }
            Err(e) => tracing::warn!("accept 실패: {e}"),
        }
    }
}

/// 접속자 한 명을 담당한다: 인증 → 입장 처리 → 쓰기 태스크 생성 → 읽기 루프
async fn handle_connection(
    stream: TcpStream,
    ip: IpAddr,
    room: mpsc::Sender<RoomCmd>,
) -> Result<()> {
    let (mut reader, mut writer) = stream.into_split();

    // ── 1단계: 인증. 통과하기 전에는 룸에 들어가지 못한다 ──
    let authenticated = timeout(
        HELLO_TIMEOUT,
        authenticate(&mut reader, &mut writer, ip, &room),
    )
    .await
    .context("인증을 기다리다 시간 초과")??;
    let Some((nickname, mut sealer, mut opener)) = authenticated else {
        return Ok(());
    };

    // 룸 액터에게 입장을 요청하고, oneshot 채널로 결과를 기다린다.
    // 비공개방이면 방장이 수락할 때까지 기다린다. 기한은 룸 액터가 관리한다.
    let (outbox_tx, mut outbox_rx) = mpsc::channel(OUTBOX_SIZE);
    let (reply_tx, reply_rx) = oneshot::channel();
    room.send(RoomCmd::Join {
        nickname,
        ip,
        outbox: outbox_tx,
        reply: reply_tx,
    })
    .await?;

    // 멤버는 Welcome 전에 아무것도 보내지 않는다. 읽기가 끝나면 기다리다 나갔다는 뜻이다.
    // 이 함수가 끝나면 reply_rx가 사라지고, 룸 액터가 그것을 보고 입장 요청을 지운다.
    let mut probe = [0u8; 1];
    let reply = tokio::select! {
        reply = reply_rx => reply?,
        _ = reader.read(&mut probe) => return Ok(()),
    };
    let (my_id, members) = match reply {
        Ok(joined) => joined,
        Err(reason) => {
            sealer
                .write(&mut writer, &ServerMsg::Reject { reason })
                .await?;
            return Ok(());
        }
    };
    let welcome = ServerMsg::Welcome {
        your_id: my_id,
        members,
    };
    // 룸은 이미 멤버로 등록했으므로, 여기서 실패해도 퇴장 처리를 해야 유령 멤버가 남지 않는다.
    if let Err(e) = sealer.write(&mut writer, &welcome).await {
        let _ = room.send(RoomCmd::Leave { id: my_id }).await;
        return Err(e);
    }

    // ── 2단계: 입장 후 ──
    // 쓰기 담당: 룸이 outbox에 넣어준 메시지를 소켓으로 보낸다.
    let mut writer_task = tokio::spawn(async move {
        while let Some(msg) = outbox_rx.recv().await {
            if sealer.write(&mut writer, &msg).await.is_err() {
                break;
            }
        }
    });

    // 읽기 담당: 이 태스크가 직접 한다. 쓰기가 먼저 끝나면(강제 퇴장 등) 연결도 끝낸다.
    let result = tokio::select! {
        result = read_loop(&mut reader, &mut opener, my_id, &room) => result,
        _ = &mut writer_task => Ok(()),
    };
    let _ = room.send(RoomCmd::Leave { id: my_id }).await;
    result
}

/// PAKE로 키를 합의하고 암호화된 Hello를 받는다.
/// 비밀값 자체는 네트워크로 오가지 않는다. Hello가 풀리면 같은 코드를 안다는 뜻이다.
async fn authenticate(
    reader: &mut OwnedReadHalf,
    writer: &mut OwnedWriteHalf,
    ip: IpAddr,
    room: &mpsc::Sender<RoomCmd>,
) -> Result<Option<(String, Sealer, Opener)>> {
    let (version, generation, member_pake) = match protocol::read_frame(reader).await? {
        Some(HandshakeMsg::Start {
            version,
            generation,
            pake,
        }) => (version, generation, pake),
        Some(_) => bail!("Start 전에 다른 메시지를 보냄"),
        None => return Ok(None), // 아무 말 없이 끊음
    };
    if version != PROTOCOL_VERSION {
        let reason = format!("버전이 다릅니다 (방장 {PROTOCOL_VERSION}, 내 앱 {version})");
        protocol::write_frame(writer, &HandshakeMsg::Rejected { reason }).await?;
        bail!("버전 불일치로 거절");
    }

    let (reply_tx, reply_rx) = oneshot::channel();
    room.send(RoomCmd::BeginAuth {
        ip,
        generation,
        reply: reply_tx,
    })
    .await?;
    let password = match reply_rx.await? {
        AuthDecision::Password(password) => password,
        AuthDecision::Blocked => {
            let reason = TOO_MANY_ATTEMPTS.to_string();
            protocol::write_frame(writer, &HandshakeMsg::Rejected { reason }).await?;
            bail!("차단된 접속");
        }
    };

    let (state, host_pake) = secure::start_as_host(&password);
    protocol::write_frame(writer, &HandshakeMsg::Reply { pake: host_pake }).await?;
    let keys = secure::finish(state, &member_pake)?;
    let (sealer, mut opener) = keys.for_host();

    let Some(sealed_hello) = protocol::read_bytes(reader).await? else {
        return Ok(None);
    };
    let nickname = match opener.open(&sealed_hello) {
        Ok(ClientMsg::Hello { nickname }) => nickname,
        Ok(_) => bail!("Hello 전에 다른 메시지를 보냄"),
        // 키가 다르면 복호화에 실패한다. = 코드가 틀렸다.
        Err(_) => {
            room.send(RoomCmd::AuthFailed { ip }).await?;
            let reason = WRONG_CODE.to_string();
            protocol::write_frame(writer, &HandshakeMsg::Rejected { reason }).await?;
            bail!("초대 코드가 틀려 거절");
        }
    };

    room.send(RoomCmd::AuthPassed { ip }).await?;
    protocol::write_frame(writer, &HandshakeMsg::Accepted).await?;
    Ok(Some((nickname, sealer, opener)))
}

async fn read_loop(
    reader: &mut OwnedReadHalf,
    opener: &mut Opener,
    my_id: MemberId,
    room: &mpsc::Sender<RoomCmd>,
) -> Result<()> {
    loop {
        match opener.read(reader).await? {
            Some(ClientMsg::Chat { text }) => {
                if text.chars().count() <= MAX_CHAT_CHARS {
                    room.send(RoomCmd::Chat { from: my_id, text }).await?;
                }
            }
            Some(ClientMsg::Hello { .. }) => {} // 이미 입장했으므로 무시
            None => return Ok(()),
        }
    }
}

/// 방 상태. 룸 액터 혼자 소유하므로 락이 필요 없다.
struct Room {
    members: HashMap<MemberId, Member>,
    next_id: MemberId,
    /// 공개방이면 None
    access: Option<Access>,
    host_id: Option<MemberId>,
    pending: Vec<PendingJoin>,
    next_request_id: u64,
    /// 강제 퇴장당한 IP. 방이 열려 있는 동안 다시 들어오지 못한다.
    banned: HashSet<IpAddr>,
    events: mpsc::Sender<HostEvent>,
}

/// 룸 액터: 명령을 하나씩 순서대로 처리한다.
async fn room_task(
    mut room: Room,
    mut commands: mpsc::Receiver<RoomCmd>,
    mut host_commands: mpsc::Receiver<HostCommand>,
) {
    let mut housekeeping = interval(Duration::from_secs(1));
    housekeeping.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            cmd = commands.recv() => match cmd {
                Some(cmd) => room.on_command(cmd),
                None => break,
            },
            Some(cmd) = host_commands.recv() => room.on_host_command(cmd),
            _ = housekeeping.tick() => room.on_tick(Instant::now()),
        }
    }
}

impl Room {
    fn new(access: Option<Access>, events: mpsc::Sender<HostEvent>) -> Self {
        Self {
            members: HashMap::new(),
            next_id: 1,
            access,
            host_id: None,
            pending: Vec::new(),
            next_request_id: 1,
            banned: HashSet::new(),
            events,
        }
    }

    fn on_command(&mut self, cmd: RoomCmd) {
        let now = Instant::now();
        match cmd {
            RoomCmd::BeginAuth {
                ip,
                generation,
                reply,
            } => {
                let _ = reply.send(self.decide_auth(ip, generation, now));
            }
            RoomCmd::AuthFailed { ip } => self.on_auth_failed(ip, now),
            RoomCmd::AuthPassed { ip } => {
                if let Some(access) = &mut self.access {
                    access.record_success(ip);
                }
            }
            RoomCmd::Join {
                nickname,
                ip,
                outbox,
                reply,
            } => self.on_join(nickname.trim().to_string(), ip, outbox, reply, now),
            RoomCmd::Chat { from, text } => {
                let Some(sender) = self.members.get(&from) else {
                    return;
                };
                let msg = ServerMsg::Chat {
                    from: sender.info.clone(),
                    text,
                };
                broadcast(&self.members, msg);
            }
            RoomCmd::Leave { id } => {
                if let Some(left) = self.members.remove(&id) {
                    broadcast(&self.members, ServerMsg::MemberLeft { member: left.info });
                }
            }
        }
    }

    fn decide_auth(&self, ip: IpAddr, generation: u8, now: Instant) -> AuthDecision {
        if self.banned.contains(&ip) {
            return AuthDecision::Blocked;
        }
        let Some(access) = &self.access else {
            return AuthDecision::Password(invite::PUBLIC_PASSWORD.to_vec());
        };
        if access.is_blocked(ip, now) {
            return AuthDecision::Blocked;
        }
        match access.secret_for(generation, now) {
            Some(secret) => AuthDecision::Password(invite::password_bytes(secret).to_vec()),
            // 만료된 코드도 틀린 코드와 똑같이 처리한다. 아무도 모르는 값으로 진행해서 실패하게 한다.
            None => match invite::generate_secret() {
                Ok(random) => AuthDecision::Password(invite::password_bytes(random).to_vec()),
                Err(_) => AuthDecision::Blocked,
            },
        }
    }

    fn on_auth_failed(&mut self, ip: IpAddr, now: Instant) {
        let Some(access) = &mut self.access else {
            return;
        };
        let outcome = access.record_failure(ip, now);

        if outcome.ip_blocked {
            let minutes = IP_BLOCK.as_secs() / 60;
            let alert = format!(
                "{ip}에서 입장 실패가 {MAX_IP_FAILURES}회 발생해 {minutes}분 동안 차단했습니다."
            );
            // 공격자가 코드를 계속 바꾸지 못하도록, 즉시 교체는 간격을 제한한다.
            if access.can_force_rotation(now) {
                self.rotate_now(format!("{alert} 초대 코드도 바꿨습니다."), now);
            } else {
                self.notify(HostEvent::Alert(alert));
            }
        }
        if outcome.total_limit_reached {
            self.notify(HostEvent::Alert(
                "입장 실패가 너무 많아 약 1분 동안 새 입장 시도를 막습니다.".into(),
            ));
        }
    }

    fn rotate_now(&mut self, alert: String, now: Instant) {
        let Some(access) = &mut self.access else {
            return;
        };
        match access.rotate_now(now) {
            Ok(()) => {
                let info = code_info(access);
                self.notify(HostEvent::CodeChanged {
                    info,
                    alert: Some(alert),
                });
            }
            Err(e) => tracing::warn!("초대 코드 교체 실패: {e:#}"),
        }
    }

    fn rotate_on_schedule(&mut self, now: Instant) {
        let Some(access) = &mut self.access else {
            return;
        };
        match access.rotate_on_schedule(now) {
            Ok(()) => {
                let info = code_info(access);
                self.notify(HostEvent::CodeChanged { info, alert: None });
            }
            Err(e) => tracing::warn!("초대 코드 교체 실패: {e:#}"),
        }
    }

    fn on_join(
        &mut self,
        nickname: String,
        ip: IpAddr,
        outbox: mpsc::Sender<ServerMsg>,
        reply: oneshot::Sender<JoinReply>,
        now: Instant,
    ) {
        if let Err(reason) = self.validate_nickname(&nickname) {
            let _ = reply.send(Err(reason));
            return;
        }

        // 맨 처음 이 PC에서 접속한 사람이 방장이다. 방장은 수락 없이 바로 들어간다.
        let is_host = self.host_id.is_none() && ip.is_loopback();
        if is_host {
            self.host_id = Some(self.next_id);
        }
        if is_host || self.access.is_none() {
            self.admit(nickname, ip, outbox, reply);
            return;
        }

        if self.pending.len() >= MAX_PENDING_JOINS {
            let _ = reply.send(Err(
                "입장 요청이 밀려 있습니다. 잠시 후 다시 시도하세요".into()
            ));
            return;
        }
        let id = self.next_request_id;
        self.next_request_id += 1;
        self.notify(HostEvent::JoinRequest {
            id,
            nickname: nickname.clone(),
            ip,
        });
        self.pending.push(PendingJoin {
            id,
            nickname,
            ip,
            outbox,
            reply,
            requested_at: now,
        });
    }

    /// 멤버 목록에 넣고 입장을 알린다.
    fn admit(
        &mut self,
        nickname: String,
        ip: IpAddr,
        outbox: mpsc::Sender<ServerMsg>,
        reply: oneshot::Sender<JoinReply>,
    ) {
        let info = MemberInfo {
            id: self.next_id,
            nickname,
        };
        self.next_id += 1;

        // 기존 멤버들에게 먼저 알리고, 그다음 목록에 추가한다.
        broadcast(
            &self.members,
            ServerMsg::MemberJoined {
                member: info.clone(),
            },
        );
        self.members.insert(
            info.id,
            Member {
                info: info.clone(),
                ip,
                outbox,
            },
        );

        let mut list: Vec<MemberInfo> = self.members.values().map(|m| m.info.clone()).collect();
        list.sort_by_key(|m| m.id);
        let _ = reply.send(Ok((info.id, list)));
    }

    fn on_host_command(&mut self, cmd: HostCommand) {
        match cmd {
            HostCommand::Approve { id } => {
                let Some(request) = self.take_pending(id) else {
                    return;
                };
                // 기다리는 사이에 같은 닉네임이 들어왔을 수 있으므로 다시 검사한다.
                match self.validate_nickname(&request.nickname) {
                    Ok(()) => {
                        self.admit(request.nickname, request.ip, request.outbox, request.reply)
                    }
                    Err(reason) => {
                        let _ = request.reply.send(Err(reason));
                    }
                }
            }
            HostCommand::Deny { id } => {
                if let Some(request) = self.take_pending(id) {
                    let reason = "방장이 입장을 거절했습니다".to_string();
                    let _ = request.reply.send(Err(reason));
                }
            }
            HostCommand::Kick { nickname } => self.kick(&nickname),
        }
    }

    fn take_pending(&mut self, id: u64) -> Option<PendingJoin> {
        let index = self.pending.iter().position(|p| p.id == id)?;
        Some(self.pending.remove(index))
    }

    fn kick(&mut self, nickname: &str) {
        let target = self
            .members
            .values()
            .find(|m| m.info.nickname == nickname)
            .map(|m| m.info.id);
        let Some(id) = target.filter(|id| Some(*id) != self.host_id) else {
            self.notify(HostEvent::Alert(format!(
                "내보낼 수 있는 '{nickname}' 멤버가 없습니다."
            )));
            return;
        };
        let Some(kicked) = self.members.remove(&id) else {
            return;
        };

        let _ = kicked.outbox.try_send(ServerMsg::Kicked);
        self.banned.insert(kicked.ip);
        broadcast(
            &self.members,
            ServerMsg::MemberLeft {
                member: kicked.info,
            },
        );

        // 나간 사람이 알고 있는 코드는 더 이상 쓸 수 없게 한다.
        let alert = format!("{nickname} 님을 내보내고 {} 를 차단했습니다.", kicked.ip);
        if self.access.is_some() {
            self.rotate_now(format!("{alert} 초대 코드도 바꿨습니다."), Instant::now());
        } else {
            self.notify(HostEvent::Alert(alert));
        }
    }

    /// 1초마다: 코드 정기 교체와 오래된 입장 요청 정리
    fn on_tick(&mut self, now: Instant) {
        if self.access.as_ref().is_some_and(|a| a.rotation_due(now)) {
            self.rotate_on_schedule(now);
        }

        let mut index = 0;
        while index < self.pending.len() {
            let request = &self.pending[index];
            let expired = now.duration_since(request.requested_at) >= APPROVAL_TIMEOUT;
            if expired || request.reply.is_closed() {
                let request = self.pending.remove(index);
                let reason = "방장이 시간 안에 수락하지 않았습니다".to_string();
                let _ = request.reply.send(Err(reason));
                self.notify(HostEvent::JoinRequestClosed { id: request.id });
            } else {
                index += 1;
            }
        }
    }

    /// 룸은 절대 기다리지 않는다. 화면이 밀려 있으면 알림을 버린다.
    fn notify(&self, event: HostEvent) {
        if self.events.try_send(event).is_err() {
            tracing::warn!("방장 화면이 밀려 알림을 버렸습니다");
        }
    }

    fn validate_nickname(&self, nickname: &str) -> Result<(), String> {
        if nickname.is_empty() {
            return Err("닉네임이 비어 있습니다".into());
        }
        if nickname.chars().count() > MAX_NICKNAME_CHARS {
            return Err(format!("닉네임은 {MAX_NICKNAME_CHARS}자 이하여야 합니다"));
        }
        let taken = self.members.values().any(|m| m.info.nickname == nickname)
            || self.pending.iter().any(|p| p.nickname == nickname);
        if taken {
            return Err(format!("'{nickname}' 닉네임은 이미 사용 중입니다"));
        }
        Ok(())
    }
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
