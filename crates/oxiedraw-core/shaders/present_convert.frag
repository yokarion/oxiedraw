#version 450

// Re-premultiply the canvas from linear into gamma space for the display dmabuf.
//
// The sRGB-format render targets store srgb(color * alpha) - premultiplied in
// linear light by the hardware OVER blend. GSK composites over the transparency
// checker in gamma space, so it wants srgb(color) * alpha instead. Feeding it
// the linear form makes semi-transparent pixels read too bright and clamp to
// white over the checker.

layout(location = 0) in vec2 v_uv;
layout(location = 0) out vec4 out_color;

layout(set = 0, binding = 0) uniform sampler2D u_src;

vec3 linear_to_srgb(vec3 c) {
    bvec3 cutoff = lessThan(c, vec3(0.0031308));
    vec3 lower = c * 12.92;
    vec3 higher = 1.055 * pow(c, vec3(1.0 / 2.4)) - 0.055;
    return mix(higher, lower, cutoff);
}

void main() {
    // Sampling an sRGB image linearises, so src is premultiplied linear.
    vec4 src = texture(u_src, v_uv);
    float a = src.a;
    vec3 straight = a > 0.0 ? src.rgb / a : vec3(0.0);
    vec3 gamma = linear_to_srgb(clamp(straight, 0.0, 1.0));
    // Target is UNORM, so this lands in memory as written.
    out_color = vec4(gamma * a, a);
}
