use lindera::dictionary::load_dictionary;
use lindera::mode::Mode;
use lindera::segmenter::Segmenter;
use lindera::tokenizer::Tokenizer as LinderaTokenizer;
use tantivy::tokenizer::{BoxTokenStream, Token, TokenStream, Tokenizer};

/// A Tantivy tokenizer that uses Lindera (IPADIC) for Japanese text segmentation.
#[derive(Clone)]
pub struct JapaneseTokenizer {
    tokenizer: LinderaTokenizer,
}

impl JapaneseTokenizer {
    pub fn new() -> Result<Self, lindera::error::LinderaError> {
        let dictionary = load_dictionary("embedded://ipadic")?;
        let segmenter = Segmenter::new(Mode::Normal, dictionary, None);
        let tokenizer = LinderaTokenizer::new(segmenter);
        Ok(Self { tokenizer })
    }
}

impl Tokenizer for JapaneseTokenizer {
    type TokenStream<'a> = BoxTokenStream<'a>;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> BoxTokenStream<'a> {
        let tokens = match self.tokenizer.tokenize(text) {
            Ok(tokens) => tokens
                .into_iter()
                .filter(|t| {
                    let s = t.surface.as_ref();
                    !s.is_empty() && !s.trim().is_empty()
                })
                .map(|t| {
                    let position = t.position;
                    Token {
                        offset_from: t.byte_start,
                        offset_to: t.byte_end,
                        position,
                        position_length: 1,
                        text: t.surface.to_string(),
                    }
                })
                .collect::<Vec<_>>(),
            Err(e) => {
                eprintln!("[tokenizer] Lindera tokenization failed: {}", e);
                Vec::new()
            }
        };
        BoxTokenStream::new(TokenVecStream::new(tokens))
    }
}

/// A Chinese character bigram tokenizer.
/// Splits text into overlapping 2-character tokens for Chinese text search.
#[derive(Clone)]
pub struct ChineseBigramTokenizer;

impl Tokenizer for ChineseBigramTokenizer {
    type TokenStream<'a> = BoxTokenStream<'a>;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> BoxTokenStream<'a> {
        let tokens: Vec<Token> = if text.is_empty() {
            Vec::new()
        } else {
            let char_indices: Vec<(usize, char)> = text.char_indices().collect();
            if char_indices.len() == 1 {
                let (byte_start, c) = char_indices[0];
                if c.is_whitespace() {
                    Vec::new()
                } else {
                    vec![Token {
                        offset_from: byte_start,
                        offset_to: byte_start + c.len_utf8(),
                        position: 0,
                        position_length: 1,
                        text: c.to_string(),
                    }]
                }
            } else {
                char_indices
                    .windows(2)
                    .enumerate()
                    .filter(|(_, w)| !w[0].1.is_whitespace() && !w[1].1.is_whitespace())
                    .map(|(i, w)| {
                        let (byte_start, c0) = w[0];
                        let (byte_end, c1) = w[1];
                        let s: String = [c0, c1].iter().collect();
                        Token {
                            offset_from: byte_start,
                            offset_to: byte_end + c1.len_utf8(),
                            position: i,
                            position_length: 1,
                            text: s,
                        }
                    })
                    .collect()
            }
        };
        BoxTokenStream::new(TokenVecStream::new(tokens))
    }
}

/// A reusable token stream backed by a Vec of Tokens.
struct TokenVecStream {
    tokens: Vec<Token>,
    index: usize,
}

impl TokenVecStream {
    fn new(tokens: Vec<Token>) -> Self {
        Self { tokens, index: 0 }
    }
}

impl TokenStream for TokenVecStream {
    fn advance(&mut self) -> bool {
        if self.index < self.tokens.len() {
            self.index += 1;
            true
        } else {
            false
        }
    }

    fn token(&self) -> &Token {
        &self.tokens[self.index - 1]
    }

    fn token_mut(&mut self) -> &mut Token {
        &mut self.tokens[self.index - 1]
    }
}

use std::sync::OnceLock;
use tantivy::tokenizer::RegexTokenizer;

fn jp_tokenizer() -> Option<&'static JapaneseTokenizer> {
    static JP: OnceLock<Option<JapaneseTokenizer>> = OnceLock::new();
    JP.get_or_init(|| {
        match JapaneseTokenizer::new() {
            Ok(tok) => Some(tok),
            Err(e) => {
                eprintln!("[tokenizer] Lindera init failed: {}", e);
                None
            }
        }
    }).as_ref()
}

