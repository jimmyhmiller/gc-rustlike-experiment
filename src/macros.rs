//! `macro_rules!`: matching invocations against rules and transcribing the
//! chosen rule's body.
//!
//! Expansion works on tokens, as in Rust. A rule's matcher is a token pattern
//! with fragments (`$x:expr`, `$name:ident`, `$t:ty`, `$tt:tt`, ...) and
//! repetitions (`$( ... ),*`, `+`, `?`). Matching is greedy and does not
//! backtrack into a repetition. The parser measures `expr`, `ty`, `pat`,
//! `path`, `block`, `stmt`, and `item` fragments by parsing them. An `expr`
//! fragment is transcribed in parentheses, so it keeps its grouping.
//! Expansion is not hygienic: names a macro body introduces are visible at the
//! use site.

use crate::lexer::{Span, TokKind, Token};
use std::collections::HashMap;

/// A macro's rules: (matcher, transcriber), each without its delimiters.
pub struct MacroRules {
    pub rules: Vec<(Vec<Matcher>, Vec<Trans>)>,
}

#[derive(Clone, Debug)]
pub enum Matcher {
    Tok(TokKind),
    Frag(String, String),
    Group(TokKind, Vec<Matcher>),
    Rep(Vec<Matcher>, Option<TokKind>, char),
}

#[derive(Clone, Debug)]
pub enum Trans {
    Tok(Token),
    Var(String, Span),
    Group(Token, Vec<Trans>, Token),
    Rep(Vec<Trans>, Option<Token>, char),
}

#[derive(Clone, Debug)]
enum Binding {
    Leaf(Vec<Token>, String),
    Seq(Vec<Binding>),
}

pub fn close_of(open: &TokKind) -> Option<TokKind> {
    match open {
        TokKind::LParen => Some(TokKind::RParen),
        TokKind::LBracket => Some(TokKind::RBracket),
        TokKind::LBrace => Some(TokKind::RBrace),
        TokKind::GroupOpen => Some(TokKind::GroupClose),
        _ => None,
    }
}

/// The index just past the token tree starting at `i`.
pub fn tree_end(toks: &[Token], i: usize) -> Option<usize> {
    let Some(close) = close_of(&toks.get(i)?.kind) else { return Some(i + 1) };
    let open = toks[i].kind.clone();
    let mut depth = 0usize;
    for (j, t) in toks.iter().enumerate().skip(i) {
        if t.kind == open { depth += 1; }
        else if t.kind == close {
            depth -= 1;
            if depth == 0 { return Some(j + 1); }
        } else if matches!(t.kind, TokKind::Eof) { return None; }
    }
    None
}

fn is_rep_op(k: &TokKind) -> Option<char> {
    match k {
        TokKind::Star => Some('*'),
        TokKind::Plus => Some('+'),
        TokKind::Question => Some('?'),
        _ => None,
    }
}

fn ident_name(k: &TokKind) -> Option<String> {
    match k {
        TokKind::Ident(s) => Some(s.clone()),
        TokKind::Keyword(kw) => Some(format!("{:?}", kw).to_lowercase()),
        _ => None,
    }
}

