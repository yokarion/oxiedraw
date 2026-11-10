#version 450

// Color-smudge dab (Krita colorsmudge, smearing mode). Rendered as a
// fullscreen triangle scissored to the dab's bounding box; the tip mask is
// the round falloff or a stamped image tip, times the canvas-anchored grain.
// `u_scratch` is a copy of the target layer taken just before this dab: read
// unshifted it is the destination (the pass replaces, blending here), read at
// the drag-shifted position it is the color under the previous dab - the
// smear. `u_before` is the pre-stroke layer: the deposit is a lerp from it
// toward the smear by `opacity`, so opacity is a true ceiling.
layout(location = 0) in vec2 v_uv;

layout(set = 0, binding = 0) uniform sampler2D u_scratch;
layout(set = 1, binding = 0) uniform sampler2D u_before;
layout(set = 2, binding = 0) uniform sampler2DArray u_atlas;

layout(push_constant) uniform Push {
    vec4 paint;       // premultiplied linear brush color
    vec2 center;      // dab centre, canvas px
    vec2 delta;       // centre - previous centre, canvas px (drag)
    vec2 inv_size;    // 1.0 / canvas size, for texture UVs
    float radius;     // canvas px
    float hardness;   // Krita fade
    float smudge_rate; // deposit strength of the smear
    float color_rate;
    float opacity;    // stroke opacity - deposit ceiling vs the pre-stroke layer
    float flow;       // per-dab opacity * flow
    float rotation;
    float aspect;
    float tex_scale;
    float tex_strength;
    float tex_mode;   // 0 multiply, 1 subtract
    float tex_gain;
    float tex_bias;
    uint tip_slice;
    uint grain_slice;
    uint smear_alpha; // 1: Krita smear alpha, transparency smears too
} push;

// Matches renderer::dab::NO_SLICE and pattern_atlas::ATLAS_SLICE_DIM.
const uint NO_SLICE = 0xFFFFFFFFu;
const float ATLAS_DIM = 512.0;

layout(location = 0) out vec4 out_color;

// Krita KisCircleMaskGenerator::valueAt round falloff (see dab.frag).
float round_coverage(vec2 local, float radius, float hardness) {
    float r = max(radius, 1e-4);
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

float tip_coverage(vec2 local) {
    if (push.tip_slice == NO_SLICE) {
        return round_coverage(local, push.radius, push.hardness);
    }
    float c = cos(push.rotation);
    float s = sin(push.rotation);
    float radius = max(push.radius, 1e-4);
    float aspect = max(push.aspect, 1e-4);
    vec2 q = vec2(c * local.x + s * local.y, c * local.y - s * local.x) / radius;
    q.y /= aspect;
    // Fix: implicit-LOD texture() after the outside-quad early return had
    // undefined derivatives at the quad edge and drew square dab borders.
    float lod = max(log2(ATLAS_DIM * 0.5 / (radius * min(aspect, 1.0))), 0.0);
    float coverage = textureLod(u_atlas, vec3(q * 0.5 + 0.5, float(push.tip_slice)), lod).a;
    return abs(q.x) > 1.0 || abs(q.y) > 1.0 ? 0.0 : coverage;
}

// Same texture composite as mask_textured.frag.
float apply_texture(float coverage, vec2 pos) {
    if (push.grain_slice == NO_SLICE || push.tex_scale <= 0.0 || push.tex_strength <= 0.0) {
        return coverage;
    }
    float g = textureLod(u_atlas, vec3(pos / push.tex_scale, float(push.grain_slice)), 0.0).a;
    g = clamp(g * push.tex_gain + push.tex_bias, 0.0, 1.0);
    if (push.tex_mode > 0.5) {
        return max(0.0, coverage - g * push.tex_strength);
    }
    return coverage * mix(1.0, g, push.tex_strength);
}

void main() {
    vec2 pos = gl_FragCoord.xy;
    float coverage = apply_texture(tip_coverage(pos - push.center), pos);
    float flow = clamp(push.flow, 0.0, 1.0);
    float rate = coverage * clamp(push.smudge_rate, 0.0, 1.0) * flow;
    if (rate <= 0.0) {
        discard;
    }
    vec2 src_uv = clamp((pos - push.delta) * push.inv_size, vec2(0.0), vec2(1.0));
    vec4 pickup = texture(u_scratch, src_uv);
    vec4 dst = texture(u_scratch, pos * push.inv_size);
    vec4 before = texture(u_before, pos * push.inv_size);
    // Krita scales the color rate by the dab opacity.
    float color_rate = clamp(push.color_rate * flow, 0.0, 1.0);
    vec4 smear;
    if (push.smear_alpha != 0u) {
        // Krita: paint OVER the premultiplied pickup, transparency included.
        smear = mix(pickup, vec4(push.paint.rgb, 1.0), color_rate);
    } else {
        // Opaque smear in straight color; transparent pickup falls back to the
        // brush color (premultiplied black would smear in dark).
        vec3 dragged = pickup.a > 1e-4 ? pickup.rgb / pickup.a : push.paint.rgb;
        smear = vec4(mix(dragged, push.paint.rgb, color_rate), 1.0);
    }

    // The actual target is a lerp from the pre-stroke pixel toward the smear
    // by opacity. Anchoring to the pre-stroke layer (not the accumulating
    // layer) makes opacity a ceiling and keeps overlapping dabs converging.
    vec4 target = mix(before, smear, clamp(push.opacity, 0.0, 1.0));

    // Smear alpha is Krita's COPY lerp; otherwise premultiplied OVER.
    out_color = push.smear_alpha != 0u
        ? mix(dst, target, rate)
        : target * rate + dst * (1.0 - target.a * rate);
}
