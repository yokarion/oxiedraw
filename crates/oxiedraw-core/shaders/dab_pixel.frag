#version 450

layout(location = 0) in vec2 v_local;
layout(location = 1) in float v_radius;
layout(location = 2) in vec4 v_color;
layout(location = 3) in float v_flow;

layout(location = 0) out vec4 out_color;

void main() {
    // Hard cutoff: pixel is fully covered or not at all. No anti-aliasing.
    float d = length(v_local);
    float coverage = step(d, v_radius);
    out_color = v_color * coverage * v_flow;
}
