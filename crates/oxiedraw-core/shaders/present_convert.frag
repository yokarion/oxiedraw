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
// Selection mask (R8). Only read when the heatmap overlay is on; its contents
// are don't-care while no selection is active.
layout(set = 0, binding = 1) uniform sampler2D u_selection;

layout(push_constant) uniform Push {
    // Heatmap opacity at a fully-selected pixel. 0 disables the overlay.
    float heatmap;
} pc;

vec3 linear_to_srgb(vec3 c) {
    bvec3 cutoff = lessThan(c, vec3(0.0031308));
    vec3 lower = c * 12.92;
    vec3 higher = 1.055 * pow(c, vec3(1.0 / 2.4)) - 0.055;
    return mix(higher, lower, cutoff);
}

// Blender weight-paint ramp: blue at the faintest selection through to red at
// fully selected. Walking the five spectrum stops (rather than lerping blue
// straight to red) keeps red and blue from ever being lit at once, so the ramp
// has no magenta/purple in it. Display-space colours, so no linearisation.
vec3 heat_ramp(float v) {
    const vec3 stops[5] = vec3[5](
        vec3(0.0, 0.0, 1.0), // blue
        vec3(0.0, 1.0, 1.0), // cyan
        vec3(0.0, 1.0, 0.0), // green
        vec3(1.0, 1.0, 0.0), // yellow
        vec3(1.0, 0.0, 0.0)  // red
    );
    float t = clamp(v, 0.0, 1.0) * 4.0;
    int stop = int(min(t, 3.0));
    return mix(stops[stop], stops[stop + 1], t - float(stop));
}

void main() {
    // Sampling an sRGB image linearises, so src is premultiplied linear.
    vec4 src = texture(u_src, v_uv);
    float a = src.a;
    vec3 straight = a > 0.0 ? src.rgb / a : vec3(0.0);
    vec3 gamma = linear_to_srgb(clamp(straight, 0.0, 1.0));
    // Target is UNORM, so this lands in memory as written.
    vec4 result = vec4(gamma * a, a);

    if (pc.heatmap > 0.0) {
        // Unselected pixels draw nothing, so the ramp fades in with coverage.
        float weight = texture(u_selection, v_uv).r;
        float overlay_a = weight * pc.heatmap;
        result = vec4(heat_ramp(weight) * overlay_a, overlay_a) + result * (1.0 - overlay_a);
    }
    out_color = result;
}
