// SPDX-License-Identifier: Apache-2.0
//! C ABI over the client. The caller owns the device buffer; adit registers it and fills it.
//!
//! ```c
//! adit_client *c = adit_client_open("{\"agent\":\"gpu0\",\"backend\":\"LIBFABRIC\",
//!     \"proxies\":[\"http://10.0.0.5:8480\"],\"chunk\":8388608,\"stripe\":true,
//!     \"addr\":140000000000,\"len\":1073741824,\"device\":0}", err, sizeof err);
//! int rc = adit_client_get(c, "bucket/key", presigned_url, 0, size, 0, err, sizeof err);
//! adit_client_close(c);
//! ```
//! Return codes: `0` complete; `1` partial (message lists missing ranges); `-1` other failure.

use std::ffi::{CStr, c_char};
use std::sync::Arc;
use std::time::Duration;

use adit_proto::Dest;
use serde::Deserialize;

use crate::client::{Client, Config, GetError};
use crate::nixl::ClientAgent;

#[derive(Deserialize)]
struct OpenConfig {
    agent: String,
    #[serde(default = "libfabric")]
    backend: String,
    proxies: Vec<String>,
    chunk: u64,
    #[serde(default)]
    stripe: bool,
    addr: u64,
    len: u64,
    #[serde(default)]
    device: u64,
    #[serde(default = "grace")]
    notif_grace_ms: u64,
    #[serde(default = "timeout")]
    timeout_s: u64,
}

fn libfabric() -> String {
    "LIBFABRIC".into()
}
fn grace() -> u64 {
    2000
}
fn timeout() -> u64 {
    600
}

pub struct Handle {
    rt: tokio::runtime::Runtime,
    client: Client,
    _agent: Arc<ClientAgent>,
}

fn write_err(buf: *mut c_char, cap: usize, msg: &str) {
    if buf.is_null() || cap == 0 {
        return;
    }
    let n = msg.len().min(cap - 1);
    // SAFETY: caller provides `cap` writable bytes at `buf`.
    unsafe {
        std::ptr::copy_nonoverlapping(msg.as_ptr(), buf as *mut u8, n);
        *buf.add(n) = 0;
    }
}

fn open(cfg: &str) -> Result<Handle, String> {
    let c: OpenConfig = serde_json::from_str(cfg).map_err(|e| e.to_string())?;
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().map_err(|e| e.to_string())?;
    let agent =
        ClientAgent::new(&c.agent, &c.backend, c.addr, c.len as usize, c.device, true).map_err(|e| e.to_string())?;
    let md = agent.metadata().map_err(|e| e.to_string())?;
    let client = rt.block_on(async {
        let client = Client::new(
            Config {
                agent: c.agent.clone(),
                proxies: c.proxies.clone(),
                chunk: c.chunk,
                stripe: c.stripe,
                notif_grace: Duration::from_millis(c.notif_grace_ms),
                timeout: Duration::from_secs(c.timeout_s),
                pump_idle: Duration::from_micros(50),
            },
            Dest { addr: c.addr, len: c.len, dev: c.device },
            agent.clone(),
        )
        .map_err(|e| e.to_string())?;
        client.register(&md).await.map_err(|e| e.to_string())?;
        Ok::<_, String>(client)
    })?;
    Ok(Handle { rt, client, _agent: agent })
}

/// # Safety
/// `cfg` is a NUL-terminated JSON string; `err` has `err_cap` writable bytes (or is null).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adit_client_open(cfg: *const c_char, err: *mut c_char, err_cap: usize) -> *mut Handle {
    // SAFETY: caller contract.
    let cfg = match unsafe { CStr::from_ptr(cfg) }.to_str() {
        Ok(s) => s,
        Err(e) => {
            write_err(err, err_cap, &e.to_string());
            return std::ptr::null_mut();
        }
    };
    match open(cfg) {
        Ok(h) => Box::into_raw(Box::new(h)),
        Err(e) => {
            write_err(err, err_cap, &e);
            std::ptr::null_mut()
        }
    }
}

/// # Safety
/// `h` came from `adit_client_open`; strings are NUL-terminated; `err` as above.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn adit_client_get(
    h: *mut Handle,
    path: *const c_char,
    url: *const c_char,
    start: u64,
    end: u64,
    dest_offset: u64,
    err: *mut c_char,
    err_cap: usize,
) -> i32 {
    // SAFETY: caller contract.
    let (h, path, url) = unsafe { (&*h, CStr::from_ptr(path), CStr::from_ptr(url)) };
    let (Ok(path), Ok(url)) = (path.to_str(), url.to_str()) else {
        write_err(err, err_cap, "non-UTF-8 path or url");
        return -1;
    };
    match h.rt.block_on(h.client.get(path, url, start..end, dest_offset)) {
        Ok(_) => 0,
        Err(e @ GetError::Partial { .. }) => {
            write_err(err, err_cap, &e.to_string());
            1
        }
        Err(e) => {
            write_err(err, err_cap, &e.to_string());
            -1
        }
    }
}

/// # Safety
/// `h` came from `adit_client_open` and is not used again.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn adit_client_close(h: *mut Handle) {
    if !h.is_null() {
        // SAFETY: caller contract.
        drop(unsafe { Box::from_raw(h) });
    }
}
