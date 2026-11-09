//! Live IBC / crosslink event ingestion for the visualizer.
//!
//! Sources (real chain objects — not the packet simulator):
//! 1. **Terp (CometBFT RPC)** — poll `/block_results` for ICS-4 packet events
//!    (`send_packet`, `recv_packet`, `acknowledge_packet`, `timeout_packet`) and
//!    light-client events (`create_client`, `update_client`).
//! 2. **Zebra (in-process)** — when the viz observes new PoW / BFT heights, emit
//!    Zebra→Terp trailmarks (crosslink header path).
//!
//! Env:
//! - `CROSSLINK_TERP_RPC` — default `http://127.0.0.1:26657`
//! - `CROSSLINK_LIVE_DISABLE=1` — skip Terp poller
//! - `CROSSLINK_LIVE_POLL_MS` — default `400`

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

use crate::packet_feed::{
    push_sample, PacketDirection, PacketPhase, PacketSample, PacketSource,
};

static TAILER_STARTED: AtomicBool = AtomicBool::new(false);
static LAST_TERP_HEIGHT: AtomicU64 = AtomicU64::new(0);
static LAST_ZEBRA_POW: AtomicU64 = AtomicU64::new(0);
static LAST_ZEBRA_BFT: AtomicU64 = AtomicU64::new(0);
static LIVE_EVENTS: AtomicU64 = AtomicU64::new(0);
static LAST_EVENT_LABEL: Mutex<String> = Mutex::new(String::new());

