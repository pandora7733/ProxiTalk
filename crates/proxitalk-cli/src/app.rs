//! 화면 상태(App)와 이벤트 루프

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use proxitalk_core::client::Session;
use proxitalk_core::protocol::{ClientMsg, MemberId, MemberInfo, ServerMsg};
use ratatui::DefaultTerminal;

use crate::ui;

/// 채팅창에 표시되는 한 줄
pub enum LogLine {
    Chat { from: MemberInfo, text: String },
    System(String),
}

/// 화면을 그리는 데 필요한 모든 상태
pub struct App {
    pub title: String,
    pub my_id: MemberId,
    pub members: Vec<MemberInfo>,
    pub log: Vec<LogLine>,
    pub input: String,
    pub connected: bool,
    pub should_quit: bool,
}

impl App {
    fn new(title: String, my_id: MemberId, members: Vec<MemberInfo>) -> Self {
        Self {
            title,
            my_id,
            members,
            log: vec![LogLine::System("입장했습니다.".into())],
            input: String::new(),
            connected: true,
            should_quit: false,
        }
    }

    fn on_key(&mut self, key: KeyEvent, outgoing: &tokio::sync::mpsc::Sender<ClientMsg>) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => self.should_quit = true,
            KeyCode::Char('c') if ctrl => self.should_quit = true,
            KeyCode::Enter => {
                let text = self.input.trim().to_string();
                if text.is_empty() || !self.connected {
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
            KeyCode::Char(c) if !ctrl => self.input.push(c),
            _ => {}
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

/// 이벤트 루프: 그리기 → (키 입력 | 서버 메시지) 기다리기 → 상태 갱신 → 반복
pub async fn run(
    terminal: &mut DefaultTerminal,
    mut session: Session,
    title: String,
) -> Result<()> {
    let mut app = App::new(title, session.my_id, std::mem::take(&mut session.members));
    let mut keys = EventStream::new();

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
        }
    }
    Ok(())
}