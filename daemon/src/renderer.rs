mod gl {
    include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
}

use std::{
    ffi::{c_void, CStr, CString},
    num::NonZeroU32,
    ops::Deref,
};

use glutin::prelude::GlDisplay;
use log::{debug, error};

/// OpenGL renderer
///
/// It uses a static set of vertices (since we will always render to the entire window)
///
/// The "draw" call simply creates and binds a texture to display on the screen
pub struct Renderer {
    program: gl::types::GLuint,
    vao: gl::types::GLuint,
    vbo: gl::types::GLuint,
    gl: gl::Gl,
}

impl Renderer {
    pub fn new<D: GlDisplay>(gl_display: &D) -> Self {
        unsafe {
            let gl = gl::Gl::load_with(|symbol| {
                let symbol = CString::new(symbol).unwrap();
                gl_display.get_proc_address(symbol.as_c_str()).cast()
            });

            #[cfg(debug_assertions)]
            {
                if let Some(renderer) = get_gl_string(&gl, gl::RENDERER) {
                    debug!("Running on {}", renderer.to_string_lossy());
                }
                if let Some(version) = get_gl_string(&gl, gl::VERSION) {
                    debug!("OpenGL Version {}", version.to_string_lossy());
                }
                if let Some(shaders_version) = get_gl_string(&gl, gl::SHADING_LANGUAGE_VERSION) {
                    debug!("Shaders version on {}", shaders_version.to_string_lossy());
                }
            }

            let vertex_shader = create_shader(&gl, gl::VERTEX_SHADER, VERTEX_SHADER_SOURCE);
            let fragment_shader = create_shader(&gl, gl::FRAGMENT_SHADER, FRAGMENT_SHADER_SOURCE);

            let program = gl.CreateProgram();

            gl.AttachShader(program, vertex_shader);
            gl.AttachShader(program, fragment_shader);

            gl.LinkProgram(program);

            gl.UseProgram(program);

            gl.DeleteShader(vertex_shader);
            gl.DeleteShader(fragment_shader);

            let mut vao = std::mem::zeroed();
            gl.GenVertexArrays(1, &mut vao);
            gl.BindVertexArray(vao);

            let mut vbo = std::mem::zeroed();
            gl.GenBuffers(1, &mut vbo);
            gl.BindBuffer(gl::ARRAY_BUFFER, vbo);
            gl.BufferData(
                gl::ARRAY_BUFFER,
                (VERTEX_DATA.len() * std::mem::size_of::<f32>()) as gl::types::GLsizeiptr,
                VERTEX_DATA.as_ptr() as *const _,
                gl::STATIC_DRAW,
            );

            gl.VertexAttribPointer(
                0,
                2,
                gl::FLOAT,
                gl::FALSE,
                4 * std::mem::size_of::<f32>() as i32,
                std::ptr::null() as *const c_void,
            );
            gl.EnableVertexAttribArray(0);

            gl.VertexAttribPointer(
                1,
                2,
                gl::FLOAT,
                gl::FALSE,
                4 * std::mem::size_of::<f32>() as i32,
                (2 * std::mem::size_of::<f32>()) as *const c_void,
            );
            gl.EnableVertexAttribArray(1);

            let uniform_name = CString::new("tex").unwrap();
            let location = gl.GetUniformLocation(program, uniform_name.as_ptr());
            gl.Uniform1i(location, 0);

            // activate texture 0 (note this will never change)
            gl.ActiveTexture(gl::TEXTURE0);

            Self {
                program,
                vao,
                vbo,
                gl,
            }
        }
    }