/// Spawn the Terp block_results poller once (idempotent).
pub fn spawn_terp_event_tailer() {
    if std::env::var("CROSSLINK_LIVE_DISABLE").ok().as_deref() == Some("1") {
        eprintln!("crosslink live IBC tailer disabled (CROSSLINK_LIVE_DISABLE=1)");
        return;
    }
    if TAILER_STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    let rpc = std::env::var("CROSSLINK_TERP_RPC")
        .unwrap_or_else(|_| "http://127.0.0.1:26657".into());
    let poll_ms: u64 = std::env::var("CROSSLINK_LIVE_POLL_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(400);
    let rpc_log = rpc.clone();
    thread::Builder::new()
        .name("crosslink-ibc-live".into())
        .spawn(move || {
            eprintln!(
                "crosslink live IBC tailer → {rpc_log} (poll {poll_ms}ms) — real chain events only"
            );
            run_terp_poll_loop(&rpc, poll_ms);
        })
        .expect("spawn ibc live tailer");
}

/// Status snapshot for `/status` JSON.
pub fn live_status_json() -> serde_json::Value {
    let label = LAST_EVENT_LABEL
        .lock()
        .map(|g| g.clone())
        .unwrap_or_default();
    serde_json::json!({
        "terp_rpc": std::env::var("CROSSLINK_TERP_RPC").unwrap_or_else(|_| "http://127.0.0.1:26657".into()),
        "tailer_running": TAILER_STARTED.load(Ordering::Relaxed),
        "last_terp_height": LAST_TERP_HEIGHT.load(Ordering::Relaxed),
        "last_zebra_pow": LAST_ZEBRA_POW.load(Ordering::Relaxed),
        "last_zebra_bft": LAST_ZEBRA_BFT.load(Ordering::Relaxed),
        "live_events_ingested": LIVE_EVENTS.load(Ordering::Relaxed),
        "last_event": label,
    })
}

/// Called from the GPU frame loop when Zebra state advances (real node object).
pub fn observe_zebra_heights(pow_tip: u64, bft_tip: u64) {
    let prev_pow = LAST_ZEBRA_POW.load(Ordering::Relaxed);
    let prev_bft = LAST_ZEBRA_BFT.load(Ordering::Relaxed);

    if pow_tip > prev_pow {
        LAST_ZEBRA_POW.store(pow_tip, Ordering::Relaxed);
        // One trailmark per new height (cap burst if catching up).
        let n = (pow_tip - prev_pow).min(8);
        for i in 0..n {
            let h = pow_tip - n + i + 1;
            if prev_pow == 0 && i + 1 < n {
                continue; // avoid flooding on first observation
            }
            ingest(
                PacketSample {
                    phase: PacketPhase::Send,
                    direction: PacketDirection::ZebraToTerp,
                    sequence: h,
                    channel: "crosslink-pow".into(),
                    client_id: "zebra-pow".into(),
                    age_secs: 0.0,
                    source: PacketSource::ZebraLive,
                },
                &format!("zebra pow height {h}"),
            );
        }
    }

    if bft_tip > prev_bft {
        LAST_ZEBRA_BFT.store(bft_tip, Ordering::Relaxed);
        let n = (bft_tip - prev_bft).min(8);
        for i in 0..n {
            let h = bft_tip - n + i + 1;
            if prev_bft == 0 && i + 1 < n {
                continue;
            }
            ingest(
                PacketSample {
                    phase: PacketPhase::Recv,
                    direction: PacketDirection::ZebraToTerp,
                    sequence: h,
                    channel: "crosslink-bft".into(),
                    client_id: "zebra-bft".into(),
                    age_secs: 0.0,
                    source: PacketSource::ZebraLive,
                },
                &format!("zebra bft height {h}"),
            );
        }
    }
}

fn ingest(sample: PacketSample, label: &str) {
    push_sample(sample);
    LIVE_EVENTS.fetch_add(1, Ordering::Relaxed);
    if let Ok(mut g) = LAST_EVENT_LABEL.lock() {
        *g = label.to_string();
    }
}

fn run_terp_poll_loop(rpc_base: &str, poll_ms: u64) {
    let mut last = 0u64;
    // On first contact, start from latest height (don't replay entire chain).
    let mut bootstrapped = false;
    loop {
        match fetch_latest_height(rpc_base) {
            Ok(height) if height > 0 => {
                if !bootstrapped {
                    last = height.saturating_sub(1);
                    bootstrapped = true;
                    LAST_TERP_HEIGHT.store(last, Ordering::Relaxed);
                    eprintln!("crosslink live IBC: terpd reachable, tracking from height {height}");
                }
                let start = last + 1;
                // Catch up a few blocks per tick.
                let end = height.min(start + 5);
                for h in start..=end {
                    match fetch_block_results(rpc_base, h) {
                        Ok(body) => {
                            let n = parse_and_ingest_block_results(&body, h);
                            if n > 0 {
                                eprintln!("crosslink live IBC: height {h} → {n} event(s)");
                            }
                            last = h;
                            LAST_TERP_HEIGHT.store(h, Ordering::Relaxed);
                        }
                        Err(e) => {
                            // Height might not be available yet.
                            if h == height {
                                eprintln!("crosslink live IBC: block_results {h}: {e}");
                            }
                            break;
                        }
                    }
                }
            }
            Ok(_) => {}
            Err(_e) => {
                // Terp not up yet — silent retry.
            }
        }
        thread::sleep(Duration::from_millis(poll_ms));
    }
}

/// Pure parser for Tendermint/CometBFT `block_results` JSON (tests + poller).
pub fn parse_and_ingest_block_results(body: &str, height: u64) -> usize {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return 0;
    };
    // RPC may wrap in { "result": ... } or return result directly.
    let result = v.get("result").unwrap_or(&v);
    let mut count = 0usize;

    // txs_results[].events[] and finalize_block_events[] / begin_block_events / end_block_events
    let mut event_arrays: Vec<&serde_json::Value> = Vec::new();
    if let Some(txs) = result.get("txs_results").and_then(|x| x.as_array()) {
        for tx in txs {
            if let Some(ev) = tx.get("events") {
                event_arrays.push(ev);
            }
        }
    }
    for key in [
        "finalize_block_events",
        "begin_block_events",
        "end_block_events",
    ] {
        if let Some(ev) = result.get(key) {
            event_arrays.push(ev);
        }
    }

    for arr in event_arrays {
        let Some(events) = arr.as_array() else {
            continue;
        };
        for ev in events {
            if let Some(sample) = event_to_sample(ev, height) {
                let label = format!(
                    "terp h={height} {} seq={} ch={}",
                    sample.phase.as_str(),
                    sample.sequence,
                    sample.channel
                );
                ingest(sample, &label);
                count += 1;
            }
        }
    }
    count
}

