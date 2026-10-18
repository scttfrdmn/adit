// SPDX-License-Identifier: Apache-2.0

package pricing

import "testing"

func TestParseOnDemand(t *testing.T) {
	doc := `{"terms":{"OnDemand":{"X.Y":{"priceDimensions":{"X.Y.Z":{"unit":"Hrs","pricePerUnit":{"USD":"8.2861000000"}}}}}}}`
	v, ok := parseOnDemand(doc)
	if !ok || v != 8.2861 {
		t.Fatalf("got %v %v", v, ok)
	}
	if _, ok := parseOnDemand(`{"terms":{"OnDemand":{"a":{"priceDimensions":{"b":{"unit":"Hrs","pricePerUnit":{"USD":"0.0000000000"}}}}}}}`); ok {
		t.Fatal("zero price accepted")
	}
}
