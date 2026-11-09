//! Lightweight HTTP companion for the crosslink visualizer.
//!
//! The browser canvas at `/` is the primary way to **see** packet particles when
//! the OpenGL window cannot attach (e.g. zebrad started with `nohup` / background).
//!
//! Default bind: `127.0.0.1:18234` (override with `CROSSLINK_VIZ_HTTP`).

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::packet_feed;

static STARTED: AtomicBool = AtomicBool::new(false);
static SERVER_START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();

/// Default HTTP port next to zebrad RPC (18232) and P2P (18233).
pub const DEFAULT_VIZ_HTTP_ADDR: &str = "127.0.0.1:18234";

/// Spawn the visualizer HTTP server once (idempotent).
pub fn spawn_viz_http_server() {
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    SERVER_START.get_or_init(Instant::now);
    let addr = std::env::var("CROSSLINK_VIZ_HTTP").unwrap_or_else(|_| DEFAULT_VIZ_HTTP_ADDR.into());
    let addr_log = addr.clone();
    thread::Builder::new()
        .name("crosslink-viz-http".into())
        .spawn(move || {
            if let Err(e) = serve(&addr) {
                eprintln!("crosslink viz HTTP server error on {addr}: {e}");
            }
        })
        .expect("spawn viz HTTP thread");
    eprintln!(
        "crosslink visualizer HTTP listening on http://{addr_log}/  \
         (canvas at /, GET /overlay, POST /demo/packets)"
    );
}

fn serve(addr: &str) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr)?;
    listener.set_nonblocking(false)?;
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                let _ = handle_client(stream);
            }
            Err(e) => {
                eprintln!("viz HTTP accept error: {e}");
                thread::sleep(Duration::from_millis(50));
            }
        }
    }
    Ok(())
}

fn handle_client(mut stream: TcpStream) -> std::io::Result<()> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut buf = [0u8; 8192];
    let n = stream.read(&mut buf)?;
    if n == 0 {
        return Ok(());
    }
    let req = String::from_utf8_lossy(&buf[..n]);
    let mut lines = req.lines();
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("GET");
    let path = parts.next().unwrap_or("/");

    let path_only = path.split('?').next().unwrap_or(path);
    let query = path.split('?').nth(1).unwrap_or("");

    let body = req
        .split("\r\n\r\n")
        .nth(1)
        .or_else(|| req.split("\n\n").nth(1))
        .unwrap_or("")
        .trim_end_matches('\0')
        .trim();

    // CORS preflight
    if method == "OPTIONS" {
        return respond(&mut stream, 204, "text/plain", "");
    }

    match (method, path_only) {
        ("GET", "/health" | "/healthz") => respond(&mut stream, 200, "text/plain", "ok\n"),
        ("GET", "/status" | "/api/status") => {
            respond(&mut stream, 200, "application/json", &status_json())
        }
        ("GET", "/overlay" | "/api/overlay") => {
            let w = query_f32(query, "w", 960.0);
            let h = query_f32(query, "h", 540.0);
            let t = SERVER_START
                .get()
                .map(|s| s.elapsed().as_secs_f32())
                .unwrap_or(0.0);
            respond(
                &mut stream,
                200,
                "application/json",
                &packet_feed::overlay_json(w, h, t),
            )
        }
        ("GET", "/" | "/index.html") => {
            respond(&mut stream, 200, "text/html; charset=utf-8", INDEX_HTML)
        }
        ("POST", "/packet" | "/api/packet") => match packet_feed::inject_json(body) {
            Ok(()) => respond(
                &mut stream,
                200,
                "application/json",
                &format!(
                    r#"{{"ok":true,"live":{}}}"#,
                    packet_feed::live_count()
                ),
            ),
            Err(e) => respond(
                &mut stream,
                400,
                "application/json",
                &format!(
                    r#"{{"ok":false,"error":{}}}"#,
                    serde_json::to_string(&e).unwrap()
                ),
            ),
        },
        ("POST", "/demo/packets" | "/api/demo/packets") => {
            let scenario = path
                .split('?')
                .nth(1)
                .and_then(|q| {
                    q.split('&').find_map(|pair| {
                        let mut kv = pair.splitn(2, '=');
                        match (kv.next(), kv.next()) {
                            (Some("scenario"), Some(v)) => packet_feed::SimScenario::from_str(v),
                            _ => None,
                        }
                    })
                })
                .or_else(|| {
                    serde_json::from_str::<serde_json::Value>(body)
                        .ok()
                        .and_then(|v| {
                            v.get("scenario")
                                .and_then(|x| x.as_str())
                                .and_then(packet_feed::SimScenario::from_str)
                        })
                });
            let n = match scenario {
                Some(sc) => packet_feed::inject_scenario(sc),
                None => packet_feed::inject_demo_stream(),
            };
            respond(
                &mut stream,
                200,
                "application/json",
                &format!(
                    r#"{{"ok":true,"injected":{n},"live":{},"scenarios":["round_trip","reverse_round_trip","burst","timeout","stress","ping_pong"]}}"#,
                    packet_feed::live_count()
                ),
            )
        }
        _ => respond(&mut stream, 404, "text/plain", "not found\n"),
    }
}

