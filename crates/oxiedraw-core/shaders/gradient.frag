#version 450

// Gradient rasteriser. Computes a ramp coordinate `t` at each canvas pixel
// from the drag endpoints (Linear / Radial / Square), samples the baked
// colour LUT (premultiplied linear RGBA), multiplies by the selection mask
// if one is active, and outputs premultiplied colour. Fixed-function OVER
// blend writes it onto the bound target (preview image or layer image).
//
// The whole ramp lives in the 256-texel LUT plus 32 bytes of push data; no
// per-pixel CPU work.

layout(location = 0) in vec2 v_uv;
layout(location = 0) out vec4 out_color;

// R8 selection mask, full canvas size. 1.0 = selected, 0.0 = not.
layout(set = 0, binding = 0) uniform sampler2D u_selection;
// Baked gradient ramp, 256x1, premultiplied linear RGBA.
layout(set = 0, binding = 1) uniform sampler2D u_lut;

layout(push_constant) uniform Push {
    // Drag endpoints in canvas pixels: x0, y0 (start) and x1, y1 (end).
    vec4 endpoints;
    // x = kind: 0 linear, 1 radial, 2 square.
    // y = selection_active flag.
    vec4 extra;
} push;

const int KIND_LINEAR = 0;
const int KIND_RADIAL = 1;
const int KIND_SQUARE = 2;

void main() {
    vec2 p = gl_FragCoord.xy - vec2(0.5);
    vec2 start = push.endpoints.xy;
    vec2 end = push.endpoints.zw;
    vec2 dir = end - start;
    float len = length(dir);
    if (len < 1e-4) {
        discard;
    }
    vec2 u = dir / len;
    vec2 rel = p - start;

    int kind = int(push.extra.x + 0.5);
    float t;
    if (kind == KIND_RADIAL) {
        t = length(rel) / len;
    } else if (kind == KIND_SQUARE) {
        vec2 perp = vec2(-u.y, u.x);
        float a = dot(rel, u) / len;
        float b = dot(rel, perp) / len;
        t = max(abs(a), abs(b));
    } else {
        t = dot(rel, u) / len;
    }
    t = clamp(t, 0.0, 1.0);

    vec4 ramp = texture(u_lut, vec2(t, 0.5));

    float coverage = 1.0;
    if (push.extra.y > 0.5) {
        coverage = texture(u_selection, v_uv).r;
    }
    if (coverage <= 0.0) {
        discard;
    }

    out_color = ramp * coverage;
}
