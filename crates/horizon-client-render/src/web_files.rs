//! Files dropped on the web build's page while the game runs (`web/horizon.js`):
//! a browser's game folder is out of the player's reach, so a world's zip goes
//! into `saves` and a resource pack into `resourcepacks`, where the native game's
//! folders would take them.
use horizon_platform::fs;
use std::{
    io::{Cursor, Read},
    path::{Path, PathBuf},
};
use wasm_bindgen::prelude::*;

/// Imports `bytes`, the dropped file `name`: what became of it, for the page
/// to show, or why nothing did (thrown).
#[wasm_bindgen]
pub fn horizon_import(name: String, bytes: Vec<u8>) -> Result<String, JsValue> {
    import(&name, bytes).map_err(|e| JsValue::from_str(&e))
}

fn import(name: &str, bytes: Vec<u8>) -> Result<String, String> {
    let not_zip = || format!("{name}: drop the .zip of a world or of a resource pack.");
    if !name.to_ascii_lowercase().ends_with(".zip") {
        return Err(not_zip());
    }
    let mut zip = ::zip::ZipArchive::new(Cursor::new(&bytes[..])).map_err(|_| not_zip())?;
    let names: Vec<String> = zip.file_names().map(|n| n.replace('\\', "/")).collect();
    let stem = Path::new(name).file_stem().unwrap_or_default().to_string_lossy();
    let folder = |root: &str| {
        let last = root.trim_end_matches('/').rsplit('/').next().unwrap_or("");
        sanitize(if last.is_empty() { &stem } else { last })
    };
    let game = horizon_platform::env::current_dir().unwrap_or_default();
    if let Some(root) = shortest_root(&names, "level.dat") {
        let dest = available(&game.join("saves"), &folder(&root));
        extract(&mut zip, &root, &dest)?;
        return Ok(format!(
            "Added the world “{}”: Singleplayer lists it.",
            file_name(&dest)
        ));
    }
    if let Some(root) = shortest_root(&names, "pack.mcmeta") {
        let has = |dir: &str| names.iter().any(|n| n.starts_with(&format!("{root}{dir}/")));
        if has("data") && !has("assets") {
            return Err(format!(
                "{name} is a data pack: add it to a world from Create World's Data Packs."
            ));
        }
        let packs = game.join("resourcepacks");
        let dest = if root.is_empty() {
            // A pack at the zip's root is read as it is.
            fs::create_dir_all(&packs).map_err(|e| e.to_string())?;
            let dest = available(&packs, &sanitize(name));
            fs::write(&dest, &bytes).map_err(|e| e.to_string())?;
            dest
        } else {
            let dest = available(&packs, &folder(&root));
            extract(&mut zip, &root, &dest)?;
            dest
        };
        return Ok(format!(
            "Added the resource pack “{}”: turn it on in Options, Resource Packs.",
            file_name(&dest)
        ));
    }
    Err(format!(
        "{name} holds no world (level.dat) and no resource pack (pack.mcmeta)."
    ))
}

/// The folder (`""` or `"a/b/"`) nearest the zip's root that holds `file`.
fn shortest_root(names: &[String], file: &str) -> Option<String> {
    names
        .iter()
        .filter_map(|n| n.strip_suffix(file))
        .filter(|root| root.is_empty() || root.ends_with('/'))
        .min_by_key(|root| (root.matches('/').count(), root.len()))
        .map(str::to_owned)
}

/// The entries under `root` written into `dest`.
fn extract(
    zip: &mut ::zip::ZipArchive<Cursor<&[u8]>>,
    root: &str,
    dest: &Path,
) -> Result<(), String> {
    let root = Path::new(root.trim_end_matches('/'));
    fs::create_dir_all(dest).map_err(|e| e.to_string())?;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).map_err(|e| e.to_string())?;
        // Never a path out of the folder (`..`, absolute).
        let Some(path) = entry.enclosed_name() else {
            continue;
        };
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        let target = dest.join(relative);
        if entry.is_dir() {
            fs::create_dir_all(&target).map_err(|e| e.to_string())?;
            continue;
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let mut data = Vec::with_capacity(entry.size() as usize);
        entry.read_to_end(&mut data).map_err(|e| e.to_string())?;
        fs::write(&target, data).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// `FileUtil.sanitizeName`: the characters no file system takes become `_`.
fn sanitize(name: &str) -> String {
    let name: String = name
        .chars()
        .map(|c| match c {
            '/' | '\n' | '\r' | '\t' | '\0' | '\x0c' | '`' | '?' | '*' | '\\' | '<' | '>'
            | '|' | '"' | ':' => '_',
            c => c,
        })
        .collect();
    if name.trim().is_empty() || name.starts_with('.') {
        format!("_{name}")
    } else {
        name
    }
}

/// `name` in `dir`, or `name (1)`, `name (2)`... when it is taken (before the
/// extension of a file name).
fn available(dir: &Path, name: &str) -> PathBuf {
    let (base, extension) = match name.rsplit_once('.') {
        Some((base, extension)) if !base.is_empty() && name.ends_with(".zip") => {
            (base, format!(".{extension}"))
        }
        _ => (name, String::new()),
    };
    (0..)
        .map(|n| match n {
            0 => dir.join(name),
            n => dir.join(format!("{base} ({n}){extension}")),
        })
        .find(|path| !fs::exists(path).unwrap_or(false))
        .expect("a free name")
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}
