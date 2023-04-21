//! All expects in this program must be carefully chosen on purpose. The idea is that if any of
//! them fail there is no point in continuing. All of the initialization code, for example, is full
//! of `expects`, **on purpose**, because we **want** to unwind and exit when they happen

mod renderer;
mod wallpaper;
use log::{debug, error, info, LevelFilter};
use nix::{
    poll::{poll, PollFd, PollFlags},
    sys::signal::{self, SigHandler, Signal},
};
use renderer::Renderer;
use simplelog::{ColorChoice, TermLogger, TerminalMode, ThreadLogMode};
use wallpaper::Wallpaper;

use glutin::{
    api::egl::{config::Config, context::PossiblyCurrentContext, display::Display},
    config::Api,
    context::ContextAttributesBuilder,
    display::GlDisplay,
};

use std::{
    fs,
    num::NonZeroU32,
    os::{
        fd::{AsRawFd, RawFd},
        unix::net::{UnixListener, UnixStream},
    },
    path::Path,
    sync::RwLock,
};

use raw_window_handle::{RawDisplayHandle, WaylandDisplayHandle};

use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState, Region},
    delegate_compositor, delegate_layer, delegate_output, delegate_registry,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    shell::{
        wlr_layer::{Layer, LayerShell, LayerShellHandler, LayerSurface, LayerSurfaceConfigure},
        WaylandSurface,
    },
};

use wayland_client::{
    globals::{registry_queue_init, GlobalList},
    protocol::{wl_output, wl_surface},
    Connection, QueueHandle,
};

use utils::communication::{get_socket_path, Answer, BgInfo, Request};

// We need this because this might be set by signals, so we can't keep it in the daemon
static EXIT: RwLock<bool> = RwLock::new(false);

fn exit_daemon() {
    let mut lock = EXIT.write().expect("failed to lock EXIT for writing");
    *lock = true;
}

fn should_daemon_exit() -> bool {
    *EXIT.read().expect("failed to read EXIT")
}

extern "C" fn signal_handler(_: i32) {
    exit_daemon();
}

type DaemonResult<T> = Result<T, String>;
fn main() -> DaemonResult<()> {
    make_logger();
    let listener = SocketWrapper::new()?;

    let handler = SigHandler::Handler(signal_handler);
    for signal in [Signal::SIGINT, Signal::SIGQUIT, Signal::SIGTERM] {
        unsafe { signal::signal(signal, handler).expect("Failed to install signal handler") };
    }

    let conn = Connection::connect_to_env().expect("failed to connect to the wayland server");
    // Enumerate the list of globals to get the protocols the server implements.
    let (globals, mut event_queue) =
        registry_queue_init(&conn).expect("failed to initialize the event queue");
    let qh = event_queue.handle();

    let mut daemon = Daemon::new(&conn, &globals, &qh);

    if let Ok(true) = sd_notify::booted() {
        if let Err(e) = sd_notify::notify(true, &[sd_notify::NotifyState::Ready]) {
            error!("Error sending status update to systemd: {}", e.to_string());
        }
    }
    info!("Initialization succeeded! Starting main loop...");
    let mut poll_handler = PollHandler::new(&listener);
    while !should_daemon_exit() {
        // Process wayland events
        event_queue
            .flush()
            .expect("failed to flush the event queue");
        event_queue
            .dispatch_pending(&mut daemon)
            .expect("failed to dispatch events");
        let read_guard = event_queue
            .prepare_read()
            .expect("failed to prepare the event queue's read");

        poll_handler.block(read_guard.connection_fd().as_raw_fd());

        if poll_handler.has_event(PollHandler::WAYLAND_FD) {
            read_guard.read().expect("failed to read the event queue");
            event_queue
                .dispatch_pending(&mut daemon)
                .expect("failed to dispatch events");
        }

        if poll_handler.has_event(PollHandler::SOCKET_FD) {
            match listener.0.accept() {
                Ok((stream, _addr)) => recv_socket_msg(&mut daemon, stream),
                Err(e) => match e.kind() {
                    std::io::ErrorKind::WouldBlock => (),
                    _ => return Err(format!("failed to accept incoming connection: {e}")),
                },
            }
        }
    }

    Ok(())
}

