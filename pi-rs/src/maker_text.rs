// maker_text.rs — 内部网页引用与泄漏 stop token 的剥离（maker-shared/src/internalCitation.ts 的移植）。
// OpenAI Web Search 用私有区分隔符 `\u{E200}cite\u{E202}…\u{E201}` 编码来源引用，Grok / xAI 会把 `<|eos|>` 之类
// 当作可见文本吐出。两者都是传输控制信息，不是给用户看的正文；流式增量要先扣住，等能判定后再放出。
// 规则必须与 TS 一致（maker-freeze 的手写场景会比对两侧产出）。
const STOP_TOKENS: [&str; 4] = ["<|endoftext|>", "<|eot_id|>", "<|im_end|>", "<|eos|>"];
const CITE_OPEN: &str = "\u{E200}cite\u{E202}";
const CITE_CLOSE: &str = "\u{E201}";

/// 流式 stop-token 缓冲：pending 是还没放出的尾部，emitted 表示已经放出过可见文本。
#[derive(Default, Clone)]
pub struct StopHold {
    pub pending: String,
    pub emitted: bool,
}

fn longest_stop_token() -> usize {
    STOP_TOKENS.iter().map(|t| t.chars().count()).max().unwrap_or(0)
}

/// 最后一个没闭合的 `\u{E200}cite\u{E202}` 起点。
fn unfinished_open(text: &str) -> Option<usize> {
    let mut from = 0;
    loop {
        let open = from + text[from..].find(CITE_OPEN)?;
        match text[open + CITE_OPEN.len()..].find(CITE_CLOSE) {
            None => return Some(open),
            Some(rel) => from = open + CITE_OPEN.len() + rel + CITE_CLOSE.len(),
        }
    }
}

/// 文本尾部是不是 `\u{E200}cite\u{E202}` 的真前缀；返回前缀起点（没有就是 text.len()）。
fn partial_open_prefix_start(text: &str) -> usize {
    let open: Vec<char> = CITE_OPEN.chars().collect();
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let max_probe = chars.len().min(open.len() - 1);
    for len in (1..=max_probe).rev() {
        let tail = &chars[chars.len() - len..];
        if tail.iter().map(|(_, c)| *c).eq(open[..len].iter().copied()) {
            return tail[0].0;
        }
    }
    text.len()
}

/// 流式快照里稳定（只增不减）的前缀边界：未闭合的引用尾巴先扣住。
pub fn stable_citation_boundary(text: &str) -> usize {
    unfinished_open(text).unwrap_or_else(|| partial_open_prefix_start(text))
}

fn strip_citation_markers(text: &str) -> String {
    if !text.contains('\u{E200}') {
        return text.to_string();
    }
    let stable = &text[..stable_citation_boundary(text)];
    if !stable.contains(CITE_OPEN) {
        return stable.to_string();
    }
    let mut out = String::new();
    let mut from = 0;
    loop {
        let Some(rel) = stable[from..].find(CITE_OPEN) else {
            out.push_str(&stable[from..]);
            return out;
        };
        let open = from + rel;
        let Some(crel) = stable[open + CITE_OPEN.len()..].find(CITE_CLOSE) else {
            out.push_str(&stable[from..open]);
            return out;
        };
        out.push_str(&stable[from..open]);
        from = open + CITE_OPEN.len() + crel + CITE_CLOSE.len();
    }
}

fn match_stop_token_at(text: &str, index: usize) -> Option<&'static str> {
    STOP_TOKENS.iter().copied().find(|t| text[index..].starts_with(t))
}

fn is_incomplete_stop_prefix(text: &str) -> bool {
    !text.is_empty() && text.chars().count() <= longest_stop_token() && STOP_TOKENS.iter().any(|t| t.starts_with(text))
}

/// 吃掉开头的 stop token 串与其周围的空白，返回 (剩余, 是否吃过 token)。
fn consume_leading_stop_control(text: &str) -> (&str, bool) {
    let mut index = 0;
    let mut consumed = false;
    while index < text.len() {
        let c = text[index..].chars().next().unwrap();
        if c.is_whitespace() {
            index += c.len_utf8();
            continue;
        }
        let Some(token) = match_stop_token_at(text, index) else { break };
        consumed = true;
        index += token.len();
    }
    (&text[index..], consumed)
}