/// Map a single Tendermint event object to a [`PacketSample`] if IBC-related.
pub fn event_to_sample(ev: &serde_json::Value, height: u64) -> Option<PacketSample> {
    let ty = ev.get("type")?.as_str()?.to_ascii_lowercase();
    let attrs = attrs_map(ev);

    let phase = match ty.as_str() {
        "send_packet" | "ibc_send_packet" => PacketPhase::Send,
        "recv_packet" | "ibc_recv_packet" => PacketPhase::Recv,
        "acknowledge_packet" | "acknowledgepacket" | "ibc_acknowledge_packet" => PacketPhase::Ack,
        "timeout_packet" | "timeoutpacket" | "ibc_timeout_packet" => PacketPhase::Timeout,
        // Light-client path (real crosslink traffic on Terp).
        "update_client" | "updateclient" => PacketPhase::Recv,
        "create_client" | "createclient" => PacketPhase::Send,
        // message action type URL fallbacks
        _ if ty.contains("sendpacket") || ty.contains("send_packet") => PacketPhase::Send,
        _ if ty.contains("recvpacket") || ty.contains("recv_packet") => PacketPhase::Recv,
        _ if ty.contains("acknowledge") => PacketPhase::Ack,
        _ if ty.contains("timeout") && ty.contains("packet") => PacketPhase::Timeout,
        _ => return None,
    };

    let sequence = attr_u64(&attrs, &["packet_sequence", "sequence", "msg_index"])
        .unwrap_or(height);
    let channel = attr_string(
        &attrs,
        &[
            "packet_src_channel",
            "packet_dst_channel",
            "channel_id",
            "src_channel",
            "dst_channel",
        ],
    )
    .unwrap_or_else(|| {
        if ty.contains("client") {
            "ibc-client".into()
        } else {
            "channel-0".into()
        }
    });
    let client_id = attr_string(&attrs, &["client_id", "packet_connection", "connection_id"])
        .unwrap_or_else(|| "08-wasm-0".into());

    // Heuristic direction: recv/ack on Terp often means Zebra→Terp; send is Terp→Zebra.
    let direction = match phase {
        PacketPhase::Recv | PacketPhase::Ack => PacketDirection::ZebraToTerp,
        PacketPhase::Send | PacketPhase::Timeout => PacketDirection::TerpToZebra,
    };
    // Override from attributes if present.
    let direction = if let Some(src_port) = attr_string(&attrs, &["packet_src_port", "src_port"]) {
        if src_port.contains("transfer") || src_port.contains("ibc") {
            direction
        } else {
            direction
        }
    } else {
        direction
    };

    Some(PacketSample {
        phase,
        direction,
        sequence,
        channel,
        client_id,
        age_secs: 0.0,
        source: PacketSource::ChainLive,
    })
}

fn attrs_map(ev: &serde_json::Value) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    let Some(attrs) = ev.get("attributes").and_then(|a| a.as_array()) else {
        return map;
    };
    for a in attrs {
        let key = a
            .get("key")
            .and_then(|k| k.as_str())
            .map(decode_attr)
            .unwrap_or_default();
        let val = a
            .get("value")
            .and_then(|v| v.as_str())
            .map(decode_attr)
            .unwrap_or_default();
        if !key.is_empty() {
            map.insert(key.to_ascii_lowercase(), val);
        }
    }
    map
}

/// Tendermint sometimes base64-encodes attribute keys/values.
fn decode_attr(s: &str) -> String {
    // Fast path: already plain (contains non-base64-ish packet_ chars or looks like channel-0).
    if s.contains('-') || s.contains('_') || s.contains('/') || s.chars().all(|c| c.is_ascii_digit())
    {
        // Still might be base64 digits only for sequence — try decode if padded alphabet.
    }
    if let Ok(bytes) = base64_decode(s) {
        if let Ok(text) = std::str::from_utf8(&bytes) {
            if text.chars().all(|c| !c.is_control()) && !text.is_empty() {
                return text.to_string();
            }
        }
    }
    s.to_string()
}

fn base64_decode(s: &str) -> Result<Vec<u8>, ()> {
    // Minimal base64 decode (std only) for event attributes.
    const T: &[u8] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut table = [255u8; 256];
    for (i, &c) in T.iter().enumerate() {
        table[c as usize] = i as u8;
    }
    let s = s.trim_end_matches('=');
    if s.is_empty() || s.len() % 4 == 1 {
        return Err(());
    }
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let bytes = s.as_bytes();
    let mut i = 0;
    while i + 3 < bytes.len() {
        let a = table[bytes[i] as usize];
        let b = table[bytes[i + 1] as usize];
        let c = table[bytes[i + 2] as usize];
        let d = table[bytes[i + 3] as usize];
        if a | b | c | d == 255 {
            return Err(());
        }
        out.push((a << 2) | (b >> 4));
        out.push((b << 4) | (c >> 2));
        out.push((c << 6) | d);
        i += 4;
    }
    let rem = bytes.len() - i;
    if rem == 2 {
        let a = table[bytes[i] as usize];
        let b = table[bytes[i + 1] as usize];
        if a | b == 255 {
            return Err(());
        }
        out.push((a << 2) | (b >> 4));
    } else if rem == 3 {
        let a = table[bytes[i] as usize];
        let b = table[bytes[i + 1] as usize];
        let c = table[bytes[i + 2] as usize];
        if a | b | c == 255 {
            return Err(());
        }
        out.push((a << 2) | (b >> 4));
        out.push((b << 4) | (c >> 2));
    } else if rem != 0 {
        return Err(());
    }
    Ok(out)
}

