//! `std::env`. On the web the page gives the arguments and variables (the
//! URL's query, `web/horizon.js`), the working directory is the game folder
//! in the in-memory file system, and the "executable" sits in `/app`, beside
//! the prepared resources in `/app/resources`, as in a release folder.
#[cfg(not(target_family = "wasm"))]
pub use std::env::{
    Args, ArgsOs, VarError, Vars, args, args_os, consts, current_dir, current_exe, set_current_dir,
    temp_dir, var, var_os, vars,
};
#[cfg(target_family = "wasm")]
pub use web::*;

#[cfg(target_family = "wasm")]
mod web {
    pub use std::env::{VarError, consts};
    use std::{
        collections::BTreeMap,
        ffi::{OsStr, OsString},
        io,
        path::{Path, PathBuf},
        sync::Mutex,
    };

    pub(crate) struct Env {
        pub args: Vec<String>,
        pub vars: BTreeMap<String, String>,
    }
    pub(crate) static ENV: Mutex<Env> = Mutex::new(Env {
        args: Vec::new(),
        vars: BTreeMap::new(),
    });
    /// The executable `current_exe` names: its folder holds `resources/`.
    pub const EXE: &str = "/app/horizon-client";
    /// The game folder: options.txt, saves/, logs/.
    pub const HOME: &str = "/home/horizon";

    pub type Args = std::vec::IntoIter<String>;
    pub type ArgsOs = std::vec::IntoIter<OsString>;
    pub type Vars = std::vec::IntoIter<(String, String)>;

    /// The program name, then the page's arguments.
    pub fn args() -> Args {
        let env = ENV.lock().unwrap();
        std::iter::once(EXE.to_owned())
            .chain(env.args.iter().cloned())
            .collect::<Vec<_>>()
            .into_iter()
    }
    pub fn args_os() -> ArgsOs {
        args().map(OsString::from).collect::<Vec<_>>().into_iter()
    }
    pub fn var<K: AsRef<OsStr>>(key: K) -> Result<String, VarError> {
        let key = key.as_ref().to_str().ok_or(VarError::NotPresent)?;
        ENV.lock()
            .unwrap()
            .vars
            .get(key)
            .cloned()
            .ok_or(VarError::NotPresent)
    }
    pub fn var_os<K: AsRef<OsStr>>(key: K) -> Option<OsString> {
        var(key).ok().map(OsString::from)
    }
    pub fn vars() -> Vars {
        let env = ENV.lock().unwrap();
        env.vars
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect::<Vec<_>>()
            .into_iter()
    }
    /// The page's variables change only before the game starts and from
    /// the game itself; nothing else reads them concurrently.
    pub fn set_var<K: AsRef<OsStr>, V: AsRef<OsStr>>(key: K, value: V) {
        if let (Some(k), Some(v)) = (key.as_ref().to_str(), value.as_ref().to_str()) {
            ENV.lock().unwrap().vars.insert(k.to_owned(), v.to_owned());
        }
    }
    pub fn remove_var<K: AsRef<OsStr>>(key: K) {
        if let Some(k) = key.as_ref().to_str() {
            ENV.lock().unwrap().vars.remove(k);
        }
    }
    pub fn current_dir() -> io::Result<PathBuf> {
        Ok(crate::web::vfs::cwd())
    }
    pub fn set_current_dir<P: AsRef<Path>>(path: P) -> io::Result<()> {
        crate::web::vfs::set_cwd(path.as_ref())
    }
    pub fn current_exe() -> io::Result<PathBuf> {
        Ok(PathBuf::from(EXE))
    }
    pub fn temp_dir() -> PathBuf {
        PathBuf::from("/tmp")
    }
}
