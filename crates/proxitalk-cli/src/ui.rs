//! App 상태를 화면에 그린다. (상태를 바꾸지 않고 읽기만 한다)

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, Paragraph};

use crate::app::{App, LogLine};

pub fn draw(frame: &mut Frame, app: &App) {
    // 초대 코드 줄은 코드가 있을 때만 1칸을 차지한다.
    let invite_height = u16::from(app.invite.is_some());
    let [main, input, help, invite] = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(3),
        Constraint::Length(1),
        Constraint::Length(invite_height),
    ])
    .areas(frame.area());
    let [chat, members] =
        Layout::horizontal([Constraint::Min(20), Constraint::Length(24)]).areas(main);

    draw_chat(frame, app, chat);
    draw_members(frame, app, members);
    draw_input(frame, app, input);

    let help_text = " Enter 전송 · Backspace 지우기 · Esc 나가기";
    frame.render_widget(Paragraph::new(help_text).dark_gray(), help);

    if let Some(code) = &app.invite {
        let text = format!(" 초대 코드: {code}");
        frame.render_widget(Paragraph::new(text).yellow(), invite);
    }
}

fn draw_chat(frame: &mut Frame, app: &App, area: Rect) {
    // 테두리 2줄을 뺀 높이만큼 "가장 최근" 줄만 보여준다 (자동 스크롤).
    let visible = area.height.saturating_sub(2) as usize;
    let start = app.log.len().saturating_sub(visible);

    let items: Vec<ListItem> = app.log[start..]
        .iter()
        .map(|line| ListItem::new(log_line(line, app.my_id)))
        .collect();

    let block = Block::bordered().title(format!(" ProxiTalk · {} ", app.title));
    frame.render_widget(List::new(items).block(block), area);
}

fn log_line(line: &LogLine, my_id: u32) -> Line<'_> {
    match line {
        LogLine::Chat { from, text } => {
            let name_style = if from.id == my_id {
                Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD)
            };
            Line::from(vec![
                Span::styled(from.nickname.as_str(), name_style),
                Span::raw(": "),
                Span::raw(text.as_str()),
            ])
        }
        LogLine::System(text) => Line::from(format!("* {text}")).dark_gray().italic(),
    }
}

fn draw_members(frame: &mut Frame, app: &App, area: Rect) {
    let items: Vec<ListItem> = app
        .members
        .iter()
        .map(|m| {
            if m.id == app.my_id {
                ListItem::new(format!("{} (나)", m.nickname)).cyan()
            } else {
                ListItem::new(m.nickname.as_str())
            }
        })
        .collect();

    let block = Block::bordered().title(format!(" 멤버 {}명 ", app.members.len()));
    frame.render_widget(List::new(items).block(block), area);
}

fn draw_input(frame: &mut Frame, app: &App, area: Rect) {
    let title = if app.connected {
        " 메시지 "
    } else {
        " 연결 끊김 "
    };
    let input = Paragraph::new(format!("> {}", app.input)).block(Block::bordered().title(title));
    frame.render_widget(input, area);

    // 커서 위치 = 테두리(1) + "> "(2) + 입력 문자열의 "화면상 폭"
    // 한글은 1글자가 2칸이므로 글자 수(chars().count())가 아니라 width()를 쓴다.
    let text_width = Line::from(app.input.as_str()).width() as u16;
    let x = (area.x + 3 + text_width).min(area.right().saturating_sub(2));
    frame.set_cursor_position(Position::new(x, area.y + 1));
}