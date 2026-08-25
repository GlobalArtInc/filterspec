//! Runs the vectors shared with the Go implementation. Both must produce
//! byte-identical SQL — that is the only thing keeping the two ports honest
//! about the wire format.

use std::collections::HashMap;

use filterspec::{
    leaves, parse, render, Column, FieldType, Node, Query, Root, Schema, ScopeFilter,
};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Deserialize)]
struct GoldenFile {
    schema: HashMap<String, Column>,
    cases: Vec<GoldenCase>,
}

#[derive(Deserialize)]
struct GoldenCase {
    name: String,
    #[serde(default)]
    scope: Vec<ScopeFilter>,
    #[serde(default)]
    filter: Value,
    #[serde(default)]
    sql: Option<String>,
    #[serde(default)]
    args: Vec<Value>,
    #[serde(default)]
    error: Option<String>,
}

#[test]
fn golden_vectors() {
    let raw = std::fs::read_to_string("../testdata/cases.json").expect("прочитать вектора");
    let golden: GoldenFile = serde_json::from_str(&raw).expect("разобрать вектора");
    let schema: Schema = golden.schema;

    for case in golden.cases {
        let filter = match &case.filter {
            Value::Null => None,
            value => match serde_json::from_value::<Root>(value.clone()) {
                Ok(root) => Some(Node::from(root)),
                Err(_) => {
                    assert!(
                        case.error.is_some(),
                        "{}: фильтр не разобрался, а ошибка не ожидалась",
                        case.name
                    );
                    continue;
                }
            },
        };

        let query = Query {
            scope: case.scope,
            filter,
        };

        match (render(&query, &schema), &case.error) {
            (Err(err), Some(expected)) => assert!(
                err.to_string().contains(expected),
                "{}: ждали ошибку {expected:?}, получили {err}",
                case.name
            ),
            (Err(err), None) => panic!("{}: неожиданная ошибка {err}", case.name),
            (Ok((sql, _)), Some(expected)) => {
                panic!(
                    "{}: ждали ошибку {expected:?}, получили SQL {sql}",
                    case.name
                )
            }
            (Ok((sql, args)), None) => {
                assert_eq!(sql, case.sql.unwrap_or_default(), "{}: SQL", case.name);
                assert_eq!(args, case.args, "{}: аргументы", case.name);
            }
        }
    }
}

/// The test whose absence let a nested group swallow its siblings in the
/// TypeScript implementation: every leaf that goes into the tree has to come out
/// of it.
#[test]
fn fold_keeps_every_leaf() {
    let schema: Schema = HashMap::from([
        (
            "name".to_string(),
            Column {
                column: "name".into(),
                field_type: FieldType::String,
                relation: None,
            },
        ),
        (
            "serviceId".to_string(),
            Column {
                column: "service_id".into(),
                field_type: FieldType::Number,
                relation: None,
            },
        ),
    ]);

    let mut rng = StdRng::seed_from_u64(20_260_825);

    for _ in 0..500 {
        let node_json = random_node(&mut rng, 0);
        let expected = count_leaves(&node_json);

        let node: Node = serde_json::from_value(node_json.clone()).expect("собрать узел");
        let spec = parse(&node, &schema).expect("разобрать дерево");
        assert_eq!(
            leaves(&spec).len(),
            expected,
            "листья потерялись на {node_json}"
        );

        let query = Query {
            scope: Vec::new(),
            filter: Some(node),
        };
        let (sql, args) = render(&query, &schema).expect("отрендерить");
        assert_eq!(
            sql.matches('$').count(),
            args.len(),
            "плейсхолдеры и аргументы разошлись: {sql}"
        );
    }
}

fn random_node(rng: &mut StdRng, depth: u32) -> Value {
    if depth >= 3 || rng.gen_range(0..3) == 0 {
        return if rng.gen_bool(0.5) {
            json!({ "type": "string", "field": "name", "operator": "$eq", "value": "v" })
        } else {
            json!({ "type": "number", "field": "serviceId", "operator": "$eq", "value": rng.gen_range(0..100) })
        };
    }

    let conjunction = ["$and", "$or", "$not"][rng.gen_range(0..3)];
    let children: Vec<Value> = (0..rng.gen_range(1..4))
        .map(|_| random_node(rng, depth + 1))
        .collect();

    json!({ "conjunction": conjunction, "children": children })
}

fn count_leaves(node: &Value) -> usize {
    match node.get("children").and_then(Value::as_array) {
        Some(children) => children.iter().map(count_leaves).sum(),
        None => 1,
    }
}

/// Makes sure no field name from the request reaches the SQL text.
#[test]
fn schema_is_the_only_source_of_identifiers() {
    let schema: Schema = HashMap::from([(
        "name".to_string(),
        Column {
            column: "name".into(),
            field_type: FieldType::String,
            relation: None,
        },
    )]);

    let node: Node = serde_json::from_value(json!({
        "type": "string",
        "field": "name\"; DROP TABLE task; --",
        "operator": "$eq",
        "value": "x"
    }))
    .expect("собрать узел");

    let query = Query {
        scope: Vec::new(),
        filter: Some(node),
    };

    assert!(
        render(&query, &schema).is_err(),
        "поле вне схемы должно быть ошибкой"
    );
}
