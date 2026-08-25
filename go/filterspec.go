// Package filterspec parses the client filter DSL into a specification tree
// and renders it into a parameterised Postgres predicate.
//
// The wire format is the one @globalart/ddd defines and the robocall front end
// already sends, so the same JSON must keep working unchanged.
package filterspec

import (
	"encoding/json"
	"errors"
	"fmt"
)

type Conjunction string

const (
	And Conjunction = "$and"
	Or  Conjunction = "$or"
	Not Conjunction = "$not"
)

type FieldType string

const (
	TypeString  FieldType = "string"
	TypeNumber  FieldType = "number"
	TypeDate    FieldType = "date"
	TypeBoolean FieldType = "boolean"
)

// Column describes one field the client is allowed to filter on. Fields absent
// from the schema are rejected, so a field name from the request never reaches
// the SQL text.
type Column struct {
	Column   string    `json:"column"`
	Type     FieldType `json:"type"`
	Relation string    `json:"relation,omitempty"`
}

type Schema map[string]Column

var (
	ErrInvalidNode        = errors.New("invalid node")
	ErrEmptyGroup         = errors.New("empty group")
	ErrUnknownField       = errors.New("unknown field")
	ErrUnsupportedOp      = errors.New("unsupported operator")
	ErrInvalidValue       = errors.New("invalid value")
	ErrTypeMismatch       = errors.New("type mismatch")
	ErrUnknownConjunction = errors.New("unknown conjunction")
)

// Node is one element of the incoming filter: either a group with a conjunction
// and children, or a leaf carrying field, operator and value.
type Node struct {
	Conjunction Conjunction `json:"conjunction,omitempty"`
	Children    []Node      `json:"children,omitempty"`

	Type     FieldType `json:"type,omitempty"`
	Field    string    `json:"field,omitempty"`
	Relation string    `json:"relation,omitempty"`
	Operator string    `json:"operator,omitempty"`
	Value    any       `json:"value,omitempty"`
}

func (n Node) isGroup() bool { return n.Conjunction != "" }
func (n Node) isLeaf() bool  { return n.Type != "" && n.Field != "" && n.Operator != "" }

// Root accepts the three shapes the DSL allows: a single filter, an array of
// filters (implicit $and) and a group.
type Root struct {
	node Node
}

func (r *Root) UnmarshalJSON(data []byte) error {
	var list []Node
	if err := json.Unmarshal(data, &list); err == nil {
		r.node = Node{Conjunction: And, Children: list}
		return nil
	}
	var node Node
	if err := json.Unmarshal(data, &node); err != nil {
		return err
	}
	r.node = node
	return nil
}

func (r Root) Node() Node { return r.node }

// Spec is the parsed tree. It is n-ary on purpose: a group folds into exactly
// one node holding every child, so no accumulator can be overwritten while
// walking the list.
type Spec interface{ isSpec() }

type AndSpec struct{ Children []Spec }
type OrSpec struct{ Children []Spec }
type NotSpec struct{ Child Spec }

// Pred is a leaf: one comparison against one column.
type Pred struct {
	Column   Column
	Operator string
	Value    any
	Values   []any // $between
}

func (AndSpec) isSpec() {}
func (OrSpec) isSpec()  {}
func (NotSpec) isSpec() {}
func (Pred) isSpec()    {}

var operators = map[FieldType]map[string]bool{
	TypeString: {
		"$eq": true, "$neq": true, "$contains": true, "$not_contains": true,
		"$starts_with": true, "$ends_with": true, "$regex": true,
		"$is_empty": true, "$is_not_empty": true,
	},
	TypeNumber: {
		"$eq": true, "$neq": true, "$gt": true, "$gte": true, "$lt": true, "$lte": true,
		"$is_empty": true, "$is_not_empty": true,
	},
	TypeDate: {
		"$eq": true, "$neq": true, "$gt": true, "$gte": true, "$lt": true, "$lte": true,
		"$between": true, "$is_today": true, "$is_tomorrow": true, "$is_yesterday": true,
		"$is_not_today": true,
	},
	TypeBoolean: {"$eq": true, "$neq": true},
}

// negated maps every "not" operator onto the positive one it wraps, so the tree
// carries a single NotSpec instead of a second family of predicates.
var negated = map[string]string{
	"$neq":          "$eq",
	"$not_contains": "$contains",
	"$is_not_empty": "$is_empty",
	"$is_not_today": "$is_today",
}

