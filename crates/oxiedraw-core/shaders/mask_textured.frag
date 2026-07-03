#version 450

layout(location = 0) in vec2 v_tip;             // raw quad corner [-1,+1]
layout(location = 1) in vec2 v_canvas_px;       // canvas-space pixel position
layout(location = 2) in vec4 v_color;
layout(location = 3) in float v_flow;
layout(location = 4) in float v_hardness;
layout(location = 5) in float v_tip_kind;
layout(location = 6) in float v_tex_scale;
layout(location = 7) in float v_tex_strength;

layout(set = 0, binding = 0) uniform sampler2DArray u_atlas;

layout(push_constant) uniform Push {
    vec2 inv_size;
    uint slice;
} push;

layout(location = 0) out vec4 out_color;

// Procedural tip coverage: round (length) or square (chebyshev) footprint
// feathered by hardness. Edge sits at dist == 1.0.
float tip_coverage(vec2 q, float hardness, float kind) {
    float dist = (kind > 0.5) ? max(abs(q.x), abs(q.y)) : length(q);
    float aa = 0.04;
    float inner = min(hardness, 1.0 - aa);
    return 1.0 - smoothstep(inner, 1.0, dist);
}

// Global grain sampled in canvas space so it is continuous across the
// whole stroke rather than repeating per dab. strength=0 -> tip only.
float grain_factor() {
    if (v_tex_scale <= 0.0 || v_tex_strength <= 0.0) {
        return 1.0;
    }
    vec2 uv = v_canvas_px / v_tex_scale;
    // Force the base mip: the grain is minified in canvas space and the
    // mip chain would otherwise average halftone dots / grit into flat
    // grey. LOD 0 keeps the pattern crisp.
    float g = textureLod(u_atlas, vec3(uv, float(push.slice)), 0.0).a;
    return mix(1.0, g, v_tex_strength);
}

void main() {
    float coverage = tip_coverage(v_tip, v_hardness, v_tip_kind) * grain_factor();
    // R8 mask: only the R channel survives, combined across dabs by the
    // pipeline blend (MAX normally, OVER for build-up). The grain is
    // canvas-anchored, so overlapping dabs agree on it and the stroke reads
    // as one continuous global texture either way.
    out_color = vec4(coverage * v_flow, 0.0, 0.0, 0.0);
}
