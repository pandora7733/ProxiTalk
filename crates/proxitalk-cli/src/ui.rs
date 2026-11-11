//! App 상태를 화면에 그린다. (상태를 바꾸지 않고 읽기만 한다)

use std::time::Instant;

use proxitalk_core::server::CodeInfo;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, Paragraph};

use crate::app::{App, LogLine};
use crate::wrap::{display_width, wrap_text};

/// 입력창이 늘어날 수 있는 최대 줄 수
const MAX_INPUT_LINES: usize = 5;
const MEMBER: &str = " Enter 전송 · Backspace 지우기 · Esc 나가기";
const HOST: &str = " Enter 전송 · /kick 닉네임 내보내기 · Esc 나가기";

pub fn draw(frame: &mut Frame, app: &App) {
    let width = frame.area().width as usize;

    // 줄 수가 내용과 폭에 따라 달라지므로, 먼저 나눠 보고 그만큼 자리를 잡는다.
    let help = if app.host_commands.is_some() {
        HOST
    } else {
        MEMBER
    };
    let help_lines = wrap_text(help, width);
    let request_lines = request_text(app).map_or(Vec::new(), |text| wrap_text(&text, width));
    let mut footer_lines = app
        .invite
        .as_ref()
        .map_or(Vec::new(), |code| wrap_text(&invite_text(code), width));
    if let Some(address) = &app.address {
        footer_lines.extend(wrap_text(address, width));
    }
    let input_view = InputView::new(&app.input, width.saturating_sub(2));

    let [main, input, requests, help_area, footer] = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(input_view.lines.len() as u16 + 2),
        Constraint::Length(request_lines.len() as u16),
        Constraint::Length(help_lines.len() as u16),
        Constraint::Length(footer_lines.len() as u16),
    ])
    .areas(frame.area());
    let [chat, members] =
        Layout::horizontal([Constraint::Min(20), Constraint::Length(24)]).areas(main);

    draw_chat(frame, app, chat);
    draw_members(frame, app, members);
    draw_input(frame, app, input, &input_view);

    frame.render_widget(plain_lines(request_lines).yellow().bold(), requests);
    frame.render_widget(plain_lines(help_lines).dark_gray(), help_area);
    frame.render_widget(plain_lines(footer_lines).yellow(), footer);
}

fn plain_lines(lines: Vec<String>) -> Paragraph<'static> {
    Paragraph::new(lines.into_iter().map(Line::from).collect::<Vec<_>>())
}

fn request_text(app: &App) -> Option<String> {
    let first = app.requests.first()?;
    let waiting = app.requests.len() - 1;
    let more = if waiting > 0 {
        format!(" (대기 {waiting}명 더)")
    } else {
        String::new()
    };
    Some(format!(
        " 입장 요청: {} ({}){more} · Ctrl+Y 수락 · Ctrl+N 거절",
        first.nickname, first.ip
    ))
}

fn invite_text(code: &CodeInfo) -> String {
    let left = code.expires_at.saturating_duration_since(Instant::now());
    let seconds = left.as_secs();
    format!(
        " 초대 코드: {} (교체까지 {}:{:02})",
        code.code,
        seconds / 60,
        seconds % 60
    )
}

