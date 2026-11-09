//! IBC packet telemetry for the GLSL visualizer — pure data + sim + live feed.
//!
//! # Layers
//! 1. **Types** — `PacketPhase`, `PacketDirection`, `PacketSample`, `PacketSource`
//! 2. **Simulator** — deterministic scenario generators (harness only)
//! 3. **Layout** — samples → crystalline manifold → shader textures
//! 4. **Live feed** — process-wide queue filled by **real** Terp/Zebra events
//!    (`ibc_live`) or optional sim inject (HTTP harness)
//!
//! Unit tests exercise pure layout without zebrad. Production path is live.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Instant;

use crate::shaders::{RenderNode, MAX_EDGES, MAX_NODES};

/// Max live packets retained for the overlay.
pub const MAX_LIVE: usize = 64;
/// How long a packet particle remains visible (seconds).
pub const PACKET_TTL_SECS: f32 = 10.0;

// ─── Pure types ─────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PacketPhase {
    Send,
    Recv,
    Ack,
    Timeout,
}

impl PacketPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Send => "send",
            Self::Recv => "recv",
            Self::Ack => "ack",
            Self::Timeout => "timeout",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "send" | "sendpacket" => Some(Self::Send),
            "recv" | "recvpacket" => Some(Self::Recv),
            "ack" | "acknowledge" | "acknowledgepacket" => Some(Self::Ack),
            "timeout" | "timeoutpacket" => Some(Self::Timeout),
            _ => None,
        }
    }

    /// Shader kind string for this phase.
    pub fn shader_kind(self) -> &'static str {
        match self {
            Self::Timeout => "ibc_timeout",
            Self::Ack => "ibc_ack",
            Self::Send | Self::Recv => "ibc_packet",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PacketDirection {
    /// Terp (IBC host) → Zebra / crosslink
    TerpToZebra,
    /// Zebra / crosslink → Terp
    ZebraToTerp,
}

impl PacketDirection {
    pub fn from_str(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "z2t" | "zebra_to_terp" | "in" => Self::ZebraToTerp,
            _ => Self::TerpToZebra,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::TerpToZebra => "terp_to_zebra",
            Self::ZebraToTerp => "zebra_to_terp",
        }
    }

    pub fn reverse(self) -> Self {
        match self {
            Self::TerpToZebra => Self::ZebraToTerp,
            Self::ZebraToTerp => Self::TerpToZebra,
        }
    }
}

/// Where a sample came from — live chain objects vs harness sim.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum PacketSource {
    /// Deterministic harness (`/demo/packets`) — not production traffic.
    #[default]
    Simulated,
    /// Parsed from Terp CometBFT `block_results` (real ICS-4 / client events).
    ChainLive,
    /// Emitted from in-process Zebra viz when PoW/BFT height advances.
    ZebraLive,
}

impl PacketSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Simulated => "simulated",
            Self::ChainLive => "chain_live",
            Self::ZebraLive => "zebra_live",
        }
    }

    pub fn is_live(self) -> bool {
        matches!(self, Self::ChainLive | Self::ZebraLive)
    }
}

/// One ICS-4 lifecycle sample ready for layout / shader packing.
///
/// `age_secs` is the pure clock: tests set it explicitly; the live feed
/// derives it from `Instant::elapsed`.
#[derive(Clone, Debug, PartialEq)]
pub struct PacketSample {
    pub phase: PacketPhase,
    pub direction: PacketDirection,
    pub sequence: u64,
    pub channel: String,
    pub client_id: String,
    /// Seconds since the event was emitted (drives travel animation).
    pub age_secs: f32,
    /// Live chain vs harness.
    pub source: PacketSource,
}

impl PacketSample {
    pub fn new(
        phase: PacketPhase,
        direction: PacketDirection,
        sequence: u64,
        channel: impl Into<String>,
        client_id: impl Into<String>,
        age_secs: f32,
    ) -> Self {
        Self {
            phase,
            direction,
            sequence,
            channel: channel.into(),
            client_id: client_id.into(),
            age_secs,
            source: PacketSource::Simulated,
        }
    }

    pub fn live(
        phase: PacketPhase,
        direction: PacketDirection,
        sequence: u64,
        channel: impl Into<String>,
        client_id: impl Into<String>,
        source: PacketSource,
    ) -> Self {
        Self {
            phase,
            direction,
            sequence,
            channel: channel.into(),
            client_id: client_id.into(),
            age_secs: 0.0,
            source,
        }
    }

