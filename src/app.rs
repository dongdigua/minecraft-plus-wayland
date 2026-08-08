use std::{
    env,
    error::Error,
    time::{Duration, Instant},
};

use rand::Rng;
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState, FrameCallbackData},
    delegate_registry,
    output::{OutputHandler, OutputState},
    reexports::{
        calloop::{
            EventLoop, LoopHandle, RegistrationToken,
            channel::{self, Event as ChannelEvent},
            timer::{TimeoutAction, Timer},
        },
        calloop_wayland_source::WaylandSource,
        client::{
            Connection, Dispatch, QueueHandle,
            globals::registry_queue_init,
            protocol::{wl_callback, wl_keyboard, wl_output, wl_seat, wl_surface},
        },
    },
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
    lock::{
        animations::LockAnimation,
        auth::pam::PamAuthenticator,
        identity::TrustedIdentity,
        secret::{LockedSecret, SecretError, disable_process_dumps},
        state::{AttemptId, AuthDecision, LockState, LockVisual},
        worker::{AuthReply, AuthRequest, AuthWorker},
    },
    modules::{
        AlphaFluidVariant, AlphaFluidsModule, BlocksModule, CreeperModule, DvdBounceModule,
        DvdBounceVariant, FootprintModule, FrameInfo, GrassModule, ItemBounceModule, ItemPopModule,
        LoadCubeModule, Module, PanoramaModule, RenderSize, SquidModule,
    },
    renderer::{RenderOutcome, Renderer},
};

const FALLBACK_SIZE: RenderSize = RenderSize {
    width: 1280,
    height: 720,
};
const UNLOCK_FLUSH_RETRY: Duration = Duration::from_millis(16);

#[derive(Clone, Copy, Debug)]
enum StartupMode {
    LayerShell,
    SessionLock,
}

#[derive(Clone, Copy, Debug)]
enum ModuleSelection {
    LoadCube,
    ItemPop,
    ItemBounce,
    DvdBounce(DvdBounceVariant),
    AlphaFluids(AlphaFluidVariant),
    Panorama,
    Footprint,
    Grass,
    Blocks,
    Squid,
    Creeper,
}

impl ModuleSelection {
    fn from_id(module_id: u8) -> Option<Self> {
        Some(match module_id {
            0 => Self::LoadCube,
            1 => Self::DvdBounce(DvdBounceVariant::Trail),
            2 => Self::DvdBounce(DvdBounceVariant::Direct),
            3 => Self::ItemPop,
            4 => Self::AlphaFluids(AlphaFluidVariant::Water),
            5 => Self::AlphaFluids(AlphaFluidVariant::Lava),
            6 => Self::Panorama,
            7 => Self::Footprint,
            8 => Self::Squid,
            9 => Self::ItemBounce,
            10 => Self::Grass,
            11 => Self::Blocks,
            12 => Self::Creeper,
            _ => return None,
        })
    }

    fn id(self) -> u8 {
        match self {
            Self::LoadCube => 0,
            Self::DvdBounce(DvdBounceVariant::Trail) => 1,
            Self::DvdBounce(DvdBounceVariant::Direct) => 2,
            Self::ItemPop => 3,
            Self::AlphaFluids(AlphaFluidVariant::Water) => 4,
            Self::AlphaFluids(AlphaFluidVariant::Lava) => 5,
            Self::Panorama => 6,
            Self::Footprint => 7,
            Self::Squid => 8,
            Self::ItemBounce => 9,
            Self::Grass => 10,
            Self::Blocks => 11,
            Self::Creeper => 12,
        }
    }

