use glow::HasContext;
use gtk::glib::{ControlFlow, Propagation};
use gtk::prelude::*;
use rustconsole_codec_ffmpeg::MappedDmaBufFrame;
use std::collections::VecDeque;
use std::ffi::{CStr, c_char, c_void};
use std::ptr;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

type EglDisplay = *mut c_void;
type EglContext = *mut c_void;
type EglClientBuffer = *mut c_void;
type EglImage = *mut c_void;
type EglBoolean = u32;
type EglInt = i32;

const EGL_NONE: EglInt = 0x3038;
const EGL_WIDTH: EglInt = 0x3057;
const EGL_HEIGHT: EglInt = 0x3056;
const EGL_LINUX_DMA_BUF_EXT: u32 = 0x3270;
const EGL_LINUX_DRM_FOURCC_EXT: EglInt = 0x3271;
const EGL_DMA_BUF_PLANE0_FD_EXT: EglInt = 0x3272;
const EGL_DMA_BUF_PLANE0_OFFSET_EXT: EglInt = 0x3273;
const EGL_DMA_BUF_PLANE0_PITCH_EXT: EglInt = 0x3274;
const EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT: EglInt = 0x3443;
const EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT: EglInt = 0x3444;

type EglCreateImageKhr =
    unsafe extern "C" fn(EglDisplay, EglContext, u32, EglClientBuffer, *const EglInt) -> EglImage;
type EglDestroyImageKhr = unsafe extern "C" fn(EglDisplay, EglImage) -> EglBoolean;
type GlEglImageTargetTexture2dOes = unsafe extern "C" fn(u32, EglImage);

#[link(name = "EGL")]
unsafe extern "C" {
    fn eglGetCurrentDisplay() -> EglDisplay;
    fn eglGetProcAddress(name: *const c_char) -> *const c_void;
}

#[derive(Clone)]
pub struct NativeVideoSink {
    queue: Arc<Mutex<VecDeque<MappedDmaBufFrame>>>,
}

impl NativeVideoSink {
    pub fn submit(&self, frame: MappedDmaBufFrame) {
        let mut queue = self.queue.lock().unwrap();
        while queue.len() >= 2 {
            queue.pop_front();
        }
        queue.push_back(frame);
    }
}

pub struct NativeVideoSurface {
    widget: gtk::GLArea,
    sink: NativeVideoSink,
    status: NativeVideoSurfaceStatus,
}

impl Default for NativeVideoSurface {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone)]
pub struct NativeVideoSurfaceStatus {
    result: Arc<Mutex<Option<Result<(), String>>>>,
}

impl NativeVideoSurfaceStatus {
    #[must_use]
    pub fn result(&self) -> Option<Result<(), String>> {
        self.result.lock().unwrap().clone()
    }
}

impl NativeVideoSurface {
    pub fn new() -> Self {
        let widget = gtk::GLArea::new();
        widget.set_required_version(3, 2);
        widget.set_has_alpha(false);
        widget.set_hexpand(true);
        widget.set_vexpand(true);
        let queue = Arc::new(Mutex::new(VecDeque::with_capacity(2)));
        let result = Arc::new(Mutex::new(None));
        let renderer = Rc::new(std::cell::RefCell::new(None::<OpenGlRenderer>));
        let last_frame = Rc::new(std::cell::RefCell::new(None::<MappedDmaBufFrame>));

        let render_queue = Arc::clone(&queue);
        let render_result = Arc::clone(&result);
        let render_state = Rc::clone(&renderer);
        let render_last_frame = Rc::clone(&last_frame);
        widget.connect_render(move |area, _| {
            let latest = {
                let mut queue = render_queue.lock().unwrap();
                let latest = queue.pop_back();
                queue.clear();
                latest
            };
            if let Some(frame) = latest {
                *render_last_frame.borrow_mut() = Some(frame);
            }
            let frame = render_last_frame.borrow();
            let Some(frame) = frame.as_ref() else {
                return Propagation::Proceed;
            };
            let result = (|| -> Result<(), String> {
                let mut state = render_state.borrow_mut();
                if state.is_none() {
                    *state = Some(OpenGlRenderer::new()?);
                }
                state.as_mut().unwrap().present(
                    frame,
                    area.allocated_width(),
                    area.allocated_height(),
                )
            })();
            if let Err(error) = result {
                *render_result.lock().unwrap() = Some(Err(error.clone()));
                area.set_error(Some(&gtk::glib::Error::new(
                    gtk::glib::FileError::Failed,
                    &error,
                )));
            } else {
                *render_result.lock().unwrap() = Some(Ok(()));
            }
            Propagation::Stop
        });

        let tick_queue = Arc::clone(&queue);
        widget.add_tick_callback(move |area, _| {
            if !tick_queue.lock().unwrap().is_empty() {
                area.queue_render();
            }
            ControlFlow::Continue
        });

        Self {
            widget,
            sink: NativeVideoSink { queue },
            status: NativeVideoSurfaceStatus { result },
        }
    }