    /// Fractional travel along the path in `[0, 1]` from phase + age.
    pub fn travel(&self, time: f32, index: usize) -> f32 {
        let t = (self.age_secs / PACKET_TTL_SECS).clamp(0.0, 1.0);
        match self.phase {
            PacketPhase::Send => (t * 1.4).clamp(0.0, 0.45),
            PacketPhase::Recv => 0.45 + (t * 0.9).clamp(0.0, 0.35),
            PacketPhase::Ack => 0.80 - (t * 0.7).clamp(0.0, 0.55),
            PacketPhase::Timeout => 0.5 + 0.1 * (time * 4.0 + index as f32).sin(),
        }
    }

    pub fn is_expired(&self) -> bool {
        // Live samples linger a bit longer so chain traffic is readable.
        let ttl = if self.source.is_live() {
            PACKET_TTL_SECS * 1.6
        } else {
            PACKET_TTL_SECS
        };
        self.age_secs > ttl
    }
}

// ─── Deterministic simulator (no GPU / network) ─────────────────────────────

/// Named harness scenarios for exercising the GLSL overlay offline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SimScenario {
    /// Single send→recv→ack on Terp→Zebra.
    RoundTrip,
    /// Reverse direction round-trip.
    ReverseRoundTrip,
    /// Several sequences both ways (default HTTP demo).
    Burst,
    /// Packet that times out mid-path.
    Timeout,
    /// Dense fan-out for stress / packing limits.
    Stress,
    /// Alternating directions (relayer load).
    PingPong,
}

impl SimScenario {
    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "round_trip" | "roundtrip" | "rt" => Some(Self::RoundTrip),
            "reverse" | "reverse_round_trip" | "rrt" => Some(Self::ReverseRoundTrip),
            "burst" | "demo" => Some(Self::Burst),
            "timeout" | "to" => Some(Self::Timeout),
            "stress" => Some(Self::Stress),
            "ping_pong" | "pingpong" => Some(Self::PingPong),
            _ => None,
        }
    }

    pub fn all() -> &'static [Self] {
        &[
            Self::RoundTrip,
            Self::ReverseRoundTrip,
            Self::Burst,
            Self::Timeout,
            Self::Stress,
            Self::PingPong,
        ]
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::RoundTrip => "round_trip",
            Self::ReverseRoundTrip => "reverse_round_trip",
            Self::Burst => "burst",
            Self::Timeout => "timeout",
            Self::Stress => "stress",
            Self::PingPong => "ping_pong",
        }
    }
}

/// Config for [`PacketSimulator`].
#[derive(Clone, Debug)]
pub struct SimConfig {
    pub channel: String,
    pub client_id: String,
    /// Spacing between successive lifecycle steps (seconds of age).
    pub step_age: f32,
    /// Starting sequence number.
    pub start_sequence: u64,
}

impl Default for SimConfig {
    fn default() -> Self {
        Self {
            channel: "channel-0".into(),
            client_id: "08-wasm-0".into(),
            step_age: 0.18,
            start_sequence: 1,
        }
    }
}

/// Builds deterministic [`PacketSample`] lists for visualization tests.
#[derive(Clone, Debug)]
pub struct PacketSimulator {
    cfg: SimConfig,
    next_seq: u64,
}

impl Default for PacketSimulator {
    fn default() -> Self {
        Self::new(SimConfig::default())
    }
}

impl PacketSimulator {
    pub fn new(cfg: SimConfig) -> Self {
        let next_seq = cfg.start_sequence;
        Self { cfg, next_seq }
    }

    pub fn config(&self) -> &SimConfig {
        &self.cfg
    }

    /// Run a named scenario into pure samples (ages assigned deterministically).
    pub fn scenario(&mut self, scenario: SimScenario) -> Vec<PacketSample> {
        match scenario {
            SimScenario::RoundTrip => self.round_trip(PacketDirection::TerpToZebra),
            SimScenario::ReverseRoundTrip => self.round_trip(PacketDirection::ZebraToTerp),
            SimScenario::Burst => self.burst(4),
            SimScenario::Timeout => self.timeout_path(PacketDirection::TerpToZebra),
            SimScenario::Stress => self.stress(48),
            SimScenario::PingPong => self.ping_pong(6),
        }
    }