    fn create(self) -> Box<dyn Module> {
        match self {
            Self::LoadCube => Box::<LoadCubeModule>::default(),
            Self::ItemPop => Box::<ItemPopModule>::default(),
            Self::ItemBounce => Box::<ItemBounceModule>::default(),
            Self::DvdBounce(variant) => Box::new(DvdBounceModule::new(variant)),
            Self::AlphaFluids(variant) => Box::new(AlphaFluidsModule::new(variant)),
            Self::Panorama => Box::<PanoramaModule>::default(),
            Self::Footprint => Box::<FootprintModule>::default(),
            Self::Grass => Box::<GrassModule>::default(),
            Self::Blocks => Box::<BlocksModule>::default(),
            Self::Squid => Box::<SquidModule>::default(),
            Self::Creeper => Box::<CreeperModule>::default(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct StartupOptions {
    mode: StartupMode,
    module: ModuleSelection,
    module_switch_interval: Option<Duration>,
}

struct LockSetup {
    worker: AuthWorker,
    replies: channel::Channel<AuthReply>,
    password: LockedSecret,
}

impl LockSetup {
    fn new() -> Result<Self, Box<dyn Error>> {
        let identity = TrustedIdentity::discover()?;
        let dump_protection = disable_process_dumps()?;
        let authenticator = PamAuthenticator::new(dump_protection);
        let (reply_sender, replies) = channel::sync_channel(1);
        let worker = AuthWorker::spawn_pam(identity.into_username(), authenticator, reply_sender)?;
        Ok(Self {
            worker,
            replies,
            password: LockedSecret::new()?,
        })
    }
}

pub fn run() -> Result<(), Box<dyn Error>> {
    let startup = parse_startup_options()?;
    // Validate the interval and establish the shared module timeline before requesting a session
    // lock. Returning an ordinary startup error after lock() would drop a requested lock object.
    let module_started_at = Instant::now();
    let next_module_switch =
        initial_module_deadline(module_started_at, startup.module_switch_interval)?;
    // Identity, dump hardening, worker construction and the editable secret all succeed before the
    // client requests a session lock.
    let mut lock_setup = match startup.mode {
        StartupMode::LayerShell => None,
        StartupMode::SessionLock => Some(LockSetup::new()?),
    };

    let connection = Connection::connect_to_env()?;
    let (globals, event_queue) = registry_queue_init(&connection)?;
    let qh = event_queue.handle();
    let mut event_loop: EventLoop<App> = EventLoop::try_new()?;
    let loop_handle = event_loop.handle();

    let compositor_state = CompositorState::bind(&globals, &qh)?;
    let output_state = OutputState::new(&globals, &qh);
    let registry_state = RegistryState::new(&globals);
    let seat_state = SeatState::new(&globals, &qh);

    let outputs = output_state.outputs().collect::<Vec<_>>();
    let mut lock_replies = None;
    let (mode, session_lock_state) = match startup.mode {
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
                startup.module,
                false,
                module_started_at,
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
            if outputs.is_empty() {
                return Err("session-lock requires at least one Wayland output".into());
            }
            let session_lock_state = SessionLockState::new(&globals, &qh);
            let session_lock = session_lock_state.lock(&qh)?;
            let LockSetup {
                worker,
                replies,
                password,
            } = lock_setup.take().expect("lock setup exists in lock mode");
            lock_replies = Some(replies);
            (
                Mode::Lock(Box::new(LockTargets {
                    targets: Vec::with_capacity(outputs.len()),
                    session_lock,
                    state: LockState::new(),
                    password,
                    worker,
                    unlock_sync: None,
                    redraw_all: false,
                    last_visual: LockVisual::Hidden,
                })),
                Some(session_lock_state),
            )
        }
    };

    let mut app = App {
        mode,
        _session_lock_state: session_lock_state,
        compositor_state,
        output_state,
        registry_state,
        seat_state,
        keyboards: Vec::new(),
        module_selection: startup.module,
        module_started_at,
        module_switch_interval: startup.module_switch_interval,
        next_module_switch,
        loop_handle: loop_handle.clone(),
        deadline_timer: None,
        scheduled_deadline: None,
        exit: false,
        exit_failure: None,
        fatal_disconnect: false,
    };

    if matches!(app.mode, Mode::Lock(_)) {
        for output in outputs {
            if let Err(error) = app.add_lock_output(&connection, &qh, output) {
                log::error!(target: "minecraft_plus_wayland::lock", "cannot cover initial output: {error}");
                app.clear_editable_secret();
                // Do not normally drop a requested session-lock object before locked/finished.
                std::process::exit(1);
            }
        }
    } else if let Mode::Layer(target) = &app.mode {
        target.surface.commit();
    }

    if let Some(replies) = lock_replies
        && let Err(error) = event_loop
            .handle()
            .insert_source(replies, |event, &mut (), app| match event {
                ChannelEvent::Msg(reply) => app.handle_auth_reply(reply),
                ChannelEvent::Closed => app.handle_worker_closed(),
            })
    {
        log::error!(target: "minecraft_plus_wayland::lock", "cannot install authentication result source: {error}");
        app.clear_editable_secret();
        std::process::exit(1);
    }

    if let Err(error) = WaylandSource::new(connection.clone(), event_queue).insert(loop_handle) {
        if matches!(app.mode, Mode::Lock(_)) {
            log::error!(target: "minecraft_plus_wayland::lock", "cannot install Wayland event source: {error}");
            app.clear_editable_secret();
            std::process::exit(1);
        }
        return Err(error.into());
    }

    while !app.exit {
        if let Err(error) = event_loop.dispatch(None, &mut app) {
            if matches!(app.mode, Mode::Lock(_)) {
                log::error!(target: "minecraft_plus_wayland::lock", "event loop dispatch failed: {error}");
                app.clear_editable_secret();
                std::process::exit(1);
            }
            return Err(error.into());
        }
        // A wl_display.sync Done callback can mark authenticated shutdown while dispatching.
        // Do not run another render/timer pass against lock surfaces that unlock_and_destroy has
        // already invalidated.
        if app.exit {
            break;
        }
        app.after_dispatch(&connection, &qh);
        if app.fatal_disconnect {
            app.clear_editable_secret();
            std::process::exit(1);
        }
    }
    log::info!(target: "minecraft_plus_wayland::lock", "event loop stopped; tearing down client resources");
    if let Some(message) = app.exit_failure {
        return Err(message.into());
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
    let mut module = None;
    let mut interval_seconds = None;
    let mut interval_supplied = false;
    let mut arguments = arguments.into_iter();

    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--lock" => mode = StartupMode::SessionLock,
            "--module" | "-m" => {
                let module_id = arguments
                    .next()
                    .ok_or("--module/-m requires a module number")?
                    .parse::<u8>()
                    .map_err(|_| "--module/-m requires an integer module number")?;
                module = Some(ModuleSelection::from_id(module_id).ok_or_else(|| {
                    format!("module={module_id} is outside the valid range 0..=12")
                })?);
            }
            "--interval" | "-t" => {
                interval_supplied = true;
                interval_seconds = Some(
                    arguments
                        .next()
                        .ok_or("--interval/-t requires a number of seconds")?
                        .parse::<u64>()
                        .map_err(
                            |_| "--interval/-t requires a non-negative integer number of seconds",
                        )?,
                );
            }
            "--help" | "-h" => {
                return Err(
                    "Usage: minecraft-plus-wayland [--lock] [--module|-m <n> | --interval|-t <seconds>]\n\nWithout --module, one of the 13 Web modules is selected randomly at startup. --interval/-t switches to a different random module every whole <seconds>; 0 or omitting the option disables switching. --interval/-t conflicts with --module/-m. --module 0 selects Web module=0 (load cube); --module 1 selects module=1 (dvd bounce trail); --module 2 selects module=2 (dvd bounce direct); --module 3 selects module=3 (item pop); --module 4 selects module=4 (alpha fluids water); --module 5 selects module=5 (alpha fluids lava); --module 6 selects module=6 (panorama); --module 7 selects module=7 (footprint); --module 8 selects module=8 (squid); --module 9 selects module=9 (item bounce); --module 10 selects module=10 (grass); --module 11 selects module=11 (blocks); --module 12 selects module=12 (creeper)."
                        .into(),
                );
            }
            _ => {
                return Err(format!(
                    "unknown argument {argument:?}; usage: minecraft-plus-wayland [--lock] [--module|-m <n> | --interval|-t <seconds>]"
                )
                .into());
            }
        }
    }

    if module.is_some() && interval_supplied {
        return Err("--module/-m conflicts with --interval/-t".into());
    }

    let module = module.unwrap_or_else(|| {
        let module_id = rand::thread_rng().gen_range(0, 13);
        let selection = ModuleSelection::from_id(module_id)
            .expect("random module id must remain inside the 13-entry module table");
        log::info!(
            target: "minecraft_plus_wayland::startup",
            "no --module argument supplied; randomly selected module={}",
            selection.id(),
        );
        selection
    });
    let module_switch_interval = interval_seconds
        .filter(|seconds| *seconds != 0)
        .map(Duration::from_secs);
    Ok(StartupOptions {
        mode,
        module,
        module_switch_interval,
    })
}

fn initial_module_deadline(
    started_at: Instant,
    interval: Option<Duration>,
) -> Result<Option<Instant>, &'static str> {
    interval
        .map(|interval| {
            started_at
                .checked_add(interval)
                .ok_or("--interval is too large to schedule")
        })
        .transpose()
}

fn module_id_excluding(current: u8, random_slot: u8) -> u8 {
    debug_assert!(current < 13);
    debug_assert!(random_slot < 12);
    if random_slot >= current {
        random_slot + 1
    } else {
        random_slot
    }
}

fn random_module_excluding(current: ModuleSelection) -> ModuleSelection {
    let random_slot = rand::thread_rng().gen_range(0, 12);
    ModuleSelection::from_id(module_id_excluding(current.id(), random_slot))
        .expect("random replacement module id must remain inside the 13-entry module table")
}

fn advance_periodic_deadline(
    deadline: Instant,
    interval: Duration,
    now: Instant,
) -> Option<Instant> {
    debug_assert!(!interval.is_zero());
    if deadline > now {
        return Some(deadline);
    }
    let elapsed_periods =
        now.saturating_duration_since(deadline).as_nanos() / interval.as_nanos() + 1;
    u32::try_from(elapsed_periods)
        .ok()
        .and_then(|periods| interval.checked_mul(periods))
        .and_then(|elapsed| deadline.checked_add(elapsed))
        .or_else(|| now.checked_add(interval))
}

fn earliest_deadline(first: Option<Instant>, second: Option<Instant>) -> Option<Instant> {
    match (first, second) {
        (Some(first), Some(second)) => Some(first.min(second)),
        (Some(deadline), None) | (None, Some(deadline)) => Some(deadline),
        (None, None) => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RenderReport {
    outcome: RenderOutcome,
    frame_time: Instant,
}

struct RenderState {
    renderer: Renderer,
    module: Box<dyn Module>,
    overlay: Option<LockAnimation>,
    configured_size: Option<RenderSize>,
    module_initialized: bool,
    frame_pending: bool,
    started_at: Instant,
    last_frame_at: Instant,
}

impl RenderState {
    fn new(
        renderer: Renderer,
        module_selection: ModuleSelection,
        lock_overlay: bool,
        module_started_at: Instant,
    ) -> Self {
        Self {
            renderer,
            module: module_selection.create(),
            overlay: lock_overlay.then(LockAnimation::new),
            configured_size: None,
            module_initialized: false,
            frame_pending: false,
            started_at: module_started_at,
            last_frame_at: module_started_at,
        }
    }

    fn prepare_module(
        &self,
        module_selection: ModuleSelection,
    ) -> Result<Box<dyn Module>, Box<dyn Error>> {
        let mut module = module_selection.create();
        if let Some(size) = self.configured_size {
            let context = self.renderer.context();
            module.initialize(&context)?;
            module.resize(&context, size);
        }
        Ok(module)
    }

    fn replace_module(&mut self, module: Box<dyn Module>, now: Instant) {
        self.module = module;
        self.module_initialized = self.configured_size.is_some();
        self.started_at = now;
        self.last_frame_at = now;
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
        if !self.module_initialized {
            self.module.initialize(&context)?;
            self.module_initialized = true;
        }
        self.module.resize(&context, size);
        if let Some(overlay) = &mut self.overlay {
            overlay.ensure_initialized(&context, size)?;
        }
        Ok(())
    }

    fn render(
        &mut self,
        visual: LockVisual,
        frame_time: Instant,
    ) -> Result<RenderOutcome, Box<dyn Error>> {
        let Some(size) = self.configured_size else {
            return Ok(RenderOutcome::Skipped);
        };
        let frame = FrameInfo {
            elapsed: frame_time.duration_since(self.started_at),
            delta: frame_time.duration_since(self.last_frame_at),
            size,
        };
        self.last_frame_at = frame_time;
        self.module.update(frame);
        self.renderer.render(
            self.module.as_mut(),
            frame,
            self.overlay
                .as_mut()
                .map(|overlay| (overlay, visual, frame_time)),
        )
    }
}

struct LayerTarget {
    render: RenderState,
    surface: LayerSurface,
}

struct LockTarget {
    // Renderer must be dropped before the protocol surface it references.
    render: RenderState,
    output: wl_output::WlOutput,
    surface: SessionLockSurface,
    dissolve_complete_presented: Option<AttemptId>,
}

struct LockTargets {
    targets: Vec<LockTarget>,
    session_lock: SessionLock,
    state: LockState,
    password: LockedSecret,
    worker: AuthWorker,
    unlock_sync: Option<Box<wl_callback::WlCallback>>,
    redraw_all: bool,
    last_visual: LockVisual,
}

enum Mode {
    Layer(Box<LayerTarget>),
    Lock(Box<LockTargets>),
}

struct SeatKeyboard {
    seat: wl_seat::WlSeat,
    keyboard: wl_keyboard::WlKeyboard,
    repeat_allowed: bool,
}

struct App {
    mode: Mode,
    _session_lock_state: Option<SessionLockState>,
    compositor_state: CompositorState,
    output_state: OutputState,
    registry_state: RegistryState,
    seat_state: SeatState,
    keyboards: Vec<SeatKeyboard>,
    module_selection: ModuleSelection,
    module_started_at: Instant,
    module_switch_interval: Option<Duration>,
    next_module_switch: Option<Instant>,
    loop_handle: LoopHandle<'static, App>,
    deadline_timer: Option<RegistrationToken>,
    scheduled_deadline: Option<Instant>,
    exit: bool,
    exit_failure: Option<&'static str>,
    fatal_disconnect: bool,
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
        Self::render_and_schedule(
            &mut target.render,
            target.surface.wl_surface(),
            qh,
            LockVisual::Hidden,
        )?;
        Ok(())
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
        if !lock.state.can_use_lock_surfaces() {
            return Ok(());
        }
        let visual = lock.state.visual(Instant::now());
        let Some(target) = lock
            .targets
            .iter_mut()
            .find(|target| target.surface.wl_surface() == surface.wl_surface())
        else {
            return Ok(());
        };
        target.render.configure(configure.new_size)?;
        let report =
            Self::render_and_schedule(&mut target.render, target.surface.wl_surface(), qh, visual)?;
        Self::record_dissolve_complete_present(target, visual, report);
        Ok(())
    }

    fn render_surface(
        &mut self,
        surface: &wl_surface::WlSurface,
        qh: &QueueHandle<Self>,
    ) -> Result<(), Box<dyn Error>> {
        match &mut self.mode {
            Mode::Layer(target) if target.surface.wl_surface() == surface => {
                Self::render_and_schedule(
                    &mut target.render,
                    target.surface.wl_surface(),
                    qh,
                    LockVisual::Hidden,
                )?;
            }
            Mode::Lock(lock) => {
                if !lock.state.can_use_lock_surfaces() {
                    return Ok(());
                }
                let visual = lock.state.visual(Instant::now());
                let Some(target) = lock
                    .targets
                    .iter_mut()
                    .find(|target| target.surface.wl_surface() == surface)
                else {
                    return Ok(());
                };
                let report = Self::render_and_schedule(
                    &mut target.render,
                    target.surface.wl_surface(),
                    qh,
                    visual,
                )?;
                Self::record_dissolve_complete_present(target, visual, report);
            }
            Mode::Layer(_) => {}
        }
        Ok(())
    }

    fn render_and_schedule(
        render: &mut RenderState,
        surface: &wl_surface::WlSurface,
        qh: &QueueHandle<Self>,
        visual: LockVisual,
    ) -> Result<RenderReport, Box<dyn Error>> {
        // This one timestamp drives the shader uniforms, frame-chain decision and completed-frame
        // marker. Sampling time again after encoding could mark an incomplete dissolve as complete.
        let frame_time = Instant::now();
        let overlay_continuous = render
            .overlay
            .as_ref()
            .is_some_and(|overlay| overlay.wants_continuous_frames(visual, frame_time));
        let continuous = continuous_frame_required(
            visual,
            render.module.wants_continuous_frames(),
            overlay_continuous,
        );
        if continuous && !render.frame_pending {
            surface.frame(qh, FrameCallbackData(surface.clone()));
            render.frame_pending = true;
        }
        let outcome = render.render(visual, frame_time)?;
        // A converged opaque overlay normally has no frame chain. If a one-shot state redraw is
        // skipped because the surface is temporarily unavailable, attach a callback to an empty
        // commit so the new mask/visual is retried instead of being lost permanently.
        if outcome == RenderOutcome::Skipped {
            if !render.frame_pending {
                surface.frame(qh, FrameCallbackData(surface.clone()));
                render.frame_pending = true;
            }
            surface.commit();
        }
        Ok(RenderReport {
            outcome,
            frame_time,
        })
    }

    fn record_dissolve_complete_present(
        target: &mut LockTarget,
        visual: LockVisual,
        report: RenderReport,
    ) {
        if report.outcome == RenderOutcome::Presented
            && let Some(attempt) = visual.completed_dissolve_attempt(report.frame_time)
        {
            target.dissolve_complete_presented = Some(attempt);
        }
    }

    fn redraw_all_lock_targets(&mut self, qh: &QueueHandle<Self>) -> Result<(), Box<dyn Error>> {
        let Mode::Lock(lock) = &mut self.mode else {
            return Ok(());
        };
        if !lock.state.can_use_lock_surfaces() {
            return Ok(());
        }
        let visual = lock.state.visual(Instant::now());
        for target in &mut lock.targets {
            let report = Self::render_and_schedule(
                &mut target.render,
                target.surface.wl_surface(),
                qh,
                visual,
            )?;
            Self::record_dissolve_complete_present(target, visual, report);
        }
        Ok(())
    }

    fn add_lock_output(
        &mut self,
        connection: &Connection,
        qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) -> Result<(), Box<dyn Error>> {
        let Mode::Lock(lock) = &self.mode else {
            return Ok(());
        };
        // unlock_and_destroy invalidates the session-lock object before the ordered sync reply.
        // Ignore registry hotplug in that interval instead of issuing get_lock_surface on it.
        if !lock.state.accepts_new_outputs() {
            return Ok(());
        }
        if lock.targets.iter().any(|target| target.output == output) {
            return Ok(());
        }
        let session_lock = lock.session_lock.clone();
        let surface = self.compositor_state.create_surface(qh);
        let lock_surface = session_lock.create_lock_surface(surface, &output, qh);
        let render = RenderState::new(
            Renderer::new(connection, lock_surface.wl_surface())?,
            self.module_selection,
            true,
            self.module_started_at,
        );
        let Mode::Lock(lock) = &mut self.mode else {
            return Ok(());
        };
        lock.targets.push(LockTarget {
            render,
            output,
            surface: lock_surface,
            dissolve_complete_presented: None,
        });
        lock.redraw_all = true;
        Ok(())
    }

    fn handle_key_event(
        &mut self,
        keyboard: &wl_keyboard::WlKeyboard,
        event: KeyEvent,
        repeated: bool,
    ) {
        let can_edit = matches!(&self.mode, Mode::Lock(lock) if lock.state.can_edit());
        if !can_edit {
            return;
        }
        let Some(input) = self
            .keyboards
            .iter_mut()
            .find(|input| &input.keyboard == keyboard)
        else {
            return;
        };
        if repeated && !input.repeat_allowed {
            return;
        }
        if !repeated {
            input.repeat_allowed = true;
        }
        let Mode::Lock(lock) = &mut self.mode else {
            return;
        };
        let now = Instant::now();
        match event.keysym {
            Keysym::Return | Keysym::KP_Enter if !repeated => self.submit_password(),
            Keysym::BackSpace => {
                if lock.password.delete_last_scalar() {
                    lock.state.note_edit(now);
                    lock.redraw_all = true;
                }
            }
            Keysym::Delete if !repeated => {
                if lock.password.delete_last_scalar() {
                    lock.state.note_edit(now);
                    lock.redraw_all = true;
                }
            }
            Keysym::Escape if !repeated => {
                lock.password.clear();
                lock.state.note_cancel(now);
                lock.redraw_all = true;
            }
            _ => {
                let Some(text) = event.utf8.as_deref() else {
                    return;
                };
                if text.is_empty() || text.chars().any(char::is_control) {
                    return;
                }
                match lock.password.append(text) {
                    Ok(()) => {
                        lock.state.note_edit(now);
                        lock.redraw_all = true;
                    }
                    Err(SecretError::TooLong | SecretError::ContainsNul) => {}
                    Err(_) => {
                        lock.password.clear();
                        lock.state.enter_fatal();
                        self.fatal_disconnect = true;
                    }
                }
            }
        }
    }

    fn submit_password(&mut self) {
        let Mode::Lock(lock) = &mut self.mode else {
            return;
        };
        if !lock.state.can_edit() {
            return;
        }
        let replacement = match LockedSecret::new() {
            Ok(password) => password,
            Err(error) => {
                log::error!(target: "minecraft_plus_wayland::auth", "cannot allocate next password buffer: {error}");
                lock.password.clear();
                lock.state.enter_fatal();
                self.fatal_disconnect = true;
                return;
            }
        };
        let password = std::mem::replace(&mut lock.password, replacement);
        let Some(attempt) = lock.state.begin_authentication(Instant::now()) else {
            drop(password);
            return;
        };
        // Existing SCTK repeat timers may outlive validation/backoff. Block them until a new
        // physical press on that keyboard explicitly starts a fresh input generation.
        for input in &mut self.keyboards {
            input.repeat_allowed = false;
        }
        log::info!(target: "minecraft_plus_wayland::auth", "authentication started: attempt={attempt:?}");
        if lock
            .worker
            .try_authenticate(AuthRequest { attempt, password })
            .is_err()
        {
            log::error!(target: "minecraft_plus_wayland::auth", "authentication request channel failed: attempt={attempt:?}");
            lock.state.enter_fatal();
            self.fatal_disconnect = true;
            return;
        }
        lock.redraw_all = true;
    }

    fn handle_auth_reply(&mut self, reply: AuthReply) {
        let Mode::Lock(lock) = &mut self.mode else {
            return;
        };
        if lock
            .state
            .authentication_result(reply.attempt, reply.decision, Instant::now())
        {
            log::info!(
                target: "minecraft_plus_wayland::auth",
                "authentication completed: attempt={:?}, category={:?}",
                reply.attempt,
                reply.decision,
            );
            lock.redraw_all = true;
            if reply.decision == AuthDecision::SystemFailure {
                self.fatal_disconnect = true;
            }
        }
    }

    fn handle_worker_closed(&mut self) {
        let Mode::Lock(lock) = &mut self.mode else {
            return;
        };
        if !lock.state.is_fatal() {
            log::error!(target: "minecraft_plus_wayland::auth", "authentication worker channel disconnected");
            lock.state.enter_fatal();
            self.fatal_disconnect = true;
        }
    }

    fn replace_modules(
        &mut self,
        module_selection: ModuleSelection,
        now: Instant,
        qh: &QueueHandle<Self>,
    ) -> Result<(), Box<dyn Error>> {
        match &mut self.mode {
            Mode::Layer(target) => {
                let module = target.render.prepare_module(module_selection)?;
                target.render.replace_module(module, now);
            }
            Mode::Lock(lock) => {
                let replacements = lock
                    .targets
                    .iter()
                    .map(|target| target.render.prepare_module(module_selection))
                    .collect::<Result<Vec<_>, _>>()?;
                for (target, module) in lock.targets.iter_mut().zip(replacements) {
                    target.render.replace_module(module, now);
                }
                lock.redraw_all = true;
            }
        }
        self.module_selection = module_selection;
        self.module_started_at = now;
        log::info!(
            target: "minecraft_plus_wayland::startup",
            "interval selected module={}",
            module_selection.id(),
        );

        if let Mode::Layer(target) = &mut self.mode {
            Self::render_and_schedule(
                &mut target.render,
                target.surface.wl_surface(),
                qh,
                LockVisual::Hidden,
            )?;
        }
        Ok(())
    }

    fn switch_module_if_due(
        &mut self,
        now: Instant,
        qh: &QueueHandle<Self>,
    ) -> Result<(), Box<dyn Error>> {
        let (Some(interval), Some(deadline)) =
            (self.module_switch_interval, self.next_module_switch)
        else {
            return Ok(());
        };
        if now < deadline
            || matches!(&self.mode, Mode::Lock(lock) if !lock.state.can_use_lock_surfaces())
        {
            return Ok(());
        }
        let next_deadline = advance_periodic_deadline(deadline, interval, now)
            .ok_or("cannot schedule the next module switch")?;
        let module_selection = random_module_excluding(self.module_selection);
        self.replace_modules(module_selection, now, qh)?;
        self.next_module_switch = Some(next_deadline);
        Ok(())
    }

    fn after_dispatch(&mut self, connection: &Connection, qh: &QueueHandle<Self>) {
        let now = Instant::now();
        if let Mode::Lock(lock) = &mut self.mode {
            let before = lock.state.visual(now);
            if lock.state.tick(now) {
                lock.password.clear();
            }
            let after = lock.state.visual(now);
            if before != after || after != lock.last_visual {
                lock.redraw_all = true;
                lock.last_visual = after;
            }
        }

        if let Err(error) = self.switch_module_if_due(now, qh) {
            if matches!(self.mode, Mode::Lock(_)) {
                self.lock_render_fault("module switch", &*error);
            } else {
                log::error!(target: "minecraft_plus_wayland::startup", "module switch failed: {error}");
                self.exit_failure = Some("module switch failed");
                self.exit = true;
            }
            return;
        }

        let redraw = matches!(&self.mode, Mode::Lock(lock) if lock.redraw_all);
        if redraw {
            if let Err(error) = self.redraw_all_lock_targets(qh) {
                self.lock_render_fault("redraw", &*error);
                return;
            }
            if let Mode::Lock(lock) = &mut self.mode {
                lock.redraw_all = false;
            }
        }

        let ready = if let Mode::Lock(lock) = &self.mode {
            let visual = lock.state.visual(now);
            visual
                .completed_dissolve_attempt(now)
                .is_some_and(|attempt| {
                    all_outputs_presented(
                        lock.targets
                            .iter()
                            .map(|target| target.dissolve_complete_presented),
                        attempt,
                    )
                })
        } else {
            false
        };
        if let Mode::Lock(lock) = &mut self.mode
            && lock.state.prepare_unlock(now, ready)
        {
            self.request_session_unlock(connection, qh);
        }
        self.reschedule_deadline_timer();
    }

    fn reschedule_deadline_timer(&mut self) {
        let now = Instant::now();
        let lock_deadline = match &self.mode {
            Mode::Lock(lock) if lock.state.awaiting_unlock_sync() => {
                now.checked_add(UNLOCK_FLUSH_RETRY)
            }
            Mode::Lock(lock) => lock.state.next_deadline(now),
            Mode::Layer(_) => None,
        };
        let module_deadline = match &self.mode {
            Mode::Layer(_) => self.next_module_switch,
            Mode::Lock(lock) if lock.state.can_use_lock_surfaces() => self.next_module_switch,
            Mode::Lock(_) => None,
        };
        let deadline = earliest_deadline(lock_deadline, module_deadline);
        if deadline == self.scheduled_deadline {
            return;
        }
        if let Some(token) = self.deadline_timer.take() {
            self.loop_handle.remove(token);
        }
        self.scheduled_deadline = deadline;
        let Some(deadline) = deadline else {
            return;
        };
        match self.loop_handle.insert_source(
            Timer::from_deadline(deadline),
            |_deadline, &mut (), app| {
                app.deadline_timer = None;
                app.scheduled_deadline = None;
                TimeoutAction::Drop
            },
        ) {
            Ok(token) => self.deadline_timer = Some(token),
            Err(error) => {
                self.scheduled_deadline = None;
                match &mut self.mode {
                    Mode::Lock(lock) => {
                        log::error!(target: "minecraft_plus_wayland::lock", "cannot schedule event-loop deadline: {error}");
                        lock.password.clear();
                        lock.state.enter_fatal();
                        self.fatal_disconnect = true;
                    }
                    Mode::Layer(_) => {
                        log::error!(target: "minecraft_plus_wayland::startup", "cannot schedule module-switch deadline: {error}");
                        self.exit_failure = Some("cannot schedule module-switch deadline");
                        self.exit = true;
                    }
                }
            }
        }
    }

    /// The only function in the repository permitted to call SessionLock::unlock().
    fn request_session_unlock(&mut self, connection: &Connection, qh: &QueueHandle<Self>) {
        let Mode::Lock(lock) = &mut self.mode else {
            return;
        };
        if !lock.state.consume_unlock_gate() {
            return;
        }
        log::info!(target: "minecraft_plus_wayland::lock", "authenticated unlock gate opened; sending unlock request");
        lock.session_lock.unlock();
        // The sync request is ordered after unlock_and_destroy. Keep its proxy alive through Done;
        // WaylandSource keeps flushing and dispatching until the compositor processes the request.
        lock.unlock_sync = Some(Box::new(connection.display().sync(qh, UnlockSyncData)));
    }

    fn unlock_sync_completed(&mut self) {
        let Mode::Lock(lock) = &mut self.mode else {
            return;
        };
        if lock.state.unlock_sync_completed() {
            // Keep the callback proxy alive through Done; dropping it immediately after sync()
            // drops its dispatch user data and can leave the application waiting forever.
            lock.unlock_sync.take();
            log::info!(target: "minecraft_plus_wayland::lock", "compositor processed unlock request; exiting");
            self.exit = true;
        }
    }

    fn clear_editable_secret(&mut self) {
        if let Mode::Lock(lock) = &mut self.mode {
            lock.password.clear();
        }
    }

    fn lock_render_fault(&mut self, operation: &str, error: &dyn std::fmt::Display) {
        log::error!(target: "minecraft_plus_wayland::lock", "lock rendering fault during {operation}: {error}");
        if let Mode::Lock(lock) = &mut self.mode {
            lock.password.clear();
            lock.state.enter_fatal();
        }
        // Disconnect without a normal locked-object destructor; the compositor retains security
        // and chooses its session-lock client failure fallback. Never convert a GPU fault to unlock.
        self.fatal_disconnect = true;
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
            log::error!("failed to configure layer surface: {error}");
            self.exit = true;
        }
    }
}

impl SessionLockHandler for App {
    fn locked(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _session_lock: SessionLock) {
        if let Mode::Lock(lock) = &mut self.mode
            && lock.state.compositor_locked()
        {
            log::info!(target: "minecraft_plus_wayland::lock", "compositor confirmed session lock");
            lock.redraw_all = true;
        }
    }

