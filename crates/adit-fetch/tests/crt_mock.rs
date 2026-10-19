// SPDX-License-Identifier: Apache-2.0
//! The CRT source against the in-process mock S3: offsets, out-of-order ranges, retries.

use std::sync::Arc;
use std::time::Duration;

use adit_fetch::crt::{CrtConfig, CrtSource, MetaType};
use adit_fetch::mock::MockS3;
use adit_fetch::{FetchError, RangeSource, Retrying, chunks};

fn bytes(n: usize, seed: u64) -> Vec<u8> {
    let mut x = seed | 1;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ranged_get_lands_at_offset_zero_of_destination() {
    let s3 = MockS3::start().await;
    let obj = bytes(3 << 20, 7);
    s3.put("b/k", obj.clone());
    let src = CrtSource::new(&CrtConfig::default()).unwrap();
    let r = (2 << 20) + 5..(3 << 20) - 3;
    let mut dst = vec![0u8; (r.end - r.start) as usize];
    let st = src.get_into(&s3.url("b/k"), r.clone(), &mut dst).await.unwrap();
    assert_eq!(st.bytes, r.end - r.start);
    assert_eq!(&dst[..], &obj[r.start as usize..r.end as usize]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn out_of_order_ranges_each_land_in_place() {
    let s3 = MockS3::start().await;
    let obj = bytes(4 << 20, 11);
    s3.put("b/k", obj.clone());
    // Later ranges answer first.
    s3.delay(|start| Duration::from_millis(200u64.saturating_sub(start >> 14)));
    let src = Arc::new(CrtSource::new(&CrtConfig::default()).unwrap());
    let url = s3.url("b/k");
    let mut dst = vec![0u8; obj.len()];
    let plan = chunks(0..obj.len() as u64, 1 << 20);
    let mut rest: &mut [u8] = &mut dst;
    let mut slices = Vec::new();
    for c in &plan {
        let (head, tail) = rest.split_at_mut(c.len as usize);
        slices.push((c.range(), head));
        rest = tail;
    }
    let futs = slices.into_iter().map(|(r, d)| {
        let (src, url) = (src.clone(), url.clone());
        async move { src.get_into(&url, r, d).await }
    });
    for r in futures::future::join_all(futs).await {
        r.unwrap();
    }
    assert!(dst == obj, "reassembled bytes differ");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn slowdown_is_retried_and_counted() {
    let s3 = MockS3::start().await;
    let obj = bytes(1 << 20, 3);
    s3.put("b/k", obj.clone());
    s3.fail_next(2);
    let src = Retrying::new(CrtSource::new(&CrtConfig::default()).unwrap(), 4, Duration::from_millis(5));
    let mut dst = vec![0u8; obj.len()];
    let st = src.get_into(&s3.url("b/k"), 0..obj.len() as u64, &mut dst).await.unwrap();
    assert!(dst == obj);
    // Two 503s plus the success, whichever layer retried them: the reported count must match
    // what the server saw.
    assert_eq!(s3.requests(), 3);
    assert_eq!(st.requests as u64, s3.requests());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn missing_key_is_not_retried() {
    let s3 = MockS3::start().await;
    let src = Retrying::new(CrtSource::new(&CrtConfig::default()).unwrap(), 4, Duration::from_millis(5));
    let mut dst = vec![0u8; 16];
    let err = src.get_into(&s3.url("b/missing"), 0..16, &mut dst).await.unwrap_err();
    assert!(matches!(err.error, FetchError::Status { status: 404, .. }), "{err:?}");
    assert_eq!(err.requests, 1, "a 404 is one request, not retried");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn get_object_meta_type_lands_at_offset_zero_of_destination() {
    let s3 = MockS3::start().await;
    let obj = bytes(9 << 20, 21);
    s3.put("b/k", obj.clone());
    let cfg = CrtConfig { meta_type: MetaType::GetObject, part_size: Some(1 << 20), ..CrtConfig::default() };
    let src = CrtSource::new(&cfg).unwrap();
    let r = (3 << 20) + 11..(8 << 20) + 3;
    let mut dst = vec![0u8; (r.end - r.start) as usize];
    let st = src.get_into(&s3.url("b/k"), r.clone(), &mut dst).await.unwrap();
    assert_eq!(st.bytes, r.end - r.start);
    assert!(dst == obj[r.start as usize..r.end as usize], "GetObject body landed at the wrong offsets");
    assert!(st.requests >= 1);
}
