use std::{
    fmt,
    fs::File,
    io,
    num::NonZeroU8,
    ops::RangeBounds,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    str::CharIndices,
};

use crate::{
    buffer_position::{BufferPosition, BufferPositionIndex, BufferRange},
    events::{EditorEvent, EditorEventQueue},
    help,
    history::{Edit, EditKind, History},
    pattern::Pattern,
    platform::{Platform, PlatformRequest, ProcessHandle, ProcessTag, SharedBuf},
    syntax::{HighlightResult, HighlightedBuffer, SyntaxCollection, SyntaxHandle},
    word_database::{WordDatabase, WordIter, WordKind},
};

pub fn find_delimiter_pair_at(text: &str, index: usize, delimiter: char) -> Option<(usize, usize)> {
    let mut is_right_delim = false;
    let mut last_i = 0;
    for (i, c) in text.char_indices() {
        if c != delimiter {
            continue;
        }

        if i >= index {
            if is_right_delim {
                return Some((last_i + delimiter.len_utf8(), i));
            }

            if i != index {
                break;
            }
        }

        is_right_delim = !is_right_delim;
        last_i = i;
    }

    None
}

pub fn parse_path_and_position(text: &str) -> (&str, Option<BufferPosition>) {
    let text = text.trim();
    match text.rfind(':') {
        Some(i) => match text[i + 1..].parse() {
            Ok(position) => (&text[..i], Some(position)),
            Err(_) => (text, None),
        },
        None => (text, None),
    }
}

pub fn find_path_and_position_at(text: &str, index: usize) -> (&str, Option<BufferPosition>) {
    let (left, right) = text.split_at(index);
    let from = match left.rfind(|c: char| c.is_ascii_whitespace()) {
        Some(i) => i + 1,
        None => 0,
    };
    let to = match right.find(|c: char| c.is_ascii_whitespace() || c == ':') {
        Some(i) => {
            if index + i - from == 1 {
                text.len()
            } else {
                index + i
            }
        }
        None => text.len(),
    };
    let path = &text[from..to];
    match path.rfind(':') {
        None | Some(1) => {
            let position = text[to..].strip_prefix(':').and_then(|t| t.parse().ok());
            (path, position)
        }
        Some(i) => {
            let position = path[i + 1..].parse().ok();
            (&path[..i], position)
        }
    }
}

pub struct CharDisplayDistance {
    pub distance: usize,
    pub char_index: usize,
}
pub struct CharDisplayDistances<'a> {
    char_indices: CharIndices<'a>,
    len: usize,
    tab_size: NonZeroU8,
}
impl<'a> CharDisplayDistances<'a> {
    pub fn new(text: &'a str, tab_size: NonZeroU8) -> Self {
        Self {
            char_indices: text.char_indices(),
            len: 0,
            tab_size,
        }
    }
}
impl<'a> CharDisplayDistances<'a> {
    fn calc_next(&mut self, char_index: usize, c: char) -> CharDisplayDistance {
        self.len += match c {
            '\t' => self.tab_size.get() as _,
            _ => 1,
        };
        CharDisplayDistance {
            distance: self.len,
            char_index,
        }
    }
}
impl<'a> Iterator for CharDisplayDistances<'a> {
    type Item = CharDisplayDistance;
    fn next(&mut self) -> Option<Self::Item> {
        let (i, c) = self.char_indices.next()?;
        Some(self.calc_next(i, c))
    }
}
impl<'a> DoubleEndedIterator for CharDisplayDistances<'a> {
    fn next_back(&mut self) -> Option<Self::Item> {
        let (i, c) = self.char_indices.next_back()?;
        Some(self.calc_next(i, c))
    }
}

pub struct WordRefWithIndex<'a> {
    pub kind: WordKind,
    pub text: &'a str,
    pub index: usize,
}
impl<'a> WordRefWithIndex<'a> {
    pub fn to_word_ref_with_position(self, line_index: usize) -> WordRefWithPosition<'a> {
        WordRefWithPosition {
            kind: self.kind,
            text: self.text,
            position: BufferPosition::line_col(line_index as _, self.index as _),
        }
    }
}

pub struct WordRefWithPosition<'a> {
    pub kind: WordKind,
    pub text: &'a str,
    pub position: BufferPosition,
}
impl<'a> WordRefWithPosition<'a> {
    pub fn end_position(&self) -> BufferPosition {
        BufferPosition::line_col(
            self.position.line_index,
            self.position.column_byte_index + self.text.len() as BufferPositionIndex,
        )
    }
}

struct BufferLinePool {
    pool: Vec<BufferLine>,
}

impl BufferLinePool {
    pub const fn new() -> Self {
        Self { pool: Vec::new() }
    }

    pub fn acquire(&mut self) -> BufferLine {
        match self.pool.pop() {
            Some(mut line) => {
                line.text.clear();
                line
            }
            None => BufferLine::new(),
        }
    }

    pub fn release(&mut self, line: BufferLine) {
        self.pool.push(line);
    }
}

pub struct BufferLine {
    text: String,
}

impl BufferLine {
    fn new() -> Self {
        Self {
            text: String::new(),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.text
    }

    pub fn chars_from<'a>(
        &'a self,
        index: usize,
    ) -> (
        impl 'a + Iterator<Item = (usize, char)>,
        impl 'a + Iterator<Item = (usize, char)>,
    ) {
        let (left, right) = self.text.split_at(index);
        let left_chars = left.char_indices().rev();
        let right_chars = right.char_indices().map(move |(i, c)| (index + i, c));
        (left_chars, right_chars)
    }

    pub fn words_from(
        &self,
        index: usize,
    ) -> (
        WordRefWithIndex,
        impl Iterator<Item = WordRefWithIndex>,
        impl Iterator<Item = WordRefWithIndex>,
    ) {
        let mid_word = self.word_at(index);
        let mid_start_index = mid_word.index;
        let mid_end_index = mid_start_index + mid_word.text.len();

        let left = &self.text[..mid_start_index];
        let right = &self.text[mid_end_index..];

        let mut left_column_index = mid_start_index;
        let left_words = WordIter(left).rev().map(move |w| {
            left_column_index -= w.text.len();
            WordRefWithIndex {
                kind: w.kind,
                text: w.text,
                index: left_column_index,
            }
        });

        let mut right_column_index = mid_end_index;
        let right_words = WordIter(right).map(move |w| {
            let index = right_column_index;
            right_column_index += w.text.len();
            WordRefWithIndex {
                kind: w.kind,
                text: w.text,
                index,
            }
        });

        (mid_word, left_words, right_words)
    }