/// This is a wrapper that makes sure to delete the socket when it is dropped
/// It also makes sure to set the listener to nonblocking mode
struct SocketWrapper(UnixListener);
impl SocketWrapper {
    fn new() -> Result<Self, String> {
        let socket_addr = get_socket_path();
        let runtime_dir = match socket_addr.parent() {
            Some(path) => path,
            None => return Err("couldn't find a valid runtime directory".to_owned()),
        };

        if !runtime_dir.exists() {
            match fs::create_dir(runtime_dir) {
                Ok(()) => (),
                Err(e) => return Err(format!("failed to create runtime dir: {e}")),
            }
        }

        let listener = match UnixListener::bind(socket_addr.clone()) {
            Ok(address) => address,
            Err(e) => return Err(format!("couldn't bind socket: {e}")),
        };

        debug!(
            "Made socket in {:?} and initialized logger. Starting daemon...",
            listener.local_addr().unwrap() //this should always work if the socket connected correctly
        );

        if let Err(e) = listener.set_nonblocking(true) {
            let _ = fs::remove_file(&socket_addr);
            return Err(format!("failed to set socket to nonblocking mode: {e}"));
        }

        Ok(Self(listener))
    }
}

impl Drop for SocketWrapper {
    fn drop(&mut self) {
        let socket_addr = get_socket_path();
        if let Err(e) = fs::remove_file(&socket_addr) {
            error!("Failed to remove socket at {socket_addr:?}: {e}");
        }
        info!("Removed socket at {:?}", socket_addr);
    }
}

struct PollHandler {
    fds: [PollFd; 2],
}

impl PollHandler {
    const SOCKET_FD: usize = 0;
    const WAYLAND_FD: usize = 1;

    pub fn new(listener: &SocketWrapper) -> Self {
        Self {
            fds: [
                PollFd::new(listener.0.as_raw_fd(), PollFlags::POLLIN),
                PollFd::new(0, PollFlags::POLLIN),
            ],
        }
    }

    pub fn block(&mut self, wayland_fd: RawFd) {
        self.fds[Self::WAYLAND_FD] = PollFd::new(wayland_fd, PollFlags::POLLIN);
        match poll(&mut self.fds, -1) {
            Ok(_) => (),
            Err(e) => match e {
                nix::errno::Errno::EINTR => (),
                _ => panic!("failed to poll file descriptors: {e}"),
            },
        };
    }

    pub fn has_event(&self, fd_index: usize) -> bool {
        if let Some(flags) = self.fds[fd_index].revents() {
            !flags.is_empty()
        } else {
            false
        }
    }
}

struct Daemon {
    // Wayland stuff
    layer_shell: LayerShell,
    compositor_state: CompositorState,
    registry_state: RegistryState,
    output_state: OutputState,

    // glutin stuff
    context: PossiblyCurrentContext,
    config: Config,
    display: Display,

    // swww stuff
    wallpapers: Vec<Wallpaper>,
    renderer: Renderer,
}