    #[must_use]
    pub fn widget(&self) -> &gtk::GLArea {
        &self.widget
    }

    #[must_use]
    pub fn sink(&self) -> NativeVideoSink {
        self.sink.clone()
    }

    #[must_use]
    pub fn status(&self) -> NativeVideoSurfaceStatus {
        self.status.clone()
    }
}

struct OpenGlRenderer {
    gl: glow::Context,
    program: glow::Program,
    vertex_array: glow::VertexArray,
    textures: [glow::Texture; 2],
    display: EglDisplay,
    create_image: EglCreateImageKhr,
    destroy_image: EglDestroyImageKhr,
    image_target: GlEglImageTargetTexture2dOes,
}

impl OpenGlRenderer {
    fn new() -> Result<Self, String> {
        let display = unsafe { eglGetCurrentDisplay() };
        if display.is_null() {
            return Err("GTK GL context has no EGL display".into());
        }
        let create_image = load_egl_function::<EglCreateImageKhr>(c"eglCreateImageKHR")?;
        let destroy_image = load_egl_function::<EglDestroyImageKhr>(c"eglDestroyImageKHR")?;
        let image_target =
            load_egl_function::<GlEglImageTargetTexture2dOes>(c"glEGLImageTargetTexture2DOES")?;
        let gl = unsafe {
            glow::Context::from_loader_function(|name| {
                let name = std::ffi::CString::new(name).unwrap();
                eglGetProcAddress(name.as_ptr())
            })
        };
        let program = create_program(&gl)?;
        let vertex_array = unsafe {
            gl.create_vertex_array()
                .map_err(|error| error.to_string())?
        };
        let textures = unsafe {
            [
                gl.create_texture().map_err(|error| error.to_string())?,
                gl.create_texture().map_err(|error| error.to_string())?,
            ]
        };
        Ok(Self {
            gl,
            program,
            vertex_array,
            textures,
            display,
            create_image,
            destroy_image,
            image_target,
        })
    }

    fn present(
        &mut self,
        frame: &MappedDmaBufFrame,
        width: i32,
        height: i32,
    ) -> Result<(), String> {
        if frame.layers().len() != 2 {
            return Err("OpenGL renderer requires separate NV12 luma and chroma layers".into());
        }
        let mut images = Vec::with_capacity(2);
        for (index, layer) in frame.layers().iter().enumerate() {
            if layer.planes.len() != 1 {
                return Err("OpenGL renderer requires one plane per DRM layer".into());
            }
            let plane = layer.planes[0];
            let object = frame
                .objects()
                .get(plane.object_index)
                .ok_or("DRM layer has no backing object")?;
            let (layer_width, layer_height) = if index == 0 {
                (frame.width(), frame.height())
            } else {
                (frame.width().div_ceil(2), frame.height().div_ceil(2))
            };
            let attributes = [
                EGL_WIDTH,
                i32::try_from(layer_width).map_err(|error| error.to_string())?,
                EGL_HEIGHT,
                i32::try_from(layer_height).map_err(|error| error.to_string())?,
                EGL_LINUX_DRM_FOURCC_EXT,
                layer.format as i32,
                EGL_DMA_BUF_PLANE0_FD_EXT,
                object.fd,
                EGL_DMA_BUF_PLANE0_OFFSET_EXT,
                i32::try_from(plane.offset).map_err(|error| error.to_string())?,
                EGL_DMA_BUF_PLANE0_PITCH_EXT,
                i32::try_from(plane.pitch).map_err(|error| error.to_string())?,
                EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT,
                object.format_modifier as u32 as i32,
                EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT,
                (object.format_modifier >> 32) as u32 as i32,
                EGL_NONE,
            ];
            let image = unsafe {
                (self.create_image)(
                    self.display,
                    ptr::null_mut(),
                    EGL_LINUX_DMA_BUF_EXT,
                    ptr::null_mut(),
                    attributes.as_ptr(),
                )
            };
            if image.is_null() {
                return Err(format!("EGL rejected DRM layer {index}"));
            }
            images.push(image);
        }

        unsafe {
            self.gl.viewport(0, 0, width.max(1), height.max(1));
            self.gl.clear_color(0.02, 0.02, 0.03, 1.0);
            self.gl.clear(glow::COLOR_BUFFER_BIT);
            self.gl.use_program(Some(self.program));
            self.gl.bind_vertex_array(Some(self.vertex_array));
            for (index, (texture, image)) in self.textures.iter().zip(&images).enumerate() {
                self.gl.active_texture(glow::TEXTURE0 + index as u32);
                self.gl.bind_texture(glow::TEXTURE_2D, Some(*texture));
                (self.image_target)(glow::TEXTURE_2D, *image);
                self.gl.tex_parameter_i32(
                    glow::TEXTURE_2D,
                    glow::TEXTURE_MIN_FILTER,
                    glow::LINEAR as i32,
                );
                self.gl.tex_parameter_i32(
                    glow::TEXTURE_2D,
                    glow::TEXTURE_MAG_FILTER,
                    glow::LINEAR as i32,
                );
            }
            if let Some(location) = self.gl.get_uniform_location(self.program, "y_plane") {
                self.gl.uniform_1_i32(Some(&location), 0);
            }
            if let Some(location) = self.gl.get_uniform_location(self.program, "uv_plane") {
                self.gl.uniform_1_i32(Some(&location), 1);
            }
            self.gl.draw_arrays(glow::TRIANGLES, 0, 3);
            self.gl.finish();
            let error = self.gl.get_error();
            if error != glow::NO_ERROR {
                return Err(format!("OpenGL draw failed with error {error:#x}"));
            }
        }
        for image in images {
            if unsafe { (self.destroy_image)(self.display, image) } == 0 {
                return Err("EGL could not destroy a presented image".into());
            }
        }
        Ok(())
    }
}