var valueless = map[string]bool{
	"$is_empty": true, "$is_not_empty": true,
	"$is_today": true, "$is_tomorrow": true, "$is_yesterday": true, "$is_not_today": true,
}

// Parse turns one filter tree into a specification tree, rejecting anything the
// schema does not allow. Nothing is dropped silently: an element that cannot be
// understood is an error, not an omission.
func Parse(node Node, schema Schema) (Spec, error) {
	if node.isGroup() {
		if len(node.Children) == 0 {
			return nil, ErrEmptyGroup
		}
		children := make([]Spec, 0, len(node.Children))
		for _, child := range node.Children {
			spec, err := Parse(child, schema)
			if err != nil {
				return nil, err
			}
			children = append(children, spec)
		}
		switch node.Conjunction {
		case And:
			return AndSpec{Children: children}, nil
		case Or:
			return OrSpec{Children: children}, nil
		case Not:
			return NotSpec{Child: AndSpec{Children: children}}, nil
		default:
			return nil, fmt.Errorf("%w: %q", ErrUnknownConjunction, node.Conjunction)
		}
	}

	if !node.isLeaf() {
		return nil, ErrInvalidNode
	}
	return parseLeaf(node, schema)
}

func parseLeaf(node Node, schema Schema) (Spec, error) {
	column, ok := schema[node.Field]
	if !ok {
		return nil, fmt.Errorf("%w: %q", ErrUnknownField, node.Field)
	}
	if column.Type != node.Type {
		return nil, fmt.Errorf("%w: field %q is %s, filter says %s", ErrTypeMismatch, node.Field, column.Type, node.Type)
	}
	if !operators[column.Type][node.Operator] {
		return nil, fmt.Errorf("%w: %q on %s", ErrUnsupportedOp, node.Operator, column.Type)
	}

	operator := node.Operator
	negate := false
	if positive, ok := negated[operator]; ok {
		operator, negate = positive, true
	}

	pred := Pred{Column: column, Operator: operator}
	if !valueless[operator] {
		if operator == "$between" {
			pair, err := betweenValue(node.Value)
			if err != nil {
				return nil, err
			}
			pred.Values = pair
		} else {
			value, err := scalarValue(column.Type, node.Value)
			if err != nil {
				return nil, err
			}
			pred.Value = value
		}
	}

	if negate {
		return NotSpec{Child: pred}, nil
	}
	return pred, nil
}

func scalarValue(fieldType FieldType, raw any) (any, error) {
	switch fieldType {
	case TypeString, TypeDate:
		value, ok := raw.(string)
		if !ok {
			return nil, fmt.Errorf("%w: expected string, got %T", ErrInvalidValue, raw)
		}
		return value, nil
	case TypeNumber:
		value, ok := raw.(float64)
		if !ok {
			return nil, fmt.Errorf("%w: expected number, got %T", ErrInvalidValue, raw)
		}
		return value, nil
	case TypeBoolean:
		value, ok := raw.(bool)
		if !ok {
			return nil, fmt.Errorf("%w: expected boolean, got %T", ErrInvalidValue, raw)
		}
		return value, nil
	}
	return nil, fmt.Errorf("%w: unknown field type %q", ErrInvalidValue, fieldType)
}

func betweenValue(raw any) ([]any, error) {
	pair, ok := raw.([]any)
	if !ok || len(pair) != 2 {
		return nil, fmt.Errorf("%w: $between expects a pair", ErrInvalidValue)
	}
	from, fromOK := pair[0].(string)
	to, toOK := pair[1].(string)
	if !fromOK || !toOK {
		return nil, fmt.Errorf("%w: $between expects two dates", ErrInvalidValue)
	}
	return []any{from, to}, nil
}

// Leaves walks the tree and returns every predicate in it. Used by the property
// test that guards the fold: what goes in must come out.
func Leaves(spec Spec) []Pred {
	switch node := spec.(type) {
	case Pred:
		return []Pred{node}
	case NotSpec:
		return Leaves(node.Child)
	case AndSpec:
		return leavesOf(node.Children)
	case OrSpec:
		return leavesOf(node.Children)
	}
	return nil
}

func leavesOf(children []Spec) []Pred {
	var out []Pred
	for _, child := range children {
		out = append(out, Leaves(child)...)
	}
	return out
}