    pub fn draw(&self, width: NonZeroU32, height: NonZeroU32, buf: &[u8]) {
        let gl = &self.gl;
        self.resize(width.get() as i32, height.get() as i32);
        unsafe {
            let tex = create_texture(gl, width, height, buf);
            gl.BindTexture(gl::TEXTURE_2D, tex);

            gl.UseProgram(self.program);
            gl.BindVertexArray(self.vao);

            gl.DrawArrays(gl::TRIANGLES, 0, 6);

            #[cfg(debug_assertions)]
            {
                let error = match gl.GetError() {
                    gl::INVALID_ENUM => "INVALID_ENUM",
                    gl::INVALID_VALUE => "INVALID_VALUE",
                    gl::INVALID_OPERATION => "INVALID_OPERATION",
                    gl::STACK_OVERFLOW => "STACK_OVERFLOW",
                    gl::OUT_OF_MEMORY => "OUT_OF_MEMORY",
                    gl::INVALID_FRAMEBUFFER_OPERATION => "INVALID_FRAMEBUFFER_OPERATION",
                    _ => "",
                };
                if !error.is_empty() {
                    error!("OpenGL_error: {error}");
                }
            }
        }
    }

    pub fn resize(&self, width: i32, height: i32) {
        unsafe {
            self.gl.Viewport(0, 0, width, height);
        }
    }
}

impl Deref for Renderer {
    type Target = gl::Gl;

    fn deref(&self) -> &Self::Target {
        &self.gl
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        unsafe {
            self.gl.DeleteProgram(self.program);
            self.gl.DeleteBuffers(1, &self.vbo);
            self.gl.DeleteVertexArrays(1, &self.vao);
        }
    }
}

unsafe fn create_texture(gl: &gl::Gl, width: NonZeroU32, height: NonZeroU32, buf: &[u8]) -> u32 {
    let mut texture: u32 = 0;
    let texture_ptr = std::ptr::addr_of_mut!(texture);
    gl.GenTextures(1, texture_ptr);
    gl.BindTexture(gl::TEXTURE_2D, texture);

    gl.TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MIN_FILTER, gl::LINEAR_MIPMAP_LINEAR as i32);
    gl.TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MAG_FILTER, gl::LINEAR as i32);

    let void_ptr: *const c_void = buf as *const _ as *const c_void;
    gl.TexImage2D(
        gl::TEXTURE_2D,
        0,
        gl::RGB8 as i32,
        width.get() as i32,
        height.get() as i32,
        0,
        gl::RGB,
        gl::UNSIGNED_BYTE,
        void_ptr,
    );
    gl.GenerateMipmap(gl::TEXTURE_2D);

    texture
}

unsafe fn create_shader(
    gl: &gl::Gl,
    shader: gl::types::GLenum,
    source: &[u8],
) -> gl::types::GLuint {
    let shader = gl.CreateShader(shader);
    gl.ShaderSource(
        shader,
        1,
        [source.as_ptr().cast()].as_ptr(),
        std::ptr::null(),
    );
    gl.CompileShader(shader);
    shader
}

fn get_gl_string(gl: &gl::Gl, variant: gl::types::GLenum) -> Option<&'static CStr> {
    unsafe {
        let s = gl.GetString(variant);
        (!s.is_null()).then(|| CStr::from_ptr(s.cast()))
    }
}

#[rustfmt::skip]
const VERTEX_DATA: [f32; 24] = [
    // Triangle 1
     -1.0, -1.0, 0.0,  0.0,
     -1.0,  1.0, 0.0,  1.0,
      1.0, -1.0, 1.0,  0.0,

     // Triangle 2
      1.0,  1.0, 1.0,  1.0,
     -1.0,  1.0, 0.0,  1.0,
      1.0, -1.0, 1.0,  0.0,
];

const VERTEX_SHADER_SOURCE: &[u8] = b"
#version 330 core

layout (location = 0) in vec2 pos;
layout (location = 1) in vec2 _texture_pos;

out vec2 texture_pos;

void main() {
	gl_Position = vec4(pos.x, -pos.y, 0.0f, 1.0f);
	texture_pos = _texture_pos;
}
\0";

const FRAGMENT_SHADER_SOURCE: &[u8] = b"
#version 330 core

out vec4 color;
in vec2 texture_pos;

uniform sampler2D tex;

void main() {
	color = vec4(texture(tex, texture_pos).rgb, 1.0f);
}
\0";
