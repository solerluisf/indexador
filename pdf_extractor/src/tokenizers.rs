use crate::indexer::TOKEN_PATTERN;
use tantivy::tokenizer::{BoxTokenStream, Token, TokenStream, Tokenizer, RegexTokenizer};

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

/// A Tantivy tokenizer that splits on `[\p{L}\p{N}\p{S}]+` and lowercases.
/// Supports English, Spanish, and any Latin-script language.
#[derive(Clone)]
pub struct LanguageAwareTokenizer;

impl Tokenizer for LanguageAwareTokenizer {
    type TokenStream<'a> = BoxTokenStream<'a>;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> BoxTokenStream<'a> {
        let mut tokenizer = RegexTokenizer::new(TOKEN_PATTERN)
            .expect("Hardcoded regex pattern should never fail");
        let mut stream = tokenizer.token_stream(text);
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
        BoxTokenStream::new(TokenVecStream::new(tokens))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tantivy::tokenizer::TextAnalyzer;

    #[test]
    fn test_language_aware_tokenizer_splits_and_lowercases() {
        let mut tok = LanguageAwareTokenizer;
        let mut stream = tok.token_stream("Hello World");
        let mut tokens = Vec::new();
        while stream.advance() {
            tokens.push(stream.token().text.clone());
        }
        assert_eq!(tokens, vec!["hello", "world"]);
    }

    #[test]
    fn test_language_aware_tokenizer_spanish_accents() {
        let mut tok = LanguageAwareTokenizer;
        let mut stream = tok.token_stream("Canción musical");
        let mut tokens = Vec::new();
        while stream.advance() {
            tokens.push(stream.token().text.clone());
        }
        assert_eq!(tokens, vec!["canción", "musical"]);
    }

    #[test]
    fn test_language_aware_tokenizer_numbers() {
        let mut tok = LanguageAwareTokenizer;
        let mut stream = tok.token_stream("test 123");
        let mut tokens = Vec::new();
        while stream.advance() {
            tokens.push(stream.token().text.clone());
        }
        assert_eq!(tokens, vec!["test", "123"]);
    }

    #[test]
    fn test_language_aware_tokenizer_empty() {
        let mut tok = LanguageAwareTokenizer;
        let mut stream = tok.token_stream("");
        assert!(!stream.advance());
    }

    #[test]
    fn test_text_analyzer_composition() {
        let mut analyzer = TextAnalyzer::builder(LanguageAwareTokenizer)
            .build();
        let mut stream = analyzer.token_stream("Hello World");
        let tokens: Vec<String> = {
            let mut result = Vec::new();
            while stream.advance() {
                result.push(stream.token().text.clone());
            }
            result
        };
        assert_eq!(tokens, vec!["hello", "world"]);
    }
}
