use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub(crate) fn wrap_to_width(text: &str, max_w: usize) -> Vec<String> {
    let max_w = max_w.max(1);
    let mut lines = Vec::new();
    for raw in text.split('\n') {
        let mut cur = String::new();
        let mut cur_w = 0usize;
        let mut last_space: Option<usize> = None;
        for ch in raw.chars() {
            let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
            if cur_w + cw > max_w && cur_w > 0 {
                if let Some(sp) = last_space {
                    let rest = cur[sp + 1..].to_string();
                    cur.truncate(sp);
                    lines.push(std::mem::take(&mut cur));
                    cur = rest;
                    cur_w = UnicodeWidthStr::width(cur.as_str());
                } else {
                    lines.push(std::mem::take(&mut cur));
                    cur_w = 0;
                }
                last_space = None;
            }
            if ch == ' ' {
                last_space = Some(cur.len());
            }
            cur.push(ch);
            cur_w += cw;
        }
        lines.push(cur);
    }
    lines
}

pub(crate) fn wrap_display_width(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;

    for word in text.split(' ').filter(|w| !w.is_empty()) {
        let word_width = UnicodeWidthStr::width(word);
        if current.is_empty() {
            if word_width <= width {
                current.push_str(word);
                current_width = word_width;
            } else {
                lines.extend(split_word_display_width(word, width));
                current_width = 0;
            }
        } else if current_width + 1 + word_width <= width {
            current.push(' ');
            current.push_str(word);
            current_width += 1 + word_width;
        } else {
            lines.push(std::mem::take(&mut current));
            current_width = 0;
            if word_width <= width {
                current.push_str(word);
                current_width = word_width;
            } else {
                lines.extend(split_word_display_width(word, width));
            }
        }
    }

    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

pub(crate) fn split_word_display_width(word: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;
    for ch in word.chars() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0).max(1);
        if !current.is_empty() && current_width + ch_width > width {
            lines.push(std::mem::take(&mut current));
            current_width = 0;
        }
        current.push(ch);
        current_width += ch_width;
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}
