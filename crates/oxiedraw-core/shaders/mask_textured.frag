#version 450

layout(location = 0) in vec2 v_tip;             // raw quad corner [-1,+1]
layout(location = 1) in vec2 v_canvas_px;       // canvas-space pixel position
layout(location = 2) in vec4 v_color;
layout(location = 3) in float v_flow;
layout(location = 4) in float v_hardness;
layout(location = 5) in float v_tip_kind;
layout(location = 6) in float v_tex_scale;
layout(location = 7) in float v_tex_strength;
layout(location = 8) in float v_radius;
layout(location = 9) in vec2 v_tip_uv;
layout(location = 10) in float v_tex_mode;      // 0 multiply, 1 subtract

layout(set = 0, binding = 0) uniform sampler2DArray u_atlas;

layout(push_constant) uniform Push {
    vec2 inv_size;
    uint slice;      // grain / texture slice (NO_SLICE = none)
    uint tip_slice;  // stamped image-tip slice (NO_SLICE = procedural tip)
} push;

// Matches renderer::dab::NO_SLICE.
const uint NO_SLICE = 0xFFFFFFFFu;

layout(location = 0) out vec4 out_color;

// Round tip ported from Krita's KisCircleMaskGenerator::valueAt (solid core
// of radius fade*R, falloff linear in squared distance). `q` is the raw quad
// corner in [-1,+1]; scale by radius to work in canvas pixels for the +1px AA.
// Square tip keeps a simple chebyshev falloff.
float tip_coverage(vec2 q, float hardness, float kind, float radius) {
    if (kind > 0.5) {
        float dist = max(abs(q.x), abs(q.y));
        float inner = min(hardness, 0.96);
        return 1.0 - smoothstep(inner, 1.0, dist);
    }
    float r = max(radius, 1e-4);
    vec2 local = q * r;
    float n = dot(local, local) / (r * r);
    if (n > 1.0) {
        return 0.0;
    }
    float fade = max(hardness, 0.01);
    float invFadeR = 1.0 / (fade * r);
    vec2 aa = abs(local) + vec2(1.0);
    float nf = dot(aa, aa) * invFadeR * invFadeR;
    if (nf < 1.0) {
        return 1.0;
    }
    return 1.0 - n * (nf - 1.0) / max(nf - n, 1e-6);
}

// Base dab coverage: procedural round/square tip, or a stamped image tip
// sampled in dab-local space (Krita predefined-tip brushes).
float base_coverage() {
    if (push.tip_slice != NO_SLICE) {
        vec2 uv = clamp(v_tip_uv, 0.0, 1.0);
        return texture(u_atlas, vec3(uv, float(push.tip_slice))).a;
    }
    return tip_coverage(v_tip, v_hardness, v_tip_kind, v_radius);
}

// Canvas-anchored texture pattern (baked brightness/contrast), applied to
// coverage the way Krita's texture option does: MULTIPLY darkens, SUBTRACT
// carves holes. Force base mip so the minified grain stays crisp.
float apply_texture(float coverage) {
    if (push.slice == NO_SLICE || v_tex_scale <= 0.0 || v_tex_strength <= 0.0) {
        return coverage;
    }
    vec2 uv = v_canvas_px / v_tex_scale;
    float g = textureLod(u_atlas, vec3(uv, float(push.slice)), 0.0).a;
    if (v_tex_mode > 0.5) {
        return max(0.0, coverage - g * v_tex_strength);
    }
    return coverage * mix(1.0, g, v_tex_strength);
}

void main() {
    float coverage = apply_texture(base_coverage());
    // R8 mask: only the R channel survives, combined across dabs by the
    // pipeline blend (MAX normally, OVER for build-up). The grain is
    // canvas-anchored, so overlapping dabs agree on it and the stroke reads
    // as one continuous global texture either way.
    out_color = vec4(coverage * v_flow, 0.0, 0.0, 0.0);
}
