#version 330
#extension GL_ARB_separate_shader_objects : require

#include <minecraft:fog.glsl>
#include <minecraft:dynamictransforms.glsl>
#include <minecraft:projection.glsl>
#include <minecraft:sample_lightmap.glsl>

layout(location = 0) in vec3 Position;
layout(location = 1) in vec4 Color;

uniform sampler2D Sampler2;

layout(location = 0) out float sphericalVertexDistance;
layout(location = 1) out float cylindricalVertexDistance;
layout(location = 2) out vec4 vertexColor;
layout(location = 3) out vec2 regionXZ;

void main() {
    vec4 viewPos = ModelViewMat * vec4(Position, 1.0);
    gl_Position = ProjMat * viewPos;

    sphericalVertexDistance = fog_spherical_distance(viewPos.xyz);
    cylindricalVertexDistance = fog_cylindrical_distance(viewPos.xyz);
    // Open-sky columns: sky light 15, block light 0, through the game's lightmap of the frame.
    vertexColor = Color * sample_lightmap(Sampler2, ivec2(0, 240));
    regionXZ = Position.xz;
}