impl Drop for OpenGlRenderer {
    fn drop(&mut self) {
        unsafe {
            self.gl.delete_texture(self.textures[0]);
            self.gl.delete_texture(self.textures[1]);
            self.gl.delete_vertex_array(self.vertex_array);
            self.gl.delete_program(self.program);
        }
    }
}

fn create_program(gl: &glow::Context) -> Result<glow::Program, String> {
    const VERTEX: &str = r"#version 150 core
out vec2 texture_coordinate;
void main() {
    vec2 position = vec2((gl_VertexID << 1) & 2, gl_VertexID & 2);
    texture_coordinate = position;
    gl_Position = vec4(position * 2.0 - 1.0, 0.0, 1.0);
}";
    const FRAGMENT: &str = r"#version 150 core
uniform sampler2D y_plane;
uniform sampler2D uv_plane;
in vec2 texture_coordinate;
out vec4 output_color;
void main() {
    vec2 sample_coordinate = vec2(texture_coordinate.x, 1.0 - texture_coordinate.y);
    float y = (texture(y_plane, sample_coordinate).r - 0.0625) * 1.16438356;
    vec2 uv = texture(uv_plane, sample_coordinate).rg - vec2(0.5);
    output_color = vec4(y + 1.79274107 * uv.y,
                        y - 0.21324861 * uv.x - 0.53290933 * uv.y,
                        y + 2.11240179 * uv.x,
                        1.0);
}";
    // SAFETY: GTK made this GLArea's context current before initialization.
    unsafe {
        let program = gl.create_program().map_err(|error| error.to_string())?;
        let vertex = compile_shader(gl, glow::VERTEX_SHADER, VERTEX)?;
        let fragment = compile_shader(gl, glow::FRAGMENT_SHADER, FRAGMENT)?;
        gl.attach_shader(program, vertex);
        gl.attach_shader(program, fragment);
        gl.link_program(program);
        gl.delete_shader(vertex);
        gl.delete_shader(fragment);
        if !gl.get_program_link_status(program) {
            let log = gl.get_program_info_log(program);
            gl.delete_program(program);
            return Err(format!("OpenGL renderer shader link failed: {log}"));
        }
        Ok(program)
    }
}

fn compile_shader(gl: &glow::Context, kind: u32, source: &str) -> Result<glow::Shader, String> {
    // SAFETY: the caller invokes this only with GTK's current GLArea context.
    unsafe {
        let shader = gl.create_shader(kind).map_err(|error| error.to_string())?;
        gl.shader_source(shader, source);
        gl.compile_shader(shader);
        if !gl.get_shader_compile_status(shader) {
            let log = gl.get_shader_info_log(shader);
            gl.delete_shader(shader);
            return Err(format!("OpenGL renderer shader compile failed: {log}"));
        }
        Ok(shader)
    }
}

fn load_egl_function<T: Copy>(name: &CStr) -> Result<T, String> {
    let address = unsafe { eglGetProcAddress(name.as_ptr()) };
    if address.is_null() {
        return Err(format!("{} is unavailable", name.to_string_lossy()));
    }
    if std::mem::size_of::<T>() != std::mem::size_of::<*const c_void>() {
        return Err("EGL function pointer has an unexpected size".into());
    }
    Ok(unsafe { std::mem::transmute_copy::<*const c_void, T>(&address) })
}
