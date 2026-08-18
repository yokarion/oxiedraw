#version 450

// Intersect an adjustment layer's grayscale mask with a clipping base's alpha.
//
// A clipped adjustment applies to its base layer alone, so its gate is the
// painted mask AND the base's silhouette. The result feeds the effect chain
// in place of the raw mask, which keeps the whole adjustment path (mask-mix,
// stroke effect, filter preview) unchanged.
//
// Output matches the mask slot's invariant: neutral gray, fully opaque.

// Binding 2 is unused; it exists so this can ride the shared three-input
// filter set and pipeline layout instead of needing its own.

layout(location = 0) in vec2 v_uv;
layout(location = 0) out vec4 out_color;

layout(set = 0, binding = 0) uniform sampler2D u_mask;
layout(set = 0, binding = 1) uniform sampler2D u_base;
layout(set = 0, binding = 2) uniform sampler2D u_unused;

layout(push_constant) uniform Push {
    vec4 params;
} push;

void main() {
    float mask = texture(u_mask, v_uv).r;
    float base = texture(u_base, v_uv).a;
    out_color = vec4(vec3(mask * base), 1.0);
}
