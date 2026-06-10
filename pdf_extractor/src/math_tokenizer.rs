use regex::Regex;
use std::sync::OnceLock;
use tantivy::tokenizer::{BoxTokenStream, Token, TokenStream, Tokenizer};

fn math_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"\\(sum|int|iint|iiint|oint|prod|coprod|bigcup|bigcap|bigvee|bigwedge)(?:_\{([^}]*)\})?(?:\^\{([^}]*)\})?"
        ).unwrap()
    })
}

fn frac_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\\(frac)\{([^}]*)\}\{([^}]*)\}").unwrap()
    })
}

fn sqrt_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\\(sqrt)(?:\[([^]]*)\])?\{([^}]*)\}").unwrap()
    })
}

fn lim_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\\(lim|max|min|sup|inf)(?:_\{([^}]*)\})?").unwrap()
    })
}

fn text_token_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"[\p{L}\p{N}\p{S}]+").unwrap()
    })
}

/// A single math construct found in text.
#[derive(Debug)]
struct MathConstruct {
    start: usize,
    end: usize,
    label: String,
}

/// Scan text for LaTeX math constructs and return their positions and labels.
fn find_math_constructs(text: &str) -> Vec<MathConstruct> {
    let mut constructs = Vec::new();

    // Operator with limits: \sum_{a}^{b}
    for cap in math_regex().captures_iter(text) {
        let m = cap.get(0).unwrap();
        let op = &cap[1];
        let sub = cap.get(2).map(|s| s.as_str()).unwrap_or("");
        let sup = cap.get(3).map(|s| s.as_str()).unwrap_or("");
        let label = if !sub.is_empty() && !sup.is_empty() {
            format!("MATH_{}_LIMITS", op.to_uppercase())
        } else if !sub.is_empty() {
            format!("MATH_{}_SUB", op.to_uppercase())
        } else if !sup.is_empty() {
            format!("MATH_{}_SUP", op.to_uppercase())
        } else {
            format!("MATH_{}", op.to_uppercase())
        };
        constructs.push(MathConstruct {
            start: m.start(),
            end: m.end(),
            label,
        });
    }

    // Fraction: \frac{a}{b}
    for cap in frac_regex().captures_iter(text) {
        let m = cap.get(0).unwrap();
        let num = &cap[2];
        let den = &cap[3];
        // Use first few chars of numerator/denominator as a fingerprint
        let fp = format!("MATH_FRAC_{}_{}", &num[..num.len().min(8)], &den[..den.len().min(8)]);
        constructs.push(MathConstruct {
            start: m.start(),
            end: m.end(),
            label: fp,
        });
    }

    // Square root: \sqrt{x} or \sqrt[n]{x}
    for cap in sqrt_regex().captures_iter(text) {
        let m = cap.get(0).unwrap();
        let radicand = &cap[3];
        let fp = format!("MATH_SQRT_{}", &radicand[..radicand.len().min(8)]);
        constructs.push(MathConstruct {
            start: m.start(),
            end: m.end(),
            label: fp,
        });
    }

    // Limit-style: \lim_{x\to\infty}
    for cap in lim_regex().captures_iter(text) {
        let m = cap.get(0).unwrap();
        let op = &cap[1];
        let sub = cap.get(2).map(|s| s.as_str()).unwrap_or("");
        let label = if !sub.is_empty() {
            format!("MATH_{}_LIMITS", op.to_uppercase())
        } else {
            format!("MATH_{}", op.to_uppercase())
        };
        constructs.push(MathConstruct {
            start: m.start(),
            end: m.end(),
            label,
        });
    }

    // Sort by start position
    constructs.sort_by_key(|c| c.start);
    // Remove nested/overlapping constructs (keep outer)
    let mut i = 0;
    while i < constructs.len() {
        let mut j = i + 1;
        while j < constructs.len() && constructs[j].start < constructs[i].end {
            j += 1;
        }
        constructs.drain(i + 1..j);
        i += 1;
    }

    constructs
}

