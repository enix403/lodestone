use lodestone::config::Config;
use lodestone::error::ExitCode;
use lodestone::rclone::{session_name_len, Rclone, FILENAME_LIMIT, MIN_VERSION};
use lodestone::session::Session;
use lodestone::snapshot::Snapshot;
use lodestone::{machine, paths, Result};
use std::path::Path;

pub fn run(config: Option<&Path>) -> Result<ExitCode> {
    let mut failed = false;
    let mut check = |label: &str, outcome: std::result::Result<String, String>| match outcome {
        Ok(detail) => println!("  ok    {label:<22} {detail}"),
        Err(detail) => {
            failed = true;
            println!("  FAIL  {label:<22} {detail}");
        }
    };

    println!("environment");
    let rclone = match Rclone::discover() {
        Ok(r) => {
            check("rclone binary", Ok(r.binary.display().to_string()));
            Some(r)
        }
        Err(e) => {
            check("rclone binary", Err(e.to_string()));
            None
        }
    };

    if let Some(r) = &rclone {
        match r.version() {
            Ok(v) if v >= MIN_VERSION => {
                check("rclone version", Ok(format!("{v} (>= {MIN_VERSION})")))
            }
            Ok(v) => check(
                "rclone version",
                Err(format!(
                    "{v} is below the required {MIN_VERSION}. Distro packages are often \
                     years stale; install with: curl https://rclone.org/install.sh | sudo bash"
                )),
            ),
            Err(e) => check("rclone version", Err(e.to_string())),
        }
    }

    check(
        "machine id",
        machine::machine_id().map_err(|e| e.to_string()),
    );
    check(
        "filters",
        Ok(format!(
            "{} built-in rule(s), {}",
            lodestone::filters::RULES.len(),
            lodestone::filters::fingerprint()
        )),
    );
    check("state dir", Ok(paths::state_dir().display().to_string()));
    check("cache dir", Ok(paths::cache_dir().display().to_string()));

    // Built once, and only if rclone is usable; every listing-based check shares it.
    let session = Session::new().ok();

    println!("\nconfiguration");
    match Config::load(config) {
        Err(e) => check("config", Err(e.to_string())),
        Ok(cfg) => {
            check("config", Ok(format!("{} folder(s)", cfg.folders.len())));

            let remotes = rclone
                .as_ref()
                .and_then(|r| r.list_remotes().ok())
                .unwrap_or_default();

            for f in &cfg.folders {
                check(
                    &format!("{}: local", f.name),
                    if f.local.is_dir() {
                        Ok(f.local.display().to_string())
                    } else {
                        Err(format!("{} does not exist", f.local.display()))
                    },
                );
                match f.remote_name() {
                    Some(name) if !remotes.is_empty() && !remotes.iter().any(|r| r == name) => {
                        check(
                            &format!("{}: remote", f.name),
                            Err(format!(
                                "{name:?} is not in rclone.conf (see `rclone listremotes`)"
                            )),
                        );
                    }
                    _ => check(&format!("{}: remote", f.name), Ok(f.remote.clone())),
                }
                // bisync flattens both full paths into one workdir filename; deep paths
                // breach the 255-byte limit and fail with "file name too long".
                let projected = session_name_len(&f.local.display().to_string(), &f.remote);
                check(
                    &format!("{}: path length", f.name),
                    if projected <= FILENAME_LIMIT {
                        Ok(format!("{projected}/{FILENAME_LIMIT} bytes"))
                    } else {
                        Err(format!(
                            "bisync needs a {projected}-byte workdir filename, over the \
                             {FILENAME_LIMIT}-byte limit. Use a shallower local path."
                        ))
                    },
                );
                check(
                    &format!("{}: baseline", f.name),
                    if Snapshot::exists(&f.name) {
                        Ok("present".into())
                    } else {
                        Err(format!("no snapshot — run `lode init {}`", f.name))
                    },
                );

                // Symlinks are reported but never fatal: rclone skips them by design, and
                // the point is only that silently-absent files should not be a surprise.
                if f.local.is_dir() {
                    let links = lodestone::hazards::find_symlinks(&f.local);
                    check(
                        &format!("{}: symlinks", f.name),
                        Ok(if links.is_empty() {
                            "none".to_string()
                        } else {
                            format!(
                                "{} found — rclone skips these, so they are absent from the remote: {}",
                                links.len(),
                                links
                                    .iter()
                                    .take(3)
                                    .map(|p| p.display().to_string())
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            )
                        }),
                    );
                    check(
                        &format!("{}: filesystem", f.name),
                        Ok(match lodestone::hazards::is_case_insensitive(&f.local) {
                            Some(true) => {
                                "case-insensitive (two names differing only in case cannot coexist)"
                                    .into()
                            }
                            Some(false) => "case-sensitive".to_string(),
                            None => "could not probe case sensitivity".to_string(),
                        }),
                    );
                }

                // The listing-based hazards need both sides, so they only run for a folder
                // that is initialised and reachable.
                if Snapshot::exists(&f.name) && f.local.is_dir() {
                    match session.as_ref().map(|s| s.plan(f)) {
                        Some(Ok(_)) => {
                            check(&format!("{}: name hazards", f.name), Ok("none".into()))
                        }
                        Some(Err(e)) => match &e {
                            lodestone::Error::NameCollisions { .. }
                            | lodestone::Error::DuplicateNames { .. } => {
                                check(&format!("{}: name hazards", f.name), Err(e.to_string()))
                            }
                            // Anything else (unreachable remote, stale snapshot) is not a
                            // name hazard and is reported by its own check.
                            _ => check(
                                &format!("{}: listings", f.name),
                                Err(first_line(&e.to_string())),
                            ),
                        },
                        None => {}
                    }
                }
            }
        }
    }

    println!();
    if failed {
        println!("doctor found problems.");
        Ok(ExitCode::Usage)
    } else {
        println!("all checks passed.");
        Ok(ExitCode::Ok)
    }
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or_default().to_string()
}
