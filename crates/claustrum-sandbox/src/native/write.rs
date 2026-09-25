//! `Write`: create or overwrite a file.

use std::path::Path;

use crate::{Error, Result, fs::GuestFs};

use super::{fs_err, locate};

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct WriteOutput {
    pub path: String,
    pub created: bool,
    pub bytes: usize,
}

pub async fn write(fs: &GuestFs, path: &str, cwd: &str, content: &str) -> Result<WriteOutput> {
    let loc = locate(fs, path, cwd)?;
    super::ensure_writable(&loc)?;
    let existed = match loc.fs.metadata(&loc.inner) {
        Ok(meta) if meta.is_dir() => {
            return Err(Error::invalid_path(&loc.guest, "is a directory"));
        }
        Ok(_) => true,
        Err(_) => false,
    };
    if let Some(parent) = loc.inner.parent()
        && parent != Path::new("/")
        && parent != Path::new("")
    {
        virtual_fs::create_dir_all(&*loc.fs, parent).map_err(fs_err(&loc.guest))?;
    }
    super::write_file(&*loc.fs, &loc.inner, content.as_bytes())
        .await
        .map_err(fs_err(&loc.guest))?;
    Ok(WriteOutput {
        path: loc.guest,
        created: !existed,
        bytes: content.len(),
    })
}
