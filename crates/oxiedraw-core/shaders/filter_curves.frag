#version 450

// Tone curves on sRGB levels: each channel's curve (atlas row R/G/B), then the
// master curve (A), as GIMP orders them.

layout(location = 0) in vec2 v_uv;
layout(location = 0) out vec4 out_color;

layout(set = 0, binding = 0) uniform sampler2D u_src;
layout(set = 0, binding = 1) uniform sampler2D u_lut;

layout(push_constant) uniform Push {
    // x = atlas row, y = 0 passes through (the atlas was full).
    vec4 params;
} push;

vec3 linear_to_srgb(vec3 c) {
    bvec3 cutoff = lessThan(c, vec3(0.0031308));
    vec3 lower = c * 12.92;
    vec3 higher = 1.055 * pow(c, vec3(1.0 / 2.4)) - 0.055;
    return mix(higher, lower, cutoff);
}

vec3 srgb_to_linear(vec3 c) {
    bvec3 lo = lessThanEqual(c, vec3(0.04045));
    vec3 a = c / 12.92;
    vec3 b = pow((c + 0.055) / 1.055, vec3(2.4));
    return mix(b, a, vec3(lo));
}

// Manual lerp: the shared filter sampler is NEAREST.
float lookup(float level, int channel, int row) {
    float pos = clamp(level, 0.0, 1.0) * 255.0;
    int lo = min(int(pos), 254);
    float lo_value = texelFetch(u_lut, ivec2(lo, row), 0)[channel];
    float hi_value = texelFetch(u_lut, ivec2(lo + 1, row), 0)[channel];
    return mix(lo_value, hi_value, pos - float(lo));
}

void main() {
    vec4 src = texture(u_src, v_uv);
    float a = src.a;
    if (push.params.y < 0.5 || a <= 0.0001) {
        out_color = src;
        return;
    }
    int row = int(push.params.x + 0.5);

    vec3 level = linear_to_srgb(clamp(src.rgb / a, 0.0, 1.0));
    vec3 mapped = vec3(
        lookup(lookup(level.r, 0, row), 3, row),
        lookup(lookup(level.g, 1, row), 3, row),
        lookup(lookup(level.b, 2, row), 3, row)
    );

    out_color = vec4(srgb_to_linear(clamp(mapped, 0.0, 1.0)) * a, a);
}