    pub fn word_at(&self, index: usize) -> WordRefWithIndex {
        let (before, after) = self.text.split_at(index);
        match WordIter(after).next() {
            Some(right) => match WordIter(before).next_back() {
                Some(left) => {
                    if left.kind == right.kind {
                        let end_index = index + right.text.len();
                        let index = index - left.text.len();
                        WordRefWithIndex {
                            kind: left.kind,
                            text: &self.text[index..end_index],
                            index,
                        }
                    } else {
                        WordRefWithIndex {
                            kind: right.kind,
                            text: right.text,
                            index,
                        }
                    }
                }
                None => WordRefWithIndex {
                    kind: right.kind,
                    text: right.text,
                    index,
                },
            },
            None => WordRefWithIndex {
                kind: WordKind::Whitespace,
                text: "",
                index,
            },
        }
    }

    pub fn split_off(&mut self, other: &mut BufferLine, index: usize) {
        other.text.clear();
        other.push_text(&self.text[index..]);

        self.text.truncate(index);
    }

    pub fn insert_text(&mut self, index: usize, text: &str) {
        self.text.insert_str(index, text);
    }

    pub fn push_text(&mut self, text: &str) {
        self.text.push_str(text);
    }

    pub fn delete_range<R>(&mut self, range: R)
    where
        R: RangeBounds<usize>,
    {
        self.text.drain(range);
    }
}

pub struct BufferContent {
    lines: Vec<BufferLine>,
    line_pool: BufferLinePool,
}

impl BufferContent {
    pub fn new() -> Self {
        Self {
            lines: vec![BufferLine::new()],
            line_pool: BufferLinePool::new(),
        }
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    pub fn lines(
        &self,
    ) -> impl ExactSizeIterator<Item = &BufferLine> + DoubleEndedIterator<Item = &BufferLine> {
        self.lines.iter()
    }

    pub fn line_at(&self, index: usize) -> &BufferLine {
        &self.lines[index]
    }

    pub fn end(&self) -> BufferPosition {
        let last_line_index = self.lines.len() - 1;
        BufferPosition::line_col(
            last_line_index as _,
            self.lines[last_line_index].as_str().len() as _,
        )
    }

    pub fn read<R>(&mut self, read: &mut R) -> io::Result<()>
    where
        R: io::BufRead,
    {
        for line in self.lines.drain(..) {
            self.line_pool.release(line);
        }

        loop {
            let mut line = self.line_pool.acquire();
            match read.read_line(&mut line.text) {
                Ok(0) => {
                    self.line_pool.release(line);
                    break;
                }
                Ok(_) => {
                    if line.text.ends_with('\n') {
                        line.text.truncate(line.text.len() - 1);
                    }
                    if line.text.ends_with('\r') {
                        line.text.truncate(line.text.len() - 1);
                    }

                    self.lines.push(line);
                }
                Err(e) => return Err(e),
            }
        }

        if self.lines.is_empty() {
            self.lines.push(self.line_pool.acquire());
        }

        if self.lines[0].text.as_bytes().starts_with(b"\xef\xbb\xbf") {
            self.lines[0].text.drain(..3);
        }

        Ok(())
    }

    pub fn write<W>(&self, write: &mut W) -> io::Result<()>
    where
        W: io::Write,
    {
        for line in &self.lines {
            writeln!(write, "{}", line.as_str())?;
        }
        Ok(())
    }

    pub fn saturate_position(&self, mut position: BufferPosition) -> BufferPosition {
        position.line_index = position.line_index.min((self.line_count() - 1) as _);
        let line = self.line_at(position.line_index as _).as_str();
        position.column_byte_index = position.column_byte_index.min(line.len() as _);
        position
    }

    pub fn append_range_text_to_string(&self, range: BufferRange, text: &mut String) {
        let from = self.saturate_position(range.from);
        let to = self.saturate_position(range.to);

        let first_line = self.lines[from.line_index as usize].as_str();
        if from.line_index == to.line_index {
            let range_text =
                &first_line[from.column_byte_index as usize..to.column_byte_index as usize];
            text.push_str(range_text);
        } else {
            text.push_str(&first_line[from.column_byte_index as usize..]);
            let lines_range = (from.line_index as usize + 1)..to.line_index as usize;
            if lines_range.start < lines_range.end {
                for line in &self.lines[lines_range] {
                    text.push('\n');
                    text.push_str(line.as_str());
                }
            }

            let to_line = &self.lines[to.line_index as usize];
            text.push('\n');
            text.push_str(&to_line.as_str()[..to.column_byte_index as usize]);
        }
    }

    pub fn find_search_ranges(&self, pattern: &Pattern, ranges: &mut Vec<BufferRange>) {
        if pattern.is_empty() {
            return;
        }
        let search_anchor = pattern.search_anchor();
        for (line_index, line) in self.lines.iter().enumerate() {
            let line = line.as_str();
            for (column_index, text) in pattern.match_indices(line, search_anchor) {
                let from = BufferPosition::line_col(line_index as _, column_index as _);
                let end = column_index + text.len();
                let to = BufferPosition::line_col(line_index as _, end as _);
                ranges.push(BufferRange::between(from, to));
            }
        }
    }

    pub fn insert_text(&mut self, position: BufferPosition, text: &str) -> BufferRange {
        if !text.contains('\n') {
            let line = &mut self.lines[position.line_index as usize];
            let previous_len = line.as_str().len();
            line.insert_text(position.column_byte_index as _, text);
            let len_diff = line.as_str().len() - previous_len;

            let end_position = BufferPosition::line_col(
                position.line_index,
                position.column_byte_index + len_diff as BufferPositionIndex,
            );
            BufferRange::between(position, end_position)
        } else {
            let mut split_line = self.line_pool.acquire();
            self.lines[position.line_index as usize]
                .split_off(&mut split_line, position.column_byte_index as _);

            let mut line_count = 0 as BufferPositionIndex;
            let mut lines = text.lines();
            if let Some(line) = lines.next() {
                self.lines[position.line_index as usize].push_text(&line);
            }
            for line_text in lines {
                line_count += 1;

                let mut line = self.line_pool.acquire();
                line.push_text(line_text);
                self.lines
                    .insert((position.line_index + line_count) as _, line);
            }

            let end_position = if text.ends_with('\n') {
                line_count += 1;
                self.lines
                    .insert((position.line_index + line_count) as usize, split_line);

                BufferPosition::line_col(position.line_index + line_count, 0)
            } else {
                let line = &mut self.lines[(position.line_index + line_count) as usize];
                let column_byte_index = line.as_str().len() as _;
                line.push_text(split_line.as_str());

                BufferPosition::line_col(position.line_index + line_count, column_byte_index)
            };

            BufferRange::between(position, end_position)
        }
    }

    pub fn delete_range(&mut self, range: BufferRange) {
        let from = range.from;
        let to = range.to;

        if from.line_index == to.line_index {
            let line = &mut self.lines[from.line_index as usize];
            line.delete_range(from.column_byte_index as usize..to.column_byte_index as usize);
        } else {
            self.lines[from.line_index as usize].delete_range(from.column_byte_index as usize..);
            let lines_range = (from.line_index as usize + 1)..to.line_index as usize;
            if lines_range.start < lines_range.end {
                for line in self.lines.drain(lines_range) {
                    self.line_pool.release(line);
                }
            }
            let to_line_index = from.line_index + 1;
            if (to_line_index as usize) < self.lines.len() {
                let to_line = self.lines.remove(to_line_index as _);
                self.lines[from.line_index as usize]
                    .push_text(&to_line.as_str()[to.column_byte_index as usize..]);
            }
        }
    }

    pub fn clear(&mut self) {
        for line in self.lines.drain(..) {
            self.line_pool.release(line);
        }
        self.lines.push(self.line_pool.acquire());
    }

    pub fn words_from(
        &self,
        position: BufferPosition,
    ) -> (
        WordRefWithPosition,
        impl Iterator<Item = WordRefWithPosition>,
        impl Iterator<Item = WordRefWithPosition>,
    ) {
        let position = self.saturate_position(position);
        let line_index = position.line_index as _;
        let column_byte_index = position.column_byte_index as _;

        let (mid_word, left_words, right_words) =
            self.line_at(line_index as _).words_from(column_byte_index);

        (
            mid_word.to_word_ref_with_position(line_index),
            left_words.map(move |w| w.to_word_ref_with_position(line_index)),
            right_words.map(move |w| w.to_word_ref_with_position(line_index)),
        )
    }

    pub fn word_at(&self, position: BufferPosition) -> WordRefWithPosition {
        let position = self.saturate_position(position);
        self.line_at(position.line_index as _)
            .word_at(position.column_byte_index as _)
            .to_word_ref_with_position(position.line_index as _)
    }

    pub fn position_before(&self, mut position: BufferPosition) -> BufferPosition {
        position.column_byte_index = self.line_at(position.line_index as _).as_str()
            [..position.column_byte_index as usize]
            .char_indices()
            .next_back()
            .map(|(i, _)| i as _)
            .unwrap_or(0);
        position
    }

    pub fn find_delimiter_pair_at(
        &self,
        position: BufferPosition,
        delimiter: char,
    ) -> Option<BufferRange> {
        let position = self.saturate_position(position);
        let line = self.line_at(position.line_index as _).as_str();
        let range = find_delimiter_pair_at(line, position.column_byte_index as _, delimiter)?;
        Some(BufferRange::between(
            BufferPosition::line_col(position.line_index, range.0 as _),
            BufferPosition::line_col(position.line_index, range.1 as _),
        ))
    }

    pub fn find_balanced_chars_at(
        &self,
        position: BufferPosition,
        left: char,
        right: char,
    ) -> Option<BufferRange> {
        fn find<I>(iter: I, target: char, other: char, balance: &mut usize) -> Option<usize>
        where
            I: Iterator<Item = (usize, char)>,
        {
            let mut b = *balance;
            for (i, c) in iter {
                if c == target {
                    if b == 0 {
                        *balance = 0;
                        return Some(i);
                    } else {
                        b -= 1;
                    }
                } else if c == other {
                    b += 1;
                }
            }
            *balance = b;
            None
        }

        let position = self.saturate_position(position);
        let line = self.line_at(position.line_index as _).as_str();
        let (before, after) = line.split_at(position.column_byte_index as _);

        let mut balance = 0;

        let mut left_position = None;
        let mut right_position = None;

        let mut after_chars = after.char_indices();
        if let Some((i, c)) = after_chars.next() {
            if c == left {
                left_position = Some(position.column_byte_index as usize + i + c.len_utf8());
            } else if c == right {
                right_position = Some(position.column_byte_index as usize + i);
            }
        }

        let right_position = match right_position {
            Some(column_index) => BufferPosition::line_col(position.line_index, column_index as _),
            None => match find(after_chars, right, left, &mut balance) {
                Some(column_byte_index) => {
                    let column_byte_index = position.column_byte_index as usize + column_byte_index;
                    BufferPosition::line_col(position.line_index, column_byte_index as _)
                }
                None => {
                    let mut pos = None;
                    for line_index in (position.line_index as usize + 1)..self.line_count() {
                        let line = self.line_at(line_index).as_str();
                        if let Some(column_byte_index) =
                            find(line.char_indices(), right, left, &mut balance)
                        {
                            pos = Some(BufferPosition::line_col(
                                line_index as _,
                                column_byte_index as _,
                            ));
                            break;
                        }
                    }
                    pos?
                }
            },
        };

        balance = 0;

        let left_position = match left_position {
            Some(column_index) => BufferPosition::line_col(position.line_index, column_index as _),
            None => match find(before.char_indices().rev(), left, right, &mut balance) {
                Some(column_byte_index) => {
                    let column_byte_index = column_byte_index + left.len_utf8();
                    BufferPosition::line_col(position.line_index, column_byte_index as _)
                }
                None => {
                    let mut pos = None;
                    for line_index in (0..position.line_index).rev() {
                        let line = self.line_at(line_index as _).as_str();
                        if let Some(column_byte_index) =
                            find(line.char_indices().rev(), left, right, &mut balance)
                        {
                            let column_byte_index = column_byte_index + left.len_utf8();
                            pos =
                                Some(BufferPosition::line_col(line_index, column_byte_index as _));
                            break;
                        }
                    }
                    pos?
                }
            },
        };

        Some(BufferRange::between(left_position, right_position))
    }
}

impl fmt::Display for BufferContent {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let end_index = self.lines.len() - 1;
        for line in &self.lines[..end_index] {
            f.write_str(line.as_str())?;
            f.write_str("\n")?;
        }
        f.write_str(self.lines[end_index].as_str())
    }
}

