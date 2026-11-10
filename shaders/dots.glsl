// A dot grid on the canvas: it pans and zooms with the windows (uwm's `grid "dots"`, as a shader).
//   options { background-shader "~/projects/uwc/shaders/dots.glsl" }
// GLSL ES 1.00. p is the output px; the canvas point under it is view + p / zoom.

vec4 background(vec2 p, vec2 size, float time, vec2 view, float zoom) {
    vec3 bg = vec3(0.10, 0.11, 0.15);
    vec3 dot = vec3(0.30, 0.33, 0.45);
    float spacing = 60.0;                          // canvas px between dots
    vec2 canvas = view + p / zoom;
    vec2 cell = mod(canvas, spacing) - spacing * 0.5;
    float r = 1.5;                                 // canvas px
    float d = length(cell) * zoom;                 // distance in output px
    float a = clamp(r * zoom + 0.5 - d, 0.0, 1.0);
    // Dots fade out when zoomed out so far they would turn into noise.
    a *= clamp((spacing * zoom - 6.0) / 6.0, 0.0, 1.0);
    return vec4(mix(bg, dot, a), 1.0);
}
