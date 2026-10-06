//! The dashboard embeds the Vite bundle in ui/dist (see dashboard/ui.rs).
//! This script builds the bundle when it is missing and fails with
//! instructions when neither the bundle nor pnpm is available. When the
//! bundle exists it only warns if ui/src looks newer, so Rust-only
//! builds stay fast and never need Node.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

fn main() {
    let ui = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../ui");
    let dist_index = ui.join("dist/index.html");

    for dir in ["src", "index.html", "package.json", "dist"] {
        println!("cargo:rerun-if-changed={}", ui.join(dir).display());
    }

    if dist_index.exists() {
        if let (Ok(src), Ok(dist)) = (newest_mtime(&ui.join("src")), modified(&dist_index)) {
            if src > dist {
                println!(
                    "cargo:warning=ui/src is newer than ui/dist - run `cd ui && pnpm build` to refresh the embedded dashboard"
                );
            }
        }
        return;
    }

    let built = Command::new("pnpm")
        .args(["install", "--frozen-lockfile"])
        .current_dir(&ui)
        .status()
        .and_then(|_| Command::new("pnpm").args(["build"]).current_dir(&ui).status());
    match built {
        Ok(status) if status.success() && dist_index.exists() => {}
        _ => panic!(
            "ui/dist is missing and could not be built automatically. \
             Install Node and pnpm, then run `cd ui && pnpm install && pnpm build`."
        ),
    }
}

fn modified(path: &Path) -> std::io::Result<SystemTime> {
    path.metadata().and_then(|m| m.modified())
}

fn newest_mtime(dir: &Path) -> std::io::Result<SystemTime> {
    let mut newest = modified(dir)?;
    for entry in std::fs::read_dir(dir)? {
        let path: PathBuf = entry?.path();
        let mtime = if path.is_dir() { newest_mtime(&path)? } else { modified(&path)? };
        if mtime > newest {
            newest = mtime;
        }
    }
    Ok(newest)
}
