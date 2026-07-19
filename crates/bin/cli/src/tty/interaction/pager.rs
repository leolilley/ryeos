use unicode_width::UnicodeWidthChar;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Pager {
    source: String,
    lines: Vec<String>,
    width: usize,
    viewport_rows: usize,
    offset: usize,
}

impl Pager {
    pub(crate) fn new(source: impl Into<String>, width: usize, viewport_rows: usize) -> Self {
        let source = source.into();
        let width = width.max(1);
        Self {
            lines: wrap(&source, width),
            source,
            width,
            viewport_rows: viewport_rows.max(1),
            offset: 0,
        }
    }

    pub(crate) fn set_geometry(&mut self, width: usize, viewport_rows: usize) {
        let width = width.max(1);
        if self.width != width {
            let old_offset = self.offset;
            self.lines = wrap(&self.source, width);
            self.width = width;
            self.offset = old_offset.min(self.max_offset());
        }
        self.viewport_rows = viewport_rows.max(1);
        self.offset = self.offset.min(self.max_offset());
    }

    pub(crate) fn replace_source(&mut self, source: impl Into<String>) {
        self.source = source.into();
        self.lines = wrap(&self.source, self.width);
        self.offset = self.offset.min(self.max_offset());
    }

    pub(crate) fn visible_lines(&self) -> &[String] {
        let end = (self.offset + self.viewport_rows).min(self.lines.len());
        &self.lines[self.offset..end]
    }

    #[cfg(test)]
    pub(crate) fn offset(&self) -> usize {
        self.offset
    }

    #[cfg(test)]
    pub(crate) fn line_count(&self) -> usize {
        self.lines.len()
    }

    pub(crate) fn down(&mut self) {
        self.offset = (self.offset + 1).min(self.max_offset());
    }

    pub(crate) fn up(&mut self) {
        self.offset = self.offset.saturating_sub(1);
    }

    pub(crate) fn page_down(&mut self) {
        self.offset = (self.offset + self.viewport_rows).min(self.max_offset());
    }

    pub(crate) fn page_up(&mut self) {
        self.offset = self.offset.saturating_sub(self.viewport_rows);
    }

    pub(crate) fn home(&mut self) {
        self.offset = 0;
    }

    pub(crate) fn end(&mut self) {
        self.offset = self.max_offset();
    }

    fn max_offset(&self) -> usize {
        self.lines.len().saturating_sub(self.viewport_rows)
    }
}

fn wrap(source: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for paragraph in source.split('\n') {
        if paragraph.is_empty() {
            lines.push(String::new());
            continue;
        }
        wrap_paragraph(paragraph, width, &mut lines);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn wrap_paragraph(paragraph: &str, width: usize, lines: &mut Vec<String>) {
    let mut line = String::new();
    let mut line_width = 0;
    for word in paragraph.split_whitespace() {
        let word_width = display_width(word);
        let separator = usize::from(!line.is_empty());
        if word_width <= width && line_width + separator + word_width <= width {
            if separator == 1 {
                line.push(' ');
            }
            line.push_str(word);
            line_width += separator + word_width;
            continue;
        }
        if !line.is_empty() {
            lines.push(std::mem::take(&mut line));
            line_width = 0;
        }
        if word_width <= width {
            line.push_str(word);
            line_width = word_width;
            continue;
        }
        for chunk in split_at_width(word, width) {
            if display_width(&chunk) == width {
                lines.push(chunk);
            } else {
                line = chunk;
                line_width = display_width(&line);
            }
        }
    }
    if !line.is_empty() {
        lines.push(line);
    }
}

fn split_at_width(value: &str, width: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut chunk = String::new();
    let mut chunk_width = 0;
    for value in value.chars() {
        let value_width = value.width().unwrap_or(0);
        if !chunk.is_empty() && chunk_width + value_width > width {
            chunks.push(std::mem::take(&mut chunk));
            chunk_width = 0;
        }
        chunk.push(value);
        chunk_width += value_width;
    }
    if !chunk.is_empty() {
        chunks.push(chunk);
    }
    chunks
}

fn display_width(value: &str) -> usize {
    value.chars().map(|value| value.width().unwrap_or(0)).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_words_and_wide_unicode_without_truncating() {
        assert_eq!(
            wrap("alpha beta 世界界", 6),
            vec!["alpha", "beta", "世界界"]
        );
        assert_eq!(wrap("abcdefgh", 3), vec!["abc", "def", "gh"]);
    }

    #[test]
    fn preserves_blank_lines() {
        assert_eq!(wrap("one\n\ntwo", 20), vec!["one", "", "two"]);
    }

    #[test]
    fn scrolling_and_resize_remain_bounded() {
        let mut pager = Pager::new("one two three four five six", 5, 2);
        pager.end();
        assert_eq!(pager.offset(), pager.line_count() - 2);
        pager.set_geometry(80, 20);
        assert_eq!(pager.offset(), 0);
        pager.page_up();
        assert_eq!(pager.offset(), 0);
    }
}
