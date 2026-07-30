#version 450

layout(location = 0) in vec2 v_local;
layout(location = 1) in float v_radius;
layout(location = 2) in vec4 v_color;
layout(location = 3) in float v_flow;
layout(location = 4) in float v_hardness;

layout(location = 0) out vec4 out_color;

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

void main() {
    float coverage = round_coverage(v_local, v_radius, v_hardness);
    // Premultiplied: scale both color and alpha by coverage * flow.
    out_color = v_color * coverage * v_flow;
}
