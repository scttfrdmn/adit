// SPDX-License-Identifier: Apache-2.0

// Command aditctl is adit's operator tool: workload generation, presigning, the
// region check, prices, fleet placement through spawn, the bench and probe
// runners, and the results schema check.
package main

import (
	"bufio"
	"context"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"os"
	"os/exec"
	"strings"
	"sync"
	"time"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/config"
	"github.com/aws/aws-sdk-go-v2/feature/ec2/imds"
	"github.com/aws/aws-sdk-go-v2/service/ec2"
	"github.com/aws/aws-sdk-go-v2/service/pricing"
	"github.com/aws/aws-sdk-go-v2/service/s3"
	"github.com/aws/aws-sdk-go-v2/service/ssm"

	"github.com/scttfrdmn/adit/internal/fleet"
	"github.com/scttfrdmn/adit/internal/gen"
	"github.com/scttfrdmn/adit/internal/manifest"
	pr "github.com/scttfrdmn/adit/internal/pricing"
	"github.com/scttfrdmn/adit/internal/s3x"
	"github.com/scttfrdmn/adit/internal/schema"
)

const usage = `aditctl <command> [flags]

  gen           generate a toy workload into the bench bucket; writes its manifest
  presign       turn a manifest into a plan (manifest lines + presigned URLs)
  region-check  fail unless the bucket is in this region
  price         $/hr for an instance type (on-demand, or spot in an AZ)
  fleet         up | down | status: place proxies and clients through spawn
  teardown      fail unless no adit instance remains (verified via spawn list)
  bench         run bench/paths/<P>/run.sh for a workload
  probe         run hack/probe.sh (G0b capability matrix) on a box
  report        --check: validate bench/results against bench/schema and the laws
`

func main() {
	if len(os.Args) < 2 {
		fmt.Fprint(os.Stderr, usage)
		os.Exit(2)
	}
	ctx := context.Background()
	var err error
	switch os.Args[1] {
	case "gen":
		err = cmdGen(ctx, os.Args[2:])
	case "presign":
		err = cmdPresign(ctx, os.Args[2:])
	case "region-check":
		err = cmdRegionCheck(ctx, os.Args[2:])
	case "price":
		err = cmdPrice(ctx, os.Args[2:])
	case "fleet":
		err = cmdFleet(ctx, os.Args[2:])
	case "teardown":
		err = cmdTeardown(ctx, os.Args[2:])
	case "bench":
		err = cmdScript(os.Args[2:], "bench")
	case "probe":
		err = cmdScript(os.Args[2:], "probe")
	case "report":
		err = cmdReport(os.Args[2:])
	case "-h", "--help", "help":
		fmt.Print(usage)
	default:
		err = fmt.Errorf("unknown command %q\n%s", os.Args[1], usage)
	}
	if err != nil {
		fmt.Fprintln(os.Stderr, "aditctl:", err)
		os.Exit(1)
	}
}

func awsConfig(ctx context.Context, region string) (aws.Config, error) {
	return config.LoadDefaultConfig(ctx, config.WithRegion(region))
}

// hereRegion is the instance's region from IMDS, or "" off EC2.
func hereRegion(ctx context.Context) string {
	cfg, err := config.LoadDefaultConfig(ctx)
	if err != nil {
		return ""
	}
	c, cancel := context.WithTimeout(ctx, 2*time.Second)
	defer cancel()
	out, err := imds.NewFromConfig(cfg).GetRegion(c, &imds.GetRegionInput{})
	if err != nil {
		return ""
	}
	return out.Region
}

func regionFlag(fs *flag.FlagSet) *string {
	return fs.String("region", "us-east-1", "AWS region (must equal the bucket's for any S3 I/O)")
}

