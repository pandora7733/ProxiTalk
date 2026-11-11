//! 화면 상태(App)와 이벤트 루프

use std::net::IpAddr;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use proxitalk_core::client::Session;
use proxitalk_core::protocol::{ClientMsg, MAX_CHAT_CHARS, MemberId, MemberInfo, ServerMsg};
use proxitalk_core::server::{CodeInfo, HostCommand, HostControl, HostEvent};
use ratatui::DefaultTerminal;
use tokio::sync::mpsc;
use tokio::time::interval;

use crate::ui;

const KICK_COMMAND: &str = "/kick ";

/// 채팅창에 표시되는 한 줄
pub enum LogLine {
    Chat { from: MemberInfo, text: String },
    System(String),
    Alert(String),
}

/// 방장의 답을 기다리는 입장 요청
pub struct JoinRequest {
    pub id: u64,
    pub nickname: String,
    pub ip: IpAddr,
}

/// 방장 화면에만 필요한 것들
pub struct HostView {
    pub address: String,
    pub code: Option<CodeInfo>,
    pub control: HostControl,
}

/// 화면을 그리는 데 필요한 모든 상태
pub struct App {
    pub title: String,
    pub address: Option<String>,
    pub invite: Option<CodeInfo>,
    pub my_id: MemberId,
    pub members: Vec<MemberInfo>,
    pub log: Vec<LogLine>,
    pub input: String,
    pub connected: bool,
    pub should_quit: bool,
    /// 방장일 때만 Some
    pub host_commands: Option<mpsc::Sender<HostCommand>>,
    pub requests: Vec<JoinRequest>,
}

impl App {
    fn new(
        title: String,
        address: Option<String>,
        invite: Option<CodeInfo>,
        my_id: MemberId,
        members: Vec<MemberInfo>,
        host_commands: Option<mpsc::Sender<HostCommand>>,
    ) -> Self {
        Self {
            title,
            address,
            invite,
            my_id,
            members,
            log: vec![LogLine::System("입장했습니다.".into())],
            input: String::new(),
            connected: true,
            should_quit: false,
            host_commands,
            requests: Vec::new(),
        }
    }

    fn on_key(&mut self, key: KeyEvent, outgoing: &mpsc::Sender<ClientMsg>) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => self.should_quit = true,
            KeyCode::Char('c') if ctrl => self.should_quit = true,
            KeyCode::Char('y') if ctrl => self.answer_request(true),
            KeyCode::Char('n') if ctrl => self.answer_request(false),
            KeyCode::Enter => {
                let text = self.input.trim().to_string();
                if text.is_empty() || !self.connected {
                    return;
                }
                if self.run_host_command(&text) {
                    self.input.clear();
                    return;
                }
                // try_send: 화면 루프는 절대 기다리지 않는다.
                if outgoing.try_send(ClientMsg::Chat { text }).is_err() {
                    self.log
                        .push(LogLine::System("메시지를 보내지 못했습니다.".into()));
                    return;
                }
                self.input.clear();
            }
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Char(c) if !ctrl && self.input.chars().count() < MAX_CHAT_CHARS => {
                self.input.push(c);
            }
            _ => {}
        }
    }

    /// 가장 오래 기다린 입장 요청에 답한다.
    fn answer_request(&mut self, approve: bool) {
        if self.requests.is_empty() {
            return;
        }
        let request = self.requests.remove(0);
        let id = request.id;
        let command = if approve {
            HostCommand::Approve { id }
        } else {
            self.log.push(LogLine::System(format!(
                "{} 님의 입장을 거절했습니다.",
                request.nickname
            )));
            HostCommand::Deny { id }
        };
        self.send_host_command(command);
    }

    /// 방장의 명령이면 처리하고 true를 돌려준다. 그 외에는 일반 채팅이다.
    fn run_host_command(&mut self, text: &str) -> bool {
        if self.host_commands.is_none() {
            return false;
        }
        let Some(nickname) = text.strip_prefix(KICK_COMMAND) else {
            return false;
        };
        self.send_host_command(HostCommand::Kick {
            nickname: nickname.trim().to_string(),
        });
        true
    }

    fn send_host_command(&mut self, command: HostCommand) {
        let sent = self
            .host_commands
            .as_ref()
            .is_some_and(|commands| commands.try_send(command).is_ok());
        if !sent {
            self.log
                .push(LogLine::System("방장 명령을 보내지 못했습니다.".into()));
        }
    }

    fn on_host_event(&mut self, event: HostEvent) {
        match event {
            HostEvent::CodeChanged { info, alert } => {
                self.invite = Some(info);
                if let Some(alert) = alert {
                    self.log.push(LogLine::Alert(alert));
                }
            }
            HostEvent::Alert(alert) => self.log.push(LogLine::Alert(alert)),
            HostEvent::JoinRequest { id, nickname, ip } => {
                self.log.push(LogLine::Alert(format!(
                    "{nickname} 님({ip})이 입장을 요청했습니다."
                )));
                self.requests.push(JoinRequest { id, nickname, ip });
            }
            HostEvent::JoinRequestClosed { id } => {
                if let Some(index) = self.requests.iter().position(|r| r.id == id) {
                    let request = self.requests.remove(index);
                    self.log.push(LogLine::System(format!(
                        "{} 님의 입장 요청이 사라졌습니다.",
                        request.nickname
                    )));
                }
            }
        }
    }

    fn on_server_msg(&mut self, msg: ServerMsg) {
        match msg {
            ServerMsg::Chat { from, text } => self.log.push(LogLine::Chat { from, text }),
            ServerMsg::MemberJoined { member } => {
                self.log.push(LogLine::System(format!(
                    "{} 님이 입장했습니다.",
                    member.nickname
                )));
                self.members.push(member);
            }
            ServerMsg::MemberLeft { member } => {
                self.log.push(LogLine::System(format!(
                    "{} 님이 나갔습니다.",
                    member.nickname
                )));
                self.members.retain(|m| m.id != member.id);
            }
            ServerMsg::Kicked => {
                self.log
                    .push(LogLine::Alert("방장이 나를 내보냈습니다.".into()));
            }
            ServerMsg::Welcome { .. } | ServerMsg::Reject { .. } => {}
        }
    }

    fn on_disconnected(&mut self) {
        self.connected = false;
        self.members.clear();
        self.log.push(LogLine::System(
            "방과의 연결이 끊어졌습니다. Esc로 종료하세요.".into(),
        ));
    }
}

