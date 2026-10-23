//! [`runtime`](crate)'s API without wasmtime, for builds that can't host it
//! (the web build: a browser runs no wasmtime). Content-only mods still turn
//! on, since the game reads their content itself; a mod with code is listed
//! and stays off, with the reason.
use crate::manifest::{Manifest, Package};
use horizon_platform::time::Duration;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

/// The `game-read` capability's records (`horizon.wit`).
#[derive(Clone, Debug, PartialEq)]
pub struct Player {
    pub x: f64,
    pub y: f64,
    pub z: f64,
    pub yaw: f32,
    pub pitch: f32,
    pub health: f32,
    pub food: u32,
    pub dimension: String,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Clock {
    pub game_time: u64,
    pub day_time: u64,
}
/// The `hud` capability's records.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Corner {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    pub text: String,
    pub color: u32,
}

#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub player: Option<Player>,
    pub clock: Option<Clock>,
}
#[derive(Clone, Debug, PartialEq)]
pub enum Output {
    Chat { name: String, text: String },
    Feedback { text: String, error: bool },
    Stopped { name: String, reason: String },
}
#[derive(Clone, Debug)]
pub struct Panel {
    pub corner: Corner,
    pub lines: Vec<Line>,
}
#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    Off,
    Running,
    Failed(String),
}
pub struct Entry {
    pub path: PathBuf,
    pub manifest: Option<Manifest>,
    pub status: Status,
    pub cost: Duration,
}

/// The installed mods; none of them runs code.
#[derive(Default)]
pub struct Mods {
    entries: Vec<Entry>,
    snapshot: Arc<Snapshot>,
}

/// The `.hmod` files and mod folders in `dir`, by name.
pub fn find(dir: &Path) -> Vec<PathBuf> {
    let Ok(read) = horizon_platform::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<PathBuf> = read
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.extension().is_some_and(|e| e == "hmod")
                || horizon_platform::fs::PathExt::fs_is_file(
                    p.join(crate::manifest::MANIFEST).as_path(),
                )
        })
        .collect();
    found.sort();
    found
}

impl Mods {
    pub fn load(
        paths: &[PathBuf],
        enabled: impl Fn(&str) -> bool,
        _reserved: impl Fn(&str) -> bool,
    ) -> Mods {
        let mut mods = Mods::default();
        for path in paths {
            let mut entry = Entry {
                path: path.clone(),
                manifest: None,
                status: Status::Off,
                cost: Duration::ZERO,
            };
            match Package::read(path) {
                Err(e) => entry.status = Status::Failed(e),
                Ok(package) => {
                    if enabled(&package.manifest.id) {
                        entry.status =
                            match (&package.wasm, crate::Content::read(path, &package.manifest)) {
                                (_, Err(e)) => Status::Failed(e),
                                (None, Ok(_)) => Status::Running,
                                (Some(_), Ok(_)) => {
                                    Status::Failed("mods with code need the native game".into())
                                }
                            };
                    }
                    entry.manifest = Some(package.manifest);
                }
            }
            mods.entries.push(entry);
        }
        mods
    }
    pub fn is_running(&self) -> bool {
        false
    }
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }
    pub fn set_snapshot(&mut self, snapshot: Snapshot) {
        self.snapshot = Arc::new(snapshot);
    }
    pub fn tick(&mut self, _tick: u64) {}
    pub fn frame(&mut self, _seconds: f32) {}
    pub fn chat(&mut self, _text: &str) {}
    pub fn has_command(&self, _name: &str) -> bool {
        false
    }
    pub fn command(&mut self, _line: &str) -> bool {
        false
    }
    pub fn commands(&self) -> impl Iterator<Item = &str> {
        std::iter::empty()
    }
    pub fn panels(&self) -> impl Iterator<Item = &Panel> {
        std::iter::empty()
    }
    pub fn drain(&mut self) -> Vec<Output> {
        Vec::new()
    }
    pub fn reload_changed(&mut self, _reserved: impl Fn(&str) -> bool) -> bool {
        false
    }
}
