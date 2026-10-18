// SPDX-License-Identifier: Apache-2.0

// Package fleet launches and retires adit's proxy fleet and GPU clients through
// spawn (spore.host). Instances are only ever launched by spawn; this package
// adds what spawn does not provide for a heterogeneous EFA fleet: one security
// group per fleet that admits all traffic from itself (the EFA requirement),
// shared by proxies and clients of different instance types in one subnet.
//
// GPU capacity is scarce and moves between AZs and regions, so a fleet is placed
// by trying ranked candidates (region, AZ, spot or on-demand) until the client
// lands; proxies then follow it into the same AZ and subnet.
package fleet

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"time"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/service/ec2"
	"github.com/aws/aws-sdk-go-v2/service/ec2/types"
)

// Candidate is one placement to try.
type Candidate struct {
	Region string
	AZ     string // zone name, e.g. us-east-1a
	Spot   bool
}

// ParseCandidates parses "us-east-1a,us-east-2b:spot,..." (region is the AZ
// name minus its letter).
func ParseCandidates(s string) ([]Candidate, error) {
	var out []Candidate
	for _, f := range strings.Split(s, ",") {
		f = strings.TrimSpace(f)
		if f == "" {
			continue
		}
		az, mode, _ := strings.Cut(f, ":")
		if len(az) < 2 {
			return nil, fmt.Errorf("bad candidate %q", f)
		}
		c := Candidate{Region: az[:len(az)-1], AZ: az}
		switch mode {
		case "", "od", "on-demand":
		case "spot":
			c.Spot = true
		default:
			return nil, fmt.Errorf("candidate %q: mode must be spot or on-demand", f)
		}
		out = append(out, c)
	}
	if len(out) == 0 {
		return nil, fmt.Errorf("no candidates")
	}
	return out, nil
}

// Role is one kind of box in the fleet.
type Role struct {
	Name         string // "proxy" or "client"
	InstanceType string
	Count        int
	AMI          string // AMI id, or "" to let the caller resolve per region
	Command      string // spawn --command, optional
	CostLimit    float64
	VolumeGiB    int32
}

// Spec is a whole fleet.
type Spec struct {
	Name       string
	Candidates []Candidate
	TTL        string
	Bucket     string // read/write grant for results and data
	Client     Role
	Proxy      Role
	Profile    string
	// AMIFor resolves an AMI for a role in a region when Role.AMI is empty.
	AMIFor func(ctx context.Context, region string, r Role) (string, error)
	// Spawn is the spawn binary; Log receives every command and its output.
	Spawn string
	Log   io.Writer
	// DryRun prints spawn commands instead of launching.
	DryRun bool
}

// Box is one launched instance.
type Box struct {
	Name, Role, InstanceID, Region, AZ string
	Spot                               bool
}

func (s *Spec) logf(format string, a ...any) {
	if s.Log != nil {
		_, _ = fmt.Fprintf(s.Log, format+"\n", a...)
	}
}

// SGName is the per-fleet security group's name.
func SGName(fleet string) string { return "adit-" + fleet }

