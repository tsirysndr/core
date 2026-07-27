use std::fmt;

const SPACER: &str = "\u{200b}";

pub trait Key: Copy + 'static {
    const PLACEHOLDERS: &'static [(&'static str, Self)];
}

#[derive(Debug, Clone, Copy)]
pub enum NoKeys {}

impl Key for NoKeys {
    const PLACEHOLDERS: &'static [(&'static str, Self)] = &[];
}

pub trait Shape {
    type Repr<K>;
}

#[derive(Debug)]
pub struct Lines;

impl Shape for Lines {
    type Repr<K> = Vec<Vec<Segment<K>>>;
}

#[derive(Debug)]
pub struct Line;

impl Shape for Line {
    type Repr<K> = Vec<Segment<K>>;
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TemplateError {
    #[error("{field}: unknown placeholder {{{name}}}")]
    UnknownPlaceholder { field: &'static str, name: String },
    #[error("{field}: unclosed {{ in template")]
    UnclosedBrace { field: &'static str },
    #[error("{field}: stray }} in template")]
    StrayBrace { field: &'static str },
    #[error("{field}: template mustn't be empty")]
    Empty { field: &'static str },
    #[error("{field}: template must be a single line")]
    Multiline { field: &'static str },
}

#[derive(Debug)]
pub enum Segment<K> {
    Literal(String),
    Placeholder(K),
}

pub struct Template<K, S: Shape> {
    repr: S::Repr<K>,
}

impl<K, S: Shape> fmt::Debug for Template<K, S>
where
    S::Repr<K>: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Template")
            .field("repr", &self.repr)
            .finish()
    }
}

impl<K: Key> Template<K, Lines> {
    pub fn parse_lines(field: &'static str, texts: &[String]) -> Result<Self, TemplateError> {
        Ok(Self {
            repr: texts
                .iter()
                .map(|text| line_segments(field, text))
                .collect::<Result<_, _>>()?,
        })
    }

    pub fn lines(&self, resolve: impl Fn(K) -> String) -> Vec<String> {
        self.repr
            .iter()
            .map(|segments| {
                let rendered = render_segments(segments, &resolve);
                match rendered.is_empty() {
                    true => SPACER.to_string(),
                    false => rendered,
                }
            })
            .collect()
    }
}

impl<K: Key> Template<K, Line> {
    // A reject reason is a Line - `receive-pack` gives it as
    // "ng <ref> <reason>\n" inside one `pkt-line`.
    // `line_segments` won't accept newlines
    // so the reason stays on that one line.
    pub fn parse(field: &'static str, text: &str) -> Result<Self, TemplateError> {
        match text.is_empty() {
            true => Err(TemplateError::Empty { field }),
            false => Ok(Self {
                repr: line_segments(field, text)?,
            }),
        }
    }

    pub fn line(&self, resolve: impl Fn(K) -> String) -> String {
        render_segments(&self.repr, &resolve)
    }
}

impl Template<NoKeys, Lines> {
    pub fn text_lines(&self) -> Vec<String> {
        self.lines(|key| match key {})
    }
}

impl Template<NoKeys, Line> {
    pub fn text(&self) -> String {
        self.line(|key| match key {})
    }
}

fn line_segments<K: Key>(
    field: &'static str,
    text: &str,
) -> Result<Vec<Segment<K>>, TemplateError> {
    match text.contains('\n') {
        true => Err(TemplateError::Multiline { field }),
        false => segments(field, text),
    }
}

fn render_segments<K: Key>(segments: &[Segment<K>], resolve: &dyn Fn(K) -> String) -> String {
    segments
        .iter()
        .map(|segment| match segment {
            Segment::Literal(text) => text.clone(),
            Segment::Placeholder(key) => resolve(*key),
        })
        .collect()
}

fn segments<K: Key>(field: &'static str, text: &str) -> Result<Vec<Segment<K>>, TemplateError> {
    let Some(at) = text.find(['{', '}']) else {
        return Ok(literal(text));
    };
    let (before, rest) = text.split_at(at);
    let (parsed, remainder) = brace(field, rest)?;
    Ok(literal(before)
        .into_iter()
        .chain(parsed)
        .chain(segments(field, remainder)?)
        .collect())
}

fn literal<K>(text: &str) -> Vec<Segment<K>> {
    match text.is_empty() {
        true => Vec::new(),
        false => vec![Segment::Literal(text.to_string())],
    }
}

type Braced<'a, K> = (Option<Segment<K>>, &'a str);

fn brace<'a, K: Key>(field: &'static str, rest: &'a str) -> Result<Braced<'a, K>, TemplateError> {
    if let Some(after) = rest.strip_prefix("{{") {
        return Ok((Some(Segment::Literal("{".to_string())), after));
    }
    if let Some(after) = rest.strip_prefix("}}") {
        return Ok((Some(Segment::Literal("}".to_string())), after));
    }
    if rest.starts_with('}') {
        return Err(TemplateError::StrayBrace { field });
    }
    let close = rest
        .find('}')
        .ok_or(TemplateError::UnclosedBrace { field })?;
    let name = &rest[1..close];
    K::PLACEHOLDERS
        .iter()
        .find(|(candidate, _)| *candidate == name)
        .map(|(_, key)| (Some(Segment::Placeholder(*key)), &rest[close + 1..]))
        .ok_or_else(|| TemplateError::UnknownPlaceholder {
            field,
            name: name.to_string(),
        })
}
