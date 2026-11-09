//! Erdős unit-distance crystalline manifold (artistic core).
//!
//! Design intent (TDXL / unit-distance conjecture flavour):
//! - **Static crystals** — a vertex is placed once and never moves
//! - **Growth only** — new events append unit-distance children (matchstick lattice)
//! - **4D topography** — each node lives in R⁴ (x,y,z,w); screen is a fixed
//!   orthographic/topographic projection, not a particle simulation
//! - **No chaotic value-decomposition** — no traveling dots, no per-frame
//!   shimmer, no arc lerps. Edges exist iff lattice distance ≈ 1
//!
//! Live traffic only **activates** (dims/brightens) existing sites or **grows**
//! a new facet. The geometry itself is a quiet, growing snowflake.

use std::collections::HashMap;
use std::f32::consts::PI;
use std::sync::Mutex;

use crate::packet_feed::{PacketDirection, PacketPhase, PacketSample};
use crate::shaders::{RenderNode, MAX_EDGES, MAX_NODES};

/// Lattice step (Erdős unit-distance edge length in lattice units).
pub const UNIT_RADIUS: f32 = 1.0;
/// Hex matchstick branch quantum (60°).
pub const BRANCH_ANGLE: f32 = PI / 3.0;
/// Collision radius on the unit-distance graph.
pub const COLLISION_EPS: f32 = 0.08;
/// Screen fit margin.
pub const FIT_MARGIN: f32 = 0.10;

/// Fixed 4D → 2D topographic projection angles (static — not time-varying).
/// These curate the “almost 4D manifold” silhouette without chaos.
const PROJ_XW: f32 = 0.42; // w folds into x
const PROJ_YW: f32 = 0.18; // w folds into y
const PROJ_Z: f32 = 0.62; // z is topographic elevation (vertical bias)
const PROJ_YZ: f32 = 0.22; // slight z→x shear for depth

// ─── 4D lattice node ────────────────────────────────────────────────────────

/// One crystalline site in R⁴ with unit-distance parent linkage.
#[derive(Clone, Debug)]
pub struct CrystalNode {
    pub key: String,
    /// Unit-distance plane coordinates (Erdős graph).
    pub x: f32,
    pub y: f32,
    /// Topographic height = generation depth from root.
    pub z: f32,
    /// Family / solid-angle coordinate (branch lineage, phase class).
    pub w: f32,
    /// Placement angle used for the unit-distance step from parent.
    pub branch_angle: f32,
    /// Visual family (maps to crystalline hue, not chaotic channels).
    pub family: CrystalFamily,
    /// Static activation in [0,1] — recent hits raise it; rests at a floor.
    pub activation: f32,
    pub parent: Option<usize>,
    /// Generation index (0 = root).
    pub generation: u32,
}

/// Crystal family → hue / facet role (curated, not per-attribute RGB noise).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CrystalFamily {
    /// Lattice origin / crosslink core.
    Core,
    /// Terp host branch.
    Terp,
    /// Zebra / PoW branch.
    Zebra,
    /// Channel / connection spine.
    Channel,
    /// Packet route spurs (send family).
    Route,
    /// Ack facet (cooler spur).
    Ack,
    /// Timeout impurity (sharp spur).
    Impurity,
}

impl CrystalFamily {
    pub fn kind_float(self) -> f32 {
        match self {
            Self::Zebra => 0.0,
            Self::Terp => 1.0,
            Self::Core => 2.0,
            Self::Route => 3.0,
            Self::Channel => 4.0,
            Self::Ack => 6.0,
            Self::Impurity => 7.0,
        }
    }

    fn from_phase(phase: PacketPhase) -> Self {
        match phase {
            PacketPhase::Timeout => Self::Impurity,
            PacketPhase::Ack => Self::Ack,
            PacketPhase::Send | PacketPhase::Recv => Self::Route,
        }
    }
}

// ─── Manifold ───────────────────────────────────────────────────────────────