/// 整条消息只剩 stop token（可带空白）才丢；`The token is <|eos|>`、`<|eos|>answer` 保持原样。
pub fn strip_standalone_stop_token(text: &str) -> String {
    let (remainder, consumed) = consume_leading_stop_control(text);
    if consumed && remainder.is_empty() { String::new() } else { text.to_string() }
}

/// 对已完成的 assistant 文本：先剥引用标记，再剥独占的 stop token。幂等。
pub fn strip_internal_web_citations(text: &str) -> String {
    strip_standalone_stop_token(&strip_citation_markers(text))
}

/// 流式增量：把 stop-token 控制串与未闭合引用扣住，放出可以确认的可见文本。
pub fn hold_standalone_stop_token_delta(buf: &mut StopHold, delta: &str) -> Option<String> {
    let combined = format!("{}{}", buf.pending, delta);
    if !buf.emitted {
        let (remainder, _) = consume_leading_stop_control(&combined);
        if remainder.is_empty() || is_incomplete_stop_prefix(remainder) {
            buf.pending = combined;
            return None;
        }
    }
    let end = stable_citation_boundary(&combined);
    buf.pending = combined[end..].to_string();
    let stable = &combined[..end];
    let visible = if buf.emitted { strip_citation_markers(stable) } else { strip_internal_web_citations(stable) };
    if visible.is_empty() {
        return None;
    }
    buf.emitted = true;
    Some(visible)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_complete_and_unfinished_citations() {
        assert_eq!(strip_internal_web_citations("a\u{E200}cite\u{E202}turn0search1\u{E201}b"), "ab");
        assert_eq!(strip_internal_web_citations("a\u{E200}cite\u{E202}turn0"), "a", "未闭合的尾巴直接去掉");
        assert_eq!(strip_internal_web_citations("a\u{E200}ci"), "a", "只到前缀也去掉");
        assert_eq!(strip_internal_web_citations("we cite sources"), "we cite sources", "普通单词不动");
        let once = strip_internal_web_citations("x\u{E200}cite\u{E202}y\u{E201}z");
        assert_eq!(strip_internal_web_citations(&once), once, "幂等");
    }

    #[test]
    fn drops_only_standalone_stop_tokens() {
        assert_eq!(strip_standalone_stop_token("<|eos|>"), "");
        assert_eq!(strip_standalone_stop_token(" <|eos|>\n<|im_end|> "), "", "多个 + 空白也算");
        assert_eq!(strip_standalone_stop_token("The token is <|eos|>"), "The token is <|eos|>");
        assert_eq!(strip_standalone_stop_token("<|eos|>answer"), "<|eos|>answer");
        assert_eq!(strip_standalone_stop_token(""), "");
    }

    #[test]
    fn holds_split_stop_tokens_until_they_resolve() {
        let mut b = StopHold::default();
        assert_eq!(hold_standalone_stop_token_delta(&mut b, "<|e"), None);
        assert_eq!(hold_standalone_stop_token_delta(&mut b, "os|>"), None, "完整 token 仍扣住，后面可能还有正文");
        assert_eq!(hold_standalone_stop_token_delta(&mut b, "answer").as_deref(), Some("<|eos|>answer"), "被证明是正文后原样放出");
        assert_eq!(hold_standalone_stop_token_delta(&mut b, " more").as_deref(), Some(" more"));
    }

    #[test]
    fn plain_prose_passes_through_immediately() {
        let mut b = StopHold::default();
        assert_eq!(hold_standalone_stop_token_delta(&mut b, "Hello").as_deref(), Some("Hello"));
        assert_eq!(hold_standalone_stop_token_delta(&mut b, " <|eos|>").as_deref(), Some(" <|eos|>"), "正文之后的提及保持原样");
    }

    #[test]
    fn holds_a_split_citation_across_deltas() {
        let mut b = StopHold::default();
        assert_eq!(hold_standalone_stop_token_delta(&mut b, "see\u{E200}ci").as_deref(), Some("see"));
        assert_eq!(hold_standalone_stop_token_delta(&mut b, "te\u{E202}t0").as_deref(), None);
        assert_eq!(hold_standalone_stop_token_delta(&mut b, "\u{E201} done").as_deref(), Some(" done"));
    }
}