fn math_tokenizer() -> Option<&'static RegexTokenizer> {
    static MATH: OnceLock<Option<RegexTokenizer>> = OnceLock::new();
    MATH.get_or_init(|| {
        match RegexTokenizer::new(r"[\p{L}\p{N}\p{S}]+") {
            Ok(tok) => Some(tok),
            Err(e) => {
                eprintln!("[tokenizer] Regex init failed: {}", e);
                None
            }
        }
    }).as_ref()
}

fn collect_tokens_lowered(stream: &mut dyn TokenStream) -> Vec<Token> {
    let mut tokens = Vec::new();
    while stream.advance() {
        let t = stream.token();
        tokens.push(Token {
            offset_from: t.offset_from,
            offset_to: t.offset_to,
            position: t.position,
            position_length: t.position_length,
            text: t.text.to_lowercase(),
        });
    }
    tokens
}

/// Check if text contains Hiragana or Katakana characters (Japanese kana).
fn has_japanese_kana(text: &str) -> bool {
    text.chars().any(|c| {
        let cp = c as u32;
        matches!(cp, 0x3040..=0x309F | 0x30A0..=0x30FF | 0x31F0..=0x31FF)
    })
}

/// Check if text contains CJK Unified Ideographs (shared by Chinese, Japanese, Korean).
/// Covers all blocks from Extension A through H and Compatibility Ideographs.
fn has_cjk_ideographs(text: &str) -> bool {
    text.chars().any(|c| {
        let cp = c as u32;
        matches!(
            cp,
            0x3400..=0x4DBF   // CJK Extension A
            | 0x4E00..=0x9FFF   // CJK Unified Ideographs
            | 0xF900..=0xFAFF   // CJK Compatibility Ideographs
            | 0x20000..=0x2A6DF // CJK Extension B
            | 0x2A700..=0x2B73F // CJK Extension C
            | 0x2B740..=0x2B81F // CJK Extension D
            | 0x2B820..=0x2CEAF // CJK Extension E
            | 0x2CEB0..=0x2EBE0 // CJK Extension F
            | 0x2F800..=0x2FA1F // CJK Compatibility Ideographs Supplement
            | 0x30000..=0x3134F // CJK Extension G
            | 0x31350..=0x323AF // CJK Extension H
        )
    })
}

/// Determine the tokenizer strategy for a text.
/// Uses character-based heuristics so it works on short queries.
/// For mixed text (Latin + CJK), uses the dominant script to choose.
fn tokenizer_for_text(text: &str) -> TextCategory {
    if has_japanese_kana(text) {
        return TextCategory::Japanese;
    }
    let total_chars = text.chars().count();
    let cjk_count = text.chars().filter(|c| {
        let cp = *c as u32;
        matches!(
            cp,
            0x3400..=0x4DBF
            | 0x4E00..=0x9FFF
            | 0xF900..=0xFAFF
            | 0x20000..=0x2A6DF
            | 0x2A700..=0x2B73F
            | 0x2B740..=0x2B81F
            | 0x2B820..=0x2CEAF
            | 0x2CEB0..=0x2EBE0
            | 0x2F800..=0x2FA1F
            | 0x30000..=0x3134F
            | 0x31350..=0x323AF
        )
    }).count();
    if total_chars > 0 && cjk_count as f64 / total_chars as f64 > 0.5 {
        TextCategory::Chinese
    } else {
        TextCategory::Default
    }
}

enum TextCategory {
    Japanese,
    Chinese,
    Default,
}

#[derive(Clone)]
pub struct LanguageAwareTokenizer;

impl Tokenizer for LanguageAwareTokenizer {
    type TokenStream<'a> = BoxTokenStream<'a>;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> BoxTokenStream<'a> {
        match tokenizer_for_text(text) {
            TextCategory::Japanese => {
                if let Some(jp) = jp_tokenizer() {
                    let mut jp = jp.clone();
                    let tokens = collect_tokens_lowered(&mut jp.token_stream(text));
                    BoxTokenStream::new(TokenVecStream::new(tokens))
                } else {
                    // Japanese tokenizer unavailable — fall back to default regex
                    fallback_token_stream(text)
                }
            }
            TextCategory::Chinese => {
                let mut zh = ChineseBigramTokenizer;
                let tokens = collect_tokens_lowered(&mut zh.token_stream(text));
                BoxTokenStream::new(TokenVecStream::new(tokens))
            }
            TextCategory::Default => {
                if let Some(re) = math_tokenizer() {
                    let mut re = re.clone();
                    let tokens = collect_tokens_lowered(&mut re.token_stream(text));
                    BoxTokenStream::new(TokenVecStream::new(tokens))
                } else {
                    // Regex tokenizer unavailable — use simple whitespace split
                    fallback_token_stream(text)
                }
            }
        }
    }
}