/// Growing unit-distance crystal (persistent across frames).
#[derive(Clone, Debug, Default)]
pub struct Manifold {
    pub nodes: Vec<CrystalNode>,
    /// Only parent→child unit-distance edges (matchstick graph).
    pub edges: Vec<(usize, usize)>,
    index: HashMap<String, usize>,
}

impl Manifold {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn get(&self, key: &str) -> Option<usize> {
        self.index.get(key).copied()
    }

    /// Seed the origin crystal + three primary facets (static backbone).
    pub fn seed_backbone(&mut self) {
        if self.get("root").is_some() {
            return;
        }
        self.insert_root("root", CrystalFamily::Core, 0.0);
        // Primary hex arms — unit distance, 0° / 120° / 240° for triangular balance
        // within the hex lattice (still π/3 quantum via placement).
        self.grow_child("terp", "root", PI, CrystalFamily::Terp, 0.0);
        self.grow_child("zebra", "root", 0.0, CrystalFamily::Zebra, 0.0);
        self.grow_child("channel", "root", PI * 0.5, CrystalFamily::Channel, 0.0);

        // Seed crystal ring: one hex layer around each hub for density without chaos.
        for (i, parent) in ["terp", "zebra", "channel"].iter().enumerate() {
            for k in 0..2 {
                let ang = (i as f32 + k as f32) * BRANCH_ANGLE + BRANCH_ANGLE * 0.5;
                let key = format!("facet:{parent}:{k}");
                self.grow_child(&key, parent, ang, CrystalFamily::Core, (k as f32) * 0.15);
            }
        }

        // Quiet resting activation on backbone.
        for n in &mut self.nodes {
            n.activation = match n.family {
                CrystalFamily::Core => 0.45,
                CrystalFamily::Terp | CrystalFamily::Zebra => 0.55,
                CrystalFamily::Channel => 0.40,
                _ => 0.35,
            };
        }
    }

    fn insert_root(&mut self, key: impl Into<String>, family: CrystalFamily, w: f32) -> usize {
        let key = key.into();
        if let Some(&i) = self.index.get(&key) {
            return i;
        }
        let idx = self.nodes.len();
        self.index.insert(key.clone(), idx);
        self.nodes.push(CrystalNode {
            key,
            x: 0.0,
            y: 0.0,
            z: 0.0,
            w,
            branch_angle: 0.0,
            family,
            activation: 0.45,
            parent: None,
            generation: 0,
        });
        idx
    }

    /// Grow a unit-distance child. Idempotent on `key` (static: no move if exists).
    pub fn grow_child(
        &mut self,
        key: impl Into<String>,
        parent_key: &str,
        preferred_angle: f32,
        family: CrystalFamily,
        w_bias: f32,
    ) -> Option<usize> {
        let key = key.into();
        if let Some(&i) = self.index.get(&key) {
            return Some(i);
        }
        if self.nodes.len() >= MAX_NODES {
            return None;
        }
        let parent = *self.index.get(parent_key)?;
        let (px, py, pz, p_gen, p_angle) = {
            let p = &self.nodes[parent];
            (p.x, p.y, p.z, p.generation, p.branch_angle)
        };
        let angle = if preferred_angle.is_finite() {
            preferred_angle
        } else {
            p_angle + BRANCH_ANGLE
        };
        let existing: Vec<(f32, f32)> = self.nodes.iter().map(|n| (n.x, n.y)).collect();
        let (x, y, used_angle) =
            unit_distance_child_coords(px, py, &existing, angle, UNIT_RADIUS, COLLISION_EPS);
        // 4th + 3rd coords: topographic generation + family solid angle.
        let z = pz + 1.0; // each generation steps up the manifold
        let w = w_bias + used_angle / PI; // lineage folds into w
        let idx = self.nodes.len();
        self.index.insert(key.clone(), idx);
        self.nodes.push(CrystalNode {
            key,
            x,
            y,
            z,
            w,
            branch_angle: used_angle,
            family,
            activation: 0.5,
            parent: Some(parent),
            generation: p_gen + 1,
        });
        self.edges.push((parent, idx));
        Some(idx)
    }

