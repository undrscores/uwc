// A border whose hue flows around the window; the focused one glows brighter.
//   options { border-width 3; border-shader "~/projects/uwc/shaders/neon.glsl" }
// GLSL ES 1.00. p and size are px of the border's box; color is what uwc would have painted.

vec3 hue(float h) {
    vec3 k = vec3(0.0, 2.0 / 3.0, 1.0 / 3.0);
    return clamp(abs(fract(h + k) * 6.0 - 3.0) - 1.0, 0.0, 1.0);
}

vec4 border(vec2 p, vec2 size, float time, vec4 color, float focused) {
    // Position along the outline, 0 to 1, from the angle around the centre.
    vec2 d = p - size * 0.5;
    float along = atan(d.y, d.x) / 6.2831853 + 0.5;
    vec3 c = hue(along + time * 0.15);
    float strength = mix(0.35, 1.0, focused);
    return vec4(mix(color.rgb, c, strength), 1.0);
}
