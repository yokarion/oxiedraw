#version 450

// Composite one premultiplied-BGRA layer onto the framebuffer using
// HW premultiplied-OVER blending. Fragment just samples the layer; the
// pipeline blend state handles the OVER math.

layout(location = 0) in vec2 v_uv;
layout(location = 0) out vec4 out_color;

layout(set = 0, binding = 0) uniform sampler2D u_layer;

void main() {
    out_color = texture(u_layer, v_uv);
}
