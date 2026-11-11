//! 방 목록 화면: mDNS로 찾은 공개방을 보여주고, 고르면 입장한다.

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use proxitalk_core::client::{self, Session};
use proxitalk_core::discovery::{RoomEvent, RoomInfo};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Style, Stylize};
use ratatui::text::Line;
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph};
use ratatui::{DefaultTerminal, Frame};
use tokio::sync::mpsc;

const SEARCHING: &str = "공개방을 찾는 중... 안 보이면 네트워크가 mDNS를 막고 있을 수 있어요. \
                         그럴 땐 join <IP:포트> <닉네임> 으로 직접 입장하세요.";

struct Lobby {
    rooms: Vec<RoomInfo>,
    selected: usize,
    status: String,
}

impl Lobby {
    fn upsert(&mut self, room: RoomInfo) {
        match self.rooms.iter_mut().find(|r| r.id == room.id) {
            Some(existing) => *existing = room,
            None => self.rooms.push(room),
        }
    }

    fn remove(&mut self, id: &str) {
        self.rooms.retain(|r| r.id != id);
        self.selected = self.selected.min(self.rooms.len().saturating_sub(1));
    }
}

/// 방을 골라 입장하면 Some((세션, 화면 제목)), 그냥 나가면 None
pub async fn run(
    terminal: &mut DefaultTerminal,
    mut rooms: mpsc::Receiver<RoomEvent>,
    nickname: &str,
) -> Result<Option<(Session, String)>> {
    let mut lobby = Lobby {
        rooms: Vec::new(),
        selected: 0,
        status: SEARCHING.into(),
    };
    let mut keys = EventStream::new();
    let mut searching = true;

    loop {
        terminal.draw(|frame| draw(frame, &lobby))?;

        tokio::select! {
            event = keys.next() => match event {
                Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                    match key.code {
                        KeyCode::Esc => return Ok(None),
                        KeyCode::Char('c') if ctrl => return Ok(None),
                        KeyCode::Up => lobby.selected = lobby.selected.saturating_sub(1),
                        KeyCode::Down => {
                            if lobby.selected + 1 < lobby.rooms.len() {
                                lobby.selected += 1;
                            }
                        }
                        KeyCode::Enter => {
                            let Some(room) = lobby.rooms.get(lobby.selected).cloned() else {
                                continue;
                            };
                            lobby.status = format!("'{}' 방에 접속하는 중...", room.name);
                            terminal.draw(|frame| draw(frame, &lobby))?;

                            match client::connect_any(&room.addrs, nickname, None).await {
                                Ok(session) => return Ok(Some((session, room.name))),
                                Err(e) => lobby.status = format!("입장 실패: {e:#}"),
                            }
                        }
                        _ => {}
                    }
                }
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(e.into()),
                None => return Ok(None),
            },
            event = rooms.recv(), if searching => match event {
                Some(RoomEvent::Found(room)) => lobby.upsert(room),
                Some(RoomEvent::Lost { id }) => lobby.remove(&id),
                None => {
                    searching = false;
                    lobby.status = "mDNS 검색이 중단되었습니다.".into();
                }
            },
        }
    }
}

fn draw(frame: &mut Frame, lobby: &Lobby) {
    let [list_area, status_area, help_area] = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(4),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    let items: Vec<ListItem> = lobby
        .rooms
        .iter()
        .map(|room| {
            let addr = room
                .addrs
                .first()
                .map_or("주소 없음".to_string(), |a| a.to_string());
            ListItem::new(Line::from(vec![
                room.name.as_str().bold(),
                format!("   방장: {}   {addr}", room.host).dark_gray(),
            ]))
        })
        .collect();

    let title = format!(" 같은 네트워크의 공개방 {}개 ", lobby.rooms.len());
    let list = List::new(items)
        .block(Block::bordered().title(title))
        .highlight_style(Style::new().reversed())
        .highlight_symbol("> ");
    // ListState: 어느 줄이 선택됐는지. 그릴 때마다 lobby.selected로 만들어 넘긴다.
    let mut state = ListState::default().with_selected(Some(lobby.selected));
    frame.render_stateful_widget(list, list_area, &mut state);

    let status = Paragraph::new(lobby.status.as_str())
        .wrap(ratatui::widgets::Wrap { trim: true })
        .block(Block::bordered().title(" 상태 "));
    frame.render_widget(status, status_area);

    let help = " ↑↓ 선택 · Enter 입장 · Esc 나가기";
    frame.render_widget(Paragraph::new(help).dark_gray(), help_area);
}
