#version 450

// Shape rasteriser. Computes the signed distance from gl_FragCoord to
// one of four primitives (Rectangle, Circle, Triangle, Line) from push
// constants alone, converts to coverage in [0, 1] (hard edge for nearest,
// 1-px linear ramp for bilinear), multiplies by the selection mask if
// one is active, and outputs premultiplied color. Fixed-function OVER
// blend writes it onto the bound target (preview image or layer image).
//
// No textures other than the selection mask, no vertex buffers, no
// per-pixel CPU work — the whole shape lives in 48 bytes of push.

layout(location = 0) in vec2 v_uv;
layout(location = 0) out vec4 out_color;

// R8 selection mask, full canvas size. 1.0 = selected, 0.0 = not.
layout(set = 0, binding = 0) uniform sampler2D u_selection;

layout(push_constant) uniform Push {
    // Premultiplied RGBA. Alpha is the fill opacity.
    vec4 color;
    // For Rectangle/Circle/Triangle: x, y, w, h of the bounding box.
    // For Line: x0, y0, x1, y1 — the two endpoints.
    vec4 rect;
    // x = kind: 0 rect, 1 circle, 2 triangle, 3 line.
    // y = antialias flag: 0 nearest (hard), 1 bilinear (1-px ramp).
    // z = line width (in canvas pixels) — only used for Line.
    // w = selection_active flag.
    vec4 extra;
} push;

const int KIND_RECT = 0;
const int KIND_CIRCLE = 1;
const int KIND_TRIANGLE = 2;
const int KIND_LINE = 3;

float sd_rect(vec2 p, vec2 c, vec2 half_size) {
    vec2 d = abs(p - c) - half_size;
    return length(max(d, vec2(0.0))) + min(max(d.x, d.y), 0.0);
}

// Axis-aligned ellipse SDF (IQ's gradient-corrected approximation).
// Exact iterative SDFs are overkill here — the gradient correction is
// pixel-accurate for the AA band we care about.
float sd_ellipse(vec2 p, vec2 c, vec2 r) {
    vec2 q = p - c;
    vec2 qr = q / max(r, vec2(1e-3));
    float k1 = length(qr);
    vec2 qr2 = q / max(r * r, vec2(1e-3));
    float k2 = length(qr2);
    return k1 * (k1 - 1.0) / max(k2, 1e-6);
}

// Signed distance from p to the segment a->b.
float sd_segment(vec2 p, vec2 a, vec2 b) {
    vec2 pa = p - a;
    vec2 ba = b - a;
    float h = clamp(dot(pa, ba) / max(dot(ba, ba), 1e-6), 0.0, 1.0);
    return length(pa - ba * h);
}

// Signed distance to triangle (IQ). Negative inside, positive outside.
float sd_triangle(vec2 p, vec2 p0, vec2 p1, vec2 p2) {
    vec2 e0 = p1 - p0;
    vec2 e1 = p2 - p1;
    vec2 e2 = p0 - p2;
    vec2 v0 = p - p0;
    vec2 v1 = p - p1;
    vec2 v2 = p - p2;
    vec2 pq0 = v0 - e0 * clamp(dot(v0, e0) / max(dot(e0, e0), 1e-6), 0.0, 1.0);
    vec2 pq1 = v1 - e1 * clamp(dot(v1, e1) / max(dot(e1, e1), 1e-6), 0.0, 1.0);
    vec2 pq2 = v2 - e2 * clamp(dot(v2, e2) / max(dot(e2, e2), 1e-6), 0.0, 1.0);
    float s = sign(e0.x * e2.y - e0.y * e2.x);
    vec2 d = min(min(
        vec2(dot(pq0, pq0), s * (v0.x * e0.y - v0.y * e0.x)),
        vec2(dot(pq1, pq1), s * (v1.x * e1.y - v1.y * e1.x))),
        vec2(dot(pq2, pq2), s * (v2.x * e2.y - v2.y * e2.x)));
    return -sqrt(d.x) * sign(d.y);
}

float shape_sdf(vec2 p, int kind) {
    // Normalise the bbox so backwards drags (negative w / h) still
    // rasterise the way the user expects.
    vec2 corner_a = push.rect.xy;
    vec2 corner_b = push.rect.xy + push.rect.zw;
    vec2 bb_min = min(corner_a, corner_b);
    vec2 bb_max = max(corner_a, corner_b);
    vec2 bb_size = bb_max - bb_min;

    if (kind == KIND_RECT) {
        vec2 half_size = 0.5 * bb_size;
        vec2 c = bb_min + half_size;
        return sd_rect(p, c, half_size);
    }
    if (kind == KIND_CIRCLE) {
        vec2 r = 0.5 * bb_size;
        vec2 c = bb_min + r;
        return sd_ellipse(p, c, r);
    }
    if (kind == KIND_TRIANGLE) {
        // Same vertex layout as CPU shape_fill: apex top-centre, then
        // bottom-left, bottom-right of the bbox.
        vec2 apex = vec2(bb_min.x + bb_size.x * 0.5, bb_min.y);
        vec2 bl = vec2(bb_min.x, bb_min.y + bb_size.y);
        vec2 br = vec2(bb_min.x + bb_size.x, bb_min.y + bb_size.y);
        return sd_triangle(p, apex, bl, br);
    }
    // KIND_LINE: capsule of width `extra.z` between endpoints in rect.
    vec2 a = push.rect.xy;
    vec2 b = push.rect.zw;
    return sd_segment(p, a, b) - push.extra.z * 0.5;
}

void main() {
    // Canvas pixel-centre coordinates.
    vec2 p = gl_FragCoord.xy - vec2(0.5);
    int kind = int(push.extra.x + 0.5);
    float sdf = shape_sdf(p, kind);

    float coverage;
    if (push.extra.y > 0.5) {
        // Bilinear / antialiased: 1-px linear ramp across the boundary.
        coverage = clamp(0.5 - sdf, 0.0, 1.0);
    } else {
        // Nearest / hard edge: half-pixel rule, matches the CPU
        // selection::rasterise scanline behaviour.
        coverage = sdf <= 0.0 ? 1.0 : 0.0;
    }

    if (push.extra.w > 0.5) {
        float m = texture(u_selection, v_uv).r;
        coverage *= m;
    }

    if (coverage <= 0.0) {
        discard;
    }

    out_color = push.color * coverage;
}