#[derive(Default)]
pub struct BufferCapabilities {
    pub has_history: bool,
    pub can_save: bool,
    pub uses_word_database: bool,
    pub auto_close: bool,
}
impl BufferCapabilities {
    pub fn text() -> Self {
        Self {
            has_history: true,
            can_save: true,
            auto_close: false,
            uses_word_database: true,
        }
    }

    pub fn log() -> Self {
        Self {
            has_history: false,
            can_save: false,
            auto_close: false,
            uses_word_database: false,
        }
    }
}

pub struct Buffer {
    alive: bool,
    handle: BufferHandle,
    pub path: PathBuf,
    content: BufferContent,
    syntax_handle: SyntaxHandle,
    highlighted: HighlightedBuffer,
    history: History,
    search_ranges: Vec<BufferRange>,
    needs_save: bool,
    pub capabilities: BufferCapabilities,
}

impl Buffer {
    fn new(handle: BufferHandle) -> Self {
        Self {
            alive: true,
            handle,
            path: PathBuf::new(),
            content: BufferContent::new(),
            syntax_handle: SyntaxHandle::default(),
            highlighted: HighlightedBuffer::new(),
            history: History::new(),
            search_ranges: Vec::new(),
            needs_save: false,
            capabilities: BufferCapabilities::default(),
        }
    }

    fn dispose(&mut self, word_database: &mut WordDatabase) {
        self.remove_all_words_from_database(word_database);
        self.content.clear();

        self.alive = false;
        self.path.clear();
        self.syntax_handle = SyntaxHandle::default();
        self.highlighted.clear();
        self.history.clear();
        self.search_ranges.clear();
        self.needs_save = false;
        self.capabilities = BufferCapabilities::default();
    }

