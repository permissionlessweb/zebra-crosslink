//! Unified GPU rendering — Erdős unit-distance crystal (static 4D topographic).
//!
//! Aesthetic (exclusive focus):
//! - Hexagonal crystal facets at fixed lattice sites
//! - Matchstick edges of unit distance only
//! - Topographic shading from packed height (no time-chaos)
//! - Quiet activation glow — no traveling shimmer, no particle arcs
//!
//! Texture packing (normalized 0–255 ≈ 0.0–1.0 when sampled as unorm):
//!   Node texture: width = MAX_NODES, height = 2
//!     row 0: pos_x_norm, pos_y_norm, activation, topo_height
//!     row 1: family/8, branch_angle_norm, reserved, reserved
//!   Edge texture: width = MAX_EDGES, height = 1
//!     RGBA: node_a_idx/MAX_NODES, node_b_idx/MAX_NODES, 0, 0

use macroquad::prelude::*;
use macroquad::window::miniquad::{BlendFactor, BlendState, BlendValue, Equation, PipelineParams};
use std::time::Instant;

pub const MAX_NODES: usize = 1024;
pub const MAX_EDGES: usize = 2048;

const VERTEX_SHADER: &str = r#"#version 100
attribute vec3 position;
attribute vec2 texcoord;
varying lowp vec2 uv;
uniform mat4 Model;
uniform mat4 Projection;

void main() {
    gl_Position = Projection * Model * vec4(position, 1.0);
    uv = texcoord;
}
"#;

/// Static crystalline fragment — no sin(time) chaos on the lattice.
const FRAGMENT_SHADER: &str = r#"#version 100
precision mediump float;

varying lowp vec2 uv;

uniform sampler2D sNodeData;
uniform sampler2D sEdgeData;
uniform float uTime;
uniform float uNodeCount;
uniform float uEdgeCount;
uniform float uPass;
uniform float uScale;
uniform vec2 uResolution;

vec4 sampleNodeRow(float col, float row) {
    float maxN = 1024.0;
    float u = (col + 0.5) / maxN;
    float v = (row + 0.5) / 2.0;
    return texture2D(sNodeData, vec2(u, v));
}

vec2 nodeScreenPos(float idx) {
    vec4 r0 = sampleNodeRow(idx, 0.0);
    return vec2(r0.r * uResolution.x, r0.g * uResolution.y);
}

float nodeActivation(float idx) {
    return sampleNodeRow(idx, 0.0).b;
}

float nodeTopo(float idx) {
    // Packed topographic height (generation) in A of row 0.
    return sampleNodeRow(idx, 0.0).a;
}

float nodeFamily(float idx) {
    return sampleNodeRow(idx, 1.0).r * 8.0;
}

float nodeAngle(float idx) {
    return (sampleNodeRow(idx, 1.0).g - 0.5) * 6.2831853;
}

// Curated crystal palette — family hues, not decomposed metric channels.
vec3 familyColor(float t) {
    if (t < 0.5) return vec3(0.92, 0.62, 0.28); // zebra
    if (t < 1.5) return vec3(0.35, 0.70, 0.98); // terp
    if (t < 2.5) return vec3(0.72, 0.82, 0.90); // core
    if (t < 3.5) return vec3(0.78, 0.55, 0.95); // route
    if (t < 4.5) return vec3(0.45, 0.88, 0.92); // channel
    if (t < 5.5) return vec3(0.70, 0.75, 0.55);
    if (t < 6.5) return vec3(0.62, 0.48, 0.95); // ack
    if (t < 7.5) return vec3(0.95, 0.40, 0.38); // impurity
    return vec3(0.65, 0.72, 0.80);
}

float hexSdf(vec2 pc) {
    vec2 p = abs(pc);
    return max(p.x * 0.8660254 + p.y * 0.5, p.y);
}

// Static hexagonal crystal contribution — size from topo, light from activation.
vec4 crystalFacet(vec2 frag, float idx) {
    vec2 p = nodeScreenPos(idx);
    float act = nodeActivation(idx);
    float topo = nodeTopo(idx);
    float fam = nodeFamily(idx);
    float ang = nodeAngle(idx);
    vec3 base = familyColor(fam);

    vec2 d = frag - p;
    // Align facets to branch angle (crystal grain), fixed.
    float ca = cos(ang);
    float sa = sin(ang);
    vec2 local = vec2(ca * d.x + sa * d.y, -sa * d.x + ca * d.y);

    // Facet size: quieter for higher topography (distant ridges slightly smaller).
    float rad = uScale * (0.70 + 0.35 * act - 0.12 * topo);
    rad = max(rad, 3.5);
    vec2 pc = local / rad;
    float hx = hexSdf(pc);
    float shape = 1.0 - smoothstep(0.72, 1.02, hx);
    float core = 1.0 - smoothstep(0.0, 0.38, length(pc));

    // Topographic cool→warm along generation (static manifold shading).
    vec3 topoTint = mix(base * 0.75, base * 1.15, topo);
    // Soft facet face + quiet core — no time shimmer.
    vec3 rgb = mix(topoTint * 0.55, vec3(0.95, 0.97, 1.0), core * act * 0.55);
    rgb = mix(rgb, base, 0.45);

    float alpha = shape * (0.40 + act * 0.55);
    // Very soft outer halo (still static).
    float halo = 1.0 - smoothstep(0.9, 2.0, length(pc));
    alpha = clamp(alpha + halo * 0.12 * act, 0.0, 1.0);
    return vec4(rgb * alpha, alpha);
}

