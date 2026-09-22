//! Stable managed-install launcher: the old version's explicit eden executable remains usable.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let root = std::path::PathBuf::from(
        args.next()
            .ok_or("usage: eden-launch MANAGED_ROOT [eden arguments...]")?,
    )
    .canonicalize()?;
    let installation = eden_protocol::updates::active_installation(&root)?.ok_or(
        "no activated installation; explicitly prepare and activate a complete release first",
    )?;
    let executable = installation
        .join("bin")
        .join(if cfg!(windows) { "eden.exe" } else { "eden" });
    let mut command = std::process::Command::new(executable);
    command.args(args).env("EDEN_MANAGED_ROOT", root);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        Err(command.exec().into())
    }
    #[cfg(not(unix))]
    {
        std::process::exit(command.status()?.code().unwrap_or(1));
    }
}