    fn remove_all_words_from_database(&mut self, word_database: &mut WordDatabase) {
        if self.capabilities.uses_word_database {
            for line in &self.content.lines {
                for word in WordIter(line.as_str()).of_kind(WordKind::Identifier) {
                    word_database.remove(word);
                }
            }
        }
    }

    pub fn handle(&self) -> BufferHandle {
        self.handle
    }

    pub fn highlighted(&self) -> &HighlightedBuffer {
        &self.highlighted
    }

    pub fn update_highlighting(&mut self, syntaxes: &SyntaxCollection) -> HighlightResult {
        self.highlighted
            .highlight_dirty_lines(syntaxes.get(self.syntax_handle), &self.content)
    }

    pub fn refresh_syntax(&mut self, syntaxes: &SyntaxCollection) {
        let path = self.path.to_str().unwrap_or("");
        if path.is_empty() {
            return;
        }

        let syntax_handle = syntaxes.find_handle_by_path(path).unwrap_or_default();

        if self.syntax_handle != syntax_handle {
            self.syntax_handle = syntax_handle;
            self.highlighted.clear();
            self.highlighted.on_insert(BufferRange::between(
                BufferPosition::zero(),
                BufferPosition::line_col((self.content.line_count() - 1) as _, 0),
            ));
        }
    }

    pub fn content(&self) -> &BufferContent {
        &self.content
    }

    pub fn needs_save(&self) -> bool {
        self.capabilities.can_save && self.needs_save
    }

    pub fn insert_text(
        &mut self,
        word_database: &mut WordDatabase,
        position: BufferPosition,
        text: &str,
        events: &mut EditorEventQueue,
    ) -> BufferRange {
        self.search_ranges.clear();
        let position = self.content.saturate_position(position);

        if text.is_empty() {
            return BufferRange::between(position, position);
        }
        self.needs_save = true;

        let range = Self::insert_text_no_history(
            &mut self.content,
            &mut self.highlighted,
            self.capabilities.uses_word_database,
            word_database,
            position,
            text,
        );

        events.enqueue_buffer_insert(self.handle, range, text);

        if self.capabilities.has_history {
            self.history.add_edit(Edit {
                kind: EditKind::Insert,
                range,
                text,
            });
        }

        range
    }

    fn insert_text_no_history(
        content: &mut BufferContent,
        highlighted: &mut HighlightedBuffer,
        uses_word_database: bool,
        word_database: &mut WordDatabase,
        position: BufferPosition,
        text: &str,
    ) -> BufferRange {
        if uses_word_database {
            for word in WordIter(content.line_at(position.line_index as _).as_str())
                .of_kind(WordKind::Identifier)
            {
                word_database.remove(word);
            }
        }

        let range = content.insert_text(position, text);
        highlighted.on_insert(range);

        if uses_word_database {
            let line_count = range.to.line_index - range.from.line_index + 1;
            for line in content
                .lines()
                .skip(range.from.line_index as _)
                .take(line_count as _)
            {
                for word in WordIter(line.as_str()).of_kind(WordKind::Identifier) {
                    word_database.add(word);
                }
            }
        }

        range
    }

    pub fn delete_range(
        &mut self,
        word_database: &mut WordDatabase,
        mut range: BufferRange,
        events: &mut EditorEventQueue,
    ) {
        self.search_ranges.clear();
        range.from = self.content.saturate_position(range.from);
        range.to = self.content.saturate_position(range.to);

        if range.from == range.to {
            return;
        }
        self.needs_save = true;

        events.enqueue(EditorEvent::BufferDeleteText {
            handle: self.handle,
            range,
        });

        let from = range.from;
        let to = range.to;

        if self.capabilities.has_history {
            fn add_history_delete_line(buffer: &mut Buffer, from: BufferPosition) {
                let line = buffer.content.line_at(from.line_index as _).as_str();
                let range = BufferRange::between(
                    BufferPosition::line_col(from.line_index, line.len() as _),
                    BufferPosition::line_col(from.line_index + 1, 0),
                );
                buffer.history.add_edit(Edit {
                    kind: EditKind::Delete,
                    range,
                    text: "\n",
                });
                buffer.history.add_edit(Edit {
                    kind: EditKind::Delete,
                    range: BufferRange::between(from, range.from),
                    text: &line[from.column_byte_index as usize..],
                });
            }

            if from.line_index == to.line_index {
                let text = &self.content.line_at(from.line_index as _).as_str()
                    [from.column_byte_index as usize..to.column_byte_index as usize];
                self.history.add_edit(Edit {
                    kind: EditKind::Delete,
                    range,
                    text,
                });
            } else {
                let text = &self.content.line_at(to.line_index as _).as_str()
                    [..to.column_byte_index as usize];
                self.history.add_edit(Edit {
                    kind: EditKind::Delete,
                    range: BufferRange::between(BufferPosition::line_col(to.line_index, 0), to),
                    text,
                });
                for line_index in ((from.line_index + 1)..to.line_index).rev() {
                    add_history_delete_line(self, BufferPosition::line_col(line_index, 0));
                }
                add_history_delete_line(self, from);
            }
        }

        Self::delete_range_no_history(
            &mut self.content,
            &mut self.highlighted,
            self.capabilities.uses_word_database,
            word_database,
            range,
        );
    }

    fn delete_range_no_history(
        content: &mut BufferContent,
        highlighted: &mut HighlightedBuffer,
        uses_word_database: bool,
        word_database: &mut WordDatabase,
        range: BufferRange,
    ) {
        if uses_word_database {
            let line_count = range.to.line_index - range.from.line_index + 1;
            for line in content
                .lines()
                .skip(range.from.line_index as _)
                .take(line_count as _)
            {
                for word in WordIter(line.as_str()).of_kind(WordKind::Identifier) {
                    word_database.remove(word);
                }
            }

            content.delete_range(range);

            for word in WordIter(content.line_at(range.from.line_index as _).as_str())
                .of_kind(WordKind::Identifier)
            {
                word_database.add(word);
            }
        } else {
            content.delete_range(range);
        }

        highlighted.on_delete(range);
    }

    pub fn commit_edits(&mut self) {
        self.history.commit_edits();
    }

