//! `std::fs`. On the web, the in-memory file system of `web::vfs`.
//!
//! `Path::exists`, `is_dir`, `is_file` and `metadata` ask std's file system
//! directly, so the workspace asks [`PathExt`]'s here instead.
use std::{io, path::Path};

#[cfg(target_family = "wasm")]
pub use crate::web::vfs::fs::*;
#[cfg(not(target_family = "wasm"))]
pub use std::fs::*;

/// `Path`'s file-system queries, on this module's file system.
pub trait PathExt {
    /// `Path::exists`.
    fn fs_exists(&self) -> bool;
    /// `Path::try_exists`.
    fn fs_try_exists(&self) -> io::Result<bool>;
    /// `Path::is_dir`.
    fn fs_is_dir(&self) -> bool;
    /// `Path::is_file`.
    fn fs_is_file(&self) -> bool;
    /// `Path::metadata`.
    fn fs_metadata(&self) -> io::Result<Metadata>;
}
impl PathExt for Path {
    fn fs_exists(&self) -> bool {
        metadata(self).is_ok()
    }
    fn fs_try_exists(&self) -> io::Result<bool> {
        exists(self)
    }
    fn fs_is_dir(&self) -> bool {
        metadata(self).is_ok_and(|m| m.is_dir())
    }
    fn fs_is_file(&self) -> bool {
        metadata(self).is_ok_and(|m| m.is_file())
    }
    fn fs_metadata(&self) -> io::Result<Metadata> {
        metadata(self)
    }
}