    fn finished(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _session_lock: SessionLock,
    ) {
        let Mode::Lock(lock) = &mut self.mode else {
            self.exit = true;
            return;
        };
        lock.password.clear();
        if lock.state.compositor_finished() {
            self.fatal_disconnect = true;
        } else {
            self.exit_failure = Some("compositor refused or abandoned the session lock");
            self.exit = true;
        }
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
            self.lock_render_fault("configure", &*error);
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
            if matches!(self.mode, Mode::Lock(_)) {
                self.lock_render_fault("frame", &*error);
            } else {
                log::error!("rendering stopped: {error}");
                self.exit = true;
            }
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
        connection: &Connection,
        qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        if let Err(error) = self.add_lock_output(connection, qh, output) {
            self.lock_render_fault("output hotplug", &*error);
        }
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
            lock.redraw_all = true;
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
        if capability != Capability::Keyboard
            || !matches!(self.mode, Mode::Lock(_))
            || self.keyboards.iter().any(|entry| entry.seat == seat)
        {
            return;
        }
        let keyboard = self.seat_state.get_keyboard_with_repeat(
            qh,
            &seat,
            None,
            self.loop_handle.clone(),
            Box::new(|app, keyboard, event| app.handle_key_event(keyboard, event, true)),
        );
        match keyboard {
            Ok(keyboard) => self.keyboards.push(SeatKeyboard {
                seat,
                keyboard,
                repeat_allowed: true,
            }),
            Err(error) => {
                log::error!(target: "minecraft_plus_wayland::lock", "failed to acquire seat keyboard: {error}");
                if let Mode::Lock(lock) = &mut self.mode {
                    lock.password.clear();
                    lock.state.enter_fatal();
                    self.fatal_disconnect = true;
                }
            }
        }
    }