fn query_f32(query: &str, key: &str, default: f32) -> f32 {
    query
        .split('&')
        .find_map(|pair| {
            let mut kv = pair.splitn(2, '=');
            match (kv.next(), kv.next()) {
                (Some(k), Some(v)) if k == key => v.parse().ok(),
                _ => None,
            }
        })
        .unwrap_or(default)
}

fn respond(stream: &mut TcpStream, code: u16, ctype: &str, body: &str) -> std::io::Result<()> {
    let reason = match code {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "Error",
    };
    let header = format!(
        "HTTP/1.1 {code} {reason}\r\n\
         Content-Type: {ctype}\r\n\
         Content-Length: {}\r\n\
         Access-Control-Allow-Origin: *\r\n\
         Access-Control-Allow-Methods: GET, POST, OPTIONS\r\n\
         Access-Control-Allow-Headers: Content-Type\r\n\
         Connection: close\r\n\
         Cache-Control: no-cache\r\n\
         \r\n",
        body.len()
    );
    stream.write_all(header.as_bytes())?;
    stream.write_all(body.as_bytes())?;
    Ok(())
}

fn status_json() -> String {
    use super::viz::VIZ_G;

    let mut out = serde_json::Map::new();
    out.insert("service".into(), "zebra-crosslink-visualizer".into());
    out.insert(
        "http".into(),
        std::env::var("CROSSLINK_VIZ_HTTP")
            .unwrap_or_else(|_| DEFAULT_VIZ_HTTP_ADDR.into())
            .into(),
    );
    out.insert("packets_total".into(), packet_feed::live_count().into());
    out.insert("packets_live".into(), packet_feed::live_real_count().into());
    out.insert("packets_sim".into(), packet_feed::live_sim_count().into());
    out.insert("live".into(), crate::ibc_live::live_status_json());
    out.insert(
        "unix_ms".into(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
            .into(),
    );
    out.insert(
        "note".into(),
        "Primary feed is LIVE: Terp block_results + Zebra height bridge. Sim buttons are harness-only.".into(),
    );

    match VIZ_G.lock() {
        Ok(guard) => match guard.as_ref() {
            Some(g) => {
                out.insert("viz_ready".into(), true.into());
                out.insert("bft_pause".into(), g.bft_pause_button.into());
                out.insert(
                    "validators".into(),
                    g.validators_at_current_height.len().into(),
                );
                out.insert("pow_blocks_cached".into(), g.state.blocks.len().into());
                out.insert("bft_blocks_cached".into(), g.state.bft_blocks.len().into());
                if let Some((h, _)) = g.state.bc_tip {
                    out.insert("zebra_pow_tip".into(), h.0.into());
                }
            }
            None => {
                out.insert("viz_ready".into(), false.into());
            }
        },
        Err(_) => {
            out.insert("viz_ready".into(), false.into());
        }
    }

    serde_json::Value::Object(out).to_string()
}

const INDEX_HTML: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="utf-8"/>
  <meta name="viewport" content="width=device-width, initial-scale=1"/>
  <title>Crosslink · crystalline manifold</title>
  <style>
    * { box-sizing: border-box; }
    body {
      margin: 0; font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
      background: #04070c; color: #d7e0ea; min-height: 100vh;
      display: flex; flex-direction: column;
    }
    header {
      padding: 0.75rem 1.25rem; border-bottom: 1px solid #1a2430;
      display: flex; flex-wrap: wrap; gap: 0.75rem; align-items: center;
      background: linear-gradient(180deg, #0a1018 0%, #070c12 100%);
    }
    h1 { font-size: 1rem; margin: 0; color: #f0a030; font-weight: 600; letter-spacing: 0.02em; }
    .stat { color: #8ab4f8; font-size: 0.85rem; }
    .ok { color: #5dca6e; }
    .wait { color: #e0b341; }
    button {
      background: #132436; color: #e8f0ff; border: 1px solid #3d6ea8;
      border-radius: 6px; padding: 0.4rem 0.75rem; cursor: pointer; font: inherit; font-size: 0.8rem;
    }
    button:hover { background: #1c3550; }
    button.primary { background: #1e3d2a; border-color: #4a9a4a; }
    main { flex: 1; display: flex; flex-direction: column; min-height: 0; }
    #stage {
      flex: 1; width: 100%; display: block; background:
        radial-gradient(ellipse at 50% 42%, #0c1a2a 0%, #04070c 68%);
      cursor: crosshair;
    }
    .hint {
      padding: 0.5rem 1.25rem; font-size: 0.75rem; color: #7a8799;
      border-top: 1px solid #1a2430;
    }
    .legend { display: flex; gap: 1rem; flex-wrap: wrap; }
    .legend i {
      display: inline-block; width: 10px; height: 10px; border-radius: 2px;
      margin-right: 0.3rem; vertical-align: middle;
      transform: rotate(30deg);
    }
  </style>
</head>
<body>
  <header>
    <h1>Crosslink · Erdős unit-distance crystal</h1>
    <span class="stat">live: <span id="pk" class="ok">0</span></span>
    <span class="stat">sim: <span id="sm" class="wait">0</span></span>
    <span class="stat">nodes: <span id="nd" class="ok">0</span></span>
    <span class="stat">terp h: <span id="th" class="ok">–</span></span>
    <span class="stat">status: <span id="st" class="wait">…</span></span>
    <button class="primary" id="btn-status">Refresh status</button>
    <button id="btn-burst" title="Harness only">Sim burst</button>
    <button id="btn-rt" title="Harness only">Sim RT</button>
    <button id="btn-pp" title="Harness only">Sim ping</button>
    <button id="btn-stress" title="Harness only">Sim stress</button>
    <button id="btn-to" title="Harness only">Sim timeout</button>
  </header>
  <main>
    <canvas id="stage"></canvas>
  </main>
  <div class="hint">
    <div class="legend">
      <span><i style="background:#4db2ff"></i>Terp</span>
      <span><i style="background:#f28c28"></i>Zebra</span>
      <span><i style="background:#8cd6f0"></i>Channel</span>
      <span><i style="background:#e65ad9"></i>Packet</span>
      <span><i style="background:#b070ff"></i>Ack</span>
      <span><i style="background:#f04040"></i>Timeout</span>
    </div>
    Erdős unit-distance crystal · static 4D topographic projection · growth-only (no particle chaos).
    Live events activate facets or grow new matchstick sites.
  </div>
<script>
const canvas = document.getElementById('stage');
const ctx = canvas.getContext('2d');
let W = 960, H = 540, t0 = performance.now();

function resize() {
  const r = canvas.getBoundingClientRect();
  const dpr = window.devicePixelRatio || 1;
  W = Math.max(320, r.width);
  H = Math.max(240, r.height);
  canvas.width = W * dpr;
  canvas.height = H * dpr;
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
}
window.addEventListener('resize', resize);
resize();

function colorForKind(k) {
  if (k < 0.5) return [242, 140, 40];
  if (k < 1.5) return [77, 178, 255];
  if (k < 2.5) return [102, 240, 122];
  if (k < 3.5) return [230, 90, 217];
  if (k < 4.5) return [140, 214, 240];
  if (k < 5.5) return [204, 200, 102];
  if (k < 6.5) return [176, 112, 255];
  if (k < 7.5) return [240, 64, 64];
  return [170, 187, 204];
}
function rgba(c, a) { return `rgba(${c[0]},${c[1]},${c[2]},${a})`; }

function glow(x, y, r, c, alpha) {
  const g = ctx.createRadialGradient(x, y, 0, x, y, r);
  g.addColorStop(0, rgba(c, alpha));
  g.addColorStop(0.45, rgba(c, alpha * 0.35));
  g.addColorStop(1, rgba(c, 0));
  ctx.fillStyle = g;
  ctx.beginPath();
  ctx.arc(x, y, r, 0, Math.PI * 2);
  ctx.fill();
}

function hexPath(x, y, r, rot) {
  ctx.beginPath();
  for (let i = 0; i < 6; i++) {
    const a = rot + i * Math.PI / 3;
    const px = x + r * Math.cos(a);
    const py = y + r * Math.sin(a);
    if (i === 0) ctx.moveTo(px, py); else ctx.lineTo(px, py);
  }
  ctx.closePath();
}

async function frame() {
  try {
    const r = await fetch(`/overlay?w=${W}&h=${H}&_=${Date.now()}`);
    const data = await r.json();
    const feed = data.feed || {};
    const liveN = feed.live != null ? feed.live : (data.packets || []).filter(p => p.source && p.source !== 'simulated').length;
    const simN = feed.simulated != null ? feed.simulated : 0;
    document.getElementById('pk').textContent = liveN;
    document.getElementById('sm').textContent = simN;
    document.getElementById('nd').textContent = (data.nodes && data.nodes.length) || 0;
    document.getElementById('st').textContent = data.mode === 'erdos_unit_distance_crystal' ? 'crystal' : (liveN > 0 ? 'LIVE' : 'waiting');
    document.getElementById('st').className = 'ok';

    ctx.clearRect(0, 0, W, H);

    // Quiet topographic void — no animated grid.
    const vg = ctx.createRadialGradient(W*0.5, H*0.48, 0, W*0.5, H*0.48, Math.max(W,H)*0.72);
    vg.addColorStop(0, 'rgba(10,18,28,0.95)');
    vg.addColorStop(1, 'rgba(3,5,8,1)');
    ctx.fillStyle = vg;
    ctx.fillRect(0, 0, W, H);

    // Static matchstick edges (unit-distance only) — no traveling waves.
    for (const e of (data.edges || [])) {
      const a = data.nodes[e[0]], b = data.nodes[e[1]];
      if (!a || !b) continue;
      const pa = a.pulse || 0.35, pb = b.pulse || 0.35;
      const act = 0.5 * (pa + pb);
      const ca = colorForKind(a.kind), cb = colorForKind(b.kind);
      const mid = [(ca[0]+cb[0])/2, (ca[1]+cb[1])/2, (ca[2]+cb[2])/2];
      ctx.lineCap = 'round';
      ctx.strokeStyle = rgba(mid, 0.10 + act * 0.14);
      ctx.lineWidth = 3.2;
      ctx.beginPath(); ctx.moveTo(a.x, a.y); ctx.lineTo(b.x, b.y); ctx.stroke();
      ctx.strokeStyle = rgba(mid, 0.40 + act * 0.35);
      ctx.lineWidth = 1.15;
      ctx.beginPath(); ctx.moveTo(a.x, a.y); ctx.lineTo(b.x, b.y); ctx.stroke();
    }

    // Static hexagonal crystal facets — all sites are hex (Erdős lattice).
    for (const n of (data.nodes || [])) {
      const c = colorForKind(n.kind);
      const act = n.pulse || 0.35;
      const topo = (n.topo != null ? n.topo : 0);
      const rad = 5.5 + 10 * act - 2 * topo;
      const rot = n.angle || 0;
      glow(n.x, n.y, Math.max(10, rad * 2.1), c, 0.14 + act * 0.22);
      hexPath(n.x, n.y, Math.max(3.5, rad * 0.55), rot);
      ctx.fillStyle = rgba(c, 0.55 + act * 0.35);
      ctx.fill();
      ctx.strokeStyle = `rgba(230,240,255,${0.20 + act * 0.35})`;
      ctx.lineWidth = 1;
      ctx.stroke();
      hexPath(n.x, n.y, Math.max(1.5, rad * 0.18), rot);
      ctx.fillStyle = rgba([240,245,255], 0.25 * act);
      ctx.fill();
    }

    if (data.hubs) {
      ctx.font = '11px ui-monospace, monospace';
      ctx.fillStyle = 'rgba(200,212,224,0.85)';
      ctx.textAlign = 'center';
      const h = data.hubs;
      if (h.terp) ctx.fillText('TERP', h.terp[0], h.terp[1] + 26);
      if (h.zebra) ctx.fillText('ZEBRA', h.zebra[0], h.zebra[1] + 26);
      if (h.channel) ctx.fillText('CHANNEL', h.channel[0], h.channel[1] - 16);
      if (h.root) {
        ctx.fillStyle = 'rgba(160,190,220,0.55)';
        ctx.fillText('crosslink', h.root[0], h.root[1] + 14);
      }
    }
  } catch (e) {
    document.getElementById('st').textContent = 'error';
    document.getElementById('st').className = 'wait';
  }
  requestAnimationFrame(() => setTimeout(frame, 45));
}

async function postScenario(name) {
  await fetch('/demo/packets?scenario=' + encodeURIComponent(name), {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ scenario: name }),
  });
}
document.getElementById('btn-burst').onclick = () => postScenario('burst');
document.getElementById('btn-rt').onclick = () => postScenario('round_trip');
document.getElementById('btn-pp').onclick = () => postScenario('ping_pong');
document.getElementById('btn-stress').onclick = () => postScenario('stress');
document.getElementById('btn-to').onclick = () => postScenario('timeout');
document.getElementById('btn-status').onclick = async () => {
  const s = await fetch('/status').then(r => r.json()).catch(() => ({}));
  const live = s.live || {};
  document.getElementById('th').textContent = live.last_terp_height || '–';
  document.getElementById('st').textContent = (s.packets_live > 0) ? 'LIVE' : 'waiting';
  console.log('status', s);
};

// No auto sim seed — wait for real Terp/Zebra events.
(async () => {
  const s = await fetch('/status').then(r => r.json()).catch(() => ({}));
  const live = s.live || {};
  document.getElementById('th').textContent = live.last_terp_height || '–';
  document.getElementById('st').textContent = live.tailer_running ? 'tailing' : 'starting';
  // poll status for terp height occasionally
  setInterval(async () => {
    try {
      const st = await fetch('/status').then(r => r.json());
      if (st.live) document.getElementById('th').textContent = st.live.last_terp_height || '–';
    } catch (_) {}
  }, 1500);
  frame();
})();
</script>
</body>
</html>
"#;
