mod app;
mod daemon;
mod imaging;
mod pipeline;

fn main() {
    let _ = app::App::open;
    let _ = daemon::is_running;
}
