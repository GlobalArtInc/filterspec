//! Parses the client filter DSL into a specification tree and renders it into a
//! parameterised Postgres predicate.
//!
//! The wire format is the one `@globalart/ddd` defines and the robocall front
//! end already sends, so the same JSON keeps working unchanged.

use std::collections::HashMap;
use std::fmt;

use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum Conjunction {
    #[serde(rename = "$and")]
    And,
    #[serde(rename = "$or")]
    Or,
    #[serde(rename = "$not")]
    Not,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FieldType {
    String,
    Number,
    Date,
    Boolean,
}

impl fmt::Display for FieldType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            FieldType::String => "string",
            FieldType::Number => "number",
            FieldType::Date => "date",
            FieldType::Boolean => "boolean",
        };
        f.write_str(name)
    }
}

/// One field the client is allowed to filter on. Fields absent from the schema
/// are rejected, so a field name from the request never reaches the SQL text.
#[derive(Debug, Clone, Deserialize)]
pub struct Column {
    pub column: String,
    #[serde(rename = "type")]
    pub field_type: FieldType,
    #[serde(default)]
    pub relation: Option<String>,
}

pub type Schema = HashMap<String, Column>;

#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    InvalidNode,
    EmptyGroup,
    UnknownField(String),
    UnsupportedOperator(String),
    InvalidValue(String),
    TypeMismatch(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::InvalidNode => write!(f, "invalid node"),
            Error::EmptyGroup => write!(f, "empty group"),
            Error::UnknownField(field) => write!(f, "unknown field: {field}"),
            Error::UnsupportedOperator(op) => write!(f, "unsupported operator: {op}"),
            Error::InvalidValue(detail) => write!(f, "invalid value: {detail}"),
            Error::TypeMismatch(detail) => write!(f, "type mismatch: {detail}"),
        }
    }
}

impl std::error::Error for Error {}

/// One element of the incoming filter: a group with a conjunction and children,
/// or a leaf carrying field, operator and value. `serde` picks the variant by
/// shape, which is what `z.discriminatedUnion` did on the TypeScript side.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Node {
    Group {
        conjunction: Conjunction,
        children: Vec<Node>,
    },
    Leaf {
        #[serde(rename = "type")]
        field_type: FieldType,
        field: String,
        #[serde(default)]
        relation: Option<String>,
        operator: String,
        #[serde(default)]
        value: Value,
    },
}

/// The three shapes the DSL allows: a single filter, an array of filters
/// (implicit `$and`) and a group.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Root {
    List(Vec<Node>),
    One(Node),
}

impl From<Root> for Node {
    fn from(root: Root) -> Node {
        match root {
            Root::List(children) => Node::Group {
                conjunction: Conjunction::And,
                children,
            },
            Root::One(node) => node,
        }
    }
}

/// The parsed tree. `And` and `Or` are n-ary on purpose: a group folds into
/// exactly one node holding every child, so no accumulator can be overwritten
/// while walking the list.
#[derive(Debug, Clone, PartialEq)]
pub enum Spec {
    And(Vec<Spec>),
    Or(Vec<Spec>),
    Not(Box<Spec>),
    Pred(Pred),
}

/// A leaf: one comparison against one column.
#[derive(Debug, Clone, PartialEq)]
pub struct Pred {
    pub column: String,
    pub relation: Option<String>,
    pub field_type: FieldType,
    pub operator: String,
    pub value: Option<Value>,
    pub range: Option<(String, String)>,
}

fn supports(field_type: FieldType, operator: &str) -> bool {
    let allowed: &[&str] = match field_type {
        FieldType::String => &[
            "$eq",
            "$neq",
            "$contains",
            "$not_contains",
            "$starts_with",
            "$ends_with",
            "$regex",
            "$is_empty",
            "$is_not_empty",
        ],
        FieldType::Number => &[
            "$eq",
            "$neq",
            "$gt",
            "$gte",
            "$lt",
            "$lte",
            "$is_empty",
            "$is_not_empty",
        ],
        FieldType::Date => &[
            "$eq",
            "$neq",
            "$gt",
            "$gte",
            "$lt",
            "$lte",
            "$between",
            "$is_today",
            "$is_tomorrow",
            "$is_yesterday",
            "$is_not_today",
        ],
        FieldType::Boolean => &["$eq", "$neq"],
    };
    allowed.contains(&operator)
}

/// Maps every "not" operator onto the positive one it wraps, so the tree carries
/// a single `Not` instead of a second family of predicates.
fn positive_of(operator: &str) -> Option<&'static str> {
    match operator {
        "$neq" => Some("$eq"),
        "$not_contains" => Some("$contains"),
        "$is_not_empty" => Some("$is_empty"),
        "$is_not_today" => Some("$is_today"),
        _ => None,
    }
}