    pub fn undo<'a>(
        &'a mut self,
        word_database: &mut WordDatabase,
        events: &mut EditorEventQueue,
    ) -> impl 'a + ExactSizeIterator<Item = Edit<'a>> + DoubleEndedIterator<Item = Edit<'a>> {
        self.apply_history_edits(word_database, events, History::undo_edits)
    }

    pub fn redo<'a>(
        &'a mut self,
        word_database: &mut WordDatabase,
        events: &mut EditorEventQueue,
    ) -> impl 'a + ExactSizeIterator<Item = Edit<'a>> + DoubleEndedIterator<Item = Edit<'a>> {
        self.apply_history_edits(word_database, events, History::redo_edits)
    }

    fn apply_history_edits<'a, F, I>(
        &'a mut self,
        word_database: &mut WordDatabase,
        events: &mut EditorEventQueue,
        selector: F,
    ) -> I
    where
        F: FnOnce(&'a mut History) -> I,
        I: 'a + Clone + ExactSizeIterator<Item = Edit<'a>>,
    {
        self.search_ranges.clear();
        self.needs_save = true;

        let content = &mut self.content;
        let highlighted = &mut self.highlighted;
        let uses_word_database = self.capabilities.uses_word_database;

        let edits = selector(&mut self.history);
        for edit in edits.clone() {
            match edit.kind {
                EditKind::Insert => {
                    Self::insert_text_no_history(
                        content,
                        highlighted,
                        uses_word_database,
                        word_database,
                        edit.range.from,
                        edit.text,
                    );
                    events.enqueue_buffer_insert(self.handle, edit.range, edit.text);
                }
                EditKind::Delete => {
                    Self::delete_range_no_history(
                        content,
                        highlighted,
                        uses_word_database,
                        word_database,
                        edit.range,
                    );
                    events.enqueue(EditorEvent::BufferDeleteText {
                        handle: self.handle,
                        range: edit.range,
                    });
                }
            }
        }

        edits
    }

    pub fn set_search(&mut self, pattern: &Pattern) {
        self.search_ranges.clear();
        self.content
            .find_search_ranges(pattern, &mut self.search_ranges);
    }

    pub fn search_ranges(&self) -> &[BufferRange] {
        &self.search_ranges
    }

    pub fn save_to_file(
        &mut self,
        new_path: Option<&Path>,
        events: &mut EditorEventQueue,
    ) -> io::Result<()> {
        let new_path = match new_path {
            Some(path) => {
                self.capabilities.can_save = true;
                self.path.clear();
                self.path.push(path);
                true
            }
            None => false,
        };

        if !self.capabilities.can_save {
            return Ok(());
        }

        let file = File::create(&self.path)?;
        self.content.write(&mut io::BufWriter::new(file))?;

        self.capabilities.can_save = true;
        self.needs_save = false;

        events.enqueue(EditorEvent::BufferSave {
            handle: self.handle,
            new_path,
        });
        Ok(())
    }

    pub fn discard_and_reload_from_file(
        &mut self,
        word_database: &mut WordDatabase,
        events: &mut EditorEventQueue,
    ) -> io::Result<()> {
        self.history.clear();
        self.search_ranges.clear();
        self.needs_save = false;

        self.remove_all_words_from_database(word_database);
        self.content.clear();
        self.highlighted.clear();

        events.enqueue(EditorEvent::BufferOpen {
            handle: self.handle,
        });

        if let Some(mut reader) = help::open(&self.path) {
            self.content.read(&mut reader)?;
        } else if let Ok(file) = File::open(&self.path) {
            let mut reader = io::BufReader::new(file);
            self.content.read(&mut reader)?;
        }

        self.highlighted.on_insert(BufferRange::between(
            BufferPosition::zero(),
            BufferPosition::line_col((self.content.line_count() - 1) as _, 0),
        ));

        if self.capabilities.uses_word_database {
            for line in &self.content.lines {
                for word in WordIter(line.as_str()).of_kind(WordKind::Identifier) {
                    word_database.add(word);
                }
            }
        }

        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct BufferHandle(pub u32);

pub struct InsertProcess {
    pub alive: bool,
    pub buffer_handle: BufferHandle,
    pub position: BufferPosition,
    pub input: Option<SharedBuf>,
    pub output: Vec<u8>,
}

#[derive(Default)]
pub struct BufferCollection {
    buffers: Vec<Buffer>,
    insert_processes: Vec<InsertProcess>,
}

impl BufferCollection {
    pub fn add_new(&mut self) -> &mut Buffer {
        let mut handle = None;
        for (i, buffer) in self.buffers.iter_mut().enumerate() {
            if !buffer.alive {
                handle = Some(BufferHandle(i as _));
                break;
            }
        }
        let handle = match handle {
            Some(handle) => handle,
            None => {
                let handle = BufferHandle(self.buffers.len() as _);
                self.buffers.push(Buffer::new(handle));
                handle
            }
        };

        let buffer = &mut self.buffers[handle.0 as usize];
        buffer.alive = true;
        buffer
    }

    pub fn get(&self, handle: BufferHandle) -> &Buffer {
        &self.buffers[handle.0 as usize]
    }

    pub fn get_mut(&mut self, handle: BufferHandle) -> &mut Buffer {
        &mut self.buffers[handle.0 as usize]
    }

    pub fn find_with_path(&self, buffers_root: &Path, path: &Path) -> Option<BufferHandle> {
        if path.as_os_str().is_empty() {
            return None;
        }

        for buffer in self.iter() {
            let buffer_path = buffer.path.as_path();
            let buffer_path = buffer_path
                .strip_prefix(buffers_root)
                .unwrap_or(buffer_path);

            if buffer_path == path {
                return Some(buffer.handle());
            }
        }

        None
    }

    pub fn iter(&self) -> impl Iterator<Item = &Buffer> {
        self.buffers.iter().filter(|b| b.alive)
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut Buffer> {
        self.buffers.iter_mut().filter(|b| b.alive)
    }

    pub fn defer_remove(&self, handle: BufferHandle, events: &mut EditorEventQueue) {
        let buffer = &self.buffers[handle.0 as usize];
        if buffer.alive {
            events.enqueue(EditorEvent::BufferClose { handle });
        }
    }

    pub fn remove(&mut self, handle: BufferHandle, word_database: &mut WordDatabase) {
        let buffer = &mut self.buffers[handle.0 as usize];
        if buffer.alive {
            buffer.dispose(word_database);
        }
    }

    pub fn spawn_insert_process(
        &mut self,
        platform: &mut Platform,
        mut command: Command,
        buffer_handle: BufferHandle,
        position: BufferPosition,
        stdin: Option<SharedBuf>,
    ) {
        let mut index = None;
        for (i, process) in self.insert_processes.iter().enumerate() {
            if !process.alive {
                index = Some(i);
                break;
            }
        }
        let index = match index {
            Some(index) => index,
            None => {
                let index = self.insert_processes.len();
                self.insert_processes.push(InsertProcess {
                    alive: false,
                    buffer_handle,
                    position,
                    input: None,
                    output: Vec::new(),
                });
                index
            }
        };

        let process = &mut self.insert_processes[index];
        process.alive = true;
        process.buffer_handle = buffer_handle;
        process.position = position;
        process.input = stdin;
        process.output.clear();

        let stdin = match process.input {
            Some(_) => Stdio::piped(),
            None => Stdio::null(),
        };
        command.stdin(stdin);
        command.stdout(Stdio::piped());
        command.stderr(Stdio::null());

        platform.enqueue_request(PlatformRequest::SpawnProcess {
            tag: ProcessTag::Buffer(index),
            command,
            buf_len: 4 * 1024,
        });
    }

    pub fn on_process_spawned(
        &mut self,
        platform: &mut Platform,
        index: usize,
        handle: ProcessHandle,
    ) {
        if let Some(buf) = self.insert_processes[index].input.take() {
            platform.enqueue_request(PlatformRequest::WriteToProcess { handle, buf });
            platform.enqueue_request(PlatformRequest::CloseProcessInput { handle });
        }
    }

    pub fn on_process_output(
        &mut self,
        word_database: &mut WordDatabase,
        index: usize,
        bytes: &[u8],
        events: &mut EditorEventQueue,
    ) {
        let process = &mut self.insert_processes[index];
        process.output.extend_from_slice(bytes);

        let len = match process.output.iter().rposition(|&b| b == b'\n') {
            Some(i) => i + 1,
            None => return,
        };

        let buffer = &mut self.buffers[process.buffer_handle.0 as usize];
        if buffer.alive {
            let text = &process.output[..len];
            let insert_range = match std::str::from_utf8(text) {
                Ok(text) => {
                    process.position = buffer.content().saturate_position(process.position);
                    buffer.insert_text(word_database, process.position, text, events)
                }
                Err(_) => BufferRange::zero(),
            };
            process.output.drain(..len);

            for process in &mut self.insert_processes {
                if process.buffer_handle == buffer.handle() {
                    process.position = process.position.insert(insert_range);
                }
            }
        }
    }

    pub fn on_process_exit(
        &mut self,
        word_database: &mut WordDatabase,
        index: usize,
        events: &mut EditorEventQueue,
    ) {
        let process = &mut self.insert_processes[index];
        process.alive = false;

        let buffer = &mut self.buffers[process.buffer_handle.0 as usize];
        if buffer.alive {
            if let Ok(text) = std::str::from_utf8(&process.output) {
                buffer.insert_text(word_database, process.position, text, events);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer_position::BufferPosition;

    #[test]
    fn test_find_delimiter_pair_at() {
        let text = "|a|bcd|efg|";
        assert_eq!(Some((1, 2)), find_delimiter_pair_at(text, 0, '|'));
        assert_eq!(Some((1, 2)), find_delimiter_pair_at(text, 2, '|'));
        assert_eq!(None, find_delimiter_pair_at(text, 4, '|'));
        assert_eq!(Some((7, 10)), find_delimiter_pair_at(text, 6, '|'));
        assert_eq!(Some((7, 10)), find_delimiter_pair_at(text, 10, '|'));
        assert_eq!(None, find_delimiter_pair_at(text, 11, '|'));
    }

    #[test]
    fn test_find_path_at() {
        let text = "/path/file:45";
        assert_eq!(
            ("/path/file", Some(BufferPosition::line_col(44, 0))),
            find_path_and_position_at(text, 0)
        );
        assert_eq!(
            ("/path/file", Some(BufferPosition::line_col(44, 0))),
            find_path_and_position_at(text, 1)
        );
        assert_eq!(
            ("/path/file", Some(BufferPosition::line_col(44, 0))),
            find_path_and_position_at(text, text.len())
        );
        assert_eq!(
            ("/path/file", Some(BufferPosition::line_col(44, 0))),
            find_path_and_position_at(text, 3)
        );
        assert_eq!(
            ("/path/file", Some(BufferPosition::line_col(44, 0))),
            find_path_and_position_at(text, 8)
        );

        let text = "xx /path/file:";
        assert_eq!(("xx", None), find_path_and_position_at(text, 0));
        assert_eq!(("xx", None), find_path_and_position_at(text, 1));
        assert_eq!(("xx", None), find_path_and_position_at(text, 2));
        assert_eq!(("/path/file", None), find_path_and_position_at(text, 3));
        assert_eq!(
            ("/path/file", None),
            find_path_and_position_at(text, text.len() - 1)
        );
        assert_eq!(
            ("/path/file", None),
            find_path_and_position_at(text, text.len())
        );

        let text = "xx /path/file:3xx";
        assert_eq!(
            ("/path/file", Some(BufferPosition::line_col(2, 0))),
            find_path_and_position_at(text, 3)
        );
        assert_eq!(
            ("/path/file", Some(BufferPosition::line_col(2, 0))),
            find_path_and_position_at(text, text.len() - 5)
        );
        assert_eq!(
            ("/path/file", Some(BufferPosition::line_col(2, 0))),
            find_path_and_position_at(text, text.len() - 4)
        );
        assert_eq!(
            ("/path/file", Some(BufferPosition::line_col(2, 0))),
            find_path_and_position_at(text, text.len() - 3)
        );
        assert_eq!(
            ("/path/file", Some(BufferPosition::line_col(2, 0))),
            find_path_and_position_at(text, text.len() - 2)
        );
        assert_eq!(
            ("/path/file", Some(BufferPosition::line_col(2, 0))),
            find_path_and_position_at(text, text.len() - 1)
        );
        assert_eq!(
            ("/path/file", Some(BufferPosition::line_col(2, 0))),
            find_path_and_position_at(text, text.len())
        );

        let text = "c:/absolute/path/file";
        assert_eq!((text, None), find_path_and_position_at(text, 0));
        assert_eq!((text, None), find_path_and_position_at(text, 1));
        assert_eq!((text, None), find_path_and_position_at(text, 2));
        assert_eq!(
            (text, None),
            find_path_and_position_at(text, text.len() - 1)
        );
        assert_eq!(
            (text, None),
            find_path_and_position_at(text, text.len() - 2)
        );

        let text = "c:/absolute/path/file:4";
        let path = "c:/absolute/path/file";
        assert_eq!(
            (path, Some(BufferPosition::line_col(3, 0))),
            find_path_and_position_at(text, 0)
        );
        assert_eq!(
            (path, Some(BufferPosition::line_col(3, 0))),
            find_path_and_position_at(text, 1)
        );
        assert_eq!(
            (path, Some(BufferPosition::line_col(3, 0))),
            find_path_and_position_at(text, 2)
        );
        assert_eq!(
            (path, Some(BufferPosition::line_col(3, 0))),
            find_path_and_position_at(text, 3)
        );

        let text = "xx c:/absolute/path/file:4,5xx";
        let path = "c:/absolute/path/file";
        assert_eq!(
            (path, Some(BufferPosition::line_col(3, 4))),
            find_path_and_position_at(text, 3)
        );
        assert_eq!(
            (path, Some(BufferPosition::line_col(3, 4))),
            find_path_and_position_at(text, 4)
        );
        assert_eq!(
            (path, Some(BufferPosition::line_col(3, 4))),
            find_path_and_position_at(text, 5)
        );
        assert_eq!(
            (path, Some(BufferPosition::line_col(3, 4))),
            find_path_and_position_at(text, 24)
        );
        assert_eq!(
            (path, Some(BufferPosition::line_col(3, 4))),
            find_path_and_position_at(text, 25)
        );
        assert_eq!(
            (path, Some(BufferPosition::line_col(3, 4))),
            find_path_and_position_at(text, 26)
        );
        assert_eq!(
            (path, Some(BufferPosition::line_col(3, 4))),
            find_path_and_position_at(text, 27)
        );
    }

    #[test]
    fn display_distance() {
        fn display_len(text: &str) -> usize {
            let tab_size = NonZeroU8::new(4).unwrap();
            CharDisplayDistances::new(text, tab_size)
                .last()
                .map(|d| d.distance)
                .unwrap_or(0)
        }

        assert_eq!(0, display_len(""));
        assert_eq!(1, display_len("a"));
        assert_eq!(1, display_len("é"));
        assert_eq!(4, display_len("    "));
        assert_eq!(4, display_len("\t"));
        assert_eq!(8, display_len("\t\t"));
        assert_eq!(8, display_len("    \t"));
        assert_eq!(5, display_len("x\t"));
        assert_eq!(6, display_len("xx\t"));
        assert_eq!(7, display_len("xxx\t"));
        assert_eq!(8, display_len("xxxx\t"));
    }

    fn buffer_from_str(text: &str) -> BufferContent {
        let mut buffer = BufferContent::new();
        buffer.insert_text(BufferPosition::zero(), text);
        buffer
    }

    #[test]
    fn buffer_utf8_support() {
        let mut buffer = buffer_from_str("abd");
        let range = buffer.insert_text(BufferPosition::line_col(0, 2), "ç");
        assert_eq!(
            BufferRange::between(
                BufferPosition::line_col(0, 2),
                BufferPosition::line_col(0, (2 + 'ç'.len_utf8()) as _)
            ),
            range
        );
    }

    #[test]
    fn buffer_content_insert_text() {
        let mut buffer = BufferContent::new();

        assert_eq!(1, buffer.line_count());
        assert_eq!("", buffer.to_string());

        buffer.insert_text(BufferPosition::line_col(0, 0), "hold");
        buffer.insert_text(BufferPosition::line_col(0, 2), "r");
        buffer.insert_text(BufferPosition::line_col(0, 1), "ello w");
        assert_eq!(1, buffer.line_count());
        assert_eq!("hello world", buffer.to_string());

        buffer.insert_text(BufferPosition::line_col(0, 5), "\n");
        buffer.insert_text(
            BufferPosition::line_col(1, 6),
            " appending more\nand more\nand even more\nlines",
        );
        assert_eq!(5, buffer.line_count());
        assert_eq!(
            "hello\n world appending more\nand more\nand even more\nlines",
            buffer.to_string()
        );

        let mut buffer = buffer_from_str("this is content");
        buffer.insert_text(BufferPosition::line_col(0, 8), "some\nmultiline ");
        assert_eq!(2, buffer.line_count());
        assert_eq!("this is some\nmultiline content", buffer.to_string());

        let mut buffer = buffer_from_str("this is content");
        buffer.insert_text(
            BufferPosition::line_col(0, 8),
            "some\nmore\nextensive\nmultiline ",
        );
        assert_eq!(4, buffer.line_count());
        assert_eq!(
            "this is some\nmore\nextensive\nmultiline content",
            buffer.to_string()
        );

        let mut buffer = buffer_from_str("abc");
        let range = buffer.insert_text(BufferPosition::line_col(0, 3), "\n");
        assert_eq!(
            BufferRange::between(
                BufferPosition::line_col(0, 3),
                BufferPosition::line_col(1, 0)
            ),
            range
        );
    }

    #[test]
    fn buffer_content_delete_range() {
        let mut buffer = buffer_from_str("abc");
        buffer.delete_range(BufferRange::between(
            BufferPosition::line_col(0, 1),
            BufferPosition::line_col(0, 1),
        ));
        assert_eq!("abc", buffer.to_string());
        buffer.delete_range(BufferRange::between(
            BufferPosition::line_col(0, 1),
            BufferPosition::line_col(0, 2),
        ));
        assert_eq!("ac", buffer.to_string());

        let mut buffer = buffer_from_str("this is the initial\ncontent of the buffer");

        assert_eq!(2, buffer.line_count());
        assert_eq!(
            "this is the initial\ncontent of the buffer",
            buffer.to_string()
        );

        buffer.delete_range(BufferRange::between(
            BufferPosition::zero(),
            BufferPosition::zero(),
        ));
        assert_eq!(2, buffer.line_count());
        assert_eq!(
            "this is the initial\ncontent of the buffer",
            buffer.to_string()
        );

        buffer.delete_range(BufferRange::between(
            BufferPosition::line_col(0, 11),
            BufferPosition::line_col(0, 19),
        ));
        assert_eq!(2, buffer.line_count());
        assert_eq!("this is the\ncontent of the buffer", buffer.to_string());

        buffer.delete_range(BufferRange::between(
            BufferPosition::line_col(0, 8),
            BufferPosition::line_col(1, 15),
        ));
        assert_eq!(1, buffer.line_count());
        assert_eq!("this is buffer", buffer.to_string());

        let mut buffer = buffer_from_str("this\nbuffer\ncontains\nmultiple\nlines\nyes");
        assert_eq!(6, buffer.line_count());
        buffer.delete_range(BufferRange::between(
            BufferPosition::line_col(1, 4),
            BufferPosition::line_col(4, 1),
        ));
        assert_eq!("this\nbuffines\nyes", buffer.to_string());
    }

    #[test]
    fn buffer_content_delete_lines() {
        let mut buffer = buffer_from_str("first line\nsecond line\nthird line");
        assert_eq!(3, buffer.line_count());
        buffer.delete_range(BufferRange::between(
            BufferPosition::line_col(1, 0),
            BufferPosition::line_col(2, 0),
        ));
        assert_eq!("first line\nthird line", buffer.to_string());

        let mut buffer = buffer_from_str("first line\nsecond line\nthird line");
        assert_eq!(3, buffer.line_count());
        buffer.delete_range(BufferRange::between(
            BufferPosition::line_col(1, 0),
            BufferPosition::line_col(1, 11),
        ));
        assert_eq!("first line\n\nthird line", buffer.to_string());
    }

    #[test]
    fn buffer_delete_undo_redo_single_line() {
        let mut word_database = WordDatabase::new();
        let mut events = EditorEventQueue::default();

        let mut buffer = Buffer::new(BufferHandle(0));
        buffer.capabilities = BufferCapabilities::text();
        buffer.insert_text(
            &mut word_database,
            BufferPosition::zero(),
            "single line content",
            &mut events,
        );
        let range = BufferRange::between(
            BufferPosition::line_col(0, 7),
            BufferPosition::line_col(0, 12),
        );
        buffer.delete_range(&mut word_database, range, &mut events);

        assert_eq!("single content", buffer.content.to_string());
        {
            let mut ranges = buffer.undo(&mut word_database, &mut events);
            assert_eq!(range, ranges.next().unwrap().range);
            ranges.next().unwrap();
            assert!(ranges.next().is_none());
        }
        assert!(buffer.content.to_string().is_empty());
        let mut redo_iter = buffer.redo(&mut word_database, &mut events);
        redo_iter.next().unwrap();
        redo_iter.next().unwrap();
        assert!(redo_iter.next().is_none());
        drop(redo_iter);
        assert_eq!("single content", buffer.content.to_string());
    }

    #[test]
    fn buffer_delete_undo_redo_multi_line() {
        let mut word_database = WordDatabase::new();
        let mut events = EditorEventQueue::default();

        let mut buffer = Buffer::new(BufferHandle(0));
        buffer.capabilities = BufferCapabilities::text();
        let insert_range = buffer.insert_text(
            &mut word_database,
            BufferPosition::zero(),
            "multi\nline\ncontent",
            &mut events,
        );
        assert_eq!("multi\nline\ncontent", buffer.content.to_string());

        let delete_range = BufferRange::between(
            BufferPosition::line_col(0, 1),
            BufferPosition::line_col(1, 3),
        );
        buffer.delete_range(&mut word_database, delete_range, &mut events);
        assert_eq!("me\ncontent", buffer.content.to_string());

        {
            let mut undo_edits = buffer.undo(&mut word_database, &mut events);
            assert_eq!(delete_range, undo_edits.next().unwrap().range);
            assert_eq!(insert_range, undo_edits.next().unwrap().range);
            assert!(undo_edits.next().is_none());
        }
        assert_eq!("", buffer.content.to_string());

        {
            let mut redo_edits = buffer.redo(&mut word_database, &mut events);
            redo_edits.next().unwrap();
            redo_edits.next().unwrap();
            assert!(redo_edits.next().is_none());
        }
        assert_eq!("me\ncontent", buffer.content.to_string());
    }

    #[test]
    fn buffer_content_range_text() {
        let buffer = buffer_from_str("abc\ndef\nghi");
        let mut text = String::new();
        buffer.append_range_text_to_string(
            BufferRange::between(
                BufferPosition::line_col(0, 2),
                BufferPosition::line_col(2, 1),
            ),
            &mut text,
        );
        assert_eq!("c\ndef\ng", &text);
    }

    #[test]
    fn buffer_content_word_at() {
        fn col(column: usize) -> BufferPosition {
            BufferPosition::line_col(0, column as _)
        }

        fn assert_word(word: WordRefWithPosition, pos: BufferPosition, kind: WordKind, text: &str) {
            assert_eq!(pos, word.position);
            assert_eq!(kind, word.kind);
            assert_eq!(text, word.text);
        }

        let buffer = buffer_from_str("word");
        assert_word(buffer.word_at(col(0)), col(0), WordKind::Identifier, "word");
        assert_word(buffer.word_at(col(2)), col(0), WordKind::Identifier, "word");
        assert_word(buffer.word_at(col(4)), col(4), WordKind::Whitespace, "");

        let buffer = buffer_from_str("asd word+? asd");
        assert_word(buffer.word_at(col(3)), col(3), WordKind::Whitespace, " ");
        assert_word(buffer.word_at(col(4)), col(4), WordKind::Identifier, "word");
        assert_word(buffer.word_at(col(6)), col(4), WordKind::Identifier, "word");
        assert_word(buffer.word_at(col(8)), col(8), WordKind::Symbol, "+?");
        assert_word(buffer.word_at(col(9)), col(8), WordKind::Symbol, "+?");
        assert_word(buffer.word_at(col(10)), col(10), WordKind::Whitespace, " ");
    }

    #[test]
    fn buffer_content_words_from() {
        fn col(column: usize) -> BufferPosition {
            BufferPosition::line_col(0, column as _)
        }

        fn assert_word(word: WordRefWithPosition, pos: BufferPosition, kind: WordKind, text: &str) {
            assert_eq!(pos, word.position);
            assert_eq!(kind, word.kind);
            assert_eq!(text, word.text);
        }

        let buffer = buffer_from_str("word");
        let (w, mut lw, mut rw) = buffer.words_from(col(0));
        assert_word(w, col(0), WordKind::Identifier, "word");
        assert!(lw.next().is_none());
        assert!(rw.next().is_none());
        let (w, mut lw, mut rw) = buffer.words_from(col(2));
        assert_word(w, col(0), WordKind::Identifier, "word");
        assert!(lw.next().is_none());
        assert!(rw.next().is_none());
        let (w, mut lw, mut rw) = buffer.words_from(col(4));
        assert_word(w, col(4), WordKind::Whitespace, "");
        assert_word(lw.next().unwrap(), col(0), WordKind::Identifier, "word");
        assert!(lw.next().is_none());
        assert!(rw.next().is_none());

        let buffer = buffer_from_str("first second third");
        let (w, mut lw, mut rw) = buffer.words_from(col(8));
        assert_word(w, col(6), WordKind::Identifier, "second");
        assert_word(lw.next().unwrap(), col(5), WordKind::Whitespace, " ");
        assert_word(lw.next().unwrap(), col(0), WordKind::Identifier, "first");
        assert!(lw.next().is_none());
        assert_word(rw.next().unwrap(), col(12), WordKind::Whitespace, " ");
        assert_word(rw.next().unwrap(), col(13), WordKind::Identifier, "third");
        assert!(rw.next().is_none());
    }

    #[test]
    fn buffer_find_balanced_chars() {
        let buffer = buffer_from_str("(\n(\na\n)\nbc)");

        assert_eq!(
            Some(BufferRange::between(
                BufferPosition::line_col(0, 1),
                BufferPosition::line_col(4, 2)
            )),
            buffer.find_balanced_chars_at(BufferPosition::line_col(0, 0), '(', ')')
        );
        assert_eq!(
            Some(BufferRange::between(
                BufferPosition::line_col(1, 1),
                BufferPosition::line_col(3, 0)
            )),
            buffer.find_balanced_chars_at(BufferPosition::line_col(2, 0), '(', ')')
        );
        assert_eq!(
            Some(BufferRange::between(
                BufferPosition::line_col(0, 1),
                BufferPosition::line_col(4, 2)
            )),
            buffer.find_balanced_chars_at(BufferPosition::line_col(0, 1), '(', ')')
        );
        assert_eq!(
            Some(BufferRange::between(
                BufferPosition::line_col(0, 1),
                BufferPosition::line_col(4, 2)
            )),
            buffer.find_balanced_chars_at(BufferPosition::line_col(4, 0), '(', ')')
        );
        assert_eq!(
            Some(BufferRange::between(
                BufferPosition::line_col(0, 1),
                BufferPosition::line_col(4, 2)
            )),
            buffer.find_balanced_chars_at(BufferPosition::line_col(0, 0), '(', ')')
        );
        assert_eq!(
            Some(BufferRange::between(
                BufferPosition::line_col(0, 1),
                BufferPosition::line_col(4, 2)
            )),
            buffer.find_balanced_chars_at(BufferPosition::line_col(4, 2), '(', ')')
        );
    }
}
