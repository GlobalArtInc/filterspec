package filterspec

import (
	"encoding/json"
	"math/rand"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

type goldenFile struct {
	Schema Schema       `json:"schema"`
	Cases  []goldenCase `json:"cases"`
}

type goldenCase struct {
	Name   string          `json:"name"`
	Scope  []ScopeFilter   `json:"scope"`
	Filter json.RawMessage `json:"filter"`
	SQL    string          `json:"sql"`
	Args   []any           `json:"args"`
	Error  string          `json:"error"`
}

// UnmarshalJSON keeps the golden file readable: {"field": ..., "value": ...}.
func (s *ScopeFilter) UnmarshalJSON(data []byte) error {
	var raw struct {
		Field string `json:"field"`
		Value any    `json:"value"`
	}
	if err := json.Unmarshal(data, &raw); err != nil {
		return err
	}
	s.Field, s.Value = raw.Field, raw.Value
	return nil
}

// TestGolden runs the vectors shared with the Rust implementation. Both must
// produce byte-identical SQL — that is the only thing keeping the two ports
// honest about the wire format.
func TestGolden(t *testing.T) {
	data, err := os.ReadFile(filepath.Join("..", "testdata", "cases.json"))
	if err != nil {
		t.Fatalf("прочитать вектора: %v", err)
	}

	var golden goldenFile
	if err := json.Unmarshal(data, &golden); err != nil {
		t.Fatalf("разобрать вектора: %v", err)
	}

	for _, testCase := range golden.Cases {
		t.Run(testCase.Name, func(t *testing.T) {
			query := Query{Scope: testCase.Scope}

			if len(testCase.Filter) > 0 && string(testCase.Filter) != "null" {
				var root Root
				if err := json.Unmarshal(testCase.Filter, &root); err != nil {
					if testCase.Error == "" {
						t.Fatalf("разобрать фильтр: %v", err)
					}
					return
				}
				node := root.Node()
				query.Filter = &node
			}

			sql, args, err := Render(query, golden.Schema)

			if testCase.Error != "" {
				if err == nil {
					t.Fatalf("ждали ошибку %q, получили SQL %q", testCase.Error, sql)
				}
				if !strings.Contains(err.Error(), testCase.Error) {
					t.Fatalf("ждали ошибку %q, получили %q", testCase.Error, err.Error())
				}
				return
			}

			if err != nil {
				t.Fatalf("неожиданная ошибка: %v", err)
			}
			if sql != testCase.SQL {
				t.Errorf("SQL не совпал\nждали:   %s\nполучили: %s", testCase.SQL, sql)
			}
			if got, want := toJSON(t, args), toJSON(t, testCase.Args); got != want {
				t.Errorf("аргументы не совпали\nждали:   %s\nполучили: %s", want, got)
			}
		})
	}
}

func toJSON(t *testing.T, values []any) string {
	t.Helper()
	if values == nil {
		values = []any{}
	}
	encoded, err := json.Marshal(values)
	if err != nil {
		t.Fatalf("сериализовать: %v", err)
	}
	return string(encoded)
}

// TestFoldKeepsEveryLeaf is the test whose absence let a nested group swallow
// its siblings in the TypeScript implementation: every leaf that goes into the
// tree has to come out of it.
func TestFoldKeepsEveryLeaf(t *testing.T) {
	schema := Schema{
		"name":      {Column: "name", Type: TypeString},
		"serviceId": {Column: "service_id", Type: TypeNumber},
	}
	random := rand.New(rand.NewSource(20260825))

	for run := 0; run < 500; run++ {
		node := randomNode(random, 0)
		expected := countLeaves(node)

		spec, err := Parse(node, schema)
		if err != nil {
			t.Fatalf("разбор упал на дереве %+v: %v", node, err)
		}

		leaves := Leaves(spec)
		if len(leaves) != expected {
			t.Fatalf("листьев на входе %d, в дереве %d\n%+v", expected, len(leaves), node)
		}

		sql, args, err := Render(Query{Filter: &node}, schema)
		if err != nil {
			t.Fatalf("рендер упал: %v", err)
		}
		if placeholders := strings.Count(sql, "$"); placeholders != len(args) {
			t.Fatalf("плейсхолдеров %d, аргументов %d: %s", placeholders, len(args), sql)
		}
	}
}

func randomNode(random *rand.Rand, depth int) Node {
	if depth >= 3 || random.Intn(3) == 0 {
		if random.Intn(2) == 0 {
			return Node{Type: TypeString, Field: "name", Operator: "$eq", Value: "v"}
		}
		return Node{Type: TypeNumber, Field: "serviceId", Operator: "$eq", Value: float64(random.Intn(100))}
	}

	conjunctions := []Conjunction{And, Or, Not}
	children := make([]Node, 1+random.Intn(3))
	for i := range children {
		children[i] = randomNode(random, depth+1)
	}
	return Node{Conjunction: conjunctions[random.Intn(len(conjunctions))], Children: children}
}

func countLeaves(node Node) int {
	if node.Conjunction == "" {
		return 1
	}
	total := 0
	for _, child := range node.Children {
		total += countLeaves(child)
	}
	return total
}

// TestSchemaIsTheOnlySourceOfIdentifiers makes sure no field name from the
// request reaches the SQL text.
func TestSchemaIsTheOnlySourceOfIdentifiers(t *testing.T) {
	schema := Schema{"name": {Column: "name", Type: TypeString}}
	node := Node{Type: TypeString, Field: `name"; DROP TABLE task; --`, Operator: "$eq", Value: "x"}

	if _, _, err := Render(Query{Filter: &node}, schema); err == nil {
		t.Fatal("поле вне схемы должно быть ошибкой")
	}
}
