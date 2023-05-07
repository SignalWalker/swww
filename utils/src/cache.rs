//! Implements basic cache functionality.
//!
//! The idea is:
//!   1. the client regiters the last image sent for each output in a file
//!   2. the daemon spawns a client that reloads that image when an output is created

use std::{
    io::{BufReader, BufWriter, Read, Write},
    path::{Path, PathBuf},
};

pub fn store(output_name: &str, img_path: &str) -> Result<(), String> {
    let mut filepath = cache_dir()?;
    filepath.push(output_name);
    let file = std::fs::File::create(filepath).map_err(|e| e.to_string())?;

    let mut writer = BufWriter::new(file);
    writer
        .write_all(img_path.as_bytes())
        .map_err(|e| format!("failed to write cache: {e}"))
}

pub fn load(output_name: &str) -> Result<(), String> {
    let mut filepath = cache_dir()?;
    filepath.push(output_name);
    if !filepath.is_file() {
        return Ok(());
    }
    let file = std::fs::File::open(filepath).map_err(|e| format!("failed to open file: {e}"))?;
    let mut reader = BufReader::new(file);
    let mut buf = Vec::with_capacity(64);
    reader
        .read_to_end(&mut buf)
        .map_err(|e| format!("failed to read file: {e}"))?;

    let img_path = std::str::from_utf8(&buf).map_err(|e| format!("failed to decode bytes: {e}"))?;
    if buf.is_empty() {
        return Ok(());
    }

    if let Ok(mut child) = std::process::Command::new("pidof").arg("swww").spawn() {
        if let Ok(status) = child.wait() {
            if status.success() {
                return Err("there is already another swww process running".to_string());
            }
        }
    }

    match std::process::Command::new("swww")
        .arg("img")
        .args([
            &format!("--outputs={output_name}"),
            "--transition-type=simple",
            "--transition-step=255",
            img_path,
        ])
        .spawn()
    {
        Ok(_) => Ok(()),
        Err(e) => Err(format!("failed to spawn child process: {e}")),
    }
}

fn create_dir(p: &Path) -> Result<(), String> {
    if !p.is_dir() {
        if let Err(e) = std::fs::create_dir(p) {
            return Err(format!("failed to create directory({p:#?}): {e}"));
        }
    }
    Ok(())
}

fn cache_dir() -> Result<PathBuf, String> {
    if let Ok(path) = std::env::var("XDG_CACHE_HOME") {
        let path: PathBuf = path.into();
        create_dir(&path)?;
        Ok(path)
    } else if let Ok(path) = std::env::var("HOME") {
        let mut path: PathBuf = path.into();
        path.push(".cache");
        path.push("swww");
        create_dir(&path)?;
        Ok(path)
    } else {
        Err("failed to read both $XDG_CACHE_HOME and $HOME environment variables".to_string())
    }
}