void main() {
    vec2 frag = uv * uResolution;
    // Deep topographic void.
    float vignette = smoothstep(1.15, 0.30, length((uv - 0.5) * vec2(1.1, 1.0)));
    vec3 voidCol = vec3(0.02, 0.03, 0.05) * vignette;

    if (uPass < 0.5) {
        // --- Facets: nearest crystals only (clean, not additive chaos) ---
        float n = min(uNodeCount, 1024.0);
        float best = 1e9;
        float bestIdx = 0.0;
        float second = 1e9;
        float secondIdx = 0.0;
        for (int i = 0; i < 512; i++) {
            float fi = float(i);
            if (fi >= n) break;
            float d = distance(frag, nodeScreenPos(fi));
            if (d < best) {
                second = best;
                secondIdx = bestIdx;
                best = d;
                bestIdx = fi;
            } else if (d < second) {
                second = d;
                secondIdx = fi;
            }
        }
        vec4 c0 = crystalFacet(frag, bestIdx);
        // Optional faint second facet for dense regions — still static.
        vec4 c1 = crystalFacet(frag, secondIdx);
        vec3 rgb = voidCol + c0.rgb + c1.rgb * 0.35;
        float a = clamp(max(c0.a, c1.a * 0.35) + 0.02, 0.0, 1.0);
        gl_FragColor = vec4(rgb, a);
    } else {
        // --- Matchstick edges: pure geometric lines, no traveling wave ---
        float e = min(uEdgeCount, 2048.0);
        float best = 1e9;
        float actMix = 0.4;
        vec3 colMix = vec3(0.55, 0.72, 0.85);

        for (int i = 0; i < 768; i++) {
            float fi = float(i);
            if (fi >= e) break;
            float maxE = 2048.0;
            float maxN = 1024.0;
            vec4 edge = texture2D(sEdgeData, vec2((fi + 0.5) / maxE, 0.5));
            float aIdx = edge.r * maxN;
            float bIdx = edge.g * maxN;
            vec2 a = nodeScreenPos(aIdx);
            vec2 b = nodeScreenPos(bIdx);
            vec2 ab = b - a;
            float ab2 = max(dot(ab, ab), 1e-4);
            float tt = clamp(dot(frag - a, ab) / ab2, 0.0, 1.0);
            float d = distance(frag, a + ab * tt);
            if (d < best) {
                best = d;
                float aa = nodeActivation(aIdx);
                float ba = nodeActivation(bIdx);
                actMix = mix(aa, ba, tt);
                colMix = mix(familyColor(nodeFamily(aIdx)), familyColor(nodeFamily(bIdx)), tt);
            }
        }

        // Crisp matchstick core + soft geometric halo (static).
        float core = 1.0 - smoothstep(0.0, 1.05, best);
        float halo = 1.0 - smoothstep(0.8, 3.2, best);
        vec3 lineCol = mix(colMix * 0.55, colMix, actMix);
        float alpha = core * (0.55 + actMix * 0.35) + halo * 0.18;
        gl_FragColor = vec4(lineCol * alpha + voidCol * 0.2, clamp(alpha, 0.0, 1.0));
    }
}
"#;

pub struct GraphShaders {
    pub material: Material,
    pub node_texture: Texture2D,
    pub edge_texture: Texture2D,
    last_update: Instant,
}

impl GraphShaders {
    pub fn new() -> Self {
        let pipeline_params = PipelineParams {
            color_blend: Some(BlendState::new(
                Equation::Add,
                BlendFactor::Value(BlendValue::SourceAlpha),
                BlendFactor::OneMinusValue(BlendValue::SourceAlpha),
            )),
            ..Default::default()
        };

        let material = load_material(
            ShaderSource::Glsl {
                vertex: VERTEX_SHADER,
                fragment: FRAGMENT_SHADER,
            },
            MaterialParams {
                uniforms: vec![
                    UniformDesc::new("uTime", UniformType::Float1),
                    UniformDesc::new("uNodeCount", UniformType::Float1),
                    UniformDesc::new("uEdgeCount", UniformType::Float1),
                    UniformDesc::new("uPass", UniformType::Float1),
                    UniformDesc::new("uScale", UniformType::Float1),
                    UniformDesc::new("uResolution", UniformType::Float2),
                ],
                textures: vec!["sNodeData".to_string(), "sEdgeData".to_string()],
                pipeline_params,
            },
        )
        .expect("unit-distance crystal material compilation failed");

        let node_bytes = vec![0u8; MAX_NODES * 2 * 4];
        let edge_bytes = vec![0u8; MAX_EDGES * 1 * 4];
        let node_texture = Texture2D::from_rgba8(MAX_NODES as u16, 2, &node_bytes);
        let edge_texture = Texture2D::from_rgba8(MAX_EDGES as u16, 1, &edge_bytes);
        node_texture.set_filter(FilterMode::Nearest);
        edge_texture.set_filter(FilterMode::Nearest);

        Self {
            material,
            node_texture,
            edge_texture,
            last_update: Instant::now(),
        }
    }

