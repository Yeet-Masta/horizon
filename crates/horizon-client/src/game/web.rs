//! The web build's start (`web/README.md`): the page hands over the prepared
//! resources and the kept game folder, the WebGPU device opens (which waits
//! on the browser, as nothing else in the game may), then the game starts as
//! it does natively, on the page's event loop.
use horizon_client::Result;

/// `main` on the web: starts the game once the page is ready, and returns.
pub fn start() {
    wasm_bindgen_futures::spawn_local(async {
        if let Err(error) = boot().await {
            stopped(Some(&*error));
        }
    });
}

async fn boot() -> Result<()> {
    horizon_platform::web::boot().await?;
    // Logs, crash reports and saves all live in the game folder.
    if horizon_platform::env::var_os("HORIZON_HOME").is_none() {
        horizon_platform::env::set_var("HORIZON_HOME", horizon_platform::env::HOME);
    }
    horizon_client::telemetry::mark_start();
    let home = horizon_client::logging::game_home();
    horizon_client::logging::init(&home, "Render thread");
    horizon_client::crash::install(home, "client");
    // A panic stops the page's game for good: say so over the page.
    let report = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        report(info);
        horizon_platform::web::status(&format!("Horizon crashed: {info}"), true);
    }));
    horizon_client::gfx::open_web_device().await?;
    super::run()
}

/// The game stopped: its error, or that it quit, goes over the page.
pub fn stopped(error: Option<&dyn std::error::Error>) {
    match error {
        Some(error) => {
            log::error!("Horizon: {error}");
            horizon_platform::web::status(&format!("Horizon stopped: {error}"), true);
        }
        None => {
            horizon_platform::web::status("Horizon quit. Reload the page to play again.", false)
        }
    }
}
