#version 450

layout(location = 0) in vec2 v_local;
layout(location = 1) in float v_radius;
layout(location = 2) in vec4 v_color;
layout(location = 3) in float v_flow;

layout(location = 0) out vec4 out_color;

void main() {
    float d = length(v_local);
    float coverage = step(d, v_radius);
    out_color = vec4(coverage * v_flow, 0.0, 0.0, 0.0);
}