    fn remove_capability(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard
            && let Some(index) = self.keyboards.iter().position(|entry| entry.seat == seat)
        {
            self.keyboards.remove(index).keyboard.release();
        }
    }

    fn remove_seat(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, seat: wl_seat::WlSeat) {
        if let Some(index) = self.keyboards.iter().position(|entry| entry.seat == seat) {
            self.keyboards.remove(index).keyboard.release();
        }
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
        _connection: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        event: KeyEvent,
    ) {
        self.handle_key_event(_keyboard, event, false);
    }

    fn repeat_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        event: KeyEvent,
    ) {
        self.handle_key_event(_keyboard, event, true);
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

struct UnlockSyncData;

impl Dispatch<wl_callback::WlCallback, UnlockSyncData> for App {
    fn event(
        state: &mut Self,
        _proxy: &wl_callback::WlCallback,
        event: wl_callback::Event,
        _data: &UnlockSyncData,
        _connection: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            state.unlock_sync_completed();
        }
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

fn continuous_frame_required(
    visual: LockVisual,
    module_continuous: bool,
    overlay_continuous: bool,
) -> bool {
    match visual {
        // Opaque lock scenes own their frame chains. Otherwise a continuously animated hidden
        // module would keep rendering behind a converged torch, a stationary creeper or backoff.
        LockVisual::Torch { .. }
        | LockVisual::Creeper { .. }
        | LockVisual::DissolvingCreeper { .. }
        | LockVisual::FatalBlack => overlay_continuous,
        LockVisual::Hidden => module_continuous || overlay_continuous,
    }
}

fn all_outputs_presented(
    markers: impl IntoIterator<Item = Option<AttemptId>>,
    attempt: AttemptId,
) -> bool {
    let mut markers = markers.into_iter();
    let Some(first) = markers.next() else {
        return false;
    };
    first == Some(attempt) && markers.all(|presented| presented == Some(attempt))
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{
        StartupMode, advance_periodic_deadline, all_outputs_presented, continuous_frame_required,
        earliest_deadline, initial_module_deadline, module_id_excluding, parse_options,
    };
    use crate::lock::state::{AttemptId, LockVisual};

    fn options(arguments: &[&str]) -> Result<super::StartupOptions, Box<dyn std::error::Error>> {
        parse_options(arguments.iter().map(|argument| (*argument).to_owned()))
    }

    #[test]
    fn module_long_and_short_options_select_a_fixed_module() {
        for option in ["--module", "-m"] {
            let parsed = options(&[option, "4"]).unwrap();
            assert_eq!(parsed.module.id(), 4);
            assert!(parsed.module_switch_interval.is_none());
            assert!(matches!(parsed.mode, StartupMode::LayerShell));
        }
    }

    #[test]
    fn interval_long_and_short_options_schedule_whole_seconds() {
        for option in ["--interval", "-t"] {
            let parsed = options(&["--lock", option, "7"]).unwrap();
            assert_eq!(parsed.module_switch_interval, Some(Duration::from_secs(7)));
            assert!(matches!(parsed.mode, StartupMode::SessionLock));
        }
    }

    #[test]
    fn zero_or_omitted_interval_disables_switching() {
        assert!(options(&[]).unwrap().module_switch_interval.is_none());
        assert!(
            options(&["--interval", "0"])
                .unwrap()
                .module_switch_interval
                .is_none()
        );
    }

    #[test]
    fn module_and_interval_conflict_in_either_order_including_zero() {
        for arguments in [
            &["--module", "3", "--interval", "5"][..],
            &["-t", "5", "-m", "3"][..],
            &["-m", "3", "-t", "0"][..],
        ] {
            assert_eq!(
                options(arguments).unwrap_err().to_string(),
                "--module/-m conflicts with --interval/-t"
            );
        }
    }

    #[test]
    fn interval_rejects_missing_negative_and_fractional_values() {
        for arguments in [
            &["--interval"][..],
            &["--interval", "-1"][..],
            &["-t", "0.5"][..],
        ] {
            assert!(options(arguments).is_err());
        }
    }

    #[test]
    fn initial_interval_is_validated_before_runtime_setup() {
        let start = Instant::now();
        assert_eq!(initial_module_deadline(start, None), Ok(None));
        assert_eq!(
            initial_module_deadline(start, Some(Duration::from_secs(5))),
            Ok(Some(start + Duration::from_secs(5)))
        );
        assert_eq!(
            initial_module_deadline(start, Some(Duration::MAX)),
            Err("--interval is too large to schedule")
        );
    }

    #[test]
    fn replacement_slot_mapping_is_uniform_and_never_repeats_current_module() {
        for current in 0..13 {
            let mut ids = (0..12)
                .map(|slot| module_id_excluding(current, slot))
                .collect::<Vec<_>>();
            assert!(ids.iter().all(|id| *id != current && *id < 13));
            ids.sort_unstable();
            ids.dedup();
            assert_eq!(ids.len(), 12);
        }
    }

    #[test]
    fn periodic_deadline_skips_missed_ticks_without_drift_or_catch_up() {
        let start = Instant::now();
        let interval = Duration::from_secs(5);
        let deadline = start + interval;
        assert_eq!(
            advance_periodic_deadline(deadline, interval, start + Duration::from_secs(16)),
            Some(start + Duration::from_secs(20))
        );
        assert_eq!(
            advance_periodic_deadline(deadline, interval, deadline),
            Some(start + Duration::from_secs(10))
        );
    }

    #[test]
    fn event_loop_uses_the_earliest_lock_or_module_deadline() {
        let now = Instant::now();
        let lock = now + Duration::from_secs(2);
        let module = now + Duration::from_secs(5);
        assert_eq!(earliest_deadline(Some(lock), Some(module)), Some(lock));
        assert_eq!(earliest_deadline(None, Some(module)), Some(module));
        assert_eq!(earliest_deadline(None, None), None);
    }

    #[test]
    fn converged_torch_ignores_a_hidden_continuous_module() {
        let torch = LockVisual::Torch {
            mask: 0b0111,
            state_id: 4,
        };
        assert!(continuous_frame_required(torch, true, true));
        assert!(!continuous_frame_required(torch, true, false));
        assert!(continuous_frame_required(LockVisual::Hidden, true, false));
        assert!(!continuous_frame_required(
            LockVisual::Creeper {
                approach_started_at: std::time::Instant::now(),
                red: false,
            },
            true,
            false,
        ));
    }

    #[test]
    fn output_add_remove_bookkeeping_cannot_reuse_an_old_dissolve_frame() {
        let attempt = AttemptId::new(3);
        let old_attempt = AttemptId::new(2);
        let mut outputs = vec![Some(attempt), Some(attempt)];
        assert!(all_outputs_presented(outputs.iter().copied(), attempt));
        assert!(!all_outputs_presented([], attempt));

        outputs.push(None);
        assert!(!all_outputs_presented(outputs.iter().copied(), attempt));
        outputs[2] = Some(old_attempt);
        assert!(!all_outputs_presented(outputs.iter().copied(), attempt));
        outputs[2] = Some(attempt);
        assert!(all_outputs_presented(outputs.iter().copied(), attempt));

        outputs.remove(1);
        assert!(all_outputs_presented(outputs, attempt));
    }
}