func cmdGen(ctx context.Context, args []string) error {
	fs := flag.NewFlagSet("gen", flag.ExitOnError)
	wl := fs.String("workload", "T1", "T1 | T2 | T3")
	objects := fs.Int("objects", 0, "generate only the first N objects (0 = all)")
	bucket := fs.String("bucket", "scttfrdmn-lith-bench", "bench bucket")
	prefix := fs.String("prefix", "adit", "key prefix")
	seed := fs.Uint64("seed", 1, "generator seed (recorded in the manifest)")
	partMiB := fs.Int64("part-mib", 8, "multipart part size = oracle part size")
	parallel := fs.Int("parallel", 32, "parts in flight per object")
	objPar := fs.Int("objects-parallel", 4, "objects in flight")
	out := fs.String("out", "", "manifest JSONL to write (default bench/workloads/<W>/manifest.jsonl)")
	region := regionFlag(fs)
	_ = fs.Parse(args)
	w, ok := gen.Toy[*wl]
	if !ok {
		return fmt.Errorf("unknown workload %q", *wl)
	}
	n := w.Objects
	if *objects > 0 && *objects < n {
		n = *objects
	}
	cfg, err := awsConfig(ctx, *region)
	if err != nil {
		return err
	}
	c := s3.NewFromConfig(cfg)
	if err := s3x.CheckRegion(ctx, c, *bucket, *region); err != nil {
		return err
	}
	if *out == "" {
		*out = fmt.Sprintf("bench/workloads/%s/manifest.jsonl", w.ID)
	}
	f, err := os.Create(*out)
	if err != nil {
		return err
	}
	defer func() { _ = f.Close() }()
	var mu sync.Mutex
	ms := make([]*manifest.Object, n)
	sem := make(chan struct{}, *objPar)
	var wg sync.WaitGroup
	var firstErr error
	start := time.Now()
	for i := range n {
		sem <- struct{}{}
		wg.Add(1)
		go func(i int) {
			defer wg.Done()
			defer func() { <-sem }()
			m, err := gen.Upload(ctx, c, *bucket, gen.Key(*prefix, w, i), w.Size, *partMiB<<20, *seed, i, *parallel)
			mu.Lock()
			defer mu.Unlock()
			if err != nil && firstErr == nil {
				firstErr = err
			}
			ms[i] = &m
		}(i)
	}
	wg.Wait()
	if firstErr != nil {
		return firstErr
	}
	enc := json.NewEncoder(f)
	for _, m := range ms {
		if err := enc.Encode(m); err != nil {
			return err
		}
	}
	gib := float64(int64(n)*w.Size) / (1 << 30)
	fmt.Fprintf(os.Stderr, "generated %s: %d objects, %.1f GiB in %s; S3 composite verified for every object; manifest %s\n", w.ID, n, gib, time.Since(start).Round(time.Second), *out)
	return nil
}

func cmdPresign(ctx context.Context, args []string) error {
	fs := flag.NewFlagSet("presign", flag.ExitOnError)
	in := fs.String("manifest", "", "manifest JSONL")
	ttl := fs.Duration("ttl", 12*time.Hour, "URL lifetime")
	out := fs.String("out", "-", "plan JSONL (- for stdout)")
	region := regionFlag(fs)
	_ = fs.Parse(args)
	ms, err := manifest.Read(*in)
	if err != nil {
		return err
	}
	cfg, err := awsConfig(ctx, *region)
	if err != nil {
		return err
	}
	c := s3.NewFromConfig(cfg)
	checked := map[string]bool{}
	w := bufio.NewWriter(os.Stdout)
	if *out != "-" {
		f, err := os.Create(*out)
		if err != nil {
			return err
		}
		defer func() { _ = f.Close() }()
		w = bufio.NewWriter(f)
	}
	defer func() { _ = w.Flush() }()
	enc := json.NewEncoder(w)
	for _, m := range ms {
		if !checked[m.Bucket] {
			if err := s3x.CheckRegion(ctx, c, m.Bucket, *region); err != nil {
				return err
			}
			checked[m.Bucket] = true
		}
		u, err := s3x.Presign(ctx, c, m.Bucket, m.Key, *ttl)
		if err != nil {
			return err
		}
		if err := enc.Encode(manifest.PlanEntry{Object: m, URL: u}); err != nil {
			return err
		}
	}
	return nil
}

func cmdRegionCheck(ctx context.Context, args []string) error {
	fs := flag.NewFlagSet("region-check", flag.ExitOnError)
	bucket := fs.String("bucket", "scttfrdmn-lith-bench", "bucket")
	region := fs.String("region", "", "expected region (default: this instance's, from IMDS)")
	_ = fs.Parse(args)
	if *region == "" {
		*region = hereRegion(ctx)
		if *region == "" {
			return errors.New("not on EC2 and no --region given")
		}
	}
	cfg, err := awsConfig(ctx, *region)
	if err != nil {
		return err
	}
	if err := s3x.CheckRegion(ctx, s3.NewFromConfig(cfg), *bucket, *region); err != nil {
		return err
	}
	fmt.Printf("ok: s3://%s is in %s\n", *bucket, *region)
	return nil
}

