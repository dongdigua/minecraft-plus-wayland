use std::{env, error::Error, time::Instant};

use smithay_client_toolkit::reexports::client::{
    Connection, QueueHandle,
    globals::registry_queue_init,
    protocol::{wl_keyboard, wl_output, wl_seat, wl_surface},
};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState, FrameCallbackData},
    delegate_registry,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        Capability, SeatHandler, SeatState,
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers},
    },
    session_lock::{
        SessionLock, SessionLockHandler, SessionLockState, SessionLockSurface,
        SessionLockSurfaceConfigure,
    },
    shell::{
        WaylandSurface,
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
    },
};

use crate::{
    renderer::{RenderOutcome, Renderer},
    scene::{FrameInfo, RenderSize, Scene, SquidScene, TriangleScene},
};

const FALLBACK_SIZE: RenderSize = RenderSize {
    width: 1280,
    height: 720,
};

#[derive(Clone, Copy, Debug)]
enum StartupMode {
    LayerShell,
    SessionLock,
}

#[derive(Clone, Copy, Debug)]
enum SceneSelection {
    Triangle,
    Squid,
}

impl SceneSelection {
    fn create(self) -> Box<dyn Scene> {
        match self {
            Self::Triangle => Box::<TriangleScene>::default(),
            Self::Squid => Box::<SquidScene>::default(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct StartupOptions {
    mode: StartupMode,
    scene: SceneSelection,
}

pub fn run() -> Result<(), Box<dyn Error>> {
    let startup = parse_startup_options()?;
    let startup_mode = startup.mode;
    let scene_selection = startup.scene;
    let connection = Connection::connect_to_env()?;
    let (globals, mut event_queue) = registry_queue_init(&connection)?;
    let qh = event_queue.handle();

    let compositor_state = CompositorState::bind(&globals, &qh)?;
    let output_state = OutputState::new(&globals, &qh);
    let registry_state = RegistryState::new(&globals);
    let seat_state = SeatState::new(&globals, &qh);

    let (mode, session_lock_state) = match startup_mode {
        StartupMode::LayerShell => {
            let layer_shell = LayerShell::bind(&globals, &qh)?;
            let surface = compositor_state.create_surface(&qh);
            let layer = layer_shell.create_layer_surface(
                &qh,
                surface,
                Layer::Background,
                Some("minecraft-plus-wayland"),
                None,
            );
            layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
            layer.set_size(0, 0);
            layer.set_exclusive_zone(-1);
            layer.set_keyboard_interactivity(KeyboardInteractivity::None);

            let render = RenderState::new(
                Renderer::new(&connection, layer.wl_surface())?,
                scene_selection,
            );
            (
                Mode::Layer(Box::new(LayerTarget {
                    render,
                    surface: layer,
                })),
                None,
            )
        }
        StartupMode::SessionLock => {
            let session_lock_state = SessionLockState::new(&globals, &qh);
            let session_lock = session_lock_state.lock(&qh)?;
            let outputs = output_state.outputs().collect::<Vec<_>>();
            if outputs.is_empty() {
                return Err("session-lock requires at least one Wayland output".into());
            }

            let mut targets = Vec::with_capacity(outputs.len());
            for output in outputs {
                let surface = compositor_state.create_surface(&qh);
                let lock_surface = session_lock.create_lock_surface(surface, &output, &qh);
                let render = RenderState::new(
                    Renderer::new(&connection, lock_surface.wl_surface())?,
                    scene_selection,
                );
                targets.push(LockTarget {
                    render,
                    output,
                    surface: lock_surface,
                });
            }

            (
                Mode::Lock(LockTargets {
                    targets,
                    session_lock,
                }),
                Some(session_lock_state),
            )
        }
    };

    let mut app = App {
        mode,
        _session_lock_state: session_lock_state,
        _compositor_state: compositor_state,
        output_state,
        registry_state,
        seat_state,
        keyboard: None,
        exit: false,
    };

    if let Mode::Layer(target) = &app.mode {
        target.surface.commit();
    }

    while !app.exit {
        event_queue.blocking_dispatch(&mut app)?;
    }

    Ok(())
}

fn parse_startup_options() -> Result<StartupOptions, Box<dyn Error>> {
    parse_options(env::args().skip(1))
}

fn parse_options(
    arguments: impl IntoIterator<Item = String>,
) -> Result<StartupOptions, Box<dyn Error>> {
    let mut mode = StartupMode::LayerShell;
    let mut scene = SceneSelection::Triangle;
    let mut arguments = arguments.into_iter();

    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--lock" => mode = StartupMode::SessionLock,
            "--scene" => {
                let module = arguments
                    .next()
                    .ok_or("--scene requires a module number")?
                    .parse::<u8>()
                    .map_err(|_| "--scene requires an integer module number")?;
                scene = match module {
                    8 => SceneSelection::Squid,
                    0..=12 => {
                        return Err(format!(
                            "module={module} is not implemented natively; only module=8 (squid) is available"
                        )
                        .into());
                    }
                    _ => {
                        return Err(
                            format!("module={module} is outside the valid range 0..=12").into()
                        );
                    }
                };
            }
            "--help" | "-h" => {
                return Err(
                    "Usage: minecraft-plus-wayland [--lock] [--scene <n>]\n\n--scene 8 selects Web module=8 (squid)."
                        .into(),
                );
            }
            _ => {
                return Err(format!(
                    "unknown argument {argument:?}; usage: minecraft-plus-wayland [--lock] [--scene <n>]"
                )
                .into());
            }
        }
    }

    Ok(StartupOptions { mode, scene })
}

#[cfg(test)]
mod startup_option_tests {
    use super::{SceneSelection, StartupMode, parse_options};

    #[test]
    fn squid_scene_maps_to_module_eight() {
        let options = parse_options(["--scene".to_owned(), "8".to_owned()]).unwrap();
        assert!(matches!(options.mode, StartupMode::LayerShell));
        assert!(matches!(options.scene, SceneSelection::Squid));
    }

    #[test]
    fn lock_and_scene_are_order_independent() {
        let options =
            parse_options(["--scene".to_owned(), "8".to_owned(), "--lock".to_owned()]).unwrap();
        assert!(matches!(options.mode, StartupMode::SessionLock));
        assert!(matches!(options.scene, SceneSelection::Squid));
    }

    #[test]
    fn other_module_numbers_are_not_silently_mapped_to_squid() {
        let error = parse_options(["--scene".to_owned(), "7".to_owned()]).unwrap_err();
        assert!(error.to_string().contains("module=7 is not implemented"));
    }
}

struct RenderState {
    renderer: Renderer,
    scene: Box<dyn Scene>,
    configured_size: Option<RenderSize>,
    scene_initialized: bool,
    frame_pending: bool,
    started_at: Instant,
    last_frame_at: Instant,
}

impl RenderState {
    fn new(renderer: Renderer, scene_selection: SceneSelection) -> Self {
        Self {
            renderer,
            scene: scene_selection.create(),
            configured_size: None,
            scene_initialized: false,
            frame_pending: false,
            started_at: Instant::now(),
            last_frame_at: Instant::now(),
        }
    }

    fn configure(&mut self, requested_size: (u32, u32)) -> Result<(), Box<dyn Error>> {
        let previous = self.configured_size.unwrap_or(FALLBACK_SIZE);
        let size = RenderSize {
            width: if requested_size.0 == 0 {
                previous.width
            } else {
                requested_size.0
            },
            height: if requested_size.1 == 0 {
                previous.height
            } else {
                requested_size.1
            },
        };

        self.renderer.configure(size)?;
        self.configured_size = Some(size);

        let context = self.renderer.context();
        if !self.scene_initialized {
            self.scene.initialize(&context)?;
            self.scene_initialized = true;
        }
        self.scene.resize(&context, size);
        Ok(())
    }

    fn render(&mut self) -> Result<RenderOutcome, Box<dyn Error>> {
        let Some(size) = self.configured_size else {
            return Ok(RenderOutcome::Skipped);
        };

        let now = Instant::now();
        let frame = FrameInfo {
            elapsed: now.duration_since(self.started_at),
            delta: now.duration_since(self.last_frame_at),
            size,
        };
        self.last_frame_at = now;
        self.scene.update(frame);
        self.renderer.render(self.scene.as_mut(), frame)
    }
}

struct LayerTarget {
    render: RenderState,
    surface: LayerSurface,
}

struct LockTarget {
    render: RenderState,
    output: wl_output::WlOutput,
    surface: SessionLockSurface,
}

struct LockTargets {
    targets: Vec<LockTarget>,
    session_lock: SessionLock,
}

enum Mode {
    Layer(Box<LayerTarget>),
    Lock(LockTargets),
}

struct App {
    mode: Mode,
    _session_lock_state: Option<SessionLockState>,
    _compositor_state: CompositorState,
    output_state: OutputState,
    registry_state: RegistryState,
    seat_state: SeatState,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    exit: bool,
}

impl App {
    fn configure_layer(
        &mut self,
        configure: LayerSurfaceConfigure,
        qh: &QueueHandle<Self>,
    ) -> Result<(), Box<dyn Error>> {
        let Mode::Layer(target) = &mut self.mode else {
            return Ok(());
        };
        target.render.configure(configure.new_size)?;
        Self::render_and_schedule(&mut target.render, target.surface.wl_surface(), qh)
    }

    fn configure_lock(
        &mut self,
        surface: &SessionLockSurface,
        configure: SessionLockSurfaceConfigure,
        qh: &QueueHandle<Self>,
    ) -> Result<(), Box<dyn Error>> {
        let Mode::Lock(lock) = &mut self.mode else {
            return Ok(());
        };
        let Some(target) = lock
            .targets
            .iter_mut()
            .find(|target| target.surface.wl_surface() == surface.wl_surface())
        else {
            return Ok(());
        };
        target.render.configure(configure.new_size)?;
        Self::render_and_schedule(&mut target.render, target.surface.wl_surface(), qh)
    }

    fn render_surface(
        &mut self,
        surface: &wl_surface::WlSurface,
        qh: &QueueHandle<Self>,
    ) -> Result<(), Box<dyn Error>> {
        match &mut self.mode {
            Mode::Layer(target) if target.surface.wl_surface() == surface => {
                Self::render_and_schedule(&mut target.render, target.surface.wl_surface(), qh)
            }
            Mode::Lock(lock) => {
                let Some(target) = lock
                    .targets
                    .iter_mut()
                    .find(|target| target.surface.wl_surface() == surface)
                else {
                    return Ok(());
                };
                Self::render_and_schedule(&mut target.render, target.surface.wl_surface(), qh)
            }
            Mode::Layer(_) => Ok(()),
        }
    }

    fn render_and_schedule(
        render: &mut RenderState,
        surface: &wl_surface::WlSurface,
        qh: &QueueHandle<Self>,
    ) -> Result<(), Box<dyn Error>> {
        if render.scene.wants_continuous_frames() && !render.frame_pending {
            surface.frame(qh, FrameCallbackData(surface.clone()));
            render.frame_pending = true;
        }
        render.render()?;
        Ok(())
    }

    fn exit_after_keypress(&mut self, connection: &Connection) {
        if let Mode::Lock(lock) = &self.mode {
            // Temporary GPU-context smoke test only: any key unlocks without password validation.
            lock.session_lock.unlock();
            let _ = connection.flush();
        }
        self.exit = true;
    }
}

impl LayerShellHandler for App {
    fn closed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _layer: &LayerSurface) {
        self.exit = true;
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        _layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        if let Err(error) = self.configure_layer(configure, qh) {
            eprintln!("failed to configure layer surface: {error}");
            self.exit = true;
        }
    }
}

impl SessionLockHandler for App {
    fn locked(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _session_lock: SessionLock) {}

