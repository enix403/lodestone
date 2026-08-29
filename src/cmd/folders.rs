use lodestone::error::ExitCode;
use lodestone::snapshot::Snapshot;
use lodestone::{config::Config, Result};

pub fn run(cfg: &Config, json: bool) -> Result<ExitCode> {
    if json {
        let items: Vec<_> = cfg
            .folders
            .iter()
            .map(|f| {
                serde_json::json!({
                    "name": f.name,
                    "local": f.local.display().to_string(),
                    "remote": f.remote,
                    "max_deletes": f.max_deletes,
                    "initialised": Snapshot::exists(&f.name),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&items)?);
        return Ok(ExitCode::Ok);
    }

    if cfg.folders.is_empty() {
        println!("No folders configured. Add one to ~/.config/lode/config.toml:");
        println!();
        println!("  [folder.silvermine]");
        println!("  local  = \"~/silvermine\"");
        println!("  remote = \"per-gdrive:Silvermine\"");
        return Ok(ExitCode::Ok);
    }

    let width = cfg.folders.iter().map(|f| f.name.len()).max().unwrap_or(0);
    for f in &cfg.folders {
        let state = if Snapshot::exists(&f.name) {
            "ready"
        } else {
            "not initialised"
        };
        println!(
            "{:<width$}  {}  ->  {}  [{}]",
            f.name,
            f.local.display(),
            f.remote,
            state,
            width = width
        );
    }
    Ok(ExitCode::Ok)
}
