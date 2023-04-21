use glutin::{
    api::egl::{
        config::Config, context::PossiblyCurrentContext, display::Display, surface::Surface,
    },
    display::GlDisplay,
    prelude::PossiblyCurrentContextGlSurfaceAccessor,
    surface::{GlSurface, SurfaceAttributesBuilder, WindowSurface},
};
use utils::communication::BgImg;

use std::{
    num::NonZeroU32,
    path::PathBuf,
    sync::{Arc, RwLock},
};

use raw_window_handle::{RawWindowHandle, WaylandWindowHandle};

use smithay_client_toolkit::{
    output::OutputInfo,
    shell::{
        wlr_layer::{Anchor, KeyboardInteractivity, LayerSurface},
        WaylandSurface,
    },
};

use wayland_client::Proxy;

use crate::renderer::Renderer;

/// A linear buffer that we guarantee will always hold correct rgb values
///
/// It has an arc so its values can be set from another thread without cloning
pub struct WallpaperBuffer {
    pub inner: Arc<RwLock<Box<[u8]>>>,
}

impl WallpaperBuffer {
    pub fn new<T: Into<Vec<u8>>>(buf: T) -> Self {
        let v: Vec<u8> = buf.into();
        if v.len() % 3 != 0 {
            todo!("Return an error here");
        }
        Self {
            inner: Arc::new(RwLock::new(v.into_boxed_slice())),
        }
    }

    pub fn set_inner_len(&mut self, new_len: usize) {
        let mut write_lock = self.inner.write().unwrap();
        *write_lock = vec![0; new_len].into_boxed_slice();
    }
}

/// Owns all the necessary information for drawing. In order to get the current image, use `buf_arc_clone`
pub struct Wallpaper {
    pub output_id: u32,
    pub width: NonZeroU32,
    pub height: NonZeroU32,
    pub scale_factor: NonZeroU32,

    buf: WallpaperBuffer,
    pub img: BgImg,

    pub layer_surface: LayerSurface,
    surface: Surface<WindowSurface>,
}

impl Wallpaper {
    pub fn new(
        output_info: OutputInfo,
        layer_surface: LayerSurface,
        config: &Config,
        display: &Display,
    ) -> Self {
        let (width, height) = if let Some(output_size) = output_info.logical_size {
            (
                NonZeroU32::new(output_size.0 as u32).unwrap(),
                NonZeroU32::new(output_size.1 as u32).unwrap(),
            )
        } else {
            (256.try_into().unwrap(), 256.try_into().unwrap())
        };

        let scale_factor = NonZeroU32::new(output_info.scale_factor as u32).unwrap();

        // Configure the layer surface
        layer_surface.set_anchor(Anchor::all());
        layer_surface.set_margin(0, 0, 0, 0);
        layer_surface.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer_surface.set_size(
            width.get() * scale_factor.get(),
            height.get() * scale_factor.get(),
        );
        // commit so that the compositor send the initial configuration
        layer_surface.commit();

        let mut handle = WaylandWindowHandle::empty();
        handle.surface = layer_surface.wl_surface().id().as_ptr() as *mut _;
        let window_handle = RawWindowHandle::Wayland(handle);
        let surface_attributes =
            SurfaceAttributesBuilder::<WindowSurface>::new().build(window_handle, width, height);
        let surface = unsafe {
            display
                .create_window_surface(config, &surface_attributes)
                .unwrap()
        };
        let buf = WallpaperBuffer::new(vec![
            0;
            width.get() as usize
                * height.get() as usize
                * scale_factor.get() as usize
                * 3
        ]);

        Self {
            output_id: output_info.id,
            width,
            height,
            scale_factor,
            layer_surface,
            surface,
            buf,
            img: BgImg::Color([0, 0, 0]),
        }
    }

    pub fn clear(&mut self, color: [u8; 3]) {
        let mut writer = self.buf.inner.write().unwrap();
        for pixel in writer.chunks_exact_mut(3) {
            pixel[0] = color[0];
            pixel[1] = color[1];
            pixel[2] = color[2];
        }
        self.img = BgImg::Color(color);
    }

    pub fn set_img(&mut self, img: &[u8], path: PathBuf) {
        let mut writer = self.buf.inner.write().unwrap();
        writer.copy_from_slice(img);
        self.img = BgImg::Img(path);
    }

    pub fn draw(&mut self, renderer: &Renderer, context: &PossiblyCurrentContext) {
        log::debug!("drawing: {}", self.img);
        context.make_current(&self.surface).unwrap();
        let buf = self.buf.inner.read().unwrap();
        renderer.draw(
            self.width.saturating_mul(self.scale_factor),
            self.height.saturating_mul(self.scale_factor),
            &buf,
        );
        self.surface.swap_buffers_with_damage(context, &[]).unwrap();
    }

    pub fn resize(
        &mut self,
        context: &PossiblyCurrentContext,
        width: NonZeroU32,
        height: NonZeroU32,
        scale_factor: NonZeroU32,
    ) {
        self.width = width;
        self.height = height;
        self.scale_factor = scale_factor;
        self.buf.set_inner_len(
            width.get() as usize * height.get() as usize * scale_factor.get() as usize * 3,
        );
        self.img = BgImg::Color([0, 0, 0]);
        self.surface.resize(
            context,
            width.saturating_mul(scale_factor),
            height.saturating_mul(scale_factor),
        );
    }

    pub fn buf_arc_clone(&self) -> Arc<RwLock<Box<[u8]>>> {
        Arc::clone(&self.buf.inner)
    }
}