func cmdPrice(ctx context.Context, args []string) error {
	fs := flag.NewFlagSet("price", flag.ExitOnError)
	it := fs.String("instance-type", "", "instance type")
	az := fs.String("az", "", "AZ for a spot price")
	spot := fs.Bool("spot", false, "spot price in --az")
	region := regionFlag(fs)
	_ = fs.Parse(args)
	if *spot {
		cfg, err := awsConfig(ctx, *region)
		if err != nil {
			return err
		}
		v, err := pr.Spot(ctx, ec2.NewFromConfig(cfg), *it, *az)
		if err != nil {
			return err
		}
		fmt.Printf("%s %s spot %.4f\n", *it, *az, v)
		return nil
	}
	cfg, err := awsConfig(ctx, "us-east-1")
	if err != nil {
		return err
	}
	v, err := pr.OnDemand(ctx, pricing.NewFromConfig(cfg), *it, *region)
	if err != nil {
		return err
	}
	fmt.Printf("%s %s on-demand %.4f\n", *it, *region, v)
	return nil
}

// amiFor resolves base AMIs from public SSM parameters: the DLAMI base (NVIDIA
// driver, CUDA, EFA) for GPU clients, stock Ubuntu 24.04 arm64 for proxies.
func amiFor(ctx context.Context, region string, r fleet.Role) (string, error) {
	param := "/aws/service/canonical/ubuntu/server/24.04/stable/current/arm64/hvm/ebs-gp3/ami-id"
	if r.Name == "client" {
		param = "/aws/service/deeplearning/ami/x86_64/base-oss-nvidia-driver-gpu-ubuntu-24.04/latest/ami-id"
	}
	cfg, err := awsConfig(ctx, region)
	if err != nil {
		return "", err
	}
	out, err := ssm.NewFromConfig(cfg).GetParameter(ctx, &ssm.GetParameterInput{Name: aws.String(param)})
	if err != nil {
		return "", fmt.Errorf("%s in %s: %w", param, region, err)
	}
	return aws.ToString(out.Parameter.Value), nil
}

