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
            Err(_) => Vec::new(),
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