    /// Soft static activation (no exponential thrash).
    pub fn activate(&mut self, idx: usize, amount: f32) {
        if let Some(n) = self.nodes.get_mut(idx) {
            n.activation = (n.activation * 0.7 + amount.clamp(0.0, 1.0) * 0.55).clamp(0.22, 1.0);
        }
    }

    pub fn activate_key(&mut self, key: &str, amount: f32) {
        if let Some(i) = self.get(key) {
            self.activate(i, amount);
        }
    }

    /// Rest all sites toward a quiet crystalline floor (slow, not chaotic decay).
    pub fn rest(&mut self) {
        for n in &mut self.nodes {
            let floor = match n.family {
                CrystalFamily::Core => 0.38,
                CrystalFamily::Terp | CrystalFamily::Zebra => 0.42,
                CrystalFamily::Channel => 0.34,
                CrystalFamily::Impurity => 0.28,
                _ => 0.30,
            };
            n.activation = n.activation * 0.985 + floor * 0.015;
        }
    }

    /// Project R⁴ → R² (static topographic projection).
    pub fn project2(&self, n: &CrystalNode) -> (f32, f32) {
        // Quiet 4D fold — fixed coefficients, no time term.
        let u = n.x + PROJ_XW * n.w - PROJ_YZ * n.z;
        let v = n.y + PROJ_YW * n.w + PROJ_Z * n.z;
        (u, v)
    }

    /// Axis-aligned bounds in projected space.
    pub fn projected_bounds(&self) -> (f32, f32, f32, f32) {
        if self.nodes.is_empty() {
            return (-1.0, -1.0, 1.0, 1.0);
        }
        let mut min_u = f32::INFINITY;
        let mut min_v = f32::INFINITY;
        let mut max_u = f32::NEG_INFINITY;
        let mut max_v = f32::NEG_INFINITY;
        for n in &self.nodes {
            let (u, v) = self.project2(n);
            min_u = min_u.min(u);
            min_v = min_v.min(v);
            max_u = max_u.max(u);
            max_v = max_v.max(v);
        }
        if (max_u - min_u).abs() < 1e-3 {
            min_u -= 1.0;
            max_u += 1.0;
        }
        if (max_v - min_v).abs() < 1e-3 {
            min_v -= 1.0;
            max_v += 1.0;
        }
        (min_u, min_v, max_u, max_v)
    }

    /// Fit projected crystal to screen (auto-dolly as the lattice grows).
    pub fn fit_to_screen(&self, screen_w: f32, screen_h: f32) -> Vec<(f32, f32)> {
        let (min_u, min_v, max_u, max_v) = self.projected_bounds();
        let cu = (min_u + max_u) * 0.5;
        let cv = (min_v + max_v) * 0.5;
        let span_u = (max_u - min_u).max(UNIT_RADIUS);
        let span_v = (max_v - min_v).max(UNIT_RADIUS);
        let usable_w = screen_w * (1.0 - 2.0 * FIT_MARGIN);
        let usable_h = screen_h * (1.0 - 2.0 * FIT_MARGIN);
        let scale = (usable_w / span_u).min(usable_h / span_v);
        let ox = screen_w * 0.5;
        let oy = screen_h * 0.5;
        self.nodes
            .iter()
            .map(|n| {
                let (u, v) = self.project2(n);
                (ox + (u - cu) * scale, oy + (v - cv) * scale)
            })
            .collect()
    }