fn draw_chat(frame: &mut Frame, app: &App, area: Rect) {
    let visible = area.height.saturating_sub(2) as usize;
    let width = area.width.saturating_sub(2) as usize;

    // 자동 스크롤: 최신 메시지부터 거꾸로 나눠서 화면이 찰 때까지만 모은다.
    let mut lines: Vec<Line> = Vec::new();
    for entry in app.log.iter().rev() {
        if lines.len() >= visible {
            break;
        }
        let mut wrapped = log_lines(entry, app.my_id, width);
        wrapped.append(&mut lines);
        lines = wrapped;
    }
    let start = lines.len().saturating_sub(visible);
    let lines = lines.split_off(start);

    let block = Block::bordered().title(format!(" ProxiTalk · {} ", app.title));
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

/// 로그 한 건을 화면 폭에 맞춘 여러 줄로 바꾼다.
fn log_lines(entry: &LogLine, my_id: u32, width: usize) -> Vec<Line<'static>> {
    match entry {
        LogLine::Chat { from, text } => {
            let name_style = if from.id == my_id {
                Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD)
            };
            let full = format!("{}: {text}", from.nickname);
            // 닉네임이 길어 여러 줄에 걸칠 수 있으므로, 남은 닉네임 글자 수를 세면서 색칠한다.
            let mut name_left = from.nickname.chars().count();
            wrap_text(&full, width)
                .into_iter()
                .map(|line| {
                    let name_part: String = line.chars().take(name_left).collect();
                    let rest: String = line.chars().skip(name_left).collect();
                    name_left -= name_part.chars().count();
                    Line::from(vec![Span::styled(name_part, name_style), Span::raw(rest)])
                })
                .collect()
        }
        LogLine::System(text) => wrap_text(&format!("* {text}"), width)
            .into_iter()
            .map(|line| Line::from(line).dark_gray().italic())
            .collect(),
        LogLine::Alert(text) => wrap_text(&format!("! {text}"), width)
            .into_iter()
            .map(|line| Line::from(line).yellow().bold())
            .collect(),
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

/// 입력창에 보여줄 줄과 커서 위치
struct InputView {
    lines: Vec<String>,
    cursor_row: usize,
    cursor_col: usize,
}

impl InputView {
    fn new(input: &str, width: usize) -> Self {
        let width = width.max(1);
        let mut lines = wrap_text(&format!("> {input}"), width);

        // 마지막 줄이 꽉 차면 다음 글자가 들어갈 빈 줄을 하나 더 만든다.
        let last_width = lines.last().map_or(0, |line| display_width(line));
        let cursor_col = if last_width >= width {
            lines.push(String::new());
            0
        } else {
            last_width
        };

        // 너무 길면 마지막 줄들만 보여준다. 커서는 항상 맨 아래 줄에 있다.
        let start = lines.len().saturating_sub(MAX_INPUT_LINES);
        let lines = lines.split_off(start);
        Self {
            cursor_row: lines.len() - 1,
            cursor_col,
            lines,
        }
    }
}

fn draw_input(frame: &mut Frame, app: &App, area: Rect, view: &InputView) {
    let title = if app.connected {
        " 메시지 "
    } else {
        " 연결 끊김 "
    };
    let input = plain_lines(view.lines.clone()).block(Block::bordered().title(title));
    frame.render_widget(input, area);

    let x = area.x + 1 + view.cursor_col as u16;
    let y = area.y + 1 + view.cursor_row as u16;
    frame.set_cursor_position(Position::new(x, y));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_follows_the_end_of_short_input() {
        let view = InputView::new("ab", 10);
        assert_eq!(view.lines, ["> ab"]);
        assert_eq!((view.cursor_row, view.cursor_col), (0, 4));
    }

    #[test]
    fn full_line_moves_the_cursor_to_a_new_line() {
        let view = InputView::new("abcd", 6);
        assert_eq!(view.lines, ["> abcd", ""]);
        assert_eq!((view.cursor_row, view.cursor_col), (1, 0));
    }

    #[test]
    fn long_input_keeps_only_the_last_lines() {
        let view = InputView::new(&"a".repeat(100), 10);
        assert_eq!(view.lines.len(), MAX_INPUT_LINES);
        assert_eq!(view.cursor_row, MAX_INPUT_LINES - 1);
    }

    #[test]
    fn chat_lines_keep_every_character() {
        let from = proxitalk_core::protocol::MemberInfo {
            id: 1,
            nickname: "닉네임".into(),
        };
        let entry = LogLine::Chat {
            from,
            text: "가나다라마바사아자차카타파하".into(),
        };
        let lines = log_lines(&entry, 1, 10);
        assert!(lines.len() > 1);
        let joined: String = lines.iter().map(ToString::to_string).collect();
        assert_eq!(joined, "닉네임: 가나다라마바사아자차카타파하");
    }
}
