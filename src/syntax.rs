use std::{cmp::Ordering, iter, ops::Range};

use crate::{
    buffer::BufferContent,
    buffer_position::{BufferPosition, BufferRange},
    pattern::{MatchResult, Pattern, PatternState},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    Whitespace,
    Text,
    Comment,
    Keyword,
    Type,
    Symbol,
    String,
    Literal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Token {
    pub kind: TokenKind,
    pub range: Range<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineKind {
    Finished,
    Unfinished(usize, PatternState),
}

impl Default for LineKind {
    fn default() -> Self {
        Self::Finished
    }
}

pub struct Syntax {
    extensions: Vec<String>,
    rules: Vec<(TokenKind, Pattern)>,
}

impl Syntax {
    pub fn new() -> Self {
        Self {
            extensions: Vec::new(),
            rules: Vec::new(),
        }
    }

    pub fn add_extension(&mut self, extension: String) {
        self.extensions.push(extension);
    }

    pub fn add_rule(&mut self, kind: TokenKind, pattern: Pattern) {
        self.rules.push((kind, pattern));
    }

    fn parse_line(
        &self,
        line: &str,
        previous_line_kind: LineKind,
        tokens: &mut Vec<Token>,
    ) -> LineKind {
        tokens.clear();

        if self.rules.len() == 0 {
            tokens.push(Token {
                kind: TokenKind::Text,
                range: 0..line.len(),
            });
            return LineKind::Finished;
        }

        let line_len = line.len();
        let mut line_index = 0;

        match previous_line_kind {
            LineKind::Finished => (),
            LineKind::Unfinished(pattern_index, state) => {
                match self.rules[pattern_index].1.matches_with_state(line, &state) {
                    MatchResult::Ok(len) => {
                        tokens.push(Token {
                            kind: self.rules[pattern_index].0,
                            range: 0..len,
                        });
                        line_index += len;
                    }
                    MatchResult::Err => (),
                    MatchResult::Pending(_, state) => {
                        tokens.push(Token {
                            kind: self.rules[pattern_index].0,
                            range: 0..line_len,
                        });
                        return LineKind::Unfinished(pattern_index, state);
                    }
                }
            }
        }

        while line_index < line_len {
            let line_slice = &line[line_index..];
            let whitespace_len = line_slice
                .bytes()
                .take_while(|b| b.is_ascii_whitespace())
                .count();
            let line_slice = &line_slice[whitespace_len..];

            let mut best_pattern_index = 0;
            let mut max_len = 0;
            for (i, (kind, pattern)) in self.rules.iter().enumerate() {
                match pattern.matches(line_slice) {
                    MatchResult::Ok(len) => {
                        if len > max_len {
                            max_len = len;
                            best_pattern_index = i;
                        }
                    }
                    MatchResult::Err => (),
                    MatchResult::Pending(_, state) => {
                        tokens.push(Token {
                            kind: *kind,
                            range: line_index..line_len,
                        });
                        return LineKind::Unfinished(i, state);
                    }
                }
            }

            let mut kind = self.rules[best_pattern_index].0;

            if max_len == 0 {
                kind = TokenKind::Text;
                max_len = line_slice
                    .bytes()
                    .take_while(|b| b.is_ascii_alphanumeric())
                    .count()
                    .max(1);
            }

            max_len += whitespace_len;

            let from = line_index;
            line_index = line_len.min(line_index + max_len);
            tokens.push(Token {
                kind,
                range: from..line_index,
            });
        }

        LineKind::Finished
    }
}

#[derive(Debug, Clone, Copy)]
pub struct SyntaxHandle(usize);

#[derive(Default)]
pub struct SyntaxCollection {
    syntaxes: Vec<Syntax>,
}

impl SyntaxCollection {
    pub fn add(&mut self, syntax: Syntax) {
        self.syntaxes.push(syntax);
    }

    pub fn find_by_extension(&self, extension: &str) -> Option<SyntaxHandle> {
        for (i, syntax) in self.syntaxes.iter().enumerate() {
            for ext in &syntax.extensions {
                if extension == ext {
                    return Some(SyntaxHandle(i));
                }
            }
        }

        None
    }

    pub fn get(&self, handle: SyntaxHandle) -> &Syntax {
        &self.syntaxes[handle.0]
    }
}

#[derive(Default, Clone)]
struct HighlightedLine {
    kind: LineKind,
    tokens: Vec<Token>,
}

#[derive(Default)]
pub struct HighlightedBuffer {
    lines: Vec<HighlightedLine>,
}

impl HighlightedBuffer {
    pub fn highligh_all(&mut self, syntax: &Syntax, buffer: &BufferContent) {
        self.lines
            .resize(buffer.line_count(), HighlightedLine::default());

        let mut previous_line_kind = LineKind::Finished;
        for (bline, hline) in buffer.lines_from(0).zip(self.lines.iter_mut()) {
            hline.kind = syntax.parse_line(&bline.text[..], previous_line_kind, &mut hline.tokens);
            previous_line_kind = hline.kind;
        }
    }

    pub fn on_insert(&mut self, syntax: &Syntax, buffer: &BufferContent, range: BufferRange) {
        let mut previous_line_kind = if range.from.line_index > 0 {
            self.lines[range.from.line_index - 1].kind
        } else {
            LineKind::Finished
        };

        if range.from.line_index == range.to.line_index {
            let bline = buffer.line(range.from.line_index);
            let hline = &mut self.lines[range.from.line_index];
            hline.kind = syntax.parse_line(&bline.text[..], previous_line_kind, &mut hline.tokens);
            previous_line_kind = hline.kind;
        } else {
            let insert_index = range.from.line_index + 1;
            let insert_count = range.to.line_index - range.from.line_index;
            self.lines.splice(
                insert_index..insert_index,
                iter::repeat(HighlightedLine::default()).take(insert_count),
            );

            for (bline, hline) in buffer
                .lines_from(range.from.line_index)
                .zip(self.lines[range.from.line_index..].iter_mut())
                .take(insert_count + 1)
            {
                hline.kind =
                    syntax.parse_line(&bline.text[..], previous_line_kind, &mut hline.tokens);
                previous_line_kind = hline.kind;
            }
        }

        let line_index = range.to.line_index + 1;
        for (bline, hline) in buffer
            .lines_from(line_index)
            .zip(self.lines[line_index..].iter_mut())
        {
            previous_line_kind =
                syntax.parse_line(&bline.text[..], previous_line_kind, &mut hline.tokens);
            if previous_line_kind == LineKind::Finished && hline.kind == previous_line_kind {
                break;
            }

            hline.kind = previous_line_kind;
        }
    }

    pub fn on_delete(&mut self, syntax: &Syntax, buffer: &BufferContent, range: BufferRange) {
        self.highligh_all(syntax, buffer);
    }

    pub fn find_token_kind_at(&self, position: BufferPosition) -> TokenKind {
        if position.line_index >= self.lines.len() {
            return TokenKind::Text;
        }

        let x = position.column_index;
        let tokens = &self.lines[position.line_index].tokens;
        match tokens.binary_search_by(|t| {
            if x < t.range.start {
                Ordering::Greater
            } else if x >= t.range.end {
                Ordering::Less
            } else {
                Ordering::Equal
            }
        }) {
            Ok(index) => tokens[index].kind,
            Err(_) => TokenKind::Text,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_token(slice: &str, kind: TokenKind, line: &str, token: &Token) {
        assert_eq!(kind, token.kind);
        assert_eq!(slice, &line[token.range.clone()]);
    }

    #[test]
    fn test_no_syntax() {
        let syntax = Syntax::new();
        let mut tokens = Vec::new();
        let line = " fn main() ;  ";
        let line_kind = syntax.parse_line(line, LineKind::Finished, &mut tokens);

        assert_eq!(LineKind::Finished, line_kind);
        assert_eq!(1, tokens.len());
        assert_token(line, TokenKind::Text, line, &tokens[0]);
    }

    #[test]
    fn test_one_rule_syntax() {
        let mut syntax = Syntax::new();
        syntax.add_rule(TokenKind::Symbol, Pattern::new(";").unwrap());

        let mut tokens = Vec::new();
        let line = " fn main() ;  ";
        let line_kind = syntax.parse_line(line, LineKind::Finished, &mut tokens);

        assert_eq!(LineKind::Finished, line_kind);
        assert_eq!(6, tokens.len());
        assert_token(" fn", TokenKind::Text, line, &tokens[0]);
        assert_token(" main", TokenKind::Text, line, &tokens[1]);
        assert_token("(", TokenKind::Text, line, &tokens[2]);
        assert_token(")", TokenKind::Text, line, &tokens[3]);
        assert_token(" ;", TokenKind::Symbol, line, &tokens[4]);
        assert_token("  ", TokenKind::Text, line, &tokens[5]);
    }

    #[test]
    fn test_simple_syntax() {
        let mut syntax = Syntax::new();
        syntax.add_rule(TokenKind::Keyword, Pattern::new("fn").unwrap());
        syntax.add_rule(TokenKind::Symbol, Pattern::new("%(").unwrap());
        syntax.add_rule(TokenKind::Symbol, Pattern::new("%)").unwrap());

        let mut tokens = Vec::new();
        let line = " fn main() ;  ";
        let line_kind = syntax.parse_line(line, LineKind::Finished, &mut tokens);

        assert_eq!(LineKind::Finished, line_kind);
        assert_eq!(6, tokens.len());
        assert_token(" fn", TokenKind::Keyword, line, &tokens[0]);
        assert_token(" main", TokenKind::Text, line, &tokens[1]);
        assert_token("(", TokenKind::Symbol, line, &tokens[2]);
        assert_token(")", TokenKind::Symbol, line, &tokens[3]);
        assert_token(" ;", TokenKind::Text, line, &tokens[4]);
        assert_token("  ", TokenKind::Text, line, &tokens[5]);
    }

    #[test]
    fn test_multiline_syntax() {
        let mut syntax = Syntax::new();
        syntax.add_rule(TokenKind::Comment, Pattern::new("/*{!(*/).$}").unwrap());

        let mut tokens = Vec::new();
        let line0 = "before /* comment";
        let line1 = "only comment";
        let line2 = "still comment */ after";

        let line0_kind = syntax.parse_line(line0, LineKind::Finished, &mut tokens);
        match line0_kind {
            LineKind::Unfinished(i, _) => assert_eq!(0, i),
            _ => panic!("{:?}", line0_kind),
        }
        assert_eq!(2, tokens.len());
        assert_token("before", TokenKind::Text, line0, &tokens[0]);
        assert_token(" /* comment", TokenKind::Comment, line0, &tokens[1]);

        let line1_kind = syntax.parse_line(line1, line0_kind, &mut tokens);
        match line1_kind {
            LineKind::Unfinished(i, _) => assert_eq!(0, i),
            _ => panic!("{:?}", line1_kind),
        }
        assert_eq!(1, tokens.len());
        assert_token("only comment", TokenKind::Comment, line1, &tokens[0]);

        let line2_kind = syntax.parse_line(line2, line1_kind, &mut tokens);
        assert_eq!(LineKind::Finished, line2_kind);
        assert_eq!(2, tokens.len());
        assert_token("still comment */", TokenKind::Comment, line2, &tokens[0]);
        assert_token(" after", TokenKind::Text, line2, &tokens[1]);
    }
}
