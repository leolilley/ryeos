use std::collections::{BTreeSet, HashSet};

use super::ast::{Expr, ExprKind, Literal};
use super::value::Numeric;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ReferenceSegment {
    Key(String),
    Index(u64),
    Dynamic,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Reference {
    root: String,
    segments: Vec<ReferenceSegment>,
}

impl Reference {
    pub fn root(&self) -> &str {
        &self.root
    }

    pub fn segments(&self) -> &[ReferenceSegment] {
        &self.segments
    }

    pub fn is_dynamic(&self) -> bool {
        self.segments
            .iter()
            .any(|segment| matches!(segment, ReferenceSegment::Dynamic))
    }

    pub fn static_key_path(&self) -> Option<Vec<&str>> {
        self.segments
            .iter()
            .map(|segment| match segment {
                ReferenceSegment::Key(key) => Some(key.as_str()),
                _ => None,
            })
            .collect()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReferenceSet {
    references: BTreeSet<Reference>,
}

impl ReferenceSet {
    pub fn iter(&self) -> impl Iterator<Item = &Reference> {
        self.references.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.references.is_empty()
    }

    pub fn len(&self) -> usize {
        self.references.len()
    }

    pub fn roots(&self) -> impl Iterator<Item = &str> {
        let mut seen = HashSet::new();
        self.references
            .iter()
            .map(|reference| reference.root.as_str())
            .filter(move |root| seen.insert(*root))
    }

    pub fn contains_exact(&self, root: &str, key_path: &[&str]) -> bool {
        self.references.iter().any(|reference| {
            reference.root == root
                && reference.segments.len() == key_path.len()
                && reference
                    .segments
                    .iter()
                    .zip(key_path)
                    .all(|(segment, key)| {
                        matches!(segment, ReferenceSegment::Key(actual) if actual == key)
                    })
        })
    }

    pub fn extend(&mut self, other: &Self) {
        self.references.extend(other.references.iter().cloned());
    }
}

pub(crate) fn collect(expression: &Expr) -> ReferenceSet {
    let mut set = ReferenceSet::default();
    visit(expression, &mut set);
    set
}

fn visit(expression: &Expr, set: &mut ReferenceSet) {
    if let Some((root, segments, dynamic_expressions)) = path(expression) {
        set.references.insert(Reference { root, segments });
        for dynamic in dynamic_expressions {
            visit(dynamic, set);
        }
        return;
    }
    match &expression.kind {
        ExprKind::Literal(_) | ExprKind::Variable(_) => {}
        ExprKind::Member { target, .. } => visit(target, set),
        ExprKind::Index { target, index } => {
            visit(target, set);
            visit(index, set);
        }
        ExprKind::Array(elements) => {
            for element in elements {
                visit(element, set);
            }
        }
        ExprKind::Object(entries) => {
            for (_, value) in entries {
                visit(value, set);
            }
        }
        ExprKind::Unary { operand, .. } | ExprKind::Group(operand) => visit(operand, set),
        ExprKind::Binary { left, right, .. } => {
            visit(left, set);
            visit(right, set);
        }
        ExprKind::Conditional {
            condition,
            then_branch,
            else_branch,
        } => {
            visit(condition, set);
            visit(then_branch, set);
            visit(else_branch, set);
        }
        ExprKind::Call { arguments, .. } => {
            for argument in arguments {
                visit(argument, set);
            }
        }
    }
}

fn path(expression: &Expr) -> Option<(String, Vec<ReferenceSegment>, Vec<&Expr>)> {
    fn descend<'a>(
        expression: &'a Expr,
        segments: &mut Vec<ReferenceSegment>,
        dynamic: &mut Vec<&'a Expr>,
    ) -> Option<String> {
        match &expression.kind {
            ExprKind::Variable(root) => Some(root.clone()),
            ExprKind::Member { target, key } => {
                let root = descend(target, segments, dynamic)?;
                segments.push(ReferenceSegment::Key(key.clone()));
                Some(root)
            }
            ExprKind::Index { target, index } => {
                let root = descend(target, segments, dynamic)?;
                if let Some(segment) = static_index_segment(index) {
                    segments.push(segment);
                } else {
                    segments.push(ReferenceSegment::Dynamic);
                    dynamic.push(index);
                }
                Some(root)
            }
            ExprKind::Group(inner) => descend(inner, segments, dynamic),
            _ => None,
        }
    }

    let mut segments = Vec::new();
    let mut dynamic = Vec::new();
    let root = descend(expression, &mut segments, &mut dynamic)?;
    Some((root, segments, dynamic))
}

fn static_index_segment(expression: &Expr) -> Option<ReferenceSegment> {
    match &expression.kind {
        ExprKind::Literal(Literal::String(key)) => Some(ReferenceSegment::Key(key.clone())),
        ExprKind::Literal(Literal::Number(Numeric::Signed(value))) if *value >= 0 => {
            Some(ReferenceSegment::Index(*value as u64))
        }
        ExprKind::Literal(Literal::Number(Numeric::Unsigned(value))) => {
            Some(ReferenceSegment::Index(*value))
        }
        ExprKind::Group(inner) => static_index_segment(inner),
        _ => None,
    }
}
