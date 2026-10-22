//! Lexer for gc-rust. Produces a flat token stream with source spans.
//!
//! See `docs/language.md` §1 for the lexical grammar. The lexer is
//! whitespace-insensitive (no significant indentation), supports line and
//! nested block comments, and recognizes typed numeric literals.

use std::fmt;
use serde::Serialize;

/// Identifies which source text a span's byte offsets index into. The primary
/// (user) source is `0`; the injected prelude and each `mod` file get their own
/// id (assigned in `compile.rs`), so spans from sources lexed in separate
/// offset spaces resolve against the RIGHT text — the debugger's multi-source
/// foundation (see docs/core-ir.md). Resolved via the
/// `CoreProgram` SourceMap.
pub type SourceId = u16;

/// A byte range into a source, `[start, end)`, tagged with which source.
#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Span {
    pub start: u32,
    pub end: u32,
    /// Which source the offsets index (0 = primary/user). See [`SourceId`].
    pub source: SourceId,
}

impl Span {
    pub fn new(start: usize, end: usize) -> Self {
        Span { start: start as u32, end: end as u32, source: 0 }
    }
    /// Combine `self.start .. other.end`, keeping `self`'s source (both operands
    /// come from the same source — this is how the parser builds compound spans).
    pub fn to(self, other: Span) -> Span {
        Span { start: self.start, end: other.end, source: self.source }
    }
}

impl fmt::Debug for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}..{}", self.start, self.end)
    }
}

