package filterspec

import (
	"fmt"
	"strings"
)

// ScopeFilter is a condition the caller adds itself — the tenant it is allowed
// to see. It is deliberately a separate field from the client tree: Render
// always emits scope AND (client), so a client filter can never become a
// sibling of the scope and push it out of the query.
type ScopeFilter struct {
	Field string
	Value any
}

// Query is what a handler builds: the scope it controls plus whatever the
// client sent.
type Query struct {
	Scope  []ScopeFilter
	Filter *Node
}

// Render returns a Postgres predicate with $n placeholders and its arguments.
// An empty query renders as TRUE rather than as an empty string, so callers can
// always concatenate it into a WHERE clause.
func Render(query Query, schema Schema) (string, []any, error) {
	var parts []Spec

	for _, scope := range query.Scope {
		column, ok := schema[scope.Field]
		if !ok {
			return "", nil, fmt.Errorf("%w: %q", ErrUnknownField, scope.Field)
		}
		value, err := scalarValue(column.Type, scope.Value)
		if err != nil {
			return "", nil, err
		}
		parts = append(parts, Pred{Column: column, Operator: "$eq", Value: value})
	}

	if query.Filter != nil {
		spec, err := Parse(*query.Filter, schema)
		if err != nil {
			return "", nil, err
		}
		parts = append(parts, spec)
	}

	if len(parts) == 0 {
		return "TRUE", nil, nil
	}

	var spec Spec = parts[0]
	if len(parts) > 1 {
		spec = AndSpec{Children: parts}
	}

	writer := &sqlWriter{}
	sql, err := writer.render(spec)
	if err != nil {
		return "", nil, err
	}
	return sql, writer.args, nil
}

type sqlWriter struct {
	args []any
}

func (w *sqlWriter) placeholder(value any) string {
	w.args = append(w.args, value)
	return fmt.Sprintf("$%d", len(w.args))
}

func (w *sqlWriter) render(spec Spec) (string, error) {
	switch node := spec.(type) {
	case AndSpec:
		return w.join(node.Children, " AND ")
	case OrSpec:
		return w.join(node.Children, " OR ")
	case NotSpec:
		inner, err := w.render(node.Child)
		if err != nil {
			return "", err
		}
		return "NOT (" + inner + ")", nil
	case Pred:
		return w.predicate(node)
	}
	return "", fmt.Errorf("%w: %T", ErrInvalidNode, spec)
}

func (w *sqlWriter) join(children []Spec, separator string) (string, error) {
	if len(children) == 0 {
		return "", ErrEmptyGroup
	}
	rendered := make([]string, 0, len(children))
	for _, child := range children {
		sql, err := w.render(child)
		if err != nil {
			return "", err
		}
		rendered = append(rendered, sql)
	}
	if len(rendered) == 1 {
		return rendered[0], nil
	}
	return "(" + strings.Join(rendered, separator) + ")", nil
}

func (w *sqlWriter) predicate(pred Pred) (string, error) {
	column := quote(pred.Column)

	switch pred.Operator {
	case "$eq":
		return column + " = " + w.placeholder(pred.Value), nil
	case "$gt":
		return column + " > " + w.placeholder(pred.Value), nil
	case "$gte":
		return column + " >= " + w.placeholder(pred.Value), nil
	case "$lt":
		return column + " < " + w.placeholder(pred.Value), nil
	case "$lte":
		return column + " <= " + w.placeholder(pred.Value), nil
	case "$contains":
		return column + " ILIKE " + w.placeholder("%"+escapeLike(pred.Value.(string))+"%"), nil
	case "$starts_with":
		return column + " ILIKE " + w.placeholder(escapeLike(pred.Value.(string))+"%"), nil
	case "$ends_with":
		return column + " ILIKE " + w.placeholder("%"+escapeLike(pred.Value.(string))), nil
	case "$regex":
		return column + " ~ " + w.placeholder(pred.Value), nil
	case "$is_empty":
		if pred.Column.Type == TypeString {
			return "(" + column + " IS NULL OR " + column + " = '')", nil
		}
		return column + " IS NULL", nil
	case "$between":
		return column + " BETWEEN " + w.placeholder(pred.Values[0]) + " AND " + w.placeholder(pred.Values[1]), nil
	case "$is_today":
		return column + "::date = CURRENT_DATE", nil
	case "$is_tomorrow":
		return column + "::date = CURRENT_DATE + 1", nil
	case "$is_yesterday":
		return column + "::date = CURRENT_DATE - 1", nil
	}
	return "", fmt.Errorf("%w: %q", ErrUnsupportedOp, pred.Operator)
}

func quote(column Column) string {
	if column.Relation != "" {
		return identifier(column.Relation) + "." + identifier(column.Column)
	}
	return identifier(column.Column)
}

// identifier quotes a name that came from the schema, never from the request.
// Doubling any quote inside keeps the output well-formed even if someone puts a
// strange column name in the schema.
func identifier(name string) string {
	return `"` + strings.ReplaceAll(name, `"`, `""`) + `"`
}

func escapeLike(value string) string {
	replacer := strings.NewReplacer(`\`, `\\`, `%`, `\%`, `_`, `\_`)
	return replacer.Replace(value)
}
