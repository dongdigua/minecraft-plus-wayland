mod app;
pub mod modules;
mod renderer;
mod resources;

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn"))
        .try_init();
    app::run()
}
