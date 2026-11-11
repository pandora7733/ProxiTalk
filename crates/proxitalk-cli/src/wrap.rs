//! 글자를 화면 폭에 맞춰 여러 줄로 나눈다. 원본 메시지는 바꾸지 않는다.

use unicode_width::UnicodeWidthChar;

/// 한 줄이 width칸을 넘지 않도록 나눈다. 한글처럼 2칸인 글자는 잘리지 않고 다음 줄로 넘어간다.
pub fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut line = String::new();
    let mut used = 0;

    for c in text.chars() {
        if c == '\n' {
            lines.push(std::mem::take(&mut line));
            used = 0;
            continue;
        }
        let char_width = c.width().unwrap_or(0);
        if used + char_width > width && !line.is_empty() {
            lines.push(std::mem::take(&mut line));
            used = 0;
        }
        line.push(c);
        used += char_width;
    }
    lines.push(line);
    lines
}

/// 문자열이 화면에서 차지하는 칸 수
pub fn display_width(text: &str) -> usize {
    text.chars().map(|c| c.width().unwrap_or(0)).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_stays_on_one_line() {
        assert_eq!(wrap_text("hello", 10), ["hello"]);
    }

    #[test]
    fn empty_text_is_one_empty_line() {
        assert_eq!(wrap_text("", 10), [""]);
    }

    #[test]
    fn wraps_exactly_at_the_width() {
        assert_eq!(wrap_text("abcdef", 3), ["abc", "def"]);
    }

    #[test]
    fn counts_korean_as_two_cells() {
        assert_eq!(wrap_text("가나다라", 4), ["가나", "다라"]);
    }

    #[test]
    fn wide_char_moves_to_the_next_line_when_one_cell_is_left() {
        assert_eq!(wrap_text("a가나", 4), ["a가", "나"]);
    }

    #[test]
    fn newline_starts_a_new_line() {
        assert_eq!(wrap_text("a\nb", 10), ["a", "b"]);
    }

    #[test]
    fn narrow_width_never_loops_forever_or_loses_text() {
        assert_eq!(wrap_text("가a", 1), ["가", "a"]);
        assert_eq!(wrap_text("ab", 0), ["a", "b"]);
    }

    #[test]
    fn no_line_is_wider_than_the_limit() {
        let text = "안녕하세요 hello 반갑습니다 world ".repeat(20);
        for width in 2..40 {
            let lines = wrap_text(&text, width);
            assert!(lines.iter().all(|line| display_width(line) <= width));
            assert_eq!(lines.concat(), text);
        }
    }

    #[test]
    fn display_width_counts_cells() {
        assert_eq!(display_width("a가"), 3);
    }
}
