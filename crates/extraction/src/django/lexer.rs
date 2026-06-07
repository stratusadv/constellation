//! Django template lexer: a single linear pass turning source into a stream of
//! borrowed tokens. Two properties matter for constellation's use: every token
//! carries its 1-based start line (the graph needs source positions), and
//! malformed constructs degrade to literal text rather than erroring; indexing
//! is best-effort and must never fail on a half-written template.

/// A fail-fast bound on the number of tokens one template may produce.
const TOKEN_COUNT_MAX: u32 = 2_000_000;

/// A divisor estimating token count from byte length, for the initial capacity.
const TOKEN_CAPACITY_DIVISOR: u32 = 8;

/// The kind of a lexed token, borrowing its text from the source.
#[derive(Clone, Debug, PartialEq)]
pub enum TokenKind<'source> {
    /// The literal text outside any template construct.
    Text(&'source str),

    /// A `{{ expression }}` token: the trimmed inner expression.
    Variable { expression: &'source str },

    /// The opening of a `{% tag arguments %}`. `tag` is the first word,
    /// `arguments` the trimmed remainder.
    BlockStart {
        tag: &'source str,
        arguments: &'source str,
    },

    /// A `{% endtag %}`: `tag` is the name with the `end` prefix removed.
    BlockEnd { tag: &'source str },

    /// A `{# comment #}` token: the trimmed inner text.
    Comment(&'source str),

    /// The literal body of a `{% verbatim %}...{% endverbatim %}`, scanned
    /// whole so its contents are never parsed as template syntax.
    Verbatim(&'source str),
}

/// A lexed token with the 1-based line its first byte sits on.
#[derive(Clone, Debug, PartialEq)]
pub struct Token<'source> {
    pub kind: TokenKind<'source>,
    pub line: u32,
}

/// A linear lexer over one template's source.
pub struct Lexer<'source> {
    input: &'source str,
    bytes: &'source [u8],
    position: u32,
    length: u32,
    line: u32,
}

impl<'source> Lexer<'source> {
    /// A lexer over `input`.
    pub fn new(input: &'source str) -> Self {
        assert!(input.len() <= u32::MAX as usize, "template length exceeds u32 maximum");

        let length = input.len() as u32;

        Self {
            input,
            bytes: input.as_bytes(),
            position: 0,
            length,
            line: 1,
        }
    }

    /// The tokens of the whole input. Each iteration consumes at least one byte, so
    /// the loop is bounded by the byte length; the token cap is a backstop.
    pub fn tokenize(&mut self) -> Vec<Token<'source>> {
        let capacity = (self.length / TOKEN_CAPACITY_DIVISOR).max(16) as usize;
        let mut tokens = Vec::with_capacity(capacity);
        let mut guard: u32 = 0;

        while self.position < self.length {
            guard += 1;

            assert!(guard <= TOKEN_COUNT_MAX, "token count exceeds {TOKEN_COUNT_MAX}");

            let start = self.position;
            let start_line = self.line;
            let kind = self.next_kind();

            assert!(self.position > start, "lexer must advance past byte {start}");

            self.line += count_newlines(&self.input[start as usize..self.position as usize]);

            tokens.push(Token { kind, line: start_line });
        }

        tokens
    }

    /// The byte `offset` ahead of the cursor, if within bounds.
    fn peek(&self, offset: u32) -> Option<u8> {
        let index = self.position.checked_add(offset)?;

        if index < self.length {
            Some(self.bytes[index as usize])
        } else {
            None
        }
    }

    /// The next construct lexed at the cursor. Always advances; a construct that
    /// is never terminated is consumed as literal text.
    fn next_kind(&mut self) -> TokenKind<'source> {
        let byte = self.bytes[self.position as usize];

        if byte == b'{' {
            match self.peek(1) {
                Some(b'{') => return self.consume_variable(),
                Some(b'%') => return self.consume_block(),
                Some(b'#') => return self.consume_comment(),
                _ => {}
            }
        }

        self.consume_text()
    }

    /// The `{{ ... }}` variable consumed at the cursor, tracking quotes so a `}}`
    /// inside a string literal does not close it early.
    fn consume_variable(&mut self) -> TokenKind<'source> {
        let start = self.position;
        self.position += 2;

        let mut quote: u8 = 0;

        while self.position < self.length {
            let byte = self.bytes[self.position as usize];

            if quote != 0 {
                if byte == b'\\' {
                    self.position = (self.position + 2).min(self.length);
                    continue;
                }

                if byte == quote {
                    quote = 0;
                }

                self.position += 1;
            } else if byte == b'"' || byte == b'\'' {
                quote = byte;
                self.position += 1;
            } else if byte == b'}' && self.peek(1) == Some(b'}') {
                let inner = &self.input[(start + 2) as usize..self.position as usize];
                self.position += 2;

                return TokenKind::Variable { expression: inner.trim() };
            } else {
                self.position += 1;
            }
        }

        TokenKind::Text(&self.input[start as usize..self.length as usize])
    }

    /// The `{% ... %}` block consumed at the cursor. Splits the inner text into a
    /// tag word and its arguments; `{% verbatim %}` is consumed whole so its body stays raw.
    fn consume_block(&mut self) -> TokenKind<'source> {
        let start = self.position;
        self.position += 2;

        let inner_start = self.position;

        while self.position < self.length {
            if self.bytes[self.position as usize] == b'%' && self.peek(1) == Some(b'}') {
                let inner = self.input[inner_start as usize..self.position as usize].trim();
                self.position += 2;

                if inner == "verbatim" || inner.starts_with("verbatim ") {
                    return self.consume_verbatim();
                }

                if let Some(rest) = inner.strip_prefix("end") {
                    let tag = rest.split_whitespace().next().unwrap_or("");

                    return TokenKind::BlockEnd { tag };
                }

                let tag = inner.split_whitespace().next().unwrap_or("");
                let arguments = inner[tag.len()..].trim();

                return TokenKind::BlockStart { tag, arguments };
            }

            self.position += 1;
        }

        TokenKind::Text(&self.input[start as usize..self.length as usize])
    }

    /// The unparsed body scanned from just past `{% verbatim %}` to the matching
    /// `{% endverbatim %}`. An unterminated block runs to end of input.
    fn consume_verbatim(&mut self) -> TokenKind<'source> {
        let body_start = self.position;

        while self.position + 1 < self.length {
            if self.bytes[self.position as usize] == b'{' && self.bytes[(self.position + 1) as usize] == b'%' {
                let after = (self.position + 2) as usize;

                if let Some(offset) = self.input[after..].find("%}") {
                    let inner = self.input[after..after + offset].trim();

                    if inner == "endverbatim" {
                        let content = &self.input[body_start as usize..self.position as usize];
                        self.position = (after + offset + 2) as u32;

                        return TokenKind::Verbatim(content);
                    }
                }
            }

            self.position += 1;
        }

        let content = &self.input[body_start as usize..self.length as usize];
        self.position = self.length;

        TokenKind::Verbatim(content)
    }

    /// The `{# ... #}` comment consumed at the cursor.
    fn consume_comment(&mut self) -> TokenKind<'source> {
        let start = self.position;
        self.position += 2;

        let inner_start = self.position;

        while self.position < self.length {
            if self.bytes[self.position as usize] == b'#' && self.peek(1) == Some(b'}') {
                let inner = &self.input[inner_start as usize..self.position as usize];
                self.position += 2;

                return TokenKind::Comment(inner.trim());
            }

            self.position += 1;
        }

        TokenKind::Text(&self.input[start as usize..self.length as usize])
    }

    /// The literal text consumed up to the next `{{` / `{%` / `{#`, or end of input.
    fn consume_text(&mut self) -> TokenKind<'source> {
        let start = self.position;

        self.position += 1;

        while self.position < self.length {
            if self.bytes[self.position as usize] == b'{' {
                match self.peek(1) {
                    Some(b'{') | Some(b'%') | Some(b'#') => break,
                    _ => {}
                }
            }

            self.position += 1;
        }

        let text = &self.input[start as usize..self.position as usize];

        assert!(!text.is_empty(), "text token must not be empty");

        TokenKind::Text(text)
    }
}

/// The number of newline bytes in a slice; bounded by the slice length.
fn count_newlines(text: &str) -> u32 {
    let mut count: u32 = 0;

    for &byte in text.as_bytes() {
        if byte == b'\n' {
            count += 1;
        }
    }

    count
}
