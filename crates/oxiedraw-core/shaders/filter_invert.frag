#version 450

// Color invert. Reads one premultiplied BGRA source (binding 0) and
// writes the inverted premultiplied result. For straight color s the
// inverse is (1 - s); in premultiplied form that is (a - rgb), with
// alpha left untouched. Selection clipping is a separate mask-mix pass.

layout(location = 0) in vec2 v_uv;
layout(location = 0) out vec4 out_color;

layout(set = 0, binding = 0) uniform sampler2D u_src;

void main() {
    vec4 src = texture(u_src, v_uv);
    out_color = vec4(src.a - src.rgb, src.a);
}
