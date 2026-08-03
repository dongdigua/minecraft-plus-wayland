mod app;
mod renderer;
mod resources;
pub mod scene;

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    app::run()
}