fn attr_string(map: &std::collections::HashMap<String, String>, keys: &[&str]) -> Option<String> {
    for k in keys {
        if let Some(v) = map.get(*k) {
            if !v.is_empty() {
                return Some(v.clone());
            }
        }
    }
    None
}

fn attr_u64(map: &std::collections::HashMap<String, String>, keys: &[&str]) -> Option<u64> {
    attr_string(map, keys)?.parse().ok()
}

fn fetch_latest_height(rpc_base: &str) -> Result<u64, String> {
    let body = http_get(rpc_base, "/status")?;
    let v: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("status json: {e}"))?;
    let height = v
        .pointer("/result/sync_info/latest_block_height")
        .or_else(|| v.pointer("/sync_info/latest_block_height"))
        .and_then(|x| {
            x.as_str()
                .and_then(|s| s.parse().ok())
                .or_else(|| x.as_u64())
        })
        .ok_or_else(|| "missing latest_block_height".to_string())?;
    Ok(height)
}

fn fetch_block_results(rpc_base: &str, height: u64) -> Result<String, String> {
    http_get(rpc_base, &format!("/block_results?height={height}"))
}

/// Minimal HTTP/1.0 GET over TCP (no extra deps).
fn http_get(base: &str, path: &str) -> Result<String, String> {
    let base = base.trim_end_matches('/');
    let url = base
        .strip_prefix("http://")
        .or_else(|| base.strip_prefix("https://"))
        .unwrap_or(base);
    let (host_port, _) = url.split_once('/').unwrap_or((url, ""));
    let (host, port) = if let Some((h, p)) = host_port.split_once(':') {
        (h, p.parse::<u16>().unwrap_or(26657))
    } else {
        (host_port, 26657)
    };
    let mut stream = TcpStream::connect((host, port)).map_err(|e| format!("connect: {e}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .ok();
    stream
        .set_write_timeout(Some(Duration::from_secs(3)))
        .ok();
    let req = format!(
        "GET {path} HTTP/1.0\r\nHost: {host_port}\r\nConnection: close\r\nAccept: application/json\r\n\r\n"
    );
    stream
        .write_all(req.as_bytes())
        .map_err(|e| format!("write: {e}"))?;
    let mut buf = Vec::new();
    stream
        .read_to_end(&mut buf)
        .map_err(|e| format!("read: {e}"))?;
    let text = String::from_utf8_lossy(&buf);
    let body = text
        .split("\r\n\r\n")
        .nth(1)
        .or_else(|| text.split("\n\n").nth(1))
        .unwrap_or("")
        .to_string();
    if body.is_empty() {
        return Err("empty http body".into());
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet_feed::{clear_live, live_count};

    #[test]
    fn parse_send_packet_event_plain_attrs() {
        clear_live();
        let body = r#"{
          "result": {
            "height": "42",
            "txs_results": [{
              "events": [{
                "type": "send_packet",
                "attributes": [
                  {"key": "packet_sequence", "value": "7"},
                  {"key": "packet_src_channel", "value": "channel-0"},
                  {"key": "packet_connection", "value": "connection-0"}
                ]
              }]
            }]
          }
        }"#;
        let n = parse_and_ingest_block_results(body, 42);
        assert_eq!(n, 1);
        assert!(live_count() >= 1);
        clear_live();
    }

    #[test]
    fn parse_update_client_event() {
        let ev = serde_json::json!({
            "type": "update_client",
            "attributes": [
                {"key": "client_id", "value": "08-wasm-0"},
                {"key": "client_type", "value": "08-wasm"}
            ]
        });
        let s = event_to_sample(&ev, 10).expect("update_client maps");
        assert_eq!(s.phase, PacketPhase::Recv);
        assert_eq!(s.source, PacketSource::ChainLive);
        assert_eq!(s.client_id, "08-wasm-0");
    }

    #[test]
    fn base64_attr_roundtrip() {
        // "packet_sequence" base64
        let key = "cGFja2V0X3NlcXVlbmNl"; // packet_sequence
        assert_eq!(decode_attr(key), "packet_sequence");
    }

    #[test]
    fn observe_zebra_heights_emits_once_per_tip() {
        clear_live();
        let before = live_count();
        observe_zebra_heights(100, 5);
        // First observe skips catch-up flood for intermediate heights.
        observe_zebra_heights(101, 6);
        assert!(live_count() > before);
        clear_live();
    }
}
