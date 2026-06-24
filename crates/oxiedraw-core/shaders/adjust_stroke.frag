#version 450

// Adjustment-layer stroke: trace the alpha edge of the backdrop (everything
// composited below the adjustment layer) and lay down a coloured band along
// it, gated by the adjustment layer's grayscale mask.
//
// There is no analytic SDF for an arbitrary alpha silhouette, so the distance
// to the nearest edge is found morphologically: for each pixel, scan a disc of
// radius `thickness` and record the closest sample whose inside/outside state
// differs from the centre. Signed distance is positive inside the silhouette,
// negative outside. `offset` slides the band from fully inside (-1) through
// centred (0) to fully outside (+1) the edge.
//
// Output is premultiplied so the caller can OVER-blend the scratch onto the
// canvas. Colour arrives as straight sRGB in 0..1 and is linearised here to
// match the linear working space of the sRGB attachment.

layout(location = 0) in vec2 v_uv;
layout(location = 0) out vec4 out_color;

layout(set = 0, binding = 0) uniform sampler2D u_backdrop; // alpha = silhouette
layout(set = 0, binding = 1) uniform sampler2D u_mask;     // .r = effect gate

layout(push_constant) uniform Push {
    vec4 color;  // rgb = straight sRGB colour, a unused
    vec4 params; // x = opacity, y = thickness (px), z = offset (-1..1), w = softness (0 hard, 1 AA)
    vec4 texel;  // xy = 1/canvas_size, zw unused
} push;

const float ALPHA_THRESHOLD = 0.5;
const int MAX_RADIUS = 64;

vec3 srgb_to_linear(vec3 c) {
    bvec3 lo = lessThanEqual(c, vec3(0.04045));
    vec3 a = c / 12.92;
    vec3 b = pow((c + 0.055) / 1.055, vec3(2.4));
    return mix(b, a, vec3(lo));
}

void main() {
    float opacity = push.params.x;
    float thickness = max(push.params.y, 0.0);
    float offset = clamp(push.params.z, -1.0, 1.0);
    bool aa = push.params.w > 0.5;
    vec2 texel = push.texel.xy;

    float center_a = texture(u_backdrop, v_uv).a;
    bool center_in = center_a > ALPHA_THRESHOLD;

    // Search radius must cover the band's far edge: |center| + half, where the
    // band centre sits at offset*half and half = thickness/2. At |offset| = 1
    // that is `thickness`, plus a pixel of AA margin. Loop only out to `radius`
    // (not the MAX_RADIUS cap) so the per-pixel cost scales with the actual
    // thickness instead of being a fixed ~16k iterations.
    int radius = clamp(int(ceil(thickness)) + 1, 0, MAX_RADIUS);

    float best = float(radius) + 2.0;
    for (int dy = -radius; dy <= radius; ++dy) {
        for (int dx = -radius; dx <= radius; ++dx) {
            float d = length(vec2(dx, dy));
            if (d > float(radius) || d >= best) { continue; }
            float a = texture(u_backdrop, v_uv + texel * vec2(dx, dy)).a;
            if ((a > ALPHA_THRESHOLD) != center_in) { best = d; }
        }
    }

    float signed_dist = center_in ? best : -best;

    // signed_dist is +inside / -outside. offset = -1 inside .. 0 centre .. +1
    // outside slides the band of width `thickness` to the negative side as
    // offset grows, so band_center = -offset * half_w.
    float half_w = thickness * 0.5;
    float band_center = -offset * half_w;
    float dist_from_band = abs(signed_dist - band_center);

    float coverage;
    if (aa) {
        coverage = 1.0 - smoothstep(half_w - 0.75, half_w + 0.75, dist_from_band);
    } else {
        coverage = dist_from_band <= half_w ? 1.0 : 0.0;
    }

    float gate = texture(u_mask, v_uv).r;
    float alpha = coverage * gate * opacity;

    vec3 lin = srgb_to_linear(clamp(push.color.rgb, 0.0, 1.0));
    out_color = vec4(lin * alpha, alpha); // premultiplied
}
