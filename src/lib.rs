mod app;
pub mod modules;
mod renderer;
mod resources;

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    app::run()
}
