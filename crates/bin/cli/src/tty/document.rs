use super::theme::Tone;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Document {
    pub title: Option<String>,
    pub sections: Vec<Section>,
    pub hints: Vec<Hint>,
}

impl Document {
    pub fn titled(title: impl Into<String>) -> Self {
        Self {
            title: Some(title.into()),
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Section {
    pub heading: Option<String>,
    pub rows: Vec<Row>,
}

impl Section {
    pub fn named(heading: impl Into<String>) -> Self {
        Self {
            heading: Some(heading.into()),
            rows: Vec::new(),
        }
    }

    pub fn row(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.rows.push(Row::key_value(key, value));
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub key: Option<String>,
    pub value: String,
    pub tone: Tone,
}

impl Row {
    pub fn key_value(key: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            key: Some(key.into()),
            value: value.into(),
            tone: Tone::Neutral,
        }
    }

    pub fn text(value: impl Into<String>) -> Self {
        Self {
            key: None,
            value: value.into(),
            tone: Tone::Neutral,
        }
    }

    pub fn with_tone(mut self, tone: Tone) -> Self {
        self.tone = tone;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hint(pub String);

impl Hint {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusBanner {
    pub tone: Tone,
    pub heading: String,
    pub detail: Option<String>,
    pub rows: Vec<Row>,
}

impl StatusBanner {
    pub fn new(tone: Tone, heading: impl Into<String>) -> Self {
        Self {
            tone,
            heading: heading.into(),
            detail: None,
            rows: Vec::new(),
        }
    }
}
