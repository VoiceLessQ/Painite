#version 330
#extension GL_ARB_separate_shader_objects : require

#include <minecraft:fog.glsl>
#include <minecraft:dynamictransforms.glsl>

layout(location = 0) in float sphericalVertexDistance;
layout(location = 1) in float cylindricalVertexDistance;
layout(location = 2) in vec4 vertexColor;
layout(location = 3) in vec2 regionXZ;

layout(location = 0) out vec4 fragColor;

void main() {
    // ColorModulator carries the hole around the player: centre xz in region blocks and its half size.
    if (max(abs(regionXZ.x - ColorModulator.x), abs(regionXZ.y - ColorModulator.y)) < ColorModulator.z) {
        discard;
    }
    // Environmental fog (weather, water) as the game has it; the far ramp curves up so the middle distance stays clear.
    float near = linear_fog_value(sphericalVertexDistance, FogEnvironmentalStart, FogEnvironmentalEnd);
    float t = linear_fog_value(cylindricalVertexDistance, FogRenderDistanceStart, FogRenderDistanceEnd);
    float far = 1.0 - exp(-(t * 2.0) * (t * 2.0));
    float fogValue = max(near, far);
    fragColor = vec4(mix(vertexColor.rgb, FogColor.rgb, fogValue * FogColor.a), vertexColor.a);
    // ColorModulator.w set: tint the mesh so a screenshot tells it from the game's chunks.
    fragColor.rgb = mix(fragColor.rgb, vec3(1.0, 0.0, 1.0), 0.5 * ColorModulator.w);
}