    /// Classic ICS-4 happy path for one sequence: Send → Recv → Ack (ack reverses dir).
    pub fn round_trip(&mut self, direction: PacketDirection) -> Vec<PacketSample> {
        let seq = self.alloc_seq();
        let steps = [
            (PacketPhase::Send, direction),
            (PacketPhase::Recv, direction),
            (PacketPhase::Ack, direction.reverse()),
        ];
        self.from_steps(seq, &steps)
    }

    /// Multiple full round-trips in the forward direction, plus one reverse.
    pub fn burst(&mut self, sequences: u64) -> Vec<PacketSample> {
        let mut out = Vec::new();
        for _ in 0..sequences {
            out.extend(self.round_trip(PacketDirection::TerpToZebra));
        }
        out.extend(self.round_trip(PacketDirection::ZebraToTerp));
        // Re-stamp ages so the whole burst is staggered as one timeline.
        retime(&mut out, self.cfg.step_age);
        out
    }

    /// Send then timeout (no recv/ack).
    pub fn timeout_path(&mut self, direction: PacketDirection) -> Vec<PacketSample> {
        let seq = self.alloc_seq();
        self.from_steps(
            seq,
            &[
                (PacketPhase::Send, direction),
                (PacketPhase::Timeout, direction),
            ],
        )
    }

    /// Dense packing of send particles (shader limit / stress).
    pub fn stress(&mut self, count: usize) -> Vec<PacketSample> {
        let mut out = Vec::with_capacity(count);
        for i in 0..count {
            let seq = self.alloc_seq();
            let dir = if i % 2 == 0 {
                PacketDirection::TerpToZebra
            } else {
                PacketDirection::ZebraToTerp
            };
            out.push(PacketSample {
                phase: PacketPhase::Send,
                direction: dir,
                sequence: seq,
                channel: self.cfg.channel.clone(),
                client_id: self.cfg.client_id.clone(),
                age_secs: i as f32 * self.cfg.step_age * 0.25,
                source: PacketSource::Simulated,
            });
        }
        out
    }

    /// Alternating direction round-trips.
    pub fn ping_pong(&mut self, pairs: u64) -> Vec<PacketSample> {
        let mut out = Vec::new();
        for i in 0..pairs {
            let dir = if i % 2 == 0 {
                PacketDirection::TerpToZebra
            } else {
                PacketDirection::ZebraToTerp
            };
            out.extend(self.round_trip(dir));
        }
        retime(&mut out, self.cfg.step_age);
        out
    }

    /// Arbitrary step list for custom harnesses.
    pub fn from_steps(
        &self,
        sequence: u64,
        steps: &[(PacketPhase, PacketDirection)],
    ) -> Vec<PacketSample> {
        steps
            .iter()
            .enumerate()
            .map(|(i, &(phase, direction))| {
                PacketSample {
                    phase,
                    direction,
                    sequence,
                    channel: self.cfg.channel.clone(),
                    client_id: self.cfg.client_id.clone(),
                    age_secs: i as f32 * self.cfg.step_age,
                    source: PacketSource::Simulated,
                }
            })
            .collect()
    }

    fn alloc_seq(&mut self) -> u64 {
        let s = self.next_seq;
        self.next_seq += 1;
        s
    }
}

fn retime(samples: &mut [PacketSample], step: f32) {
    for (i, s) in samples.iter_mut().enumerate() {
        s.age_secs = i as f32 * step;
    }
}

// ─── Pure layout (samples → crystalline manifold → shader buffers) ──────────

/// Screen-space layout of the unit-distance tracing manifold.
#[derive(Clone, Debug)]
pub struct OverlayLayout {
    pub nodes: Vec<RenderNode>,
    pub edges: Vec<(u32, u32)>,
    /// Hub positions after lattice→screen fit (labels / HTTP).
    pub hubs: HubLayout,
}

/// Named hub screen coordinates (crystalline backbone).
#[derive(Clone, Debug)]
pub struct HubLayout {
    pub terp: (f32, f32),
    pub zebra: (f32, f32),
    pub channel: (f32, f32),
    pub root: (f32, f32),
}

