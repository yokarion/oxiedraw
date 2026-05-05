#version 450

layout(location = 0) in vec2 v_uv;
layout(location = 1) in vec4 v_color;
layout(location = 2) in float v_flow;

layout(set = 0, binding = 0) uniform sampler2DArray u_atlas;

layout(push_constant) uniform Push {
    vec2 inv_size;
    uint slice;
} push;

layout(location = 0) out vec4 out_color;

void main() {
    vec4 stamp = texture(u_atlas, vec3(v_uv, float(push.slice)));
    // Atlas patterns store premultiplied RGBA. Modulate by stroke
    // colour (per-dab) and flow.
    out_color = stamp * v_color * v_flow;
}