func cmdFleet(ctx context.Context, args []string) error {
	if len(args) == 0 {
		return errors.New("fleet up | down | status")
	}
	fs := flag.NewFlagSet("fleet "+args[0], flag.ExitOnError)
	name := fs.String("name", "", "fleet name (instances are adit-<name>-<role>-<i>)")
	cands := fs.String("candidates", "us-east-1a,us-east-1c,us-east-1a:spot,us-east-1c:spot", "ranked AZs to try, AZ[:spot], comma-separated")
	clientType := fs.String("client-type", "g7e.12xlarge", "GPU client instance type")
	clients := fs.Int("clients", 1, "GPU clients")
	proxyType := fs.String("proxy-type", "c8gn.16xlarge", "proxy instance type")
	proxies := fs.Int("proxies", 1, "proxies")
	ttl := fs.String("ttl", "2h", "TTL per box")
	clientCap := fs.Float64("client-cost-limit", 0, "cost limit per client box, USD (hard kill; required)")
	proxyCap := fs.Float64("proxy-cost-limit", 0, "cost limit per proxy box, USD (hard kill; required)")
	clientAMI := fs.String("client-ami", "", "client AMI (default: DLAMI base via SSM)")
	proxyAMI := fs.String("proxy-ami", "", "proxy AMI (default: Ubuntu 24.04 arm64 via SSM)")
	clientCmd := fs.String("client-command", "", "spawn --command for clients")
	proxyCmd := fs.String("proxy-command", "", "spawn --command for proxies")
	bucket := fs.String("bucket", "scttfrdmn-lith-bench", "bucket granted read/write")
	profile := fs.String("profile", os.Getenv("AWS_PROFILE"), "AWS profile passed to spawn")
	dry := fs.Bool("dry-run", false, "print spawn commands only")
	spawnBin := fs.String("spawn", envOr("ADIT_SPAWN", "spawn"), "spawn binary (env ADIT_SPAWN)")
	clusterPG := fs.Bool("cluster-pg", false, "place every box in one cluster placement group per AZ")
	proxyEFA := fs.Int("proxy-efa-interfaces", 0, "spawn --efa-interfaces for proxies (0 = all; 1 needs no Elastic IP)")
	proxyStart := fs.Int("proxy-start", 0, "first proxy index (add proxies to a placed fleet with --clients 0 and --candidates its AZ)")
	proxySpot := fs.Bool("proxy-spot", false, "launch proxies on spot first, on-demand when spot has no capacity or quota (size --proxy-cost-limit for on-demand)")
	_ = fs.Parse(args[1:])
	if *name == "" {
		return errors.New("--name is required")
	}
	switch args[0] {
	case "status":
		ls, err := fleet.List(ctx, *spawnBin, *profile, *name)
		if err != nil {
			return err
		}
		for _, l := range ls {
			fmt.Printf("%s\t%s\t%s\t%s\t%s\tprivate=%s\tttl=%s\n", l.Name, l.InstanceID, l.InstanceType, l.State, l.AZ, l.PrivateIP, l.TTL)
		}
		fmt.Printf("%d instance(s)\n", len(ls))
		return nil
	case "up":
		if *clientCap <= 0 || *proxyCap <= 0 {
			return errors.New("--client-cost-limit and --proxy-cost-limit are required (every run has a cost limit)")
		}
		cs, err := fleet.ParseCandidates(*cands)
		if err != nil {
			return err
		}
		s := &fleet.Spec{
			Name: *name, Candidates: cs, ClusterPG: *clusterPG, TTL: *ttl, Bucket: *bucket, Profile: *profile,
			Client: fleet.Role{Name: "client", InstanceType: *clientType, Count: *clients, AMI: *clientAMI, Command: *clientCmd, CostLimit: *clientCap, VolumeGiB: 200},
			Proxy:  fleet.Role{Name: "proxy", InstanceType: *proxyType, Count: *proxies, AMI: *proxyAMI, Command: *proxyCmd, CostLimit: *proxyCap, VolumeGiB: 100, EFAInterfaces: *proxyEFA},
			AMIFor: amiFor, Spawn: *spawnBin, Log: os.Stderr, DryRun: *dry, ProxySpot: *proxySpot, ProxyStart: *proxyStart,
		}
		ec2For := func(region string) *ec2.Client {
			cfg, _ := awsConfig(ctx, region)
			return ec2.NewFromConfig(cfg)
		}
		boxes, err := s.Up(ctx, ec2For)
		for _, b := range boxes {
			fmt.Printf("%s\t%s\t%s\t%s\tspot=%v\n", b.Name, b.Role, b.InstanceID, b.AZ, b.Spot)
		}
		if err != nil || *dry {
			return err
		}
		// Never trust the flag: an EFA fleet whose boxes have no EFA device is not
		// a fleet (spawn#1112). Check the attached interface type of every box.
		return fleet.VerifyEFA(ctx, ec2For, boxes, map[string]int{"proxy": *proxyEFA})
	case "down":
		ls, err := fleet.List(ctx, *spawnBin, *profile, *name)
		if err != nil {
			return err
		}
		s := &fleet.Spec{Name: *name, Spawn: *spawnBin, Profile: *profile, Log: os.Stderr, DryRun: *dry}
		var boxes []fleet.Box
		regions := map[string]bool{}
		for _, l := range ls {
			boxes = append(boxes, fleet.Box{Name: l.Name, InstanceID: l.InstanceID, Region: l.Region})
			regions[l.Region] = true
		}
		if err := s.Down(ctx, boxes); err != nil {
			return err
		}
		left, err := fleet.WaitGone(ctx, *spawnBin, *profile, *name, 15*time.Second, time.Now().Add(10*time.Minute))
		if err != nil {
			return err
		}
		if len(left) > 0 {
			return fmt.Errorf("%d instance(s) still not terminated after 10 min", len(left))
		}
		for _, c := range strings.Split(*cands, ",") {
			if cs, err := fleet.ParseCandidates(c); err == nil {
				regions[cs[0].Region] = true
			}
		}
		for r := range regions {
			cfg, _ := awsConfig(ctx, r)
			e := ec2.NewFromConfig(cfg)
			// spawn list drops shutting-down instances; wait on EC2 itself.
			for deadline := time.Now().Add(20 * time.Minute); ; {
				live, err := fleet.Live(ctx, e, *name)
				if err != nil {
					return err
				}
				if len(live) == 0 {
					break
				}
				if time.Now().After(deadline) {
					return fmt.Errorf("%s: still live after 20 min: %v", r, live)
				}
				time.Sleep(15 * time.Second)
			}
			if err := fleet.DeleteSG(ctx, e, *name); err != nil {
				fmt.Fprintf(os.Stderr, "warning: %s: %v\n", r, err)
			}
			if err := fleet.DeletePGs(ctx, e, *name); err != nil {
				fmt.Fprintf(os.Stderr, "warning: %s: %v\n", r, err)
			}
		}
		fmt.Printf("fleet %s: terminated, verified via spawn list and EC2\n", *name)
		return nil
	}
	return fmt.Errorf("fleet %s: unknown", args[0])
}