/// Build GLSL overlay: **static Erdős unit-distance crystal** projected from R⁴.
///
/// Growth is additive only (no traveling particles / chaotic arcs). Live samples
/// either activate existing facets or grow new unit-distance sites.
pub fn layout_overlay(
    samples: &[PacketSample],
    screen_w: f32,
    screen_h: f32,
    _time: f32,
    base_idx: u32,
) -> OverlayLayout {
    // Persistent crystal for live viz; pure rebuild is used only in isolated tests
    // via manifold::build_packet_manifold. Here we grow the process crystal.
    let (_m, nodes, edges, hubs_s) =
        crate::manifold::layout_crystal(samples, screen_w, screen_h, base_idx);
    OverlayLayout {
        nodes,
        edges,
        hubs: HubLayout {
            terp: hubs_s.terp,
            zebra: hubs_s.zebra,
            channel: hubs_s.channel,
            root: hubs_s.root,
        },
    }
}

/// Legacy arc helper (kept for tests / travel diagnostics).
/// Prefer manifold positions for rendering.
pub fn packet_xy(
    direction: PacketDirection,
    travel: f32,
    screen_w: f32,
    screen_h: f32,
) -> (f32, f32) {
    let (x0, y0, x1, y1) = match direction {
        PacketDirection::TerpToZebra => (
            screen_w * 0.18,
            screen_h * 0.50,
            screen_w * 0.82,
            screen_h * 0.50,
        ),
        PacketDirection::ZebraToTerp => (
            screen_w * 0.82,
            screen_h * 0.50,
            screen_w * 0.18,
            screen_h * 0.50,
        ),
    };
    let mx = screen_w * 0.50;
    let my = screen_h * 0.28;
    if travel < 0.5 {
        let u = travel * 2.0;
        (lerp(x0, mx, u), lerp(y0, my, u))
    } else {
        let u = (travel - 0.5) * 2.0;
        (lerp(mx, x1, u), lerp(my, y1, u))
    }
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

// ─── Live process feed (HTTP / demo) ────────────────────────────────────────

#[derive(Clone, Debug)]
struct LiveEvent {
    sample: PacketSample,
    created: Instant,
}

lazy_static::lazy_static! {
    static ref PACKET_FEED: Mutex<VecDeque<LiveEvent>> = Mutex::new(VecDeque::new());
}

/// Push a live event (age starts at 0). Prefer [`PacketSource::ChainLive`] /
/// [`PacketSource::ZebraLive`] for real traffic.
pub fn push_sample(sample: PacketSample) {
    let mut q = PACKET_FEED.lock().unwrap();
    while q.len() >= MAX_LIVE {
        q.pop_front();
    }
    q.push_back(LiveEvent {
        sample: PacketSample {
            age_secs: 0.0,
            ..sample
        },
        created: Instant::now(),
    });
}

/// Count samples currently in the feed that are real chain/zebra traffic.
pub fn live_real_count() -> usize {
    snapshot_live()
        .iter()
        .filter(|s| s.source.is_live())
        .count()
}

/// Count harness-simulated samples currently in the feed.
pub fn live_sim_count() -> usize {
    snapshot_live()
        .iter()
        .filter(|s| s.source == PacketSource::Simulated)
        .count()
}

/// Push a raw lifecycle event with wall-clock start.
pub fn push_event(
    phase: PacketPhase,
    direction: PacketDirection,
    sequence: u64,
    channel: impl Into<String>,
    client_id: impl Into<String>,
) {
    push_sample(PacketSample::new(
        phase,
        direction,
        sequence,
        channel,
        client_id,
        0.0,
    ));
}

/// Load a simulated scenario into the live feed (for HTTP / spawn harness).
pub fn inject_scenario(scenario: SimScenario) -> usize {
    let mut sim = PacketSimulator::default();
    let samples = sim.scenario(scenario);
    let n = samples.len();
    // Stagger wall-clock so particles fan out like the pure ages.
    let step_ms = (sim.config().step_age * 1000.0) as u64;
    let mut q = PACKET_FEED.lock().unwrap();
    for (i, sample) in samples.into_iter().enumerate() {
        while q.len() >= MAX_LIVE {
            q.pop_front();
        }
        q.push_back(LiveEvent {
            sample,
            created: Instant::now() - std::time::Duration::from_millis(step_ms * i as u64),
        });
    }
    n
}

/// JSON inject used by the HTTP companion.
pub fn inject_json(body: &str) -> Result<(), String> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("invalid json: {e}"))?;
    // Optional full scenario inject: {"scenario":"burst"}
    if let Some(name) = v.get("scenario").and_then(|x| x.as_str()) {
        let sc = SimScenario::from_str(name)
            .ok_or_else(|| format!("unknown scenario '{name}'"))?;
        inject_scenario(sc);
        return Ok(());
    }
    let phase = v
        .get("phase")
        .and_then(|x| x.as_str())
        .and_then(PacketPhase::from_str)
        .ok_or_else(|| "missing/invalid phase (send|recv|ack|timeout)".to_string())?;
    let direction = v
        .get("direction")
        .and_then(|x| x.as_str())
        .map(PacketDirection::from_str)
        .unwrap_or(PacketDirection::TerpToZebra);
    let sequence = v.get("sequence").and_then(|x| x.as_u64()).unwrap_or(0);
    let channel = v
        .get("channel")
        .and_then(|x| x.as_str())
        .unwrap_or("channel-0")
        .to_string();
    let client_id = v
        .get("client_id")
        .and_then(|x| x.as_str())
        .unwrap_or("08-wasm-0")
        .to_string();
    let source = match v
        .get("source")
        .and_then(|x| x.as_str())
        .unwrap_or("live")
        .to_ascii_lowercase()
        .as_str()
    {
        "sim" | "simulated" | "demo" => PacketSource::Simulated,
        "zebra" | "zebra_live" => PacketSource::ZebraLive,
        _ => PacketSource::ChainLive, // default: treat external inject as real
    };
    push_sample(PacketSample::live(
        phase, direction, sequence, channel, client_id, source,
    ));
    Ok(())
}

