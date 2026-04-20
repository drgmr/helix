use std::{
    fs,
    path::{Path, PathBuf},
};

use serde::Serialize;

#[derive(Serialize)]
struct LockFile<'a> {
    pid: u32,
    #[serde(rename = "workspaceFolders")]
    workspace_folders: Vec<String>,
    #[serde(rename = "ideName")]
    ide_name: &'a str,
    transport: &'a str,
    #[serde(rename = "runningInWindows")]
    running_in_windows: bool,
    #[serde(rename = "authToken")]
    auth_token: &'a str,
}

pub struct LockFileGuard {
    path: PathBuf,
}

impl LockFileGuard {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for LockFileGuard {
    fn drop(&mut self) {
        if let Err(e) = fs::remove_file(&self.path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                log::warn!("failed to remove lockfile {}: {e}", self.path.display());
            }
        }
    }
}

fn lock_dir() -> anyhow::Result<PathBuf> {
    let home = etcetera::home_dir().map_err(|e| anyhow::anyhow!("home dir: {e}"))?;
    let dir = home.join(".claude").join("ide");
    fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
    }
    Ok(dir)
}

pub fn write_lockfile(
    port: u16,
    auth_token: &str,
    workspace_folders: &[PathBuf],
) -> anyhow::Result<LockFileGuard> {
    let dir = lock_dir()?;
    let path = dir.join(format!("{port}.lock"));

    let body = LockFile {
        pid: std::process::id(),
        workspace_folders: workspace_folders
            .iter()
            .map(|p| p.display().to_string())
            .collect(),
        ide_name: "Helix",
        transport: "ws",
        running_in_windows: cfg!(windows),
        auth_token,
    };

    let json = serde_json::to_vec(&body)?;
    fs::write(&path, &json)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
    }

    Ok(LockFileGuard { path })
}