    pub fn update_node_texture(
        &self,
        nodes: &[RenderNode],
        _time: f32,
        screen_w: f32,
        screen_h: f32,
    ) {
        let count = nodes.len().min(MAX_NODES);
        let mut bytes = vec![0u8; MAX_NODES * 2 * 4];
        let w = screen_w.max(1.0);
        let h = screen_h.max(1.0);

        for (i, node) in nodes.iter().enumerate().take(count) {
            let x = (node.pos_x / w).clamp(0.0, 1.0);
            let y = (node.pos_y / h).clamp(0.0, 1.0);
            let act = node.pulse.clamp(0.0, 1.0);
            // last_pulse_time carries topographic height from manifold.
            let topo = node.last_pulse_time.clamp(0.0, 1.0);
            let ty = (node.node_type_float / 8.0).clamp(0.0, 1.0);
            let ang = ((node.parent_angle / (std::f32::consts::TAU)) + 0.5).clamp(0.0, 1.0);

            let r0 = i * 4;
            bytes[r0] = (x * 255.0) as u8;
            bytes[r0 + 1] = (y * 255.0) as u8;
            bytes[r0 + 2] = (act * 255.0) as u8;
            bytes[r0 + 3] = (topo * 255.0) as u8;

            let r1 = (MAX_NODES + i) * 4;
            bytes[r1] = (ty * 255.0) as u8;
            bytes[r1 + 1] = (ang * 255.0) as u8;
            bytes[r1 + 2] = 0;
            bytes[r1 + 3] = 255;
        }

        self.node_texture
            .update_from_bytes(MAX_NODES as u32, 2, &bytes);
    }

    pub fn update_edge_texture(&self, edges: &[(u32, u32)], edge_count: usize) {
        let count = edge_count.min(MAX_EDGES);
        let mut bytes = vec![0u8; MAX_EDGES * 4];
        let max_n = MAX_NODES as f32;

        for (i, &(a, b)) in edges.iter().enumerate().take(count) {
            let base = i * 4;
            bytes[base] = ((a as f32 / max_n) * 255.0) as u8;
            bytes[base + 1] = ((b as f32 / max_n) * 255.0) as u8;
            bytes[base + 2] = 0;
            bytes[base + 3] = 255;
        }

        self.edge_texture
            .update_from_bytes(MAX_EDGES as u32, 1, &bytes);
    }

    pub fn render(&self, node_count: u32, edge_count: u32, screen_w: f32, screen_h: f32) {
        gl_use_material(&self.material);

        self.material.set_uniform("uTime", get_time() as f32);
        self.material.set_uniform("uNodeCount", node_count as f32);
        self.material.set_uniform("uEdgeCount", edge_count as f32);
        // Facet scale for static hex crystals.
        self.material.set_uniform("uScale", 18.0f32);
        self.material
            .set_uniform("uResolution", vec2(screen_w, screen_h));
        self.material
            .set_texture("sNodeData", self.node_texture.clone());
        self.material
            .set_texture("sEdgeData", self.edge_texture.clone());

        // Edges under, facets over.
        self.material.set_uniform("uPass", 1.0f32);
        draw_rectangle(0.0, 0.0, screen_w, screen_h, WHITE);

        self.material.set_uniform("uPass", 0.0f32);
        draw_rectangle(0.0, 0.0, screen_w, screen_h, WHITE);

        gl_use_default_material();
        let _ = self.last_update;
    }
}

impl Default for GraphShaders {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone)]
pub struct RenderNode {
    pub pos_x: f32,
    pub pos_y: f32,
    /// Activation (static glow), not a traveling particle intensity.
    pub pulse: f32,
    /// Topographic height packing (0–1).
    pub last_pulse_time: f32,
    pub node_type_float: f32,
    pub parent_angle: f32,
}

impl RenderNode {
    pub fn kind_to_float(kind: &str) -> f32 {
        match kind {
            "consensus" | "bft" | "zebra" => 0.0,
            "api_call" | "terp" => 1.0,
            "block" | "core" => 2.0,
            "tx" | "ibc_packet" | "packet" => 3.0,
            "p2p" | "ibc_channel" | "channel" => 4.0,
            "store_read" => 5.0,
            "abci" | "ibc_ack" | "ack" => 6.0,
            "rust_compute" | "ibc_timeout" | "timeout" => 7.0,
            _ => 8.0,
        }
    }
}
