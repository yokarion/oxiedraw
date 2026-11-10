#version 450

layout(location = 0) in vec2 v_local;
layout(location = 1) in float v_radius;
layout(location = 2) in vec4 v_color;
layout(location = 3) in float v_flow;
layout(location = 4) in float v_hardness;

layout(location = 0) out vec4 out_color;

layout(set = 0, binding = 0) uniform Falloff {
    vec4 lut[256];
} u_falloff;

layout(push_constant) uniform Push {
    vec2 inv_size;
    uint slice;
    uint tip_slice;
    uint falloff;
} push;

// Coverage of a soft round dab, ported from Krita's
// KisCircleMaskGenerator::valueAt (libs/image/kis_circle_mask_generator.cpp).
// `hardness` is Krita's fade (hfade/vfade): a solid core of radius fade*R,
// then a falloff linear in squared distance out to the edge. The +1px on each
// axis reproduces Krita's edge anti-aliasing (local is in canvas pixels).
float round_coverage(vec2 local, float radius, float hardness) {
    float r = max(radius, 1e-4);
    float n = dot(local, local) / (r * r);
    if (n > 1.0) {
        return 0.0;
    }
    // safeSoftnessCoeff clamps fade at 0.01 in Krita; do the same here so a
    // zero-hardness brush stays a smooth falloff instead of dividing by zero.
    float fade = max(hardness, 0.01);
    float invFadeR = 1.0 / (fade * r);
    vec2 aa = abs(local) + vec2(1.0);
    float nf = dot(aa, aa) * invFadeR * invFadeR;
    if (nf < 1.0) {
        return 1.0;
    }
    return 1.0 - n * (nf - 1.0) / max(nf - n, 1e-6);
}

float falloff_at(float x) {
    float p = clamp(x, 0.0, 1.0) * 1023.0;
    int i = min(int(p), 1022);
    float a = u_falloff.lut[i >> 2][i & 3];
    float b = u_falloff.lut[(i + 1) >> 2][(i + 1) & 3];
    return mix(a, b, p - float(i));
}

// Krita's KisCurveCircleMaskGenerator: the curve of squared distance, read
// through its table of round(diameter * 4) + 2 samples, no edge AA.
float curve_coverage(vec2 local, float radius) {
    float r = max(radius, 1e-4);
    float d = dot(local, local) / (r * r);
    if (d > 1.0) {
        return 0.0;
    }
    float res = floor(8.0 * r + 0.5);
    float pos = d * res;
    float i = floor(pos);
    float a = falloff_at(i / (res + 1.0));
    float b = falloff_at((i + 1.0) / (res + 1.0));
    return clamp(mix(a, b, pos - i), 0.0, 1.0);
}

void main() {
    float coverage = push.falloff == 1u
        ? curve_coverage(v_local, v_radius)
        : round_coverage(v_local, v_radius, v_hardness);
    // Premultiplied: scale both color and alpha by coverage * flow.
    out_color = v_color * coverage * v_flow;
}