/// Compile a matcher (the tokens inside a rule's pattern delimiters).
pub fn parse_matcher(toks: &[Token]) -> Result<Vec<Matcher>, (String, Span)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        let t = &toks[i];
        if t.kind == TokKind::Dollar {
            match toks.get(i + 1).map(|t| &t.kind) {
                Some(TokKind::LParen) => {
                    let end = tree_end(toks, i + 1).ok_or(("unclosed `$(` in macro pattern".to_string(), t.span))?;
                    let inner = parse_matcher(&toks[i + 2..end - 1])?;
                    let mut j = end;
                    let mut sep = None;
                    let op = match toks.get(j).and_then(|t| is_rep_op(&t.kind)) {
                        Some(op) => op,
                        None => {
                            sep = toks.get(j).map(|t| t.kind.clone());
                            j += 1;
                            toks.get(j).and_then(|t| is_rep_op(&t.kind))
                                .ok_or(("expected `*`, `+`, or `?` after a macro repetition".to_string(), t.span))?
                        }
                    };
                    out.push(Matcher::Rep(inner, sep, op));
                    i = j + 1;
                }
                Some(k) => {
                    let name = ident_name(k).ok_or(("expected a fragment name after `$`".to_string(), t.span))?;
                    if toks.get(i + 2).map(|t| &t.kind) != Some(&TokKind::Colon) {
                        return Err((format!("`${name}` needs a fragment kind (`${name}:expr`)"), t.span));
                    }
                    let kind = toks.get(i + 3).and_then(|t| ident_name(&t.kind))
                        .ok_or(("expected a fragment kind".to_string(), t.span))?;
                    if !matches!(kind.as_str(), "expr" | "ident" | "ty" | "tt" | "literal" | "pat" | "pat_param" | "path" | "block" | "stmt" | "item" | "vis" | "lifetime") {
                        return Err((format!("unknown macro fragment kind `{kind}`"), toks[i + 3].span));
                    }
                    out.push(Matcher::Frag(name, kind));
                    i += 4;
                }
                None => return Err(("`$` at the end of a macro pattern".to_string(), t.span)),
            }
        } else if close_of(&t.kind).is_some() {
            let end = tree_end(toks, i).ok_or(("unclosed delimiter in macro pattern".to_string(), t.span))?;
            out.push(Matcher::Group(t.kind.clone(), parse_matcher(&toks[i + 1..end - 1])?));
            i = end;
        } else {
            out.push(Matcher::Tok(t.kind.clone()));
            i += 1;
        }
    }
    Ok(out)
}

/// Compile a transcriber (the tokens inside a rule's body delimiters).
pub fn parse_transcriber(toks: &[Token]) -> Result<Vec<Trans>, (String, Span)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        let t = &toks[i];
        if t.kind == TokKind::Dollar {
            match toks.get(i + 1).map(|t| &t.kind) {
                Some(TokKind::LParen) => {
                    let end = tree_end(toks, i + 1).ok_or(("unclosed `$(` in macro body".to_string(), t.span))?;
                    let inner = parse_transcriber(&toks[i + 2..end - 1])?;
                    let mut j = end;
                    let mut sep = None;
                    if toks.get(j).and_then(|t| is_rep_op(&t.kind)).is_none() {
                        sep = toks.get(j).cloned();
                        j += 1;
                    }
                    let op = toks.get(j).and_then(|t| is_rep_op(&t.kind))
                        .ok_or(("expected `*`, `+`, or `?` after a macro repetition".to_string(), t.span))?;
                    out.push(Trans::Rep(inner, sep, op));
                    i = j + 1;
                }
                // `$crate::path` names this crate: the path is used as written.
                Some(TokKind::Ident(n)) if n == "crate" => {
                    i += 2;
                    if toks.get(i).map(|t| &t.kind) == Some(&TokKind::ColonColon) { i += 1; }
                }
                Some(k) => {
                    let name = ident_name(k).ok_or(("expected a name after `$`".to_string(), t.span))?;
                    out.push(Trans::Var(name, toks[i + 1].span));
                    i += 2;
                }
                None => return Err(("`$` at the end of a macro body".to_string(), t.span)),
            }
        } else if close_of(&t.kind).is_some() {
            let end = tree_end(toks, i).ok_or(("unclosed delimiter in macro body".to_string(), t.span))?;
            out.push(Trans::Group(t.clone(), parse_transcriber(&toks[i + 1..end - 1])?, toks[end - 1].clone()));
            i = end;
        } else {
            out.push(Trans::Tok(t.clone()));
            i += 1;
        }
    }
    Ok(out)
}

/// Measures parsed fragments: the number of tokens of `toks` that form one
/// fragment of `kind`, or `None` if they do not start with one.
pub type Measure<'a> = dyn Fn(&str, &[Token]) -> Option<usize> + 'a;

type Env = HashMap<String, Binding>;

