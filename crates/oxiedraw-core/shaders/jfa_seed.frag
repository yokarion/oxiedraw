#version 450

// Jump-flood seed pass for the adjustment-layer stroke. Classifies each pixel
// as inside/outside the backdrop silhouette (alpha threshold) and writes the
// initial nearest-seed offset fields the flood passes refine.
//
// Output packs two offset vectors in pixel units, relative to this pixel: RG =
// offset to the nearest INSIDE pixel, BA = offset to the nearest OUTSIDE pixel.
// A pixel seeds its own class with a zero offset (the seed is itself) and the
// opposite field with a large sentinel, so any real seed beats it when flooding.

layout(location = 0) in vec2 v_uv;
layout(location = 0) out vec4 out_coord;

layout(set = 0, binding = 0) uniform sampler2D u_backdrop; // alpha = silhouette

layout(push_constant) uniform Push {
    vec4 p; // xy = 1/canvas_size (unused here), zw unused
} push;

const float ALPHA_THRESHOLD = 0.5;
const float BIG = 1e4;

void main() {
    bool inside = texture(u_backdrop, v_uv).a > ALPHA_THRESHOLD;
    vec2 in_off = inside ? vec2(0.0) : vec2(BIG);
    vec2 out_off = inside ? vec2(BIG) : vec2(0.0);
    out_coord = vec4(in_off, out_off);
}