fn is_valueless(operator: &str) -> bool {
    matches!(
        operator,
        "$is_empty"
            | "$is_not_empty"
            | "$is_today"
            | "$is_tomorrow"
            | "$is_yesterday"
            | "$is_not_today"
    )
}

/// Turns one filter tree into a specification tree, rejecting anything the
/// schema does not allow. Nothing is dropped silently: an element that cannot be
/// understood is an error, not an omission.
pub fn parse(node: &Node, schema: &Schema) -> Result<Spec, Error> {
    match node {
        Node::Group {
            conjunction,
            children,
        } => {
            if children.is_empty() {
                return Err(Error::EmptyGroup);
            }
            let parsed = children
                .iter()
                .map(|child| parse(child, schema))
                .collect::<Result<Vec<_>, _>>()?;

            Ok(match conjunction {
                Conjunction::And => Spec::And(parsed),
                Conjunction::Or => Spec::Or(parsed),
                Conjunction::Not => Spec::Not(Box::new(Spec::And(parsed))),
            })
        }
        Node::Leaf { .. } => parse_leaf(node, schema),
    }
}

fn parse_leaf(node: &Node, schema: &Schema) -> Result<Spec, Error> {
    let Node::Leaf {
        field_type,
        field,
        operator,
        value,
        ..
    } = node
    else {
        return Err(Error::InvalidNode);
    };

    let column = schema
        .get(field)
        .ok_or_else(|| Error::UnknownField(field.clone()))?;

    if column.field_type != *field_type {
        return Err(Error::TypeMismatch(format!(
            "field {field} is {}, filter says {field_type}",
            column.field_type
        )));
    }
    if !supports(column.field_type, operator) {
        return Err(Error::UnsupportedOperator(format!(
            "{operator} on {}",
            column.field_type
        )));
    }

    let (effective, negate) = match positive_of(operator) {
        Some(positive) => (positive.to_string(), true),
        None => (operator.clone(), false),
    };

    let mut pred = Pred {
        column: column.column.clone(),
        relation: column.relation.clone(),
        field_type: column.field_type,
        operator: effective.clone(),
        value: None,
        range: None,
    };

    if !is_valueless(&effective) {
        if effective == "$between" {
            pred.range = Some(between_value(value)?);
        } else {
            pred.value = Some(scalar_value(column.field_type, value)?);
        }
    }

    let spec = Spec::Pred(pred);
    Ok(if negate {
        Spec::Not(Box::new(spec))
    } else {
        spec
    })
}

fn scalar_value(field_type: FieldType, raw: &Value) -> Result<Value, Error> {
    let ok = match field_type {
        FieldType::String | FieldType::Date => raw.is_string(),
        FieldType::Number => raw.is_number(),
        FieldType::Boolean => raw.is_boolean(),
    };
    if ok {
        Ok(raw.clone())
    } else {
        Err(Error::InvalidValue(format!(
            "expected {field_type}, got {raw}"
        )))
    }
}

fn between_value(raw: &Value) -> Result<(String, String), Error> {
    let pair = raw
        .as_array()
        .filter(|items| items.len() == 2)
        .ok_or_else(|| Error::InvalidValue("$between expects a pair".into()))?;

    match (pair[0].as_str(), pair[1].as_str()) {
        (Some(from), Some(to)) => Ok((from.to_string(), to.to_string())),
        _ => Err(Error::InvalidValue("$between expects two dates".into())),
    }
}

/// A condition the caller adds itself — the tenant it is allowed to see. It is
/// deliberately a separate field from the client tree: [`render`] always emits
/// `scope AND (client)`, so a client filter can never become a sibling of the
/// scope and push it out of the query.
#[derive(Debug, Clone, Deserialize)]
pub struct ScopeFilter {
    pub field: String,
    pub value: Value,
}

/// What a handler builds: the scope it controls plus whatever the client sent.
#[derive(Debug, Clone, Default)]
pub struct Query {
    pub scope: Vec<ScopeFilter>,
    pub filter: Option<Node>,
}

/// Returns a Postgres predicate with `$n` placeholders and its arguments. An
/// empty query renders as `TRUE` rather than as an empty string, so callers can
/// always concatenate it into a `WHERE` clause.
pub fn render(query: &Query, schema: &Schema) -> Result<(String, Vec<Value>), Error> {
    let mut parts: Vec<Spec> = Vec::new();

    for scope in &query.scope {
        let column = schema
            .get(&scope.field)
            .ok_or_else(|| Error::UnknownField(scope.field.clone()))?;
        let value = scalar_value(column.field_type, &scope.value)?;
        parts.push(Spec::Pred(Pred {
            column: column.column.clone(),
            relation: column.relation.clone(),
            field_type: column.field_type,
            operator: "$eq".into(),
            value: Some(value),
            range: None,
        }));
    }

    if let Some(filter) = &query.filter {
        parts.push(parse(filter, schema)?);
    }

    let spec = match parts.len() {
        0 => return Ok(("TRUE".into(), Vec::new())),
        1 => parts.remove(0),
        _ => Spec::And(parts),
    };

    let mut args = Vec::new();
    let sql = write_spec(&spec, &mut args)?;
    Ok((sql, args))
}

