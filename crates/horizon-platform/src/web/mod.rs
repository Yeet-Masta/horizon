//! What the web build asks of its page (`web/horizon.js`): the arguments,
//! the prepared resources and the kept game folder before the game starts,
//! a place to keep what the game writes, the console and randomness.
pub mod rtc;
pub mod socket;
pub mod vfs;

use crate::time::{Duration, SystemTime, UNIX_EPOCH};
use js_sys::{Array, Object, Reflect, Uint8Array};
use std::path::Path;
use wasm_bindgen::{JsCast, JsValue, prelude::*};

#[wasm_bindgen]
extern "C" {
    /// The page's start: resolves to `{ args, env, resources, files }`.
    #[wasm_bindgen(js_namespace = globalThis, js_name = horizonBoot)]
    fn horizon_boot() -> js_sys::Promise;
    /// Keeps `[path, kind, bytes, mtime]` records ("file", "dir" or "removed").
    #[wasm_bindgen(js_namespace = globalThis, js_name = horizonPersist)]
    fn horizon_persist(records: Array);
    /// Shows a message over the page (an error the game could not show itself).
    #[wasm_bindgen(js_namespace = globalThis, js_name = horizonStatus)]
    fn horizon_status(message: &str, error: bool);
    #[wasm_bindgen(js_name = setInterval)]
    fn set_interval(callback: &Closure<dyn FnMut()>, ms: i32) -> i32;
    #[wasm_bindgen(js_namespace = console, js_name = error)]
    fn console_error(message: &str);
    #[wasm_bindgen(js_namespace = console, js_name = warn)]
    fn console_warn(message: &str);
    #[wasm_bindgen(js_namespace = console, js_name = info)]
    fn console_info(message: &str);
    #[wasm_bindgen(js_namespace = console, js_name = debug)]
    fn console_debug(message: &str);
    #[wasm_bindgen(js_namespace = crypto, js_name = getRandomValues)]
    fn get_random_values(buf: &mut [u8]);
}

/// The browser console: 1 error, 2 warning, 3 info, else debug.
pub fn console(level: u8, message: &str) {
    match level {
        1 => console_error(message),
        2 => console_warn(message),
        3 => console_info(message),
        _ => console_debug(message),
    }
}
/// A message over the page, for what fails before the game can draw.
pub fn status(message: &str, error: bool) {
    horizon_status(message, error);
}

fn get(object: &JsValue, key: &str) -> JsValue {
    Reflect::get(object, &JsValue::from_str(key)).unwrap_or(JsValue::UNDEFINED)
}
fn millis(time: SystemTime) -> f64 {
    time.duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64() * 1000.0)
}
fn time(ms: &JsValue) -> SystemTime {
    match ms.as_f64() {
        Some(ms) if ms > 0.0 => UNIX_EPOCH + Duration::from_secs_f64(ms / 1000.0),
        _ => SystemTime::now(),
    }
}

/// Where the prepared resources are mounted: `resources/` beside the
/// executable, as in a release folder.
pub const RESOURCES: &str = "/app/resources";
/// What marks the prepared resources' folder in the player's zip.
const RESOURCES_MARKER: &str = "horizon-resources.json";

