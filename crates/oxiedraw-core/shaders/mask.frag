#version 450

// Same vertex outputs as the dab pipeline. v_color is unused on the mask pass.
layout(location = 0) in vec2 v_local;
layout(location = 1) in float v_radius;
layout(location = 2) in vec4 v_color;
layout(location = 3) in float v_flow;
layout(location = 4) in float v_hardness;

layout(location = 0) out vec4 out_color;

// Round dab coverage, ported from Krita's KisCircleMaskGenerator::valueAt.
// `hardness` is Krita's fade: solid core of radius fade*R, then a falloff
// linear in squared distance to the edge; +1px per axis is the edge AA.
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

void main() {
    float coverage = round_coverage(v_local, v_radius, v_hardness);
    // R8_UNORM stroke buffer keeps only the R channel. Coverage * flow goes
    // there; combined across overlapping dabs by the MAX blend op so
    // the mask saturates instead of accumulating.
    out_color = vec4(coverage * v_flow, 0.0, 0.0, 0.0);
}