// EnsureSG creates (or reuses) the fleet's security group in the region's
// default VPC: all traffic from itself in and out (EFA), all egress, and SSH
// from the operator's address so spawn can connect.
func EnsureSG(ctx context.Context, c *ec2.Client, fleet, sshCIDR string) (sgID, vpcID string, err error) {
	vpcs, err := c.DescribeVpcs(ctx, &ec2.DescribeVpcsInput{Filters: []types.Filter{{Name: aws.String("is-default"), Values: []string{"true"}}}})
	if err != nil || len(vpcs.Vpcs) == 0 {
		return "", "", fmt.Errorf("default VPC: %v", err)
	}
	vpcID = aws.ToString(vpcs.Vpcs[0].VpcId)
	name := SGName(fleet)
	got, err := c.DescribeSecurityGroups(ctx, &ec2.DescribeSecurityGroupsInput{Filters: []types.Filter{
		{Name: aws.String("group-name"), Values: []string{name}},
		{Name: aws.String("vpc-id"), Values: []string{vpcID}},
	}})
	if err != nil {
		return "", "", err
	}
	if len(got.SecurityGroups) > 0 {
		sgID = aws.ToString(got.SecurityGroups[0].GroupId)
	} else {
		out, err := c.CreateSecurityGroup(ctx, &ec2.CreateSecurityGroupInput{
			GroupName:   aws.String(name),
			Description: aws.String("adit fleet " + fleet + ": EFA all-traffic between members"),
			VpcId:       aws.String(vpcID),
			TagSpecifications: []types.TagSpecification{{ResourceType: types.ResourceTypeSecurityGroup, Tags: []types.Tag{
				{Key: aws.String("adit:fleet"), Value: aws.String(fleet)},
			}}},
		})
		if err != nil {
			return "", "", fmt.Errorf("create %s: %w", name, err)
		}
		sgID = aws.ToString(out.GroupId)
	}
	self := []types.IpPermission{{IpProtocol: aws.String("-1"), UserIdGroupPairs: []types.UserIdGroupPair{{GroupId: aws.String(sgID)}}}}
	benign := func(err error) bool {
		return err == nil || strings.Contains(err.Error(), "Duplicate") || strings.Contains(err.Error(), "already exists")
	}
	if _, err := c.AuthorizeSecurityGroupIngress(ctx, &ec2.AuthorizeSecurityGroupIngressInput{GroupId: &sgID, IpPermissions: self}); !benign(err) {
		return "", "", err
	}
	if _, err := c.AuthorizeSecurityGroupEgress(ctx, &ec2.AuthorizeSecurityGroupEgressInput{GroupId: &sgID, IpPermissions: self}); !benign(err) {
		return "", "", err
	}
	if sshCIDR != "" {
		ssh := []types.IpPermission{{IpProtocol: aws.String("tcp"), FromPort: aws.Int32(22), ToPort: aws.Int32(22), IpRanges: []types.IpRange{{CidrIp: aws.String(sshCIDR), Description: aws.String("operator SSH")}}}}
		if _, err := c.AuthorizeSecurityGroupIngress(ctx, &ec2.AuthorizeSecurityGroupIngressInput{GroupId: &sgID, IpPermissions: ssh}); !benign(err) {
			return "", "", err
		}
	}
	return sgID, vpcID, nil
}

// DeleteSG removes the fleet's group; it fails while instances still use it.
func DeleteSG(ctx context.Context, c *ec2.Client, fleet string) error {
	got, err := c.DescribeSecurityGroups(ctx, &ec2.DescribeSecurityGroupsInput{Filters: []types.Filter{{Name: aws.String("group-name"), Values: []string{SGName(fleet)}}}})
	if err != nil {
		return err
	}
	for _, g := range got.SecurityGroups {
		if _, err := c.DeleteSecurityGroup(ctx, &ec2.DeleteSecurityGroupInput{GroupId: g.GroupId}); err != nil {
			return fmt.Errorf("delete %s: %w", aws.ToString(g.GroupId), err)
		}
	}
	return nil
}

// SubnetFor returns the default-for-AZ subnet of vpc in az.
func SubnetFor(ctx context.Context, c *ec2.Client, vpcID, az string) (string, error) {
	out, err := c.DescribeSubnets(ctx, &ec2.DescribeSubnetsInput{Filters: []types.Filter{
		{Name: aws.String("vpc-id"), Values: []string{vpcID}},
		{Name: aws.String("availability-zone"), Values: []string{az}},
		{Name: aws.String("default-for-az"), Values: []string{"true"}},
	}})
	if err != nil {
		return "", err
	}
	if len(out.Subnets) == 0 {
		return "", fmt.Errorf("no default subnet in %s", az)
	}
	return aws.ToString(out.Subnets[0].SubnetId), nil
}

// OperatorCIDR returns this machine's public address as a /32.
func OperatorCIDR(ctx context.Context) (string, error) {
	req, _ := http.NewRequestWithContext(ctx, http.MethodGet, "https://checkip.amazonaws.com", nil)
	r, err := http.DefaultClient.Do(req)
	if err != nil {
		return "", err
	}
	defer func() { _ = r.Body.Close() }()
	b, err := io.ReadAll(r.Body)
	if err != nil {
		return "", err
	}
	return strings.TrimSpace(string(b)) + "/32", nil
}

// LaunchArgs builds the spawn command line for one box. Exposed for tests and
// --dry-run.
func (s *Spec) LaunchArgs(r Role, i int, c Candidate, subnet, sg, ami, idFile string) []string {
	name := fmt.Sprintf("adit-%s-%s-%d", s.Name, r.Name, i)
	a := []string{"launch", name,
		"--region", c.Region, "--az", c.AZ, "--subnet-id", subnet, "--security-group-ids", sg,
		"--instance-type", r.InstanceType, "--efa",
		"--ttl", s.TTL, "--cost-limit", fmt.Sprintf("%.2f", r.CostLimit),
		"--tag", "adit:fleet=" + s.Name, "--tag", "adit:role=" + r.Name,
		"--output-id", idFile, "-y",
	}
	if ami != "" {
		a = append(a, "--ami", ami)
	}
	if s.Bucket != "" {
		a = append(a, "--s3-read", s.Bucket, "--s3-write", s.Bucket)
	}
	if c.Spot {
		a = append(a, "--spot")
	}
	if r.VolumeGiB > 0 {
		a = append(a, "--volume-size", fmt.Sprint(r.VolumeGiB))
	}
	if r.Command != "" {
		a = append(a, "--command", r.Command, "--on-complete", "terminate")
	}
	return a
}

