//! Open a URL in the desktop's default browser.

use std::process::{Command, Stdio};

/// The platform's URL opener and its leading arguments. `start` takes a
/// window title first, hence the empty one.
fn opener() -> (&'static str, &'static [&'static str]) {
    if cfg!(target_os = "macos") {
        ("open", &[])
    } else if cfg!(target_os = "windows") {
        ("cmd", &["/C", "start", ""])
    } else {
        ("xdg-open", &[])
    }
}

/// Launch the browser on `url` without waiting for it. Streams are detached
/// so it cannot draw over the TUI; a thread reaps it.
pub fn open_url(url: &str) -> std::io::Result<()> {
    let (program, args) = opener();
    let mut child = Command::new(program)
        .args(args)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}
