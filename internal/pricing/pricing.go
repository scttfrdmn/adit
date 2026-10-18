// SPDX-License-Identifier: Apache-2.0

// Package pricing looks up instance prices for the dollars column: on-demand
// from the Price List API, spot from the region's spot price history.
package pricing

import (
	"context"
	"encoding/json"
	"fmt"
	"strconv"
	"time"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/service/ec2"
	ec2types "github.com/aws/aws-sdk-go-v2/service/ec2/types"
	"github.com/aws/aws-sdk-go-v2/service/pricing"
	"github.com/aws/aws-sdk-go-v2/service/pricing/types"
)

// OnDemand returns the Linux, shared-tenancy on-demand $/hr of instanceType in
// region. The Price List API itself is served from us-east-1.
func OnDemand(ctx context.Context, c *pricing.Client, instanceType, region string) (float64, error) {
	f := func(k, v string) types.Filter {
		return types.Filter{Type: types.FilterTypeTermMatch, Field: aws.String(k), Value: aws.String(v)}
	}
	out, err := c.GetProducts(ctx, &pricing.GetProductsInput{
		ServiceCode: aws.String("AmazonEC2"),
		Filters: []types.Filter{
			f("instanceType", instanceType), f("regionCode", region), f("operatingSystem", "Linux"),
			f("tenancy", "Shared"), f("preInstalledSw", "NA"), f("capacitystatus", "Used"),
		},
		MaxResults: aws.Int32(10),
	})
	if err != nil {
		return 0, err
	}
	for _, p := range out.PriceList {
		if v, ok := parseOnDemand(p); ok {
			return v, nil
		}
	}
	return 0, fmt.Errorf("no on-demand price for %s in %s", instanceType, region)
}

func parseOnDemand(doc string) (float64, bool) {
	var p struct {
		Terms struct {
			OnDemand map[string]struct {
				PriceDimensions map[string]struct {
					Unit         string            `json:"unit"`
					PricePerUnit map[string]string `json:"pricePerUnit"`
				} `json:"priceDimensions"`
			} `json:"OnDemand"`
		} `json:"terms"`
	}
	if json.Unmarshal([]byte(doc), &p) != nil {
		return 0, false
	}
	for _, t := range p.Terms.OnDemand {
		for _, d := range t.PriceDimensions {
			if d.Unit != "Hrs" {
				continue
			}
			if v, err := strconv.ParseFloat(d.PricePerUnit["USD"], 64); err == nil && v > 0 {
				return v, true
			}
		}
	}
	return 0, false
}

// Spot returns the most recent spot $/hr of instanceType in az.
func Spot(ctx context.Context, c *ec2.Client, instanceType, az string) (float64, error) {
	out, err := c.DescribeSpotPriceHistory(ctx, &ec2.DescribeSpotPriceHistoryInput{
		InstanceTypes:       []ec2types.InstanceType{ec2types.InstanceType(instanceType)},
		AvailabilityZone:    aws.String(az),
		ProductDescriptions: []string{"Linux/UNIX"},
		StartTime:           aws.Time(time.Now()),
	})
	if err != nil {
		return 0, err
	}
	for _, h := range out.SpotPriceHistory {
		if v, err := strconv.ParseFloat(aws.ToString(h.SpotPrice), 64); err == nil {
			return v, nil
		}
	}
	return 0, fmt.Errorf("no spot price for %s in %s", instanceType, az)
}