func (s *Spec) spawn(ctx context.Context, region string, args ...string) (string, error) {
	s.logf("$ %s %s", s.Spawn, strings.Join(quote(args), " "))
	if s.DryRun {
		return "", nil
	}
	cmd := exec.CommandContext(ctx, s.Spawn, args...)
	cmd.Env = append(os.Environ(), "AWS_REGION="+region)
	if s.Profile != "" {
		cmd.Env = append(cmd.Env, "AWS_PROFILE="+s.Profile)
	}
	var out bytes.Buffer
	cmd.Stdout, cmd.Stderr = &out, &out
	err := cmd.Run()
	s.logf("%s", strings.TrimSpace(out.String()))
	return out.String(), err
}

func quote(a []string) []string {
	out := make([]string, len(a))
	for i, s := range a {
		if strings.ContainsAny(s, " \"'$;&|") {
			out[i] = "'" + strings.ReplaceAll(s, "'", `'\''`) + "'"
		} else {
			out[i] = s
		}
	}
	return out
}

// Capacity errors mean "try the next candidate"; anything else stops the launch.
func isCapacity(out string) bool {
	for _, s := range []string{"InsufficientInstanceCapacity", "Unsupported", "capacity-not-available", "MaxSpotInstanceCountExceeded", "SpotMaxPriceTooLow", "InsufficientCapacity", "not supported in your requested Availability Zone"} {
		if strings.Contains(out, s) {
			return true
		}
	}
	return false
}

// EC2For returns an EC2 client for a region.
type EC2For func(region string) *ec2.Client

// Up places the fleet: for each candidate, launch the clients there; if they
// land, launch the proxies into the same AZ and subnet. Returns launched boxes;
// on error, boxes already launched are returned so the caller can retire them.
func (s *Spec) Up(ctx context.Context, ec2For EC2For) ([]Box, error) {
	cands, err := s.candidates()
	if err != nil {
		return nil, err
	}
	cidr, err := OperatorCIDR(ctx)
	if err != nil {
		return nil, fmt.Errorf("operator address: %w", err)
	}
	var boxes []Box
	tmp, err := os.MkdirTemp("", "aditctl-fleet-")
	if err != nil {
		return nil, err
	}
	defer func() { _ = os.RemoveAll(tmp) }()
	launch := func(r Role, i int, c Candidate, subnet, sg string) (Box, string, error) {
		ami := r.AMI
		if ami == "" && s.AMIFor != nil {
			if ami, err = s.AMIFor(ctx, c.Region, r); err != nil {
				return Box{}, "", err
			}
		}
		idFile := filepath.Join(tmp, fmt.Sprintf("%s-%d-%s", r.Name, i, c.AZ))
		out, err := s.spawn(ctx, c.Region, s.LaunchArgs(r, i, c, subnet, sg, ami, idFile)...)
		id, _ := os.ReadFile(idFile)
		b := Box{Name: fmt.Sprintf("adit-%s-%s-%d", s.Name, r.Name, i), Role: r.Name, InstanceID: strings.TrimSpace(string(id)), Region: c.Region, AZ: c.AZ, Spot: c.Spot}
		return b, out, err
	}
	for _, c := range cands {
		e := ec2For(c.Region)
		var sg, vpc string
		if s.DryRun {
			// No AWS mutation in a dry run: resolve the VPC, invent the group.
			sg, vpc, err = "sg-DRYRUN", "", nil
			if v, verr := e.DescribeVpcs(ctx, &ec2.DescribeVpcsInput{Filters: []types.Filter{{Name: aws.String("is-default"), Values: []string{"true"}}}}); verr == nil && len(v.Vpcs) > 0 {
				vpc = aws.ToString(v.Vpcs[0].VpcId)
			}
		} else {
			sg, vpc, err = EnsureSG(ctx, e, s.Name, cidr)
		}
		if err != nil {
			return boxes, err
		}
		subnet, err := SubnetFor(ctx, e, vpc, c.AZ)
		if err != nil {
			s.logf("# %s: %v; next candidate", c.AZ, err)
			continue
		}
		var placed []Box
		ok := true
		for i := range s.Client.Count {
			b, out, err := launch(s.Client, i, c, subnet, sg)
			if err != nil {
				if isCapacity(out) {
					s.logf("# %s %s: no capacity; next candidate", c.AZ, s.Client.InstanceType)
					ok = false
					break
				}
				return append(boxes, placed...), fmt.Errorf("launch %s: %w", b.Name, err)
			}
			placed = append(placed, b)
		}
		if !ok {
			if len(placed) > 0 {
				// Partial client placement: retire what landed here and move on.
				_ = s.Down(ctx, placed)
			}
			continue
		}
		boxes = append(boxes, placed...)
		// Proxies follow the clients. They need not share the clients' spot mode.
		pc := Candidate{Region: c.Region, AZ: c.AZ}
		for i := range s.Proxy.Count {
			b, out, err := launch(s.Proxy, i, pc, subnet, sg)
			if err != nil && isCapacity(out) {
				pc.Spot = !pc.Spot
				s.logf("# %s %s: no capacity on-demand; trying spot", c.AZ, s.Proxy.InstanceType)
				b, _, err = launch(s.Proxy, i, pc, subnet, sg)
			}
			if err != nil {
				return boxes, fmt.Errorf("launch %s in %s next to the clients: %w", b.Name, c.AZ, err)
			}
			boxes = append(boxes, b)
		}
		return boxes, nil
	}
	return boxes, fmt.Errorf("no candidate had %s capacity", s.Client.InstanceType)
}

