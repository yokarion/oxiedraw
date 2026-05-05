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
    float coverage = texture(u_atlas, vec3(v_uv, float(push.slice))).a;
    // R8 mask: only the R channel survives. MAX blend across dabs.
    out_color = vec4(coverage * v_flow, 0.0, 0.0, 0.0);
}