/// A custom tokenizer that:
///  1. Splits text on `[\p{L}\p{N}\p{S}]+` (same as the "math" tokenizer)
///  2. Detects LaTeX constructs (`\sum_{...}^{...}`, `\frac{...}{...}`, etc.)
///  3. Emits both individual tokens AND composed tokens with a `MATH_*` label.
#[derive(Clone)]
pub struct MathAwareTokenizer;

impl Tokenizer for MathAwareTokenizer {
    type TokenStream<'a> = BoxTokenStream<'a>;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> BoxTokenStream<'a> {
        let base_re = text_token_regex();
        let mut tokens: Vec<Token> = Vec::new();

        // 1. Collect individual text tokens.
        for m in base_re.find_iter(text) {
            let tok = Token {
                offset_from: m.start(),
                offset_to: m.end(),
                position: tokens.len(),
                position_length: 1,
                text: m.as_str().to_string(),
            };
            tokens.push(tok);
        }

        // 2. Find LaTeX constructs.
        let constructs = find_math_constructs(text);

        // 3. Append composed tokens for each construct.
        for construct in &constructs {
            // Find the position of the first individual token that falls within this construct.
            let first_pos = tokens.iter().position(|t| t.offset_from >= construct.start);
            if let Some(pos) = first_pos {
                // Count how many individual tokens are covered by this construct.
                let count = tokens[pos..]
                    .iter()
                    .take_while(|t| t.offset_to <= construct.end)
                    .count();
                let composed = Token {
                    offset_from: construct.start,
                    offset_to: construct.end,
                    position: tokens.len(),
                    position_length: count.max(1),
                    text: construct.label.clone(),
                };
                tokens.push(composed);
            } else {
                // No underlying token — can happen for standalone construct text like "√".
                let composed = Token {
                    offset_from: construct.start,
                    offset_to: construct.end,
                    position: tokens.len(),
                    position_length: 1,
                    text: construct.label.clone(),
                };
                tokens.push(composed);
            }
        }

        BoxTokenStream::new(TokenVecStream::new(tokens))
    }
}

/// Reusable token stream backed by a Vec of Tokens.
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