fn match_seq(ms: &[Matcher], toks: &[Token], measure: &Measure, env: &mut Env) -> Option<usize> {
    let mut pos = 0;
    for (k, m) in ms.iter().enumerate() {
        match m {
            Matcher::Tok(kind) => {
                if toks.get(pos).map(|t| &t.kind) != Some(kind) { return None; }
                pos += 1;
            }
            Matcher::Group(open, inner) => {
                if toks.get(pos).map(|t| &t.kind) != Some(open) { return None; }
                let end = tree_end(toks, pos)?;
                let mut sub = Env::new();
                let used = match_seq(inner, &toks[pos + 1..end - 1], measure, &mut sub)?;
                if used != end - pos - 2 { return None; }
                env.extend(sub);
                pos = end;
            }
            Matcher::Frag(name, kind) => {
                let rest = &toks[pos..];
                let n = match kind.as_str() {
                    "ident" => matches!(rest.first().map(|t| &t.kind), Some(TokKind::Ident(_)) | Some(TokKind::Keyword(crate::lexer::Kw::SelfValue))).then_some(1)?,
                    "tt" => if rest.is_empty() { return None } else { tree_end(rest, 0)? },
                    "literal" => match rest.first().map(|t| &t.kind) {
                        Some(TokKind::Int(..) | TokKind::Float(..) | TokKind::Str(_) | TokKind::Char(_)
                            | TokKind::Keyword(crate::lexer::Kw::True | crate::lexer::Kw::False)) => 1,
                        Some(TokKind::Minus) if matches!(rest.get(1).map(|t| &t.kind), Some(TokKind::Int(..) | TokKind::Float(..))) => 2,
                        _ => return None,
                    },
                    "lifetime" => return None,
                    "vis" => if matches!(rest.first().map(|t| &t.kind), Some(TokKind::Keyword(crate::lexer::Kw::Pub))) { 1 } else { 0 },
                    other => {
                        if rest.is_empty() { return None; }
                        measure(other, rest)?
                    }
                };
                env.insert(name.clone(), Binding::Leaf(rest[..n].to_vec(), kind.clone()));
                pos += n;
            }
            Matcher::Rep(inner, sep, op) => {
                let mut iters: Vec<Env> = Vec::new();
                loop {
                    if *op == '?' && iters.len() == 1 { break; }
                    let mut start = pos;
                    if !iters.is_empty() {
                        if let Some(s) = sep {
                            if toks.get(pos).map(|t| &t.kind) != Some(s) { break; }
                            start += 1;
                        }
                    }
                    if start >= toks.len() { break; }
                    // Stop when the token after the repetition comes next and
                    // the repetition cannot start with it.
                    let mut sub = Env::new();
                    match match_seq(inner, &toks[start..], measure, &mut sub) {
                        Some(used) if used > 0 => { iters.push(sub); pos = start + used; }
                        _ => break,
                    }
                    let _ = k;
                }
                if *op == '+' && iters.is_empty() { return None; }
                for name in matcher_vars(inner) {
                    let seq = iters.iter().map(|e| e.get(&name).cloned().unwrap_or(Binding::Seq(vec![]))).collect();
                    env.insert(name, Binding::Seq(seq));
                }
            }
        }
    }
    Some(pos)
}

fn matcher_vars(ms: &[Matcher]) -> Vec<String> {
    let mut out = Vec::new();
    for m in ms {
        match m {
            Matcher::Frag(n, _) => out.push(n.clone()),
            Matcher::Group(_, inner) | Matcher::Rep(inner, _, _) => out.extend(matcher_vars(inner)),
            Matcher::Tok(_) => {}
        }
    }
    out
}

fn trans_vars(ts: &[Trans], out: &mut Vec<String>) {
    for t in ts {
        match t {
            Trans::Var(n, _) => out.push(n.clone()),
            Trans::Group(_, inner, _) | Trans::Rep(inner, _, _) => trans_vars(inner, out),
            Trans::Tok(_) => {}
        }
    }
}