impl Daemon {
    pub fn new(conn: &Connection, globals: &GlobalList, qh: &QueueHandle<Self>) -> Self {
        // The compositor (not to be confused with the server which is commonly called the compositor) allows
        // configuring surfaces to be presented.
        let compositor_state =
            CompositorState::bind(globals, qh).expect("wl_compositor is not available");

        let layer_shell = LayerShell::bind(globals, qh).expect("layer shell is not available");

        let mut handle = WaylandDisplayHandle::empty();
        handle.display = conn.backend().display_ptr() as *mut _;
        let display_handle = RawDisplayHandle::Wayland(handle);
        let display =
            unsafe { Display::new(display_handle).expect("failed to create egl display") };
        let config_template = glutin::config::ConfigTemplateBuilder::new()
            .with_api(Api::OPENGL)
            .with_alpha_size(0)
            .build();
        let config = unsafe {
            display
                .find_configs(config_template)
                .expect("failed to find display configurations")
                .next()
                .expect("empty display configurations")
        };
        let context = unsafe {
            display
                .create_context(
                    &config,
                    &ContextAttributesBuilder::new()
                        .with_debug(false)
                        .with_profile(glutin::context::GlProfile::Core)
                        .build(None),
                )
                .expect("failed to create egl context")
        }
        .make_current_surfaceless()
        .expect("failed to make egl context current");

        Self {
            // Outputs may be hotplugged at runtime, therefore we need to setup a registry state to
            // listen for Outputs.
            registry_state: RegistryState::new(globals),
            output_state: OutputState::new(globals, qh),
            compositor_state,
            layer_shell,

            renderer: Renderer::new(&display),
            wallpapers: Vec::new(),
            context,
            display,
            config,
        }
    }

    pub fn wallpapers_info(&self) -> Vec<BgInfo> {
        self.output_state
            .outputs()
            .filter_map(|output| {
                if let Some(info) = self.output_state.info(&output) {
                    if let Some(wallpaper) = self.wallpapers.iter().find(|w| w.output_id == info.id)
                    {
                        return Some(BgInfo {
                            name: info.name.unwrap_or("?".to_string()),
                            dim: info
                                .logical_size
                                .map(|(width, height)| (width as u32, height as u32))
                                .unwrap_or((0, 0)),
                            scale_factor: info.scale_factor,
                            img: wallpaper.img.clone(),
                        });
                    }
                }
                None
            })
            .collect()
    }

    pub fn find_wallpapers_id_by_names(&self, names: Vec<String>) -> Vec<u32> {
        self.output_state
            .outputs()
            .filter_map(|output| {
                if let Some(info) = self.output_state.info(&output) {
                    if let Some(name) = info.name {
                        if names.is_empty() || names.contains(&name) {
                            return Some(info.id);
                        }
                    }
                }
                None
            })
            .collect()
    }

    pub fn clear_by_id(&mut self, ids: Vec<u32>, color: [u8; 3]) {
        // TODO: STOP ANIMATIONS
        for wallpaper in self.wallpapers.iter_mut() {
            if ids.contains(&wallpaper.output_id) {
                wallpaper.clear(color);
                wallpaper.draw(&self.renderer, &self.context);
            }
        }
    }

    pub fn set_img_by_id(&mut self, ids: Vec<u32>, img: &[u8], path: &Path) {
        // TODO: STOP ANIMATIONS
        for wallpaper in self.wallpapers.iter_mut() {
            if ids.contains(&wallpaper.output_id) {
                wallpaper.set_img(img, path.to_owned());
                wallpaper.draw(&self.renderer, &self.context);
            }
        }
    }
}

impl CompositorHandler for Daemon {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        new_factor: i32,
    ) {
        for wallpaper in self.wallpapers.iter_mut() {
            if wallpaper.layer_surface.wl_surface() == surface {
                wallpaper.resize(
                    &self.context,
                    wallpaper.width,
                    wallpaper.height,
                    NonZeroU32::new(new_factor as u32).unwrap(),
                );
                wallpaper.draw(&self.renderer, &self.context);
                return;
            }
        }
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
        for wallpaper in self.wallpapers.iter_mut() {
            if wallpaper.layer_surface.wl_surface() == surface {
                wallpaper.draw(&self.renderer, &self.context);
                return;
            }
        }
    }
}

impl OutputHandler for Daemon {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        if let Some(output_info) = self.output_state.info(&output) {
            let surface = self.compositor_state.create_surface(qh);

