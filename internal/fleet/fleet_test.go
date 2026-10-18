// SPDX-License-Identifier: Apache-2.0

package fleet

import (
	"strings"
	"testing"
)

func TestParseCandidates(t *testing.T) {
	c, err := ParseCandidates("us-east-1a, us-east-2c:spot,us-west-2b:od")
	if err != nil {
		t.Fatal(err)
	}
	if len(c) != 3 || c[0].Region != "us-east-1" || c[1].Region != "us-east-2" || !c[1].Spot || c[2].Spot {
		t.Fatalf("%+v", c)
	}
	if _, err := ParseCandidates("us-east-1a:cheap"); err == nil {
		t.Fatal("bad mode accepted")
	}
	if _, err := ParseCandidates(""); err == nil {
		t.Fatal("empty accepted")
	}
}

func TestLaunchArgsCarryTTLCostLimitPlacementAndEFA(t *testing.T) {
	s := &Spec{Name: "g0", TTL: "3h", Bucket: "scttfrdmn-lith-bench"}
	r := Role{Name: "client", InstanceType: "g7e.12xlarge", CostLimit: 30}
	a := strings.Join(s.LaunchArgs(r, 0, Candidate{Region: "us-east-2", AZ: "us-east-2c", Spot: true}, "subnet-1", "sg-1", "ami-1", "/tmp/id"), " ")
	for _, want := range []string{"adit-g0-client-0", "--region us-east-2", "--az us-east-2c", "--subnet-id subnet-1", "--security-group-ids sg-1", "--efa", "--ttl 3h", "--cost-limit 30.00", "--spot", "--ami ami-1", "--s3-read scttfrdmn-lith-bench", "--tag adit:fleet=g0"} {
		if !strings.Contains(a, want) {
			t.Errorf("missing %q in %s", want, a)
		}
	}
}

func TestCapacityErrorsAreRecognised(t *testing.T) {
	if !isCapacity("api error InsufficientInstanceCapacity: We currently do not have sufficient g7e.12xlarge capacity") {
		t.Fatal("capacity error not recognised")
	}
	if isCapacity("UnauthorizedOperation") {
		t.Fatal("auth error treated as capacity")
	}
}