/// 방장이 아니면 영원히 기다린다. (select!에서 이 분기가 절대 실행되지 않는다)
async fn next_host_event(events: &mut Option<mpsc::Receiver<HostEvent>>) -> Option<HostEvent> {
    match events {
        Some(events) => events.recv().await,
        None => std::future::pending().await,
    }
}

/// 이벤트 루프: 그리기 → (키 입력 | 서버 메시지 | 방장 알림 | 1초) 기다리기 → 상태 갱신 → 반복
pub async fn run(
    terminal: &mut DefaultTerminal,
    mut session: Session,
    title: String,
    host_view: Option<HostView>,
) -> Result<()> {
    let (address, invite, mut host_events, host_commands) = match host_view {
        Some(view) => (
            Some(view.address),
            view.code,
            Some(view.control.events),
            Some(view.control.commands),
        ),
        None => (None, None, None, None),
    };
    let mut app = App::new(
        title,
        address,
        invite,
        session.my_id,
        std::mem::take(&mut session.members),
        host_commands,
    );
    let mut keys = EventStream::new();
    // 초대 코드의 남은 시간을 갱신하기 위한 타이머
    let mut redraw = interval(Duration::from_secs(1));

    while !app.should_quit {
        terminal.draw(|frame| ui::draw(frame, &app))?;

        tokio::select! {
            event = keys.next() => match event {
                // Windows는 키를 뗄 때(Release)도 이벤트를 보내므로 Press만 처리한다.
                Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                    app.on_key(key, &session.outgoing);
                }
                Some(Ok(_)) => {} // 창 크기 변경 등: 다음 루프에서 다시 그리기만 하면 된다.
                Some(Err(e)) => return Err(e.into()),
                None => break,
            },
            // 연결이 끊긴 뒤에는 이 분기를 아예 기다리지 않는다.
            msg = session.incoming.recv(), if app.connected => match msg {
                Some(msg) => app.on_server_msg(msg),
                None => app.on_disconnected(),
            },
            event = next_host_event(&mut host_events) => match event {
                Some(event) => app.on_host_event(event),
                None => host_events = None,
            },
            _ = redraw.tick(), if app.invite.is_some() => {}
        }
    }
    Ok(())
}