/// Default demo stream (burst scenario).
pub fn inject_demo_stream() -> usize {
    inject_scenario(SimScenario::Burst)
}

pub fn live_count() -> usize {
    snapshot_live().len()
}

/// Clear the live feed (tests).
pub fn clear_live() {
    PACKET_FEED.lock().unwrap().clear();
}

fn snapshot_live() -> Vec<PacketSample> {
    let mut q = PACKET_FEED.lock().unwrap();
    q.retain(|e| {
        let age = e.created.elapsed().as_secs_f32();
        let mut s = e.sample.clone();
        s.age_secs = age;
        !s.is_expired()
    });
    q.iter()
        .map(|e| {
            let mut s = e.sample.clone();
            s.age_secs = e.created.elapsed().as_secs_f32();
            s
        })
        .collect()
}

/// Build overlay from the live process feed (used by the GPU frame loop).
pub fn build_overlay(
    screen_w: f32,
    screen_h: f32,
    time: f32,
    base_idx: u32,
) -> (Vec<RenderNode>, Vec<(u32, u32)>) {
    let samples = snapshot_live();
    let layout = layout_overlay(&samples, screen_w, screen_h, time, base_idx);
    (layout.nodes, layout.edges)
}

/// JSON snapshot for the browser canvas (same manifold layout as GLSL).
pub fn overlay_json(screen_w: f32, screen_h: f32, time: f32) -> String {
    let samples = snapshot_live();
    let layout = layout_overlay(&samples, screen_w, screen_h, time, 0);
    let nodes: Vec<serde_json::Value> = layout
        .nodes
        .iter()
        .map(|n| {
            serde_json::json!({
                "x": n.pos_x,
                "y": n.pos_y,
                "pulse": n.pulse,
                "topo": n.last_pulse_time,
                "kind": n.node_type_float,
                "angle": n.parent_angle,
            })
        })
        .collect();
    let edges: Vec<serde_json::Value> = layout
        .edges
        .iter()
        .map(|(a, b)| serde_json::json!([a, b]))
        .collect();
    let packets: Vec<serde_json::Value> = samples
        .iter()
        .map(|s| {
            serde_json::json!({
                "phase": s.phase.as_str(),
                "direction": s.direction.as_str(),
                "sequence": s.sequence,
                "channel": s.channel,
                "client_id": s.client_id,
                "age_secs": s.age_secs,
                "travel": s.travel(time, 0),
                "source": s.source.as_str(),
            })
        })
        .collect();
    let real_n = samples.iter().filter(|s| s.source.is_live()).count();
    let sim_n = samples.len().saturating_sub(real_n);
    serde_json::json!({
        "screen_w": screen_w,
        "screen_h": screen_h,
        "time": time,
        "mode": "erdos_unit_distance_crystal",
        "aesthetic": "static_4d_topographic_manifold",
        "feed": {
            "total": samples.len(),
            "live": real_n,
            "simulated": sim_n,
        },
        "nodes": nodes,
        "edges": edges,
        "packets": packets,
        "hubs": {
            "terp": [layout.hubs.terp.0, layout.hubs.terp.1],
            "zebra": [layout.hubs.zebra.0, layout.hubs.zebra.1],
            "channel": [layout.hubs.channel.0, layout.hubs.channel.1],
            "root": [layout.hubs.root.0, layout.hubs.root.1],
        }
    })
    .to_string()
}

