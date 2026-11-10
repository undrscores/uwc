// Slow aurora bands drifting across the screen (fixed to the screen, not the canvas).
//   options { background-shader "~/projects/uwc/shaders/aurora.glsl" }
// GLSL ES 1.00. Uses `time`, so it is redrawn every frame (costs a little GPU all the time).

vec4 background(vec2 p, vec2 size, float time, vec2 view, float zoom) {
    vec2 uv = p / size;
    float t = time * 0.05;
    float band1 = sin(uv.x * 3.0 + t * 2.0 + sin(uv.y * 2.0 + t)) * 0.5 + 0.5;
    float band2 = sin(uv.x * 5.0 - t * 1.5 + cos(uv.y * 3.0 - t * 0.7)) * 0.5 + 0.5;
    float glow = smoothstep(0.2, 0.9, band1 * band2) * (1.0 - uv.y);
    vec3 night = vec3(0.04, 0.05, 0.09);
    vec3 green = vec3(0.10, 0.55, 0.40);
    vec3 violet = vec3(0.35, 0.20, 0.55);
    vec3 c = night + glow * mix(green, violet, band2) * 0.6;
    return vec4(c, 1.0);
}
