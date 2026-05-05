#version 450

// Blends a fully-filtered layer (binding 0) with the original layer
// (binding 1) according to the selection mask (binding 2, R8). Inside
// the selection the filtered color wins, outside the original is kept:
// out = mix(original, filtered, mask). When no selection is active the
// caller passes selection_active = 0 and the whole layer is filtered.

layout(location = 0) in vec2 v_uv;
layout(location = 0) out vec4 out_color;

layout(set = 0, binding = 0) uniform sampler2D u_filtered;
layout(set = 0, binding = 1) uniform sampler2D u_original;
layout(set = 0, binding = 2) uniform sampler2D u_mask;

layout(push_constant) uniform Push {
    // x = selection_active (1 = clip to mask, 0 = whole layer), yzw unused.
    vec4 params;
} push;

void main() {
    vec4 filtered = texture(u_filtered, v_uv);
    vec4 original = texture(u_original, v_uv);
    float m = (push.params.x > 0.5) ? texture(u_mask, v_uv).r : 1.0;
    out_color = mix(original, filtered, m);
}
