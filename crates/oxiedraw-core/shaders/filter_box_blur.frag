#version 450

// One axis of a separable box blur. Averages (2*radius + 1) taps of the
// source (binding 0) along the direction given by the texel step in the
// push constant. Run twice (horizontal then vertical) for a full box
// blur. Works directly on premultiplied color so transparent edges do
// not bleed dark halos.

layout(location = 0) in vec2 v_uv;
layout(location = 0) out vec4 out_color;

layout(set = 0, binding = 0) uniform sampler2D u_src;

layout(push_constant) uniform Push {
    // xy = texel step for this axis (1/size on the active axis, 0 on the
    // other), z = radius in pixels, w = unused.
    vec4 params;
} push;

void main() {
    int radius = int(push.params.z + 0.5);
    vec2 step = push.params.xy;

    vec4 sum = vec4(0.0);
    for (int i = -radius; i <= radius; i++) {
        sum += texture(u_src, v_uv + step * float(i));
    }
    float count = float(2 * radius + 1);
    out_color = sum / count;
}