fn write_spec(spec: &Spec, args: &mut Vec<Value>) -> Result<String, Error> {
    match spec {
        Spec::And(children) => write_group(children, " AND ", args),
        Spec::Or(children) => write_group(children, " OR ", args),
        Spec::Not(child) => Ok(format!("NOT ({})", write_spec(child, args)?)),
        Spec::Pred(pred) => write_pred(pred, args),
    }
}

fn write_group(children: &[Spec], separator: &str, args: &mut Vec<Value>) -> Result<String, Error> {
    if children.is_empty() {
        return Err(Error::EmptyGroup);
    }
    let mut rendered = children
        .iter()
        .map(|child| write_spec(child, args))
        .collect::<Result<Vec<_>, _>>()?;

    Ok(if rendered.len() == 1 {
        rendered.remove(0)
    } else {
        format!("({})", rendered.join(separator))
    })
}

fn write_pred(pred: &Pred, args: &mut Vec<Value>) -> Result<String, Error> {
    let column = quote(pred);

    let push = |value: Value, args: &mut Vec<Value>| {
        args.push(value);
        format!("${}", args.len())
    };

    let text = |value: &Option<Value>| -> Result<String, Error> {
        value
            .as_ref()
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| Error::InvalidValue("expected string".into()))
    };

    let sql = match pred.operator.as_str() {
        "$eq" => format!("{column} = {}", push(required(&pred.value)?, args)),
        "$gt" => format!("{column} > {}", push(required(&pred.value)?, args)),
        "$gte" => format!("{column} >= {}", push(required(&pred.value)?, args)),
        "$lt" => format!("{column} < {}", push(required(&pred.value)?, args)),
        "$lte" => format!("{column} <= {}", push(required(&pred.value)?, args)),
        "$contains" => {
            let pattern = format!("%{}%", escape_like(&text(&pred.value)?));
            format!("{column} ILIKE {}", push(Value::String(pattern), args))
        }
        "$starts_with" => {
            let pattern = format!("{}%", escape_like(&text(&pred.value)?));
            format!("{column} ILIKE {}", push(Value::String(pattern), args))
        }
        "$ends_with" => {
            let pattern = format!("%{}", escape_like(&text(&pred.value)?));
            format!("{column} ILIKE {}", push(Value::String(pattern), args))
        }
        "$regex" => format!("{column} ~ {}", push(required(&pred.value)?, args)),
        "$is_empty" => {
            if pred.field_type == FieldType::String {
                format!("({column} IS NULL OR {column} = '')")
            } else {
                format!("{column} IS NULL")
            }
        }
        "$between" => {
            let (from, to) = pred
                .range
                .clone()
                .ok_or_else(|| Error::InvalidValue("$between expects a pair".into()))?;
            let from = push(Value::String(from), args);
            let to = push(Value::String(to), args);
            format!("{column} BETWEEN {from} AND {to}")
        }
        "$is_today" => format!("{column}::date = CURRENT_DATE"),
        "$is_tomorrow" => format!("{column}::date = CURRENT_DATE + 1"),
        "$is_yesterday" => format!("{column}::date = CURRENT_DATE - 1"),
        other => return Err(Error::UnsupportedOperator(other.to_string())),
    };

    Ok(sql)
}

fn required(value: &Option<Value>) -> Result<Value, Error> {
    value
        .clone()
        .ok_or_else(|| Error::InvalidValue("operator needs a value".into()))
}

fn quote(pred: &Pred) -> String {
    match &pred.relation {
        Some(relation) => format!("{}.{}", identifier(relation), identifier(&pred.column)),
        None => identifier(&pred.column),
    }
}

/// Quotes a name that came from the schema, never from the request. Doubling any
/// quote inside keeps the output well-formed even if someone puts a strange
/// column name in the schema.
fn identifier(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn escape_like(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// Walks the tree and returns every predicate in it. Used by the property test
/// that guards the fold: what goes in must come out.
pub fn leaves(spec: &Spec) -> Vec<&Pred> {
    match spec {
        Spec::Pred(pred) => vec![pred],
        Spec::Not(child) => leaves(child),
        Spec::And(children) | Spec::Or(children) => {
            children.iter().flat_map(|child| leaves(child)).collect()
        }
    }
}
