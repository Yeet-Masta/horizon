//! The web build's start (`web/README.md`): the page hands over the prepared
//! resources and the kept game folder, the WebGPU device opens (which waits
//! on the browser, as nothing else in the game may), then the game starts as
//! it does natively, on the page's event loop.
use horizon_client::Result;
use horizon_platform::time::{Duration, Instant};

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

/// The page's frames, for Max Framerate (`skip_frame`): when the last came,
/// how far apart they come (the display's refresh), and when the next drawn
/// frame is due.
struct Pace {
    last: Option<Instant>,
    display: f64,
    due: Option<Instant>,
}
thread_local! {
    static PACE: std::cell::RefCell<Pace> = const {
        std::cell::RefCell::new(Pace { last: None, display: 1.0 / 60.0, due: None })
    };
}

/// Whether to skip this frame of the page's under Max Framerate `limit`
/// (260: unlimited). The page's frames come at the display's rate and the
/// web build can't sleep between them, so a frame draws when its slot has
/// come, to within half a display frame; slots follow each other by the
/// limit's interval, so 60 on a 144 Hz display draws 60 a second, not 48. A
/// browser draws no faster than its display: above that the limit does
/// nothing.
pub fn skip_frame(limit: i32) -> bool {
    let now = Instant::now();
    PACE.with_borrow_mut(|pace| {
        if let Some(last) = pace.last.replace(now) {
            let gap = now.duration_since(last).as_secs_f64();
            // A frame that took long says nothing of the display.
            if gap < 0.05 {
                pace.display = pace.display * 0.9 + gap * 0.1;
            }
        }
        if limit >= 260 {
            pace.due = None;
            return false;
        }
        let slot = Duration::from_secs_f64(1.0 / f64::from(limit.max(1)));
        let early = Duration::from_secs_f64(pace.display / 2.0);
        let due = *pace.due.get_or_insert(now);
        if now + early < due {
            return true;
        }
        // The next slot follows this one; after a slow frame, it follows now.
        let next = due + slot;
        pace.due = Some(if next + early < now { now + slot } else { next });
        false
    })
}