    pub fn to_render(
        &self,
        screen_w: f32,
        screen_h: f32,
        base_idx: u32,
    ) -> (Vec<RenderNode>, Vec<(u32, u32)>) {
        let screen = self.fit_to_screen(screen_w, screen_h);
        let mut nodes = Vec::with_capacity(self.nodes.len().min(MAX_NODES));
        for (i, n) in self.nodes.iter().enumerate().take(MAX_NODES) {
            let (sx, sy) = screen[i];
            // Size cue from generation (topography), not from chaotic pulse noise.
            let topo = (n.z / (1.0 + n.z)).clamp(0.0, 1.0);
            nodes.push(RenderNode {
                pos_x: sx,
                pos_y: sy,
                pulse: n.activation.clamp(0.0, 1.0),
                last_pulse_time: topo, // pack topographic height into last_hit channel
                node_type_float: n.family.kind_float(),
                parent_angle: n.branch_angle,
            });
        }
        let edges: Vec<(u32, u32)> = self
            .edges
            .iter()
            .filter_map(|&(a, b)| {
                if a < nodes.len() && b < nodes.len() {
                    Some((a as u32 + base_idx, b as u32 + base_idx))
                } else {
                    None
                }
            })
            .take(MAX_EDGES)
            .collect();
        (nodes, edges)
    }

    pub fn hub_screen(&self, screen_w: f32, screen_h: f32) -> HubScreen {
        let pts = self.fit_to_screen(screen_w, screen_h);
        let pos = |key: &str, fb: (f32, f32)| {
            self.get(key)
                .and_then(|i| pts.get(i).copied())
                .unwrap_or(fb)
        };
        HubScreen {
            terp: pos("terp", (screen_w * 0.25, screen_h * 0.5)),
            zebra: pos("zebra", (screen_w * 0.75, screen_h * 0.5)),
            channel: pos("channel", (screen_w * 0.5, screen_h * 0.28)),
            root: pos("root", (screen_w * 0.5, screen_h * 0.5)),
        }
    }
}

#[derive(Clone, Debug)]
pub struct HubScreen {
    pub terp: (f32, f32),
    pub zebra: (f32, f32),
    pub channel: (f32, f32),
    pub root: (f32, f32),
}

// ─── Unit-distance placement ────────────────────────────────────────────────

/// Place a child at exact unit distance with hex collision fallbacks (matchstick).
pub fn unit_distance_child_coords(
    parent_x: f32,
    parent_y: f32,
    existing: &[(f32, f32)],
    preferred_angle: f32,
    radius: f32,
    epsilon: f32,
) -> (f32, f32, f32) {
    for k in 0..6 {
        let ang = preferred_angle + k as f32 * BRANCH_ANGLE;
        let cx = parent_x + radius * ang.cos();
        let cy = parent_y + radius * ang.sin();
        let collides = existing
            .iter()
            .any(|&(ex, ey)| (cx - ex).hypot(cy - ey) < epsilon);
        if !collides {
            return (cx, cy, ang);
        }
    }
    let ang = preferred_angle;
    (
        parent_x + radius * ang.cos(),
        parent_y + radius * ang.sin(),
        ang,
    )
}

pub fn lattice_dist(ax: f32, ay: f32, bx: f32, by: f32) -> f32 {
    (ax - bx).hypot(ay - by)
}

// ─── Persistent crystal (process-wide) ──────────────────────────────────────

lazy_static::lazy_static! {
    static ref CRYSTAL: Mutex<Manifold> = Mutex::new(Manifold::new());
}

/// Access the process-wide growing crystal (for tests / reset).
pub fn clear_crystal() {
    let mut g = CRYSTAL.lock().unwrap();
    *g = Manifold::new();
}

/// Ensure backbone exists; return a clone for pure layout tests.
pub fn crystal_snapshot() -> Manifold {
    let mut g = CRYSTAL.lock().unwrap();
    if g.is_empty() {
        g.seed_backbone();
    }
    g.clone()
}