func (s *Spec) candidates() ([]Candidate, error) {
	if len(s.Candidates) == 0 {
		return nil, fmt.Errorf("fleet %s: no placement candidates", s.Name)
	}
	return s.Candidates, nil
}

// Down terminates boxes through spawn.
func (s *Spec) Down(ctx context.Context, boxes []Box) error {
	var errs []string
	for _, b := range boxes {
		id := b.InstanceID
		if id == "" {
			id = b.Name
		}
		if _, err := s.spawn(ctx, b.Region, "terminate", id, "--yes"); err != nil {
			errs = append(errs, fmt.Sprintf("%s: %v", b.Name, err))
		}
	}
	if len(errs) > 0 {
		return fmt.Errorf("terminate: %s", strings.Join(errs, "; "))
	}
	return nil
}

// Listed is one row of `spawn list --output json`.
type Listed struct {
	InstanceID   string `json:"instance_id"`
	Name         string `json:"name"`
	InstanceType string `json:"instance_type"`
	State        string `json:"state"`
	Region       string `json:"region"`
	AZ           string `json:"availability_zone"`
	PrivateIP    string `json:"private_ip"`
	PublicIP     string `json:"public_ip"`
	LaunchTime   string `json:"launch_time"`
	TTL          string `json:"ttl"`
}

// List returns spawn-managed instances whose name starts with "adit-<fleet>-"
// (all adit instances if fleet is empty), in any state but terminated.
func List(ctx context.Context, spawn, profile, fleet string) ([]Listed, error) {
	cmd := exec.CommandContext(ctx, spawn, "list", "--output", "json")
	if profile != "" {
		cmd.Env = append(os.Environ(), "AWS_PROFILE="+profile)
	}
	var out, errb bytes.Buffer
	cmd.Stdout, cmd.Stderr = &out, &errb
	if err := cmd.Run(); err != nil {
		return nil, fmt.Errorf("spawn list: %v: %s", err, errb.String())
	}
	// spawn may print progress before the JSON array.
	b := out.Bytes()
	if i := bytes.IndexByte(b, '['); i >= 0 {
		b = b[i:]
	}
	var all []Listed
	if err := json.Unmarshal(b, &all); err != nil {
		return nil, fmt.Errorf("spawn list output: %w", err)
	}
	prefix := "adit-"
	if fleet != "" {
		prefix = "adit-" + fleet + "-"
	}
	var mine []Listed
	for _, l := range all {
		if strings.HasPrefix(l.Name, prefix) && l.State != "terminated" {
			mine = append(mine, l)
		}
	}
	return mine, nil
}

// WaitGone polls until no instance of the fleet remains or the deadline passes.
func WaitGone(ctx context.Context, spawn, profile, fleet string, every time.Duration, deadline time.Time) ([]Listed, error) {
	for {
		left, err := List(ctx, spawn, profile, fleet)
		if err != nil || len(left) == 0 || time.Now().After(deadline) {
			return left, err
		}
		time.Sleep(every)
	}
}
