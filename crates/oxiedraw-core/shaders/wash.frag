#version 450

// Krita-like wash mode

layout(location = 0) out vec4 out_color;

layout(set = 0, binding = 0) uniform sampler2D u_before;
layout(std430, set = 0, binding = 1) readonly buffer Dabs {
    float d[];
} dabs;

layout(set = 1, binding = 0) uniform Falloff {
    vec4 lut[256];
} u_falloff;

layout(push_constant) uniform Push {
    uint first;
    uint count;
    uint falloff;
} pc;

const uint STRIDE = 21u;

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

float falloff_at(float x) {
    float p = clamp(x, 0.0, 1.0) * 1023.0;
    int i = min(int(p), 1022);
    float a = u_falloff.lut[i >> 2][i & 3];
    float b = u_falloff.lut[(i + 1) >> 2][(i + 1) & 3];
    return mix(a, b, p - float(i));
}

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

uint scale8(float v) {
    return uint(clamp(v * 255.0, 0.0, 255.0) + 0.5);
}

uint mul8(uint a, uint b) {
    uint c = a * b + 0x80u;
    return ((c >> 8) + c) >> 8;
}

uint div8(uint a, uint b) {
    return (a * 255u + (b >> 1)) / b;
}

uint lerp8(uint a, uint b, uint t) {
    int c = (int(b) - int(a)) * int(t) + 0x80;
    c = ((c >> 8) + c) >> 8;
    return uint(int(a) + c);
}

void main() {
    vec2 p = gl_FragCoord.xy;
    uint dst = scale8(texelFetch(u_before, ivec2(p), 0).r);
    for (uint i = pc.first; i < pc.first + pc.count; ++i) {
        uint b = i * STRIDE;
        vec2 local = p - vec2(dabs.d[b], dabs.d[b + 1u]);
        float radius = dabs.d[b + 2u];
        if (any(greaterThan(abs(local), vec2(radius + 1.0)))) {
            continue;
        }
        float cov = pc.falloff == 1u
            ? curve_coverage(local, radius)
            : round_coverage(local, radius, dabs.d[b + 14u]);
        uint msk = scale8(cov);
        uint op = scale8(dabs.d[b + 19u]);
        uint avg = scale8(dabs.d[b + 20u]);
        float flow = dabs.d[b + 5u];
        uint src = mul8(msk, op);
        uint full;
        if (avg > op) {
            full = avg > dst ? lerp8(src, avg, div8(dst, avg)) : dst;
        } else {
            full = op > dst ? lerp8(dst, op, msk) : dst;
        }
        dst = flow >= 1.0 ? full : lerp8(dst, full, scale8(flow));
    }
    out_color = vec4(float(dst) / 255.0, 0.0, 0.0, 0.0);
}