/// The names a macro body binds with `let` (`let tmp = ..`, `let mut n`).
/// They are renamed per expansion, so they cannot capture or shadow names in
/// the code the macro is used in (partial hygiene).
fn let_names(ts: &[Trans], out: &mut Vec<String>) {
    let mut i = 0;
    while i < ts.len() {
        match &ts[i] {
            Trans::Tok(Token { kind: TokKind::Keyword(crate::lexer::Kw::Let), .. }) => {
                let mut j = i + 1;
                if matches!(ts.get(j), Some(Trans::Tok(Token { kind: TokKind::Keyword(crate::lexer::Kw::Mut), .. }))) { j += 1; }
                if let Some(Trans::Tok(Token { kind: TokKind::Ident(n), .. })) = ts.get(j) { out.push(n.clone()); }
            }
            Trans::Group(_, inner, _) | Trans::Rep(inner, _, _) => let_names(inner, out),
            _ => {}
        }
        i += 1;
    }
}

thread_local! { static EXPANSION: std::cell::Cell<u32> = const { std::cell::Cell::new(0) }; }

fn transcribe(ts: &[Trans], env: &Env, out: &mut Vec<Token>, hygiene: &(Vec<String>, u32)) -> Result<(), (String, Span)> {
    for t in ts {
        match t {
            Trans::Tok(Token { kind: TokKind::Ident(n), span }) if hygiene.0.contains(n) => {
                out.push(Token { kind: TokKind::Ident(format!("{n}#m{}", hygiene.1)), span: *span });
            }
            Trans::Tok(tok) => out.push(tok.clone()),
            Trans::Group(open, inner, close) => {
                out.push(open.clone());
                transcribe(inner, env, out, hygiene)?;
                out.push(close.clone());
            }
            Trans::Var(name, span) => match env.get(name) {
                Some(Binding::Leaf(toks, kind)) => {
                    // An expression keeps its grouping where it is used,
                    // through invisible delimiters.
                    let wrap = kind == "expr" && toks.len() > 1;
                    let at = toks.first().map(|t| t.span).unwrap_or(*span);
                    if wrap { out.push(Token { kind: TokKind::GroupOpen, span: at }); }
                    out.extend(toks.iter().cloned());
                    if wrap { out.push(Token { kind: TokKind::GroupClose, span: at }); }
                }
                Some(Binding::Seq(_)) => return Err((format!("`${name}` is repeated; use it inside `$( ... )*`"), *span)),
                // Not a macro variable: `$` was literal text (rare).
                None => return Err((format!("unknown macro variable `${name}`"), *span)),
            },
            Trans::Rep(inner, sep, op) => {
                let mut names = Vec::new();
                trans_vars(inner, &mut names);
                let mut count: Option<usize> = None;
                for n in &names {
                    if let Some(Binding::Seq(items)) = env.get(n) {
                        match count {
                            None => count = Some(items.len()),
                            Some(c) if c != items.len() => {
                                return Err((format!("macro variables in one repetition repeat a different number of times (`${n}`)"), sep.as_ref().map(|s| s.span).unwrap_or(Span::new(0, 0))));
                            }
                            _ => {}
                        }
                    }
                }
                let Some(count) = count else {
                    return Err((format!("a `${}` repetition uses no repeated macro variable", op), Span::new(0, 0)));
                };
                for i in 0..count {
                    if i > 0 { if let Some(s) = sep { out.push(s.clone()); } }
                    let mut inner_env = env.clone();
                    for n in &names {
                        if let Some(Binding::Seq(items)) = env.get(n) {
                            inner_env.insert(n.clone(), items[i].clone());
                        }
                    }
                    transcribe(inner, &inner_env, out, hygiene)?;
                }
            }
        }
    }
    Ok(())
}

/// Expand an invocation: the tokens of the first rule whose matcher matches
/// all of `input`.
pub fn expand(name: &str, rules: &MacroRules, input: &[Token], measure: &Measure, at: Span) -> Result<Vec<Token>, (String, Span)> {
    for (matcher, body) in &rules.rules {
        let mut env = Env::new();
        if match_seq(matcher, input, measure, &mut env) == Some(input.len()) {
            let mut names = Vec::new();
            let_names(body, &mut names);
            let id = EXPANSION.with(|e| { e.set(e.get() + 1); e.get() });
            let mut out = Vec::new();
            transcribe(body, &env, &mut out, &(names, id))?;
            return Ok(out);
        }
    }
    Err((format!("no rule of macro `{name}!` matches this invocation"), at))
}