/// Apply live samples: **grow** new sites for new routes; **activate** on hits.
/// Never places traveling particles. Geometry stays static once grown.
pub fn grow_from_samples(samples: &[PacketSample]) {
    let mut g = CRYSTAL.lock().unwrap();
    if g.is_empty() {
        g.seed_backbone();
    }
    g.rest();

    // Unique routes (channel, sequence) grow a fixed spur; phases only activate.
    let mut seen_routes: HashMap<(String, u64), ()> = HashMap::new();
    for (route_i, s) in samples.iter().filter(|s| !s.is_expired()).enumerate() {
        let route_key = (s.channel.clone(), s.sequence);
        let hub = match s.direction {
            PacketDirection::TerpToZebra => "terp",
            PacketDirection::ZebraToTerp => "zebra",
        };
        let family = CrystalFamily::from_phase(s.phase);
        let anchor = format!("route:{}:{}", s.channel, s.sequence);

        if !seen_routes.contains_key(&route_key) {
            seen_routes.insert(route_key, ());
            // Hex family angle from sequence — deterministic, not random.
            let spur = match s.direction {
                PacketDirection::TerpToZebra => PI + BRANCH_ANGLE * ((s.sequence % 6) as f32),
                PacketDirection::ZebraToTerp => BRANCH_ANGLE * ((s.sequence % 6) as f32),
            };
            let w_bias = (s.sequence as f32 * 0.07) + (route_i as f32 * 0.03);
            g.grow_child(&anchor, hub, spur, CrystalFamily::Channel, w_bias);

            // Mid + tip: two more unit-distance steps (static path, not animation).
            let mid = format!("route:{}:{}:mid", s.channel, s.sequence);
            let tip = format!("route:{}:{}:tip", s.channel, s.sequence);
            let mid_ang = spur + BRANCH_ANGLE * if s.sequence % 2 == 0 { 1.0 } else { -1.0 };
            g.grow_child(&mid, &anchor, mid_ang, CrystalFamily::Route, w_bias + 0.2);
            g.grow_child(
                &tip,
                &mid,
                mid_ang + BRANCH_ANGLE * 0.5,
                family,
                w_bias + 0.4,
            );
        }

        // Activation only — which facet lights for this phase (no motion).
        let site = match s.phase {
            PacketPhase::Send => format!("route:{}:{}", s.channel, s.sequence),
            PacketPhase::Recv => format!("route:{}:{}:mid", s.channel, s.sequence),
            PacketPhase::Ack => format!("route:{}:{}:tip", s.channel, s.sequence),
            PacketPhase::Timeout => format!("route:{}:{}:mid", s.channel, s.sequence),
        };
        let amount = match s.phase {
            PacketPhase::Timeout => 0.95,
            PacketPhase::Ack => 0.75,
            PacketPhase::Send => 0.85,
            PacketPhase::Recv => 0.80,
        };
        // Live traffic slightly brighter floor; still static crystal.
        let amount = if s.source.is_live() {
            amount
        } else {
            amount * 0.75
        };
        g.activate_key(&site, amount);
        // Soft-activate hub of origin.
        g.activate_key(hub, 0.55);
        g.activate_key("root", 0.40);
        g.activate_key("channel", 0.45);

        if g.len() >= MAX_NODES {
            break;
        }
    }
}

/// Layout for the shader: grow crystal from samples, then project.
pub fn layout_crystal(
    samples: &[PacketSample],
    screen_w: f32,
    screen_h: f32,
    base_idx: u32,
) -> (Manifold, Vec<RenderNode>, Vec<(u32, u32)>, HubScreen) {
    grow_from_samples(samples);
    let m = crystal_snapshot();
    let hubs = m.hub_screen(screen_w, screen_h);
    let (nodes, edges) = m.to_render(screen_w, screen_h, base_idx);
    (m, nodes, edges, hubs)
}