/// Integer/float literal suffix recording the requested concrete type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum NumSuffix {
    None,
    I8, I16, I32, I64,
    U8, U16, U32, U64,
    F32, F64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub enum TokKind {
    // literals
    Int(u64, NumSuffix),
    Float(f64, NumSuffix),
    Str(String),
    Char(char),
    // identifiers + keywords
    Ident(String),
    Keyword(Kw),
    // punctuation / operators
    LParen, RParen, LBrace, RBrace, LBracket, RBracket,
    Comma, Semi, Colon, ColonColon, Arrow, FatArrow, Dot, DotDot, DotDotEq,
    Question, At, Hash, Dollar,
    /// `'name`: a loop label (or a lifetime, which is skipped).
    Lifetime(String),
    /// `b"ab"`: the bytes of a byte string.
    ByteStr(Vec<u8>),
    /// Invisible delimiters around an `expr` a macro substituted: they group
    /// like parentheses but are not written (`stringify!`).
    GroupOpen, GroupClose,
    Plus, Minus, Star, Slash, Percent,
    Eq, EqEq, Ne, Lt, Le, Gt, Ge,
    AndAnd, OrOr, Not,
    Amp, Pipe, Caret, Shl, Shr,
    PlusEq, MinusEq, StarEq, SlashEq, PercentEq,
    Eof,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Kw {
    Fn, Let, Mut, Struct, Enum, Impl, Trait, For, In, If, Else, Match,
    While, Loop, Return, Break, Continue, True, False, As, Where, Pub, Mod,
    Use, Type, Const, Static, SelfValue, SelfType, Extern, Unsafe,
}

impl Kw {
    fn from_ident(s: &str) -> Option<Kw> {
        Some(match s {
            "fn" => Kw::Fn, "let" => Kw::Let, "mut" => Kw::Mut,
            "struct" => Kw::Struct, "enum" => Kw::Enum, "impl" => Kw::Impl,
            "trait" => Kw::Trait, "for" => Kw::For, "in" => Kw::In,
            "if" => Kw::If, "else" => Kw::Else, "match" => Kw::Match,
            "while" => Kw::While, "loop" => Kw::Loop, "return" => Kw::Return,
            "break" => Kw::Break, "continue" => Kw::Continue,
            "true" => Kw::True, "false" => Kw::False, "as" => Kw::As,
            "where" => Kw::Where, "pub" => Kw::Pub, "mod" => Kw::Mod,
            "use" => Kw::Use, "type" => Kw::Type, "const" => Kw::Const,
            "static" => Kw::Static, "self" => Kw::SelfValue,
            "Self" => Kw::SelfType,
            "extern" => Kw::Extern,
            "unsafe" => Kw::Unsafe,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Token {
    pub kind: TokKind,
    pub span: Span,
}

#[derive(Debug)]
pub struct LexError {
    pub msg: String,
    pub span: Span,
}

pub fn lex(src: &str) -> Result<Vec<Token>, LexError> {
    lex_with_source(src, 0)
}

thread_local! {
    /// Each lexed source's text and (when it is a file) path, for the
    /// compiler's built-in macros: `line!`, `column!`, `file!`, `include_str!`.
    static SOURCES: std::cell::RefCell<std::collections::HashMap<SourceId, (std::rc::Rc<str>, Option<std::path::PathBuf>)>> = Default::default();
}

/// Record the file a source was read from (`file!`, `include_str!`).
pub fn register_source_path(source: SourceId, path: &std::path::Path) {
    SOURCES.with(|s| {
        if let Some(entry) = s.borrow_mut().get_mut(&source) { entry.1 = Some(path.to_path_buf()); }
    });
}

/// The 1-based line and column of a byte offset in a lexed source.
pub fn source_position(span: Span) -> (u32, u32) {
    SOURCES.with(|s| {
        let s = s.borrow();
        let Some((text, _)) = s.get(&span.source) else { return (1, 1) };
        let before = &text[..(span.start as usize).min(text.len())];
        let line = before.matches('\n').count() as u32 + 1;
        let column = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) as u32 + 1;
        (line, column)
    })
}

/// The path of the file a source came from, if any.
pub fn source_path(source: SourceId) -> Option<std::path::PathBuf> {
    SOURCES.with(|s| s.borrow().get(&source).and_then(|e| e.1.clone()))
}

/// Lex `src` tagging every token span with `source` (the source's [`SourceId`]).
/// Used by `compile.rs` for the prelude / `mod` files (lexed in their own offset
/// spaces) so their spans resolve against the right text. `lex` is `source = 0`.
pub fn lex_with_source(src: &str, source: SourceId) -> Result<Vec<Token>, LexError> {
    SOURCES.with(|s| { s.borrow_mut().insert(source, (std::rc::Rc::from(src), None)); });
    let mut toks = Lexer::new(src).run().map_err(|mut error| {
        error.span.source = source;
        error
    })?;
    for t in &mut toks {
        t.span.source = source;
    }
    Ok(toks)
}

struct Lexer<'a> {
    src: &'a [u8],
    text: &'a str,
    pos: usize,
    toks: Vec<Token>,
}

impl<'a> Lexer<'a> {
    fn new(text: &'a str) -> Self {
        Lexer { src: text.as_bytes(), text, pos: 0, toks: Vec::new() }
    }

    fn peek(&self) -> u8 {
        if self.pos < self.src.len() { self.src[self.pos] } else { 0 }
    }
    fn peek2(&self) -> u8 {
        if self.pos + 1 < self.src.len() { self.src[self.pos + 1] } else { 0 }
    }
    fn bump(&mut self) -> u8 {
        let c = self.peek();
        if self.pos < self.src.len() {
            self.pos += 1;
        }
        c
    }

    fn bump_char(&mut self) -> Option<char> {
        let ch = self.text[self.pos..].chars().next()?;
        self.pos += ch.len_utf8();
        Some(ch)
    }

    fn err(&self, msg: impl Into<String>, start: usize) -> LexError {
        LexError { msg: msg.into(), span: Span::new(start, self.pos) }
    }

    fn run(mut self) -> Result<Vec<Token>, LexError> {
        loop {
            self.skip_trivia()?;
            let start = self.pos;
            if self.pos >= self.src.len() {
                self.push(TokKind::Eof, start);
                return Ok(self.toks);
            }
            let c = self.peek();
            match c {
                b'0'..=b'9' => self.lex_number(start)?,
                b'"' => self.lex_string(start)?,
                // Raw strings `r"..."`, `r#"..."#`; byte strings `b"..."`,
                // `br"..."`; bytes `b'a'`.
                b'r' if self.raw_string_ahead(1) => self.lex_raw_string(start, 1, false)?,
                b'b' if self.peek_at(1) == b'r' && self.raw_string_ahead(2) => self.lex_raw_string(start, 2, true)?,
                b'b' if self.peek_at(1) == b'"' => {
                    self.pos += 1;
                    self.lex_string(start)?;
                    let Some(Token { kind: TokKind::Str(text), .. }) = self.toks.pop() else { unreachable!() };
                    self.push(TokKind::ByteStr(text.into_bytes()), start);
                }
                b'b' if self.peek_at(1) == b'\'' => {
                    self.pos += 1;
                    self.lex_char(start)?;
                    let Some(Token { kind: TokKind::Char(c), .. }) = self.toks.pop() else { unreachable!() };
                    if (c as u32) > 255 { return Err(self.err("a byte literal must be ASCII or `\\xNN`", start)); }
                    self.push(TokKind::Int(c as u64, NumSuffix::U8), start);
                }
                // `'name` (no closing quote after one char) is a label.
                b'\'' if self.is_label_start() => {
                    self.pos += 1;
                    let s = self.pos;
                    while self.pos < self.src.len() && is_ident_continue(self.peek()) { self.pos += 1; }
                    let name = self.text[s..self.pos].to_string();
                    self.push(TokKind::Lifetime(name), start);
                }
                b'\'' => self.lex_char(start)?,
                c if is_ident_start(c) => self.lex_ident(start),
                _ => self.lex_punct(start)?,
            }
        }
    }

    fn peek_at(&self, n: usize) -> u8 {
        self.src.get(self.pos + n).copied().unwrap_or(0)
    }

    /// Does a raw string (`"` after zero or more `#`) start `skip` bytes on?
    fn raw_string_ahead(&self, skip: usize) -> bool {
        let mut i = skip;
        while self.peek_at(i) == b'#' { i += 1; }
        self.peek_at(i) == b'"'
    }

    /// `r#"..."#`: no escapes; ends at `"` followed by as many `#`.
    fn lex_raw_string(&mut self, start: usize, skip: usize, bytes: bool) -> Result<(), LexError> {
        self.pos += skip;
        let mut hashes = 0;
        while self.peek() == b'#' { hashes += 1; self.pos += 1; }
        self.pos += 1; // opening quote
        let body_start = self.pos;
        loop {
            if self.pos >= self.src.len() {
                return Err(self.err("unterminated raw string literal", start));
            }
            if self.src[self.pos] == b'"' && (1..=hashes).all(|k| self.peek_at(k) == b'#') {
                let text = self.text[body_start..self.pos].to_string();
                self.pos += 1 + hashes;
                self.push(if bytes { TokKind::ByteStr(text.into_bytes()) } else { TokKind::Str(text) }, start);
                return Ok(());
            }
            self.pos += 1;
        }
    }

    fn push(&mut self, kind: TokKind, start: usize) {
        self.toks.push(Token { kind, span: Span::new(start, self.pos) });
    }

    fn skip_trivia(&mut self) -> Result<(), LexError> {
        loop {
            match self.peek() {
                b' ' | b'\t' | b'\r' | b'\n' => { self.pos += 1; }
                b'/' if self.peek2() == b'/' => {
                    while self.pos < self.src.len() && self.peek() != b'\n' {
                        self.pos += 1;
                    }
                }
                b'/' if self.peek2() == b'*' => {
                    let start = self.pos;
                    self.pos += 2;
                    let mut depth = 1;
                    while depth > 0 {
                        if self.pos >= self.src.len() {
                            return Err(self.err("unterminated block comment", start));
                        }
                        if self.peek() == b'/' && self.peek2() == b'*' {
                            depth += 1; self.pos += 2;
                        } else if self.peek() == b'*' && self.peek2() == b'/' {
                            depth -= 1; self.pos += 2;
                        } else {
                            self.pos += 1;
                        }
                    }
                }
                _ => return Ok(()),
            }
        }
    }

    fn lex_ident(&mut self, start: usize) {
        while is_ident_continue(self.peek()) {
            self.pos += 1;
        }
        let s = &self.text[start..self.pos];
        match Kw::from_ident(s) {
            Some(kw) => self.push(TokKind::Keyword(kw), start),
            None => self.push(TokKind::Ident(s.to_string()), start),
        }
    }

    fn lex_number(&mut self, start: usize) -> Result<(), LexError> {
        // Radix prefixes.
        if self.peek() == b'0' && matches!(self.peek2(), b'x' | b'b' | b'o') {
            let radix = match self.peek2() { b'x' => 16, b'b' => 2, _ => 8 };
            self.pos += 2;
            let digits_start = self.pos;
            while is_radix_digit(self.peek(), radix) || self.peek() == b'_' {
                self.pos += 1;
            }
            let raw: String =
                self.text[digits_start..self.pos].chars().filter(|c| *c != '_').collect();
            if raw.is_empty() {
                return Err(self.err("missing digits after radix prefix", start));
            }
            let v = u64::from_str_radix(&raw, radix)
                .map_err(|_| self.err("integer literal out of range", start))?;
            let suffix = self.lex_num_suffix();
            if matches!(suffix, NumSuffix::F32 | NumSuffix::F64) {
                return Err(self.err("non-decimal integer literal cannot have a floating suffix", start));
            }
            self.push(TokKind::Int(v, suffix), start);
            return Ok(());
        }

        // A number following field access is a tuple index. Its following
        // dot belongs to the next access, not a fractional component.
        let tuple_index = self.toks.last().is_some_and(|token| token.kind == TokKind::Dot);
        // Decimal integer or float.
        while self.peek().is_ascii_digit() || self.peek() == b'_' {
            self.pos += 1;
        }
        let mut is_float = false;
        // Fractional part: a dot NOT followed by another dot (range) or an
        // ident-start (method/field access like `1.foo`).
        if !tuple_index && self.peek() == b'.' && self.peek2().is_ascii_digit() {
            is_float = true;
            self.pos += 1;
            while self.peek().is_ascii_digit() || self.peek() == b'_' {
                self.pos += 1;
            }
        }
        // Exponent.
        if !tuple_index && matches!(self.peek(), b'e' | b'E') {
            let save = self.pos;
            self.pos += 1;
            if matches!(self.peek(), b'+' | b'-') {
                self.pos += 1;
            }
            if self.peek().is_ascii_digit() {
                is_float = true;
                while self.peek().is_ascii_digit() || self.peek() == b'_' {
                    self.pos += 1;
                }
            } else {
                // Not an exponent (e.g. a suffix starting with e); back off.
                self.pos = save;
            }
        }

        let body: String =
            self.text[start..self.pos].chars().filter(|c| *c != '_').collect();
        let suffix = self.lex_num_suffix();
        let suffix_is_float = matches!(suffix, NumSuffix::F32 | NumSuffix::F64);
        if is_float && suffix != NumSuffix::None && !suffix_is_float {
            return Err(self.err("floating literal cannot have an integer suffix", start));
        }
        if is_float || suffix_is_float {
            let v: f64 = body
                .parse()
                .map_err(|_| self.err("invalid float literal", start))?;
            self.push(TokKind::Float(v, suffix), start);
        } else {
            let v: u64 = body
                .parse()
                .map_err(|_| self.err("integer literal out of range", start))?;
            self.push(TokKind::Int(v, suffix), start);
        }
        Ok(())
    }

    fn lex_num_suffix(&mut self) -> NumSuffix {
        let start = self.pos;
        if !is_ident_start(self.peek()) {
            return NumSuffix::None;
        }
        while is_ident_continue(self.peek()) {
            self.pos += 1;
        }
        match &self.text[start..self.pos] {
            "i8" => NumSuffix::I8, "i16" => NumSuffix::I16,
            "i32" => NumSuffix::I32, "i64" => NumSuffix::I64,
            // `usize`/`isize` are `i64` (indices are signed 64-bit).
            "usize" | "isize" => NumSuffix::I64,
            "u8" => NumSuffix::U8, "u16" => NumSuffix::U16,
            "u32" => NumSuffix::U32, "u64" => NumSuffix::U64,
            "f32" => NumSuffix::F32, "f64" => NumSuffix::F64,
            // Unknown suffix: rewind so it lexes as a separate token (will be
            // a parse error in context, which is the right place to report it).
            _ => { self.pos = start; NumSuffix::None }
        }
    }

    fn lex_string(&mut self, start: usize) -> Result<(), LexError> {
        self.pos += 1; // opening quote
        let mut s = String::new();
        loop {
            if self.pos >= self.src.len() {
                return Err(self.err("unterminated string literal", start));
            }
            let c = self.bump();
            match c {
                b'"' => break,
                b'\\' => s.push(self.lex_escape(start)?),
                _ => {
                    // Reconstruct the UTF-8 char from the source slice.
                    let cstart = self.pos - 1;
                    let ch = self.text[cstart..].chars().next().unwrap();
                    // bump() already advanced one byte; advance the rest.
                    self.pos = cstart + ch.len_utf8();
                    s.push(ch);
                }
            }
        }
        self.push(TokKind::Str(s), start);
        Ok(())
    }

    /// At `'`: does a label or lifetime (`'outer`, `'a`) start here, rather
    /// than a char literal (`'a'`)?
    fn is_label_start(&self) -> bool {
        let b = self.src;
        let i = self.pos;
        i + 1 < b.len() && is_ident_start(b[i + 1])
            && !(i + 2 < b.len() && b[i + 2] == b'\'')
    }

    fn lex_char(&mut self, start: usize) -> Result<(), LexError> {
        self.pos += 1; // opening quote
        if self.pos >= self.src.len() {
            return Err(self.err("unterminated char literal", start));
        }
        let ch = if self.peek() == b'\\' {
            self.pos += 1;
            self.lex_escape(start)?
        } else {
            let cstart = self.pos;
            let ch = self.text[cstart..].chars().next().unwrap();
            self.pos = cstart + ch.len_utf8();
            ch
        };
        if self.peek() != b'\'' {
            return Err(self.err("unterminated char literal", start));
        }
        self.pos += 1; // closing quote
        self.push(TokKind::Char(ch), start);
        Ok(())
    }

    fn lex_escape(&mut self, start: usize) -> Result<char, LexError> {
        let e = self.bump_char().ok_or_else(|| self.err("unterminated escape", start))?;
        Ok(match e {
            'n' => '\n', 't' => '\t', 'r' => '\r', '0' => '\0',
            // `\x41`: a byte in hex (ASCII in strings and chars).
            'x' => {
                let hex = self.text.get(self.pos..self.pos + 2).ok_or_else(|| self.err("expected two hex digits after `\\x`", start))?;
                let code = u8::from_str_radix(hex, 16).map_err(|_| self.err("invalid `\\x` escape", start))?;
                self.pos += 2;
                code as char
            }
            '\\' => '\\', '\'' => '\'', '"' => '"',
            'u' => {
                if self.bump_char() != Some('{') {
                    return Err(self.err("expected `{` in unicode escape", start));
                }
                let hs = self.pos;
                while self.peek() != b'}' && self.pos < self.src.len() {
                    self.bump_char();
                }
                if self.pos == self.src.len() {
                    return Err(self.err("unterminated unicode escape", start));
                }
                let code = u32::from_str_radix(&self.text[hs..self.pos], 16)
                    .map_err(|_| self.err("invalid unicode escape", start))?;
                self.pos += 1; // closing brace
                char::from_u32(code)
                    .ok_or_else(|| self.err("invalid unicode scalar", start))?
            }
            _ => return Err(self.err("unknown escape", start)),
        })
    }

    fn lex_punct(&mut self, start: usize) -> Result<(), LexError> {
        let c = self.bump();
        let two = |l: &mut Self, k: TokKind| { l.pos += 1; k };
        let kind = match c {
            b'(' => TokKind::LParen, b')' => TokKind::RParen,
            b'{' => TokKind::LBrace, b'}' => TokKind::RBrace,
            b'[' => TokKind::LBracket, b']' => TokKind::RBracket,
            b',' => TokKind::Comma, b';' => TokKind::Semi,
            b'@' => TokKind::At, b'#' => TokKind::Hash, b'$' => TokKind::Dollar,
            b':' => if self.peek() == b':' { two(self, TokKind::ColonColon) } else { TokKind::Colon },
            b'-' => match self.peek() {
                b'>' => two(self, TokKind::Arrow),
                b'=' => two(self, TokKind::MinusEq),
                _ => TokKind::Minus,
            },
            b'+' => if self.peek() == b'=' { two(self, TokKind::PlusEq) } else { TokKind::Plus },
            b'*' => if self.peek() == b'=' { two(self, TokKind::StarEq) } else { TokKind::Star },
            b'/' => if self.peek() == b'=' { two(self, TokKind::SlashEq) } else { TokKind::Slash },
            b'%' => if self.peek() == b'=' { two(self, TokKind::PercentEq) } else { TokKind::Percent },
            b'=' => match self.peek() {
                b'=' => two(self, TokKind::EqEq),
                b'>' => two(self, TokKind::FatArrow),
                _ => TokKind::Eq,
            },
            b'!' => if self.peek() == b'=' { two(self, TokKind::Ne) } else { TokKind::Not },
            b'<' => match self.peek() {
                b'=' => two(self, TokKind::Le),
                b'<' => two(self, TokKind::Shl),
                _ => TokKind::Lt,
            },
            b'>' => match self.peek() {
                b'=' => two(self, TokKind::Ge),
                b'>' => two(self, TokKind::Shr),
                _ => TokKind::Gt,
            },
            b'&' => if self.peek() == b'&' { two(self, TokKind::AndAnd) } else { TokKind::Amp },
            b'|' => if self.peek() == b'|' { two(self, TokKind::OrOr) } else { TokKind::Pipe },
            b'^' => TokKind::Caret,
            b'?' => TokKind::Question,
            b'.' => match self.peek() {
                b'.' => {
                    self.pos += 1;
                    if self.peek() == b'=' { self.pos += 1; TokKind::DotDotEq } else { TokKind::DotDot }
                }
                _ => TokKind::Dot,
            },
            _ => {
                // Punctuation dispatch is byte-based; diagnostics must consume
                // the entire scalar, including for unsupported identifiers.
                self.pos = start;
                let ch = self.bump_char().expect("punctuation starts before EOF");
                return Err(self.err(format!("unexpected character {ch:?}"), start));
            }
        };
        self.push(kind, start);
        Ok(())
    }
}

fn is_ident_start(c: u8) -> bool {
    c == b'_' || c.is_ascii_alphabetic()
}
fn is_ident_continue(c: u8) -> bool {
    c == b'_' || c.is_ascii_alphanumeric()
}
fn is_radix_digit(c: u8, radix: u32) -> bool {
    (c as char).is_digit(radix)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(src: &str) -> Vec<TokKind> {
        lex(src).unwrap().into_iter().map(|t| t.kind).collect()
    }

    #[test]
    fn malformed_unicode_and_truncated_escapes_have_valid_spans() {
        for src in ["é", "🦀", "\"\\é\"", "'\\🦀'", "\"\\", "'\\", "\"\\u", "\"\\ué", "\"\\u{41", "\"\\u{🦀}"] {
            let error = lex(src).unwrap_err();
            let start = error.span.start as usize;
            let end = error.span.end as usize;
            assert!(start <= end && end <= src.len(), "{src:?}: {error:?}");
            assert!(src.is_char_boundary(start) && src.is_char_boundary(end), "{src:?}: {error:?}");
            let rendered = crate::diag::render("bad.gcr", src, error.span, &error.msg);
            assert!(rendered.contains("bad.gcr:1:1"), "{rendered}");
        }
        assert!(lex("é").unwrap_err().msg.contains('é'));
        assert!(lex("🦀").unwrap_err().msg.contains('🦀'));
    }

    #[test]
    fn lexer_errors_preserve_source_identity() {
        for src in ["é", "\"\\", "/* unfinished", "18446744073709551616"] {
            let error = lex_with_source(src, 17).unwrap_err();
            assert_eq!(error.span.source, 17, "{src:?}");
        }
        assert!(lex_with_source("42", 17).unwrap().iter().all(|t| t.span.source == 17));
    }

    #[test]
    fn keywords_and_idents() {
        let k = kinds("fn foo let mut value struct");
        assert_eq!(k[0], TokKind::Keyword(Kw::Fn));
        assert_eq!(k[1], TokKind::Ident("foo".into()));
        assert_eq!(k[2], TokKind::Keyword(Kw::Let));
        assert_eq!(k[3], TokKind::Keyword(Kw::Mut));
        // `value` is no longer a keyword — it's an ordinary identifier now that
        // the value-type representation is selected via the `#[value]` attribute.
        assert_eq!(k[4], TokKind::Ident("value".into()));
        assert_eq!(k[5], TokKind::Keyword(Kw::Struct));
    }

    #[test]
    fn numbers() {
        assert_eq!(kinds("42")[0], TokKind::Int(42, NumSuffix::None));
        assert_eq!(kinds("255u8")[0], TokKind::Int(255, NumSuffix::U8));
        assert_eq!(kinds("0xFF")[0], TokKind::Int(255, NumSuffix::None));
        assert_eq!(kinds("0b1010")[0], TokKind::Int(10, NumSuffix::None));
        assert_eq!(kinds("0o17")[0], TokKind::Int(15, NumSuffix::None));
        assert_eq!(kinds("1_000")[0], TokKind::Int(1000, NumSuffix::None));
        assert_eq!(kinds("3.14")[0], TokKind::Float(3.14, NumSuffix::None));
        assert_eq!(kinds("2.5f32")[0], TokKind::Float(2.5, NumSuffix::F32));
        assert_eq!(kinds("1.0e-9")[0], TokKind::Float(1.0e-9, NumSuffix::None));
    }

    #[test]
    fn chained_tuple_indices_keep_their_access_dots() {
        let tokens = kinds("value.0.1 + 2.5");
        assert_eq!(&tokens[..7], &[
            TokKind::Ident("value".into()), TokKind::Dot, TokKind::Int(0, NumSuffix::None),
            TokKind::Dot, TokKind::Int(1, NumSuffix::None), TokKind::Plus,
            TokKind::Float(2.5, NumSuffix::None),
        ]);
        let tokens = kinds("value . /* field */ 0 . 1");
        assert_eq!(tokens[2], TokKind::Int(0, NumSuffix::None));
        assert_eq!(tokens[3], TokKind::Dot);
        assert_eq!(tokens[4], TokKind::Int(1, NumSuffix::None));
    }

    #[test]
    fn dot_vs_float_vs_range() {
        // `1.foo` is int, dot, ident — not a float.
        let k = kinds("1.foo");
        assert_eq!(k[0], TokKind::Int(1, NumSuffix::None));
        assert_eq!(k[1], TokKind::Dot);
        assert_eq!(k[2], TokKind::Ident("foo".into()));
        // `0..5` is int, dotdot, int.
        let k = kinds("0..5");
        assert_eq!(k[0], TokKind::Int(0, NumSuffix::None));
        assert_eq!(k[1], TokKind::DotDot);
        assert_eq!(k[2], TokKind::Int(5, NumSuffix::None));
    }

    #[test]
    fn operators() {
        let k = kinds("-> => :: == != <= >= && || << >> += ..=");
        use TokKind::*;
        assert_eq!(
            k[..12],
            [Arrow, FatArrow, ColonColon, EqEq, Ne, Le, Ge, AndAnd, OrOr, Shl, Shr, PlusEq]
        );
        assert_eq!(k[12], DotDotEq);
    }

    #[test]
    fn strings_and_chars() {
        assert_eq!(kinds(r#""hi\n""#)[0], TokKind::Str("hi\n".into()));
        assert_eq!(kinds("'a'")[0], TokKind::Char('a'));
        assert_eq!(kinds(r"'\n'")[0], TokKind::Char('\n'));
        assert_eq!(kinds(r"'\u{41}'")[0], TokKind::Char('A'));
        assert_eq!(kinds("'🦀'")[0], TokKind::Char('🦀'));
        assert_eq!(kinds(r"'\u{1F980}'")[0], TokKind::Char('🦀'));
        assert_eq!(kinds(r#""é界🦀\u{1F980}""#)[0], TokKind::Str("é界🦀🦀".into()));
    }

    #[test]
    fn comments() {
        let k = kinds("1 // line\n /* a /* nested */ b */ 2");
        assert_eq!(k[0], TokKind::Int(1, NumSuffix::None));
        assert_eq!(k[1], TokKind::Int(2, NumSuffix::None));
    }

    #[test]
    fn unterminated_block_comment_errors() {
        assert!(lex("/* oops").is_err());
    }
}
