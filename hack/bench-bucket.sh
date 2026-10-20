#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Create a regional bench bucket configured like scttfrdmn-adit-bench-use2: 7-day expiry, abort
# incomplete uploads after a day, public access blocked, SSE-S3, tags, and a policy granting the
# spawn/spored instance roles object read/write. Bucket creation is Scott's decision (CLAUDE.md);
# run this only for regions he approved (#48).
#
#   AWS_PROFILE=aws bash hack/bench-bucket.sh <region> <short>   # -> scttfrdmn-adit-bench-<short>
set +e
set -uo pipefail
echo "opts=$-"
R=${1:?region} S=${2:?short name}
B=scttfrdmn-adit-bench-$S
ACCT=$(aws sts get-caller-identity --query Account --output text) || exit 1
ROLES="\"arn:aws:iam::$ACCT:role/spawn-instance-010d8a6d\",\"arn:aws:iam::$ACCT:role/spored-instance-role\""
if aws s3api head-bucket --bucket "$B" --region "$R" 2>/dev/null; then
    echo "$B exists"
else
    loc=(); [[ $R != us-east-1 ]] && loc=(--create-bucket-configuration "LocationConstraint=$R")
    aws s3api create-bucket --bucket "$B" --region "$R" "${loc[@]}" >/dev/null || { echo "create $B failed"; exit 1; }
fi
aws s3api put-public-access-block --bucket "$B" --region "$R" --public-access-block-configuration \
    BlockPublicAcls=true,IgnorePublicAcls=true,BlockPublicPolicy=true,RestrictPublicBuckets=true || exit 1
aws s3api put-bucket-encryption --bucket "$B" --region "$R" --server-side-encryption-configuration \
    '{"Rules":[{"ApplyServerSideEncryptionByDefault":{"SSEAlgorithm":"AES256"}}]}' || exit 1
aws s3api put-bucket-lifecycle-configuration --bucket "$B" --region "$R" --lifecycle-configuration \
    '{"Rules":[{"ID":"expire-7d","Status":"Enabled","Filter":{},"Expiration":{"Days":7},"AbortIncompleteMultipartUpload":{"DaysAfterInitiation":1}}]}' || exit 1
aws s3api put-bucket-tagging --bucket "$B" --region "$R" --tagging \
    'TagSet=[{Key=adit:purpose,Value=bench},{Key=expiry,Value=7d}]' || exit 1
aws s3api put-bucket-policy --bucket "$B" --region "$R" --policy "{\"Version\":\"2012-10-17\",\"Statement\":[
 {\"Sid\":\"AditBenchBoxes\",\"Effect\":\"Allow\",\"Principal\":{\"AWS\":[$ROLES]},\"Action\":[\"s3:GetObject\",\"s3:PutObject\",\"s3:AbortMultipartUpload\",\"s3:ListMultipartUploadParts\",\"s3:GetObjectAttributes\"],\"Resource\":\"arn:aws:s3:::$B/*\"},
 {\"Sid\":\"AditBenchList\",\"Effect\":\"Allow\",\"Principal\":{\"AWS\":[$ROLES]},\"Action\":[\"s3:ListBucket\",\"s3:GetBucketLocation\"],\"Resource\":\"arn:aws:s3:::$B\"}]}" || exit 1
echo "ok: $B in $(aws s3api get-bucket-location --bucket "$B" --query LocationConstraint --output text)"
