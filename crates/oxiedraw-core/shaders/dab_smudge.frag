#version 450

// Colour-smudge dab (Krita colorsmudge, smearing mode). Rendered as a
// fullscreen triangle scissored to the dab's bounding box; the round tip
// mask is computed per fragment. `u_scratch` is a copy of the target layer
// taken just before this dab, sampled at the drag-shifted position so the
// colour under the previous dab position is dragged onto this one - the smear.
// `u_before` is the pre-stroke layer: the deposit is a lerp from it toward the
// smear by `opacity`, so opacity is a true ceiling (the layer can only move
// `opacity` of the way from its pre-stroke value) instead of accumulating.
layout(location = 0) in vec2 v_uv;

layout(set = 0, binding = 0) uniform sampler2D u_scratch;
layout(set = 1, binding = 0) uniform sampler2D u_before;

layout(push_constant) uniform Push {
    vec4 paint;       // premultiplied linear brush colour
    vec2 center;      // dab centre, canvas px
    vec2 delta;       // centre - previous centre, canvas px (drag)
    vec2 inv_size;    // 1.0 / canvas size, for texture UVs
    float radius;     // canvas px
    float hardness;   // Krita fade
    float smudge_rate; // how much of the dragged colour carries (1 = full drag)
    float color_rate;
    float opacity;    // stroke opacity - deposit ceiling vs the pre-stroke layer
} push;

layout(location = 0) out vec4 out_color;

// Krita KisCircleMaskGenerator::valueAt round falloff (see dab.frag).
float round_coverage(vec2 local, float radius, float hardness) {
    float r = max(radius, 1e-4);
    float n = dot(local, local) / (r * r);
    if (n > 1.0) {
        return 0.0;
    }
    float fade = max(hardness, 0.01);
    float invFadeR = 1.0 / (fade * r);
    vec2 aa = abs(local) + vec2(1.0);
    float nf = dot(aa, aa) * invFadeR * invFadeR;
    if (nf < 1.0) {
        return 1.0;
    }
    return 1.0 - n * (nf - 1.0) / max(nf - n, 1e-6);
}

void main() {
    vec2 pos = gl_FragCoord.xy;
    float coverage = round_coverage(pos - push.center, push.radius, push.hardness);
    if (coverage <= 0.0) {
        discard;
    }
    // Drag the colour from the previous dab position onto this one. The layer
    // copy is premultiplied; un-premultiply so mixing works in straight colour.
    // Where a sample is transparent there's no colour to smear, so fall back to
    // the brush colour (otherwise premultiplied black would smear in dark).
    vec2 src_uv = clamp((pos - push.delta) * push.inv_size, vec2(0.0), vec2(1.0));
    vec4 pickup = texture(u_scratch, src_uv);
    vec4 before = texture(u_before, pos * push.inv_size);
    vec3 dragged = pickup.a > 1e-4 ? pickup.rgb / pickup.a : push.paint.rgb;
    vec3 here = before.a > 1e-4 ? before.rgb / before.a : push.paint.rgb;
    // Smudge rate: how much of the dragged colour carries vs. keeping this
    // pixel's own pre-stroke colour (rate 1 = full drag, 0 = no smear).
    vec3 picked = mix(here, dragged, clamp(push.smudge_rate, 0.0, 1.0));
    vec3 smear = mix(picked, push.paint.rgb, clamp(push.color_rate, 0.0, 1.0));

    // Full-strength deposit is the opaque smear colour; the actual target is a
    // lerp from the pre-stroke pixel toward it by opacity. Anchoring to the
    // pre-stroke layer (not the accumulating layer) makes opacity a ceiling and
    // keeps overlapping dabs converging smoothly (no beading).
    vec4 target = mix(before, vec4(smear, 1.0), clamp(push.opacity, 0.0, 1.0));

    // Premultiplied OVER, masked by tip coverage: converges to `target`.
    out_color = target * coverage;
}