func envOr(k, def string) string {
	if v := os.Getenv(k); v != "" {
		return v
	}
	return def
}

func cmdTeardown(ctx context.Context, args []string) error {
	fs := flag.NewFlagSet("teardown", flag.ExitOnError)
	profile := fs.String("profile", os.Getenv("AWS_PROFILE"), "AWS profile")
	regions := fs.String("regions", "us-east-1,us-east-2,us-west-2,us-west-1,eu-north-1,ap-south-1,ap-northeast-1,ap-northeast-2", "regions to check on EC2 directly")
	noEIP := fs.Bool("no-eip-cleanup", false, "skip releasing orphaned spawn Elastic IPs (spawn cleanup --elastic-ips)")
	_ = fs.Parse(args)
	ls, err := fleet.List(ctx, envOr("ADIT_SPAWN", "spawn"), *profile, "")
	if err != nil {
		return err
	}
	n := 0
	for _, l := range ls {
		fmt.Printf("REMAINING (spawn) %s %s %s %s %s\n", l.Name, l.InstanceID, l.InstanceType, l.State, l.Region)
		n++
	}
	// spawn list omits shutting-down instances; EC2 is the authority.
	for _, r := range strings.Split(*regions, ",") {
		cfg, err := awsConfig(ctx, r)
		if err != nil {
			return err
		}
		live, err := fleet.Live(ctx, ec2.NewFromConfig(cfg), "")
		if err != nil {
			return fmt.Errorf("%s: %w", r, err)
		}
		for _, l := range live {
			fmt.Printf("REMAINING (ec2 %s) %s\n", r, l)
			n++
		}
		if *noEIP {
			continue
		}
		// A spot reclaim leaves the launch Elastic IP of a multi-interface box allocated (spawn#1232):
		// release orphans, then verify on EC2 that none remain.
		if out, err := fleet.CleanupEIPs(ctx, envOr("ADIT_SPAWN", "spawn"), *profile, r); err != nil {
			fmt.Printf("eip cleanup %s: %v: %s\n", r, err, out)
		}
		orphans, err := fleet.OrphanEIPs(ctx, ec2.NewFromConfig(cfg))
		if err != nil {
			return fmt.Errorf("%s addresses: %w", r, err)
		}
		for _, o := range orphans {
			fmt.Printf("REMAINING (eip %s) %s\n", r, o)
			n++
		}
	}
	if n > 0 {
		return fmt.Errorf("%d adit instance(s) or orphaned Elastic IP(s) remain", n)
	}
	fmt.Printf("teardown verified: no adit instances in spawn list or EC2, no orphaned spawn Elastic IPs (%s)\n", *regions)
	return nil
}

func cmdScript(args []string, kind string) error {
	fs := flag.NewFlagSet(kind, flag.ExitOnError)
	path := fs.String("path", "E", "path A-E (bench)")
	wl := fs.String("workload", "T1", "workload (bench)")
	_ = fs.Parse(args)
	script, sargs := "hack/probe.sh", fs.Args()
	if kind == "bench" {
		script = fmt.Sprintf("bench/paths/%s/run.sh", *path)
		sargs = append([]string{*wl}, sargs...)
	}
	cmd := exec.Command("bash", append([]string{script}, sargs...)...)
	cmd.Stdout, cmd.Stderr, cmd.Stdin = os.Stdout, os.Stderr, os.Stdin
	return cmd.Run()
}

func cmdReport(args []string) error {
	fs := flag.NewFlagSet("report", flag.ExitOnError)
	check := fs.Bool("check", false, "validate CSVs against bench/schema and the laws")
	schemaDir := fs.String("schema", "bench/schema", "schema directory")
	_ = fs.Parse(args)
	dir := "bench/results"
	if fs.NArg() > 0 {
		dir = fs.Arg(0)
	}
	if !*check {
		return errors.New("only --check is implemented; the comparison table comes with G3 data")
	}
	schemas, err := schema.Load(*schemaDir)
	if err != nil {
		return err
	}
	probs, n, err := schema.CheckDir(dir, schemas)
	if err != nil {
		return err
	}
	for _, p := range probs {
		fmt.Println(p)
	}
	if len(probs) > 0 {
		return fmt.Errorf("%d problem(s) in %d file(s)", len(probs), n)
	}
	fmt.Printf("ok: %d results file(s) match bench/schema\n", n)
	return nil
}