// ─── Tests (pure data — no zebrad / GPU) ────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn reset_crystal() {
        crate::manifold::clear_crystal();
    }

    #[test]
    fn phase_parse_roundtrip() {
        for p in [
            PacketPhase::Send,
            PacketPhase::Recv,
            PacketPhase::Ack,
            PacketPhase::Timeout,
        ] {
            assert_eq!(PacketPhase::from_str(p.as_str()), Some(p));
        }
        assert!(PacketPhase::from_str("nope").is_none());
    }

    #[test]
    fn direction_reverse() {
        assert_eq!(
            PacketDirection::TerpToZebra.reverse(),
            PacketDirection::ZebraToTerp
        );
        assert_eq!(
            PacketDirection::ZebraToTerp.reverse(),
            PacketDirection::TerpToZebra
        );
    }

    #[test]
    fn round_trip_has_send_recv_ack() {
        let mut sim = PacketSimulator::default();
        let samples = sim.round_trip(PacketDirection::TerpToZebra);
        assert_eq!(samples.len(), 3);
        assert_eq!(samples[0].phase, PacketPhase::Send);
        assert_eq!(samples[1].phase, PacketPhase::Recv);
        assert_eq!(samples[2].phase, PacketPhase::Ack);
        assert_eq!(samples[0].direction, PacketDirection::TerpToZebra);
        assert_eq!(samples[2].direction, PacketDirection::ZebraToTerp);
        assert_eq!(samples[0].sequence, samples[1].sequence);
        assert!(samples[0].age_secs < samples[1].age_secs);
    }

    #[test]
    fn all_scenarios_nonempty_and_layoutable() {
        let mut sim = PacketSimulator::default();
        for sc in SimScenario::all() {
            reset_crystal();
            let samples = sim.scenario(*sc);
            assert!(
                !samples.is_empty(),
                "scenario {} produced no samples",
                sc.as_str()
            );
            let layout = layout_overlay(&samples, 1280.0, 720.0, 1.0, 0);
            // Backbone + crystal ring at minimum.
            assert!(layout.nodes.len() >= 4, "{}", sc.as_str());
            assert!(layout.edges.len() >= 3, "{}", sc.as_str());
            // Positions finite and on-screen-ish (fit may pad slightly).
            for n in &layout.nodes {
                assert!(n.pos_x.is_finite() && n.pos_y.is_finite());
                assert!((-40.0..=1320.0).contains(&n.pos_x), "x={}", n.pos_x);
                assert!((-40.0..=760.0).contains(&n.pos_y), "y={}", n.pos_y);
            }
            // Edge indices in range.
            let n = layout.nodes.len() as u32;
            for &(a, b) in &layout.edges {
                assert!(a < n && b < n, "edge ({a},{b}) out of range {n}");
            }
            // Hubs resolved.
            assert!(layout.hubs.terp.0.is_finite());
            assert!(layout.hubs.zebra.0.is_finite());
        }
    }

    #[test]
    fn manifold_layout_grows_with_packets() {
        reset_crystal();
        let empty = layout_overlay(&[], 800.0, 600.0, 0.0, 0);
        let n0 = empty.nodes.len();
        let mut sim = PacketSimulator::default();
        let samples = sim.scenario(SimScenario::Burst);
        let grown = layout_overlay(&samples, 800.0, 600.0, 1.0, 0);
        assert!(grown.nodes.len() > n0);
        assert!(grown.edges.len() > empty.edges.len());
        // Second layout with same samples must not thrash positions (static crystal).
        let again = layout_overlay(&samples, 800.0, 600.0, 1.0, 0);
        assert_eq!(grown.nodes.len(), again.nodes.len());
    }

    #[test]
    fn travel_monotonic_for_send_age() {
        let s0 = PacketSample::new(
            PacketPhase::Send,
            PacketDirection::TerpToZebra,
            1,
            "channel-0",
            "08-wasm-0",
            0.0,
        );
        let s1 = PacketSample {
            age_secs: 2.0,
            ..s0.clone()
        };
        assert!(s0.travel(0.0, 0) < s1.travel(0.0, 0));
    }

    #[test]
    fn packet_xy_endpoints_match_hubs() {
        let (w, h) = (1000.0_f32, 800.0);
        let (x0, y0) = packet_xy(PacketDirection::TerpToZebra, 0.0, w, h);
        assert!((x0 - w * 0.18).abs() < 1e-3);
        assert!((y0 - h * 0.50).abs() < 1e-3);
        let (x1, y1) = packet_xy(PacketDirection::TerpToZebra, 1.0, w, h);
        assert!((x1 - w * 0.82).abs() < 1e-3);
        assert!((y1 - h * 0.50).abs() < 1e-3);
    }

    #[test]
    fn stress_respects_max_nodes() {
        reset_crystal();
        let mut sim = PacketSimulator::default();
        let samples = sim.stress(MAX_NODES + 50);
        let layout = layout_overlay(&samples, 1920.0, 1080.0, 0.5, 0);
        assert!(layout.nodes.len() <= MAX_NODES);
        assert!(layout.edges.len() <= MAX_EDGES);
    }

    #[test]
    fn base_idx_offsets_edges() {
        reset_crystal();
        let mut sim = PacketSimulator::default();
        let samples = sim.round_trip(PacketDirection::TerpToZebra);
        let layout = layout_overlay(&samples, 800.0, 600.0, 0.0, 10);
        for &(a, b) in &layout.edges {
            assert!(a >= 10 && b >= 10);
        }
    }

    #[test]
    fn expired_samples_skipped() {
        reset_crystal();
        let s = PacketSample::new(
            PacketPhase::Send,
            PacketDirection::TerpToZebra,
            1,
            "c",
            "id",
            PACKET_TTL_SECS + 1.0,
        );
        assert!(s.is_expired());
        let layout = layout_overlay(&[s], 800.0, 600.0, 0.0, 0);
        reset_crystal();
        let empty = layout_overlay(&[], 800.0, 600.0, 0.0, 0);
        // Expired samples must not grow beyond backbone crystal.
        assert_eq!(layout.nodes.len(), empty.nodes.len());
    }

    #[test]
    fn inject_json_and_scenario_into_live_feed() {
        clear_live();
        inject_json(r#"{"phase":"send","sequence":7,"channel":"channel-1"}"#).unwrap();
        assert_eq!(live_count(), 1);
        inject_json(r#"{"scenario":"round_trip"}"#).unwrap();
        assert!(live_count() >= 4); // 1 prior + 3 round-trip
        clear_live();
        assert_eq!(live_count(), 0);
    }

    #[test]
    fn inject_json_rejects_bad_phase() {
        assert!(inject_json(r#"{"phase":"nope"}"#).is_err());
    }

    #[test]
    fn shader_kinds_map_distinct() {
        use crate::shaders::RenderNode;
        let send = RenderNode::kind_to_float(PacketPhase::Send.shader_kind());
        let ack = RenderNode::kind_to_float(PacketPhase::Ack.shader_kind());
        let to = RenderNode::kind_to_float(PacketPhase::Timeout.shader_kind());
        assert_ne!(send, ack);
        assert_ne!(ack, to);
    }

    #[test]
    fn harness_matrix_counts() {
        // Document expected sizes for spawn-local-demo / HTTP scenario names.
        let mut sim = PacketSimulator::default();
        assert_eq!(sim.scenario(SimScenario::RoundTrip).len(), 3);
        assert_eq!(sim.scenario(SimScenario::Timeout).len(), 2);
        assert_eq!(sim.scenario(SimScenario::Stress).len(), 48);
        let burst = PacketSimulator::default().scenario(SimScenario::Burst);
        // 4 forward RTs (12) + 1 reverse RT (3) = 15
        assert_eq!(burst.len(), 15);
    }
}
