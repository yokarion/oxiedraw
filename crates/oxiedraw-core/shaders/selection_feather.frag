#version 450

// Blur brush for the selection mask. One separable Gaussian pass per axis;
// the caller runs it twice (horizontal, then vertical with the mix enabled).
//
// The kernel is deliberately small and its taps stay about a pixel apart, so a
// pass can never show the sampling grid as blocky steps. Width comes from
// repetition instead: every motion event runs the pair again over the mask it
// produced last time, and stacked Gaussians add variance, so holding the brush
// on a hard edge keeps softening it - matching how the strength of the Add and
// Erase brushes builds up over time.

layout(location = 0) in vec2 v_uv;
layout(location = 0) out vec4 out_color;

// The image being blurred: the mask copy on the first pass, the horizontally
// blurred result on the second.
layout(set = 0, binding = 0) uniform sampler2D u_src;
// The mask as it stood before this pass pair, for the coverage mix.
layout(set = 0, binding = 1) uniform sampler2D u_original;
layout(set = 0, binding = 2) uniform sampler2D u_coverage;

layout(push_constant) uniform Push {
    // Tap step in uv: the blur axis scaled by 1 / canvas size.
    vec2 step_uv;
    // Gaussian sigma in taps.
    float sigma;
    // Mix multiplier; < 0 blurs without mixing (the first pass).
    float mix_scale;
} pc;

// Two sigma of kernel radius. A third tap would weigh ~1% of the total.
const int TAPS = 2;

void main() {
    float sum = texture(u_src, v_uv).r;
    float total = 1.0;
    for (int i = 1; i <= TAPS; ++i) {
        float offset = float(i);
        float weight = exp(-(offset * offset) / (2.0 * pc.sigma * pc.sigma));
        sum += texture(u_src, v_uv + pc.step_uv * offset).r * weight;
        sum += texture(u_src, v_uv - pc.step_uv * offset).r * weight;
        total += 2.0 * weight;
    }
    float blurred = sum / total;

    if (pc.mix_scale < 0.0) {
        out_color = vec4(blurred, 0.0, 0.0, 1.0);
        return;
    }

    // Only the brushed area takes the blur; everything else keeps its value.
    // The coverage already carries the strength slider and the pen pressure.
    float original = texture(u_original, v_uv).r;
    float amount = clamp(texture(u_coverage, v_uv).r * pc.mix_scale, 0.0, 1.0);
    out_color = vec4(mix(original, blurred, amount), 0.0, 0.0, 1.0);
}
