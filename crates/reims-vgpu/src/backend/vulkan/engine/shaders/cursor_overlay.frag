#version 450
layout(set = 0, binding = 0) uniform sampler2D cursor_tex;
layout(location = 0) in vec2 v_uv;
layout(location = 0) out vec4 frag;
void main() { frag = texture(cursor_tex, v_uv); }
