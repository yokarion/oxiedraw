#version 450

// One axis of a separable Gaussian blur, the weighted twin of
// filter_box_blur.frag. Tap i weighs ratio^(i*i), stepped by multiplication.

layout(location = 0) in vec2 v_uv;
layout(location = 0) out vec4 out_color;

layout(set = 0, binding = 0) uniform sampler2D u_src;

layout(push_constant) uniform Push {
    // xy = texel step for this axis, z = radius in pixels,
    // w = tap ratio from `filters::gaussian_ratio`.
    vec4 params;
} push;

void main() {
    int radius = max(int(push.params.z + 0.5), 0);
    vec2 step = push.params.xy;
    float ratio = push.params.w;

    vec4 sum = texture(u_src, v_uv);
    float total = 1.0;
    float weight = 1.0;
    float factor = ratio;
    for (int i = 1; i <= radius; i++) {
        weight *= factor;
        factor *= ratio * ratio;
        vec2 offset = step * float(i);
        sum += (texture(u_src, v_uv + offset) + texture(u_src, v_uv - offset)) * weight;
        total += 2.0 * weight;
    }
    out_color = sum / total;
}
