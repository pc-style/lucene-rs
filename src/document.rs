//! Documents and fields, mirroring Lucene's `Document`, `Field`, `FieldType`, `TextField`,
//! `StringField` and `StoredField`.

/// What an indexed field records in its postings (Lucene's `IndexOptions`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum IndexOptions {
    /// Not indexed.
    None,
    /// Doc IDs only: matches, but no term frequencies (scores use freq = 1).
    Docs,
    /// Doc IDs and term frequencies.
    DocsAndFreqs,
    /// Doc IDs, frequencies and positions (needed for phrase queries).
    DocsAndFreqsAndPositions,
}

impl IndexOptions {
    #[must_use]
    pub fn has_freqs(self) -> bool {
        self >= Self::DocsAndFreqs
    }
    #[must_use]
    pub fn has_positions(self) -> bool {
        self == Self::DocsAndFreqsAndPositions
    }
    pub(crate) const fn to_byte(self) -> u8 {
        match self {
            Self::None => 0,
            Self::Docs => 1,
            Self::DocsAndFreqs => 2,
            Self::DocsAndFreqsAndPositions => 3,
        }
    }
    pub(crate) const fn from_byte(b: u8) -> Option<Self> {
        Some(match b {
            0 => Self::None,
            1 => Self::Docs,
            2 => Self::DocsAndFreqs,
            3 => Self::DocsAndFreqsAndPositions,
            _ => return None,
        })
    }
}

/// How a field is indexed and stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FieldType {
    pub index_options: IndexOptions,
    /// Run the value through the analyzer (otherwise the whole value is one token).
    pub tokenized: bool,
    /// Keep the original value so it can be retrieved with search results.
    pub stored: bool,
    /// Skip length normalization (one byte per document per field otherwise).
    pub omit_norms: bool,
}

impl FieldType {
    /// Lucene's `TextField.TYPE_NOT_STORED`: tokenized, with frequencies, positions and norms.
    pub const TEXT: Self = Self {
        index_options: IndexOptions::DocsAndFreqsAndPositions,
        tokenized: true,
        stored: false,
        omit_norms: false,
    };
    /// Lucene's `StringField.TYPE_NOT_STORED`: the exact value as a single token, docs only.
    pub const STRING: Self = Self {
        index_options: IndexOptions::Docs,
        tokenized: false,
        stored: false,
        omit_norms: true,
    };
    /// Lucene's `StoredField.TYPE`: stored, not indexed.
    pub const STORED_ONLY: Self = Self {
        index_options: IndexOptions::None,
        tokenized: false,
        stored: true,
        omit_norms: true,
    };

    #[must_use]
    pub const fn stored(mut self, stored: bool) -> Self {
        self.stored = stored;
        self
    }
    #[must_use]
    pub const fn with_index_options(mut self, options: IndexOptions) -> Self {
        self.index_options = options;
        self
    }
    #[must_use]
    pub const fn with_omit_norms(mut self, omit: bool) -> Self {
        self.omit_norms = omit;
        self
    }
    #[must_use]
    pub fn is_indexed(&self) -> bool {
        self.index_options != IndexOptions::None
    }
}

/// Whether a convenience field constructor also stores the value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Store {
    Yes,
    No,
}

/// A field value. Only `Text` can be indexed; all variants can be stored.
#[derive(Clone, Debug, PartialEq)]
pub enum FieldValue {
    Text(String),
    Bytes(Vec<u8>),
    I64(i64),
    F64(f64),
}

impl FieldValue {
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Text(s) => Some(s),
            _ => None,
        }
    }
    #[must_use]
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Bytes(b) => Some(b),
            _ => None,
        }
    }
    #[must_use]
    pub const fn as_i64(&self) -> Option<i64> {
        match self {
            Self::I64(v) => Some(*v),
            _ => None,
        }
    }
    #[must_use]
    pub const fn as_f64(&self) -> Option<f64> {
        match self {
            Self::F64(v) => Some(*v),
            _ => None,
        }
    }
}

impl From<&str> for FieldValue {
    fn from(s: &str) -> Self {
        Self::Text(s.to_string())
    }
}
impl From<String> for FieldValue {
    fn from(s: String) -> Self {
        Self::Text(s)
    }
}
impl From<Vec<u8>> for FieldValue {
    fn from(b: Vec<u8>) -> Self {
        Self::Bytes(b)
    }
}
impl From<i64> for FieldValue {
    fn from(v: i64) -> Self {
        Self::I64(v)
    }
}
impl From<f64> for FieldValue {
    fn from(v: f64) -> Self {
        Self::F64(v)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Field {
    pub name: String,
    pub value: FieldValue,
    pub field_type: FieldType,
}

impl Field {
    pub fn new(
        name: impl Into<String>,
        value: impl Into<FieldValue>,
        field_type: FieldType,
    ) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
            field_type,
        }
    }
    /// Full-text field (Lucene `TextField`).
    pub fn text(name: impl Into<String>, value: impl Into<String>, store: Store) -> Self {
        Self::new(
            name,
            value.into(),
            FieldType::TEXT.stored(store == Store::Yes),
        )
    }
    /// Exact-match keyword field such as an ID or tag (Lucene `StringField`).
    pub fn string(name: impl Into<String>, value: impl Into<String>, store: Store) -> Self {
        Self::new(
            name,
            value.into(),
            FieldType::STRING.stored(store == Store::Yes),
        )
    }
    /// Stored-only field (Lucene `StoredField`).
    pub fn stored(name: impl Into<String>, value: impl Into<FieldValue>) -> Self {
        Self::new(name, value, FieldType::STORED_ONLY)
    }
}

/// A document: an ordered list of fields. A field name may repeat (multi-valued field).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Document {
    fields: Vec<Field>,
}

impl Document {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
    pub fn add(&mut self, field: Field) -> &mut Self {
        self.fields.push(field);
        self
    }
    /// Builder-style `add`.
    #[must_use]
    pub fn with(mut self, field: Field) -> Self {
        self.fields.push(field);
        self
    }
    #[must_use]
    pub fn fields(&self) -> &[Field] {
        &self.fields
    }
    /// First value of a field.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&FieldValue> {
        self.fields
            .iter()
            .find(|f| f.name == name)
            .map(|f| &f.value)
    }
    /// First value of a field, if it is text.
    pub fn get_str(&self, name: &str) -> Option<&str> {
        self.get(name).and_then(FieldValue::as_str)
    }
    pub fn get_all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a FieldValue> + 'a {
        self.fields
            .iter()
            .filter(move |f| f.name == name)
            .map(|f| &f.value)
    }
    #[must_use]
    pub const fn len(&self) -> usize {
        self.fields.len()
    }
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }
}