/// Fallback tokenizer that splits on whitespace, used when the primary
/// tokenizer fails to initialize.
fn fallback_token_stream<'a>(text: &'a str) -> BoxTokenStream<'a> {
    let tokens: Vec<Token> = text
        .split_whitespace()
        .enumerate()
        .map(|(i, word)| Token {
            offset_from: 0,
            offset_to: 0,
            position: i,
            position_length: 1,
            text: word.to_lowercase(),
        })
        .collect();
    BoxTokenStream::new(TokenVecStream::new(tokens))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tantivy::tokenizer::LowerCaser;

    #[test]
    fn test_japanese_tokenizer_creates_tokens() {
        let mut tok = JapaneseTokenizer::new().unwrap();
        let mut stream = tok.token_stream("私は猫です");
        let tokens: Vec<String> = {
            let mut result = Vec::new();
            while stream.advance() {
                result.push(stream.token().text.clone());
            }
            result
        };
        assert!(!tokens.is_empty(), "Should produce at least one token");
    }

    #[test]
    fn test_japanese_tokenizer_empty_text() {
        let mut tok = JapaneseTokenizer::new().unwrap();
        let mut stream = tok.token_stream("");
        assert!(!stream.advance(), "Empty text should produce no tokens");
    }

    #[test]
    fn test_japanese_tokenizer_short_text() {
        let mut tok = JapaneseTokenizer::new().unwrap();
        let mut stream = tok.token_stream("猫");
        let tokens: Vec<String> = {
            let mut result = Vec::new();
            while stream.advance() {
                result.push(stream.token().text.clone());
            }
            result
        };
        assert_eq!(tokens, vec!["猫"]);
    }

    #[test]
    fn test_chinese_bigram_tokenizer() {
        let mut tok = ChineseBigramTokenizer;
        let mut stream = tok.token_stream("中文测试");
        let tokens: Vec<String> = {
            let mut result = Vec::new();
            while stream.advance() {
                result.push(stream.token().text.clone());
            }
            result
        };
        assert!(!tokens.is_empty());
        assert!(tokens.contains(&"中文".to_string()));
        assert!(tokens.contains(&"文测".to_string()));
        assert!(tokens.contains(&"测试".to_string()));
    }

    #[test]
    fn test_chinese_bigram_short_text() {
        let mut tok = ChineseBigramTokenizer;
        let mut stream = tok.token_stream("中");
        let tokens: Vec<String> = {
            let mut result = Vec::new();
            while stream.advance() {
                result.push(stream.token().text.clone());
            }
            result
        };
        assert_eq!(tokens, vec!["中"]);
    }

    #[test]
    fn test_chinese_bigram_empty() {
        let mut tok = ChineseBigramTokenizer;
        let mut stream = tok.token_stream("");
        assert!(!stream.advance());
    }

    #[test]
    fn test_chinese_bigram_with_whitespace() {
        let mut tok = ChineseBigramTokenizer;
        let mut stream = tok.token_stream("中文 测试");
        let tokens: Vec<String> = {
            let mut result = Vec::new();
            while stream.advance() {
                result.push(stream.token().text.clone());
            }
            result
        };
        assert_eq!(tokens, vec!["中文", "测试"]);
    }

    #[test]
    fn test_text_analyzer_composition() {
        let mut analyzer = tantivy::tokenizer::TextAnalyzer::builder(ChineseBigramTokenizer)
            .filter(LowerCaser)
            .build();
        let mut stream = analyzer.token_stream("中文测试");
        let tokens: Vec<String> = {
            let mut result = Vec::new();
            while stream.advance() {
                result.push(stream.token().text.clone());
            }
            result
        };
        assert!(tokens.contains(&"中文".to_string()));
    }
}