/// Extract LaTeX math regions from text by detecting common delimiters.
/// Returns the raw LaTeX content if math regions are found, or None otherwise.
/// Detects: $...$$, ...$$, \(...\), \[...\]
pub fn extract_math_source(text: &str) -> Option<String> {
    // Quick-filter: skip regex entirely if no math delimiters are present
    if !text.contains('$') && !text.contains('\\') {
        return None;
    }

    // Try display math first: $$...$$, \[...\]
    static RE_DISPLAY: OnceLock<Regex> = OnceLock::new();
    let re_display = RE_DISPLAY.get_or_init(|| Regex::new(r"\$\$(.+?)\$\$|\\\[(.+?)\\\]").unwrap());
    // Then inline: $...$, \(...\)
    static RE_INLINE: OnceLock<Regex> = OnceLock::new();
    let re_inline = RE_INLINE.get_or_init(|| Regex::new(r"\$(.+?)\$|\\\((.+?)\\\)").unwrap());
    let mut parts: Vec<String> = Vec::new();

    for cap in re_display.captures_iter(text) {
        if let Some(m) = cap.get(1).or_else(|| cap.get(2)) {
            parts.push(format!("display:{}", m.as_str()));
        }
    }

    for cap in re_inline.captures_iter(text) {
        if let Some(m) = cap.get(1).or_else(|| cap.get(2)) {
            parts.push(format!("inline:{}", m.as_str()));
        }
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" | "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- MathAwareTokenizer tests ---

    #[test]
    fn test_basic_text_still_tokenized() {
        let mut tok = MathAwareTokenizer;
        let mut stream = tok.token_stream("hello world");
        let tokens: Vec<String> = {
            let mut r = Vec::new();
            while stream.advance() {
                r.push(stream.token().text.clone());
            }
            r
        };
        assert_eq!(tokens, vec!["hello", "world"]);
    }

    #[test]
    fn test_basic_text_with_lowercase_filter() {
        let mut analyzer = tantivy::tokenizer::TextAnalyzer::builder(MathAwareTokenizer)
            .filter(tantivy::tokenizer::LowerCaser)
            .build();
        let mut stream = analyzer.token_stream("Hello World");
        let tokens: Vec<String> = {
            let mut r = Vec::new();
            while stream.advance() {
                r.push(stream.token().text.clone());
            }
            r
        };
        assert_eq!(tokens, vec!["hello", "world"]);
    }

    #[test]
    fn test_math_sum_construct_emitted() {
        let mut tok = MathAwareTokenizer;
        let mut stream = tok.token_stream(r"\sum_{i=1}^{n} x_i");
        let tokens: Vec<String> = {
            let mut r = Vec::new();
            while stream.advance() {
                r.push(stream.token().text.clone());
            }
            r
        };
        // Individual tokens from the regex tokenizer: "sum", "i", "1", "n", "x_i"
        // (backslash is not captured by [\p{L}\p{N}\p{S}])
        assert!(
            tokens.iter().any(|t| t.starts_with("MATH_SUM_")),
            "Should emit MATH_SUM_* composed token, got: {:?}", tokens
        );
        assert!(
            tokens.contains(&"sum".to_string()),
            "Should keep individual sum token, got: {:?}", tokens
        );
    }

    #[test]
    fn test_math_frac_construct_emitted() {
        let mut tok = MathAwareTokenizer;
        let mut stream = tok.token_stream(r"\frac{a}{b}");
        let tokens: Vec<String> = {
            let mut r = Vec::new();
            while stream.advance() {
                r.push(stream.token().text.clone());
            }
            r
        };
        assert!(
            tokens.iter().any(|t| t.starts_with("MATH_FRAC_")),
            "Should emit MATH_FRAC_* composed token, got: {:?}", tokens
        );
        assert!(
            tokens.contains(&"frac".to_string()),
            "Should keep individual \\frac token, got: {:?}", tokens
        );
    }

    #[test]
    fn test_math_sqrt_construct_emitted() {
        let mut tok = MathAwareTokenizer;
        let mut stream = tok.token_stream(r"\sqrt{x^2+y^2}");
        let tokens: Vec<String> = {
            let mut r = Vec::new();
            while stream.advance() {
                r.push(stream.token().text.clone());
            }
            r
        };
        assert!(
            tokens.iter().any(|t| t.starts_with("MATH_SQRT_")),
            "Should emit MATH_SQRT_* composed token, got: {:?}", tokens
        );
    }

    #[test]
    fn test_math_int_construct_emitted() {
        let mut tok = MathAwareTokenizer;
        let mut stream = tok.token_stream(r"\int_{0}^{\infty} f(x) dx");
        let tokens: Vec<String> = {
            let mut r = Vec::new();
            while stream.advance() {
                r.push(stream.token().text.clone());
            }
            r
        };
        assert!(
            tokens.iter().any(|t| t.starts_with("MATH_INT_")),
            "Should emit MATH_INT_* composed token, got: {:?}", tokens
        );
    }

    #[test]
    fn test_lim_construct_emitted() {
        let mut tok = MathAwareTokenizer;
        let mut stream = tok.token_stream(r"\lim_{x\to\infty} f(x)");
        let tokens: Vec<String> = {
            let mut r = Vec::new();
            while stream.advance() {
                r.push(stream.token().text.clone());
            }
            r
        };
        assert!(
            tokens.iter().any(|t| t.starts_with("MATH_LIM_")),
            "Should emit MATH_LIM_* composed token, got: {:?}", tokens
        );
    }

    #[test]
    fn test_empty_text() {
        let mut tok = MathAwareTokenizer;
        let mut stream = tok.token_stream("");
        assert!(!stream.advance());
    }

    #[test]
    fn test_math_symbols_preserved() {
        let mut tok = MathAwareTokenizer;
        let mut stream = tok.token_stream("E = mc^2 and ∑ ∫ symbols");
        let tokens: Vec<String> = {
            let mut r = Vec::new();
            while stream.advance() {
                r.push(stream.token().text.clone());
            }
            r
        };
        assert!(tokens.contains(&"E".to_string()));
        assert!(tokens.contains(&"∑".to_string()));
        assert!(tokens.contains(&"∫".to_string()));
    }

    // --- extract_math_source tests ---

    #[test]
    fn test_extract_inline_math() {
        let text = "The value is $\\alpha + \\beta$ in this equation.";
        let result = extract_math_source(text);
        assert!(result.is_some(), "Should find inline math");
        let source = result.unwrap();
        assert!(source.contains("inline:"));
        assert!(source.contains(r"\alpha + \beta"));
    }

    #[test]
    fn test_extract_display_math() {
        let text = "Consider $$\\int_{0}^{\\infty} e^{-x} dx$$ which converges.";
        let result = extract_math_source(text);
        assert!(result.is_some(), "Should find display math");
        let source = result.unwrap();
        assert!(source.contains("display:"));
        assert!(source.contains(r"\int_{0}^{\infty}"));
    }

    #[test]
    fn test_extract_no_math() {
        let text = "This is plain text without any math delimiters.";
        let result = extract_math_source(text);
        assert!(result.is_none(), "Plain text should return None");
    }

    #[test]
    fn test_extract_multiple_math_regions() {
        let text = "Inline $a+b$ and display $$\\sum_{i=1}^n i$$";
        let result = extract_math_source(text).unwrap();
        assert!(result.contains("inline:"));
        assert!(result.contains("display:"));
    }

    #[test]
    fn test_extract_math_empty_delimiters() {
        let text = "Empty $$ $$ math";
        let result = extract_math_source(text);
        assert!(result.is_some());
    }

    // ── Regression: extract_math_source quick-filter equivalence ──

    #[test]
    fn test_extract_math_source_backslash_path_no_math() {
        // Backslash present but not a math delimiter — quick-filter passes it through
        let text = r"file\path\name.txt";
        let result = extract_math_source(text);
        // Quick-filter: text contains '\' → regex runs → no math patterns → None
        assert!(result.is_none(), "backslash-only path should return None");
    }

    #[test]
    fn test_extract_math_source_very_long_no_math() {
        let text = "This is plain text without any math delimiters. ".repeat(100);
        let result = extract_math_source(&text);
        assert!(result.is_none(), "long text without math should return None (hits quick-filter)");
    }

    #[test]
    fn test_extract_math_source_empty_string() {
        let result = extract_math_source("");
        assert!(result.is_none(), "empty string should return None");
    }

    #[test]
    fn test_extract_math_source_whitespace_only() {
        let result = extract_math_source("   \n  \t  ");
        assert!(result.is_none(), "whitespace-only should return None (hits quick-filter)");
    }

    #[test]
    fn test_extract_math_source_only_backslash() {
        // Single backslash triggers quick-filter pass-through; regex finds no math
        let result = extract_math_source("\\");
        assert!(result.is_none(), "single backslash has no math delimiters");
    }

    #[test]
    fn test_extract_math_source_quick_filter_still_detects_inline_math() {
        // Dollar sign present AND valid inline math — quick-filter must
        // pass it through to regex, which must detect it.
        let text = r"Solve $\int_{0}^{1} x^2 dx$ for the area.";
        let result = extract_math_source(text);
        assert!(result.is_some(), "quick-filter should still detect inline math");
        let source = result.unwrap();
        assert!(source.contains("inline:"), "should contain inline math content");
    }

}