/// Takes what the page gives the game: the arguments and variables, the
/// prepared resources (a zip, mounted at [`RESOURCES`], or loose files) and
/// the game folder kept from earlier visits. Then keeps the game folder:
/// every two seconds, and when the page asks (`globalThis.horizonFlush`,
/// called as the page is hidden), what changed in it goes to the page.
pub async fn boot() -> Result<(), String> {
    let boot = wasm_bindgen_futures::JsFuture::from(horizon_boot())
        .await
        .map_err(|e| format!("the page did not start the game: {e:?}"))?;
    {
        let mut env = crate::env::ENV.lock().unwrap();
        if let Ok(args) = get(&boot, "args").dyn_into::<Array>() {
            env.args = args.iter().filter_map(|a| a.as_string()).collect();
        }
        if let Some(vars) = get(&boot, "env").dyn_ref::<Object>() {
            for entry in Object::entries(vars).iter() {
                let entry: Array = entry.unchecked_into();
                if let (Some(k), Some(v)) = (entry.get(0).as_string(), entry.get(1).as_string()) {
                    env.vars.insert(k, v);
                }
            }
        }
    }
    if let Ok(zip) = get(&boot, "resources").dyn_into::<Uint8Array>() {
        vfs::mount_zip(Path::new(RESOURCES), zip.to_vec(), Some(RESOURCES_MARKER))
            .map_err(|e| format!("the resources zip could not be read: {e}"))?;
    }
    // Loose files, `[path, bytes]`: a folder the player picked.
    if let Ok(files) = get(&boot, "resourceFiles").dyn_into::<Array>() {
        for file in files.iter() {
            let file: Array = file.unchecked_into();
            if let (Some(path), Ok(bytes)) = (
                file.get(0).as_string(),
                file.get(1).dyn_into::<Uint8Array>(),
            ) {
                vfs::restore(
                    &format!("{RESOURCES}/{}", path.trim_start_matches('/')),
                    vfs::Kept::File(bytes.to_vec(), SystemTime::now()),
                );
            }
        }
    }
    // The kept game folder, `[path, kind, bytes, mtime]`.
    if let Ok(files) = get(&boot, "files").dyn_into::<Array>() {
        for record in files.iter() {
            let record: Array = record.unchecked_into();
            let Some(path) = record.get(0).as_string() else {
                continue;
            };
            let kept = match record.get(1).as_string().as_deref() {
                Some("dir") => vfs::Kept::Dir(time(&record.get(3))),
                Some("file") => match record.get(2).dyn_into::<Uint8Array>() {
                    Ok(bytes) => vfs::Kept::File(bytes.to_vec(), time(&record.get(3))),
                    Err(_) => continue,
                },
                _ => continue,
            };
            vfs::restore(&path, kept);
        }
    }
    vfs::persist(Path::new(crate::env::HOME));
    let flush = Closure::<dyn FnMut()>::new(flush);
    set_interval(&flush, 2000);
    let _ = Reflect::set(
        &js_sys::global(),
        &JsValue::from_str("horizonFlush"),
        flush.as_ref(),
    );
    flush.forget();
    Ok(())
}

/// Hands the page what changed in the game folder.
pub fn flush() {
    let changes = vfs::drain();
    if changes.is_empty() {
        return;
    }
    let records = Array::new();
    for (path, kept) in changes {
        let record = Array::new();
        record.push(&JsValue::from_str(&path));
        match kept {
            vfs::Kept::Dir(time) => {
                record.push(&JsValue::from_str("dir"));
                record.push(&JsValue::NULL);
                record.push(&JsValue::from_f64(millis(time)));
            }
            vfs::Kept::File(bytes, time) => {
                record.push(&JsValue::from_str("file"));
                record.push(&Uint8Array::from(bytes.as_slice()));
                record.push(&JsValue::from_f64(millis(time)));
            }
            vfs::Kept::Removed => {
                record.push(&JsValue::from_str("removed"));
            }
        }
        records.push(&record);
    }
    horizon_persist(records);
}

fn fill(buf: &mut [u8]) {
    // getRandomValues fills at most 64 KiB per call.
    for chunk in buf.chunks_mut(65536) {
        get_random_values(chunk);
    }
}

/// getrandom 0.3's source (`--cfg getrandom_backend="custom"`, .cargo/config.toml).
///
/// # Safety
/// `dest` is valid for writes of `len` bytes, as getrandom promises.
#[unsafe(no_mangle)]
unsafe extern "Rust" fn __getrandom_v03_custom(
    dest: *mut u8,
    len: usize,
) -> Result<(), getrandom::Error> {
    // SAFETY: getrandom passes a buffer of `len` writable bytes.
    fill(unsafe { std::slice::from_raw_parts_mut(dest, len) });
    Ok(())
}

fn getrandom_02(buf: &mut [u8]) -> Result<(), getrandom02::Error> {
    fill(buf);
    Ok(())
}
getrandom02::register_custom_getrandom!(getrandom_02);