/// Pure one-shot manifold (no process state) — for unit tests.
pub fn build_packet_manifold(samples: &[PacketSample], _time: f32) -> Manifold {
    let mut m = Manifold::new();
    m.seed_backbone();
    // Inline growth without global lock (test isolation).
    let mut seen = HashMap::new();
    for (route_i, s) in samples.iter().filter(|s| !s.is_expired()).enumerate() {
        let rk = (s.channel.clone(), s.sequence);
        let hub = match s.direction {
            PacketDirection::TerpToZebra => "terp",
            PacketDirection::ZebraToTerp => "zebra",
        };
        let family = CrystalFamily::from_phase(s.phase);
        let anchor = format!("route:{}:{}", s.channel, s.sequence);
        if !seen.contains_key(&rk) {
            seen.insert(rk, ());
            let spur = match s.direction {
                PacketDirection::TerpToZebra => PI + BRANCH_ANGLE * ((s.sequence % 6) as f32),
                PacketDirection::ZebraToTerp => BRANCH_ANGLE * ((s.sequence % 6) as f32),
            };
            let w_bias = s.sequence as f32 * 0.07 + route_i as f32 * 0.03;
            m.grow_child(&anchor, hub, spur, CrystalFamily::Channel, w_bias);
            let mid = format!("route:{}:{}:mid", s.channel, s.sequence);
            let tip = format!("route:{}:{}:tip", s.channel, s.sequence);
            let mid_ang = spur + BRANCH_ANGLE * if s.sequence % 2 == 0 { 1.0 } else { -1.0 };
            m.grow_child(&mid, &anchor, mid_ang, CrystalFamily::Route, w_bias + 0.2);
            m.grow_child(
                &tip,
                &mid,
                mid_ang + BRANCH_ANGLE * 0.5,
                family,
                w_bias + 0.4,
            );
        }
        let site = match s.phase {
            PacketPhase::Send => format!("route:{}:{}", s.channel, s.sequence),
            PacketPhase::Recv | PacketPhase::Timeout => {
                format!("route:{}:{}:mid", s.channel, s.sequence)
            }
            PacketPhase::Ack => format!("route:{}:{}:tip", s.channel, s.sequence),
        };
        m.activate_key(&site, 0.8);
        if m.len() >= MAX_NODES {
            break;
        }
    }
    m
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet_feed::{PacketSimulator, SimScenario};

    #[test]
    fn unit_distance_child_is_exactly_radius() {
        let existing = vec![(0.0, 0.0)];
        let (x, y, _) =
            unit_distance_child_coords(0.0, 0.0, &existing[1..], 0.0, UNIT_RADIUS, COLLISION_EPS);
        assert!((lattice_dist(0.0, 0.0, x, y) - UNIT_RADIUS).abs() < 1e-5);
    }

    #[test]
    fn collision_falls_back_to_hex_offset() {
        let parent = (0.0_f32, 0.0);
        let occupied = (UNIT_RADIUS, 0.0);
        let existing = vec![parent, occupied];
        let (x, y, ang) = unit_distance_child_coords(
            parent.0,
            parent.1,
            &existing,
            0.0,
            UNIT_RADIUS,
            COLLISION_EPS,
        );
        assert!((ang - BRANCH_ANGLE).abs() < 1e-4);
        assert!((lattice_dist(parent.0, parent.1, x, y) - UNIT_RADIUS).abs() < 1e-5);
        assert!(lattice_dist(x, y, occupied.0, occupied.1) > COLLISION_EPS);
    }

    #[test]
    fn all_edges_are_unit_distance() {
        let m = build_packet_manifold(&[], 0.0);
        for &(a, b) in &m.edges {
            let d = lattice_dist(m.nodes[a].x, m.nodes[a].y, m.nodes[b].x, m.nodes[b].y);
            assert!(
                (d - UNIT_RADIUS).abs() < 1e-4,
                "edge {a}-{b} dist={d} keys {}-{}",
                m.nodes[a].key,
                m.nodes[b].key
            );
        }
    }

    #[test]
    fn crystal_grows_statically_with_packets() {
        let empty = build_packet_manifold(&[], 0.0);
        let mut sim = PacketSimulator::default();
        let samples = sim.scenario(SimScenario::Burst);
        let grown = build_packet_manifold(&samples, 0.0);
        assert!(grown.len() > empty.len());
        // Positions of backbone keys must match (static).
        for key in ["root", "terp", "zebra", "channel"] {
            let a = empty.get(key).unwrap();
            let b = grown.get(key).unwrap();
            assert!((empty.nodes[a].x - grown.nodes[b].x).abs() < 1e-5);
            assert!((empty.nodes[a].y - grown.nodes[b].y).abs() < 1e-5);
        }
        for &(a, b) in &grown.edges {
            let d = lattice_dist(
                grown.nodes[a].x,
                grown.nodes[a].y,
                grown.nodes[b].x,
                grown.nodes[b].y,
            );
            assert!((d - UNIT_RADIUS).abs() < 1e-3);
        }
    }

    #[test]
    fn four_d_projection_is_deterministic() {
        let mut sim = PacketSimulator::default();
        let samples = sim.scenario(SimScenario::RoundTrip);
        let a = build_packet_manifold(&samples, 0.0);
        let b = build_packet_manifold(&samples, 99.0); // time ignored
        assert_eq!(a.len(), b.len());
        for (na, nb) in a.nodes.iter().zip(b.nodes.iter()) {
            assert_eq!(na.key, nb.key);
            assert!((na.x - nb.x).abs() < 1e-5);
            assert!((na.y - nb.y).abs() < 1e-5);
            assert!((na.z - nb.z).abs() < 1e-5);
            assert!((na.w - nb.w).abs() < 1e-5);
            let (ua, va) = a.project2(na);
            let (ub, vb) = b.project2(nb);
            assert!((ua - ub).abs() < 1e-5 && (va - vb).abs() < 1e-5);
        }
    }

    #[test]
    fn no_ephemeral_particle_keys() {
        let mut sim = PacketSimulator::default();
        let samples = sim.scenario(SimScenario::Burst);
        let m = build_packet_manifold(&samples, 1.0);
        for n in &m.nodes {
            assert!(
                !n.key.starts_with("pkt:"),
                "ephemeral particle key leaked: {}",
                n.key
            );
        }
    }

    #[test]
    fn fit_to_screen_keeps_points_on_canvas() {
        let mut sim = PacketSimulator::default();
        let samples = sim.scenario(SimScenario::PingPong);
        let m = build_packet_manifold(&samples, 0.0);
        let (w, h) = (1280.0_f32, 720.0);
        for (x, y) in m.fit_to_screen(w, h) {
            assert!(x.is_finite() && y.is_finite());
            assert!((-30.0..=w + 30.0).contains(&x));
            assert!((-30.0..=h + 30.0).contains(&y));
        }
    }

    #[test]
    fn to_render_respects_caps() {
        let mut sim = PacketSimulator::default();
        let samples = sim.stress(80);
        let m = build_packet_manifold(&samples, 0.0);
        let (nodes, edges) = m.to_render(1920.0, 1080.0, 0);
        assert!(nodes.len() <= MAX_NODES);
        assert!(edges.len() <= MAX_EDGES);
        let n = nodes.len() as u32;
        for &(a, b) in &edges {
            assert!(a < n && b < n);
        }
    }

    #[test]
    fn generation_increases_along_spur() {
        let mut sim = PacketSimulator::default();
        let samples = sim.scenario(SimScenario::RoundTrip);
        let m = build_packet_manifold(&samples, 0.0);
        let root_g = m.nodes[m.get("root").unwrap()].generation;
        let terp_g = m.nodes[m.get("terp").unwrap()].generation;
        assert_eq!(root_g, 0);
        assert_eq!(terp_g, 1);
        if let Some(i) = m.get("route:channel-0:1:tip") {
            assert!(m.nodes[i].generation > terp_g);
            assert!(m.nodes[i].z > m.nodes[m.get("terp").unwrap()].z);
        }
    }
}