    fn finished(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _session_lock: SessionLock,
    ) {
        self.exit = true;
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        surface: SessionLockSurface,
        configure: SessionLockSurfaceConfigure,
        _serial: u32,
    ) {
        if let Err(error) = self.configure_lock(&surface, configure, qh) {
            eprintln!("failed to configure session-lock surface: {error}");
            self.exit = true;
        }
    }
}

impl CompositorHandler for App {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_factor: i32,
    ) {
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
        match &mut self.mode {
            Mode::Layer(target) if target.surface.wl_surface() == surface => {
                target.render.frame_pending = false;
            }
            Mode::Lock(lock) => {
                if let Some(target) = lock
                    .targets
                    .iter_mut()
                    .find(|target| target.surface.wl_surface() == surface)
                {
                    target.render.frame_pending = false;
                }
            }
            Mode::Layer(_) => return,
        }

        if let Err(error) = self.render_surface(surface, qh) {
            eprintln!("rendering stopped: {error}");
            self.exit = true;
        }
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        if let Mode::Lock(lock) = &mut self.mode {
            lock.targets.retain(|target| target.output != output);
        }
    }
}

impl SeatHandler for App {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _seat: wl_seat::WlSeat) {}

    fn new_capability(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            self.keyboard = self.seat_state.get_keyboard(qh, &seat, None).ok();
        }
    }

    fn remove_capability(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard
            && let Some(keyboard) = self.keyboard.take()
        {
            keyboard.release();
        }
    }

    fn remove_seat(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _seat: wl_seat::WlSeat) {
    }
}

impl KeyboardHandler for App {
    fn enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _surface: &wl_surface::WlSurface,
        _serial: u32,
        _raw: &[u32],
        _keysyms: &[Keysym],
    ) {
    }

    fn leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _surface: &wl_surface::WlSurface,
        _serial: u32,
    ) {
    }

    fn press_key(
        &mut self,
        connection: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        _event: KeyEvent,
    ) {
        self.exit_after_keypress(connection);
    }

    fn repeat_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        _event: KeyEvent,
    ) {
    }

    fn release_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        _event: KeyEvent,
    ) {
    }

    fn update_modifiers(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        _modifiers: Modifiers,
        _raw_modifiers: RawModifiers,
        _layout: u32,
    ) {
    }
}

delegate_registry!(App);

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }

    registry_handlers![OutputState, SeatState];
}

smithay_client_toolkit::delegate_dispatch2!(App);
