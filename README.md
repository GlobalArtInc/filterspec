# filterspec

Two implementations — Go and Rust — of one thing: turning the client filter DSL
into a parameterised Postgres predicate.

The DSL is the one `@globalart/ddd` defines and the robocall front end already
sends, so the same JSON keeps working unchanged. Both implementations are driven
by the same golden vectors in `testdata/` and must produce byte-identical SQL.

## Layout

```
go/         Go module github.com/GlobalArtInc/filterspec/go
rust/       crate filterspec
testdata/   cases.json — shared vectors, the contract both implementations obey
```

## Why this exists

It is the only part of `@globalart/ddd` worth porting. Value objects, the CQRS
base classes and the repository/uow/outbox interfaces are TypeScript scaffolding:
in Go and Rust they are newtypes, struct methods and function signatures, not a
library. What does not dissolve is the filter pipeline — DSL parsing, folding
into a specification tree, and rendering to SQL.

It also exists because the TypeScript version drops conditions. In
`packages/ddd/src/filter/filter.ts` the fold walks a list while mutating an
accumulator, and a nested group **replaces** the accumulator instead of combining
with it:

```ts
} else if (isGroup(filter)) {
  spec = convertFilterOrGroupList(filter.children, filter.conjunction);
}
```

A caller that adds a tenant scope as the first child of `$and` loses that scope
whenever the client sends a group. The `$not` branch has the same shape of bug:
it returns after the first child and silently discards the rest.

Both bugs come from folding with an accumulator over a binary tree. Here the tree
is n-ary and the fold is pure — children are parsed into a `Vec`, then one
constructor is applied to all of them. There is no accumulator to overwrite.

## Contract

Three shapes are accepted at the root, matching the TypeScript `RootFilter`:

```json
{ "type": "string", "field": "name", "operator": "$eq", "value": "test" }

[ { "type": "number", "field": "serviceId", "operator": "$eq", "value": 42 } ]

{ "conjunction": "$and", "children": [ ... ] }
```

Conjunctions are `$and`, `$or`, `$not`. `$not` negates the conjunction of **all**
its children, not just the first one.

Field types and their operators:

| type      | operators |
|-----------|-----------|
| `string`  | `$eq` `$neq` `$contains` `$not_contains` `$starts_with` `$ends_with` `$regex` `$is_empty` `$is_not_empty` |
| `number`  | `$eq` `$neq` `$gt` `$gte` `$lt` `$lte` `$is_empty` `$is_not_empty` |
| `date`    | `$eq` `$neq` `$gt` `$gte` `$lt` `$lte` `$between` `$is_today` `$is_tomorrow` `$is_yesterday` `$is_not_today` |
| `boolean` | `$eq` `$neq` |

Negative operators are parsed as `Not` wrapping the positive one, so there is one
family of predicates and the renderer implements negation once.

### Nothing is dropped silently

An element that cannot be understood is an error, not an omission. A group
without a conjunction, an empty group, a field outside the schema, an operator
the field type does not support, a value of the wrong type — all are rejected.
The TypeScript version returns `None` for most of these and the condition simply
disappears from the query.

### Fields come from the schema, never from the request

The caller passes a whitelist. A field name that is not in it is an error, and
identifiers in the generated SQL are always the schema's column names:

```go
schema := filterspec.Schema{
    "name":      {Column: "name", Type: filterspec.TypeString},
    "serviceId": {Column: "service_id", Type: filterspec.TypeNumber},
}
```

### Scope is structural, not a sibling

The tenant condition is a separate field of the query, and rendering always emits
`scope AND (client filter)`. A client filter cannot become a sibling of the scope
and push it out — which is exactly how the leak happens in the TypeScript
implementation, where the handler merges both into one `children` array.

```go
sql, args, err := filterspec.Render(filterspec.Query{
    Scope:  []filterspec.ScopeFilter{{Field: "serviceId", Value: currentServiceID}},
    Filter: clientFilter, // may be nil
}, schema)
```

```rust
let (sql, args) = filterspec::render(&Query {
    scope: vec![ScopeFilter { field: "serviceId".into(), value: json!(current_service_id) }],
    filter: client_filter, // may be None
}, &schema)?;
```

An empty query renders as `TRUE`, so the result can always be concatenated into a
`WHERE` clause.

## Tests

```bash
cd go   && go test ./...
cd rust && cargo test
```

Both run `testdata/cases.json`, which includes the regressions above as explicit
cases, plus a property test: **every leaf that goes into the tree must come out
of it**. That test is the point of the repository — its absence is what let the
condition-dropping bug reach production.

## Adding an operator

1. Add it to `testdata/cases.json` with the SQL you expect.
2. Watch both implementations fail.
3. Make them pass.

Never add an operator to one implementation only — the vectors are the contract.
