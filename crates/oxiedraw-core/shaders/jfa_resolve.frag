#version 450

// Jump-flood resolve pass for the adjustment-layer stroke: turn the converged
// nearest-seed offset field into a coloured band along the backdrop's alpha
// edge, gated by the adjustment layer's grayscale mask. This is the tail of the
// old brute-force stroke shader; only the distance lookup changed (a texture
// read of the flooded field instead of a per-pixel disc scan).
//
// Signed distance is positive inside the silhouette, negative outside. `offset`
// slides the band from fully inside (-1) through centred (0) to fully outside
// (+1). Output is premultiplied; colour arrives straight sRGB and is linearised
// to match the sRGB attachment.

layout(location = 0) in vec2 v_uv;
layout(location = 0) out vec4 out_color;

layout(set = 0, binding = 0) uniform sampler2D u_coord;    // RG inside, BA outside
layout(set = 0, binding = 1) uniform sampler2D u_backdrop; // alpha = silhouette
layout(set = 0, binding = 2) uniform sampler2D u_mask;     // .r = effect gate

layout(push_constant) uniform Push {
    vec4 color;  // rgb = straight sRGB colour, a unused
    vec4 params; // x = opacity, y = thickness (px), z = offset (-1..1), w = softness
    vec4 texel;  // unused here
} push;

const float ALPHA_THRESHOLD = 0.5;

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

    bool center_in = texture(u_backdrop, v_uv).a > ALPHA_THRESHOLD;
    vec4 field = texture(u_coord, v_uv);
    // Distance to the nearest pixel of the opposite class (matches the old disc
    // scan): inside pixels read the outside field, outside pixels the inside.
    float dist = center_in ? length(field.zw) : length(field.xy);
    float signed_dist = center_in ? dist : -dist;

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