            // Wayland clients are expected to render the cursor on their input region.
            // By setting the input region to an empty region, the compositor renders the
            // default cursor. Without this, an empty desktop won't render a cursor.
            if let Ok(region) = Region::new(&self.compositor_state) {
                surface.set_input_region(Some(region.wl_region()));
            }
            let layer_surface = self.layer_shell.create_layer_surface(
                qh,
                surface,
                Layer::Background,
                Some("swww"),
                Some(&output),
            );

            self.wallpapers.push(Wallpaper::new(
                output_info,
                layer_surface,
                &self.config,
                &self.display,
            ));
        }
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        if let Some(output_info) = self.output_state.info(&output) {
            if let Some(output_size) = output_info.logical_size {
                if output_size.0 == 0 || output_size.1 == 0 {
                    // TODO: print error
                    return;
                }
                for wallpaper in self.wallpapers.iter_mut() {
                    if wallpaper.output_id == output_info.id {
                        let (width, height) = (
                            NonZeroU32::new(output_size.0 as u32).unwrap(),
                            NonZeroU32::new(output_size.1 as u32).unwrap(),
                        );
                        let scale_factor =
                            NonZeroU32::new(output_info.scale_factor as u32).unwrap();
                        if (width, height, scale_factor)
                            != (wallpaper.width, wallpaper.height, wallpaper.scale_factor)
                        {
                            wallpaper.resize(&self.context, width, height, scale_factor);
                        }
                        return;
                    }
                }
            }
        }
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        if let Some(output_info) = self.output_state.info(&output) {
            self.wallpapers.retain(|w| w.output_id != output_info.id);
        }
    }
}

impl LayerShellHandler for Daemon {
    fn closed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, layer: &LayerSurface) {
        self.wallpapers.retain(|w| w.layer_surface != *layer)
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        for wallpaper in self.wallpapers.iter_mut() {
            if wallpaper.layer_surface == *layer {
                let (width, height) = if configure.new_size.0 == 0 || configure.new_size.1 == 0 {
                    (256.try_into().unwrap(), 256.try_into().unwrap())
                } else {
                    (
                        configure.new_size.0.try_into().unwrap(),
                        configure.new_size.1.try_into().unwrap(),
                    )
                };
                wallpaper.resize(&self.context, width, height, wallpaper.scale_factor);
                wallpaper.draw(&self.renderer, &self.context);
                return;
            }
        }
    }
}

delegate_compositor!(Daemon);
delegate_output!(Daemon);

delegate_layer!(Daemon);

delegate_registry!(Daemon);

impl ProvidesRegistryState for Daemon {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState];
}

fn make_logger() {
    let config = simplelog::ConfigBuilder::new()
        .set_thread_level(LevelFilter::Info) //let me see where the processing is happening
        .set_thread_mode(ThreadLogMode::Both)
        .build();

    TermLogger::init(
        LevelFilter::Debug,
        config,
        TerminalMode::Stderr,
        ColorChoice::AlwaysAnsi,
    )
    .expect("Failed to initialize logger. Cancelling...");
}

fn recv_socket_msg(daemon: &mut Daemon, stream: UnixStream) {
    let request = Request::receive(&stream);
    let answer = match request {
        Ok(request) => match request {
            Request::Animation(_animations) => Answer::Err("Not implemented".to_string()),
            Request::Clear(clear) => {
                let ids = daemon.find_wallpapers_id_by_names(clear.outputs);
                daemon.clear_by_id(ids, clear.color);
                Answer::Ok
            }
            Request::Init => Answer::Ok,
            Request::Kill => {
                exit_daemon();
                Answer::Ok
            }
            Request::Query => Answer::Info(daemon.wallpapers_info()),
            Request::Img((_transition, imgs)) => {
                for img in imgs {
                    let ids = daemon.find_wallpapers_id_by_names(img.1);
                    daemon.set_img_by_id(ids, &img.0.img, &img.0.path);
                }
                Answer::Ok
            }
        },
        Err(e) => Answer::Err(e),
    };
    if let Err(e) = answer.send(&stream) {
        error!("error sending answer to client: {e}");
    }
}
