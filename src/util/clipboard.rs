//! Copy text to the user's clipboard from inside the TUI.
//!
//! Two channels, both attempted:
//!
//! 1. **OSC 52** — an escape sequence the terminal itself interprets.
//!    Works over SSH (the whole point of a terminal client you can run
//!    on a remote box) in WezTerm, iTerm2, kitty, Ghostty, foot and
//!    Alacritty. Silently ignored by terminals that don't support it.
//! 2. **`pbcopy`** on macOS — belt and suspenders for Terminal.app,
//!    which ignores OSC 52.
//!
//! Neither reports success reliably, so the caller should phrase its
//! toast as "copied" rather than promising a specific destination.

use std::io::{self, Write};

use base64::Engine;

/// Push `text` to the clipboard. Errors only when even stdout can't be
/// written to — the escape sequence itself is fire-and-forget.
pub fn copy(text: &str) -> io::Result<()> {
    let b64 = base64::engine::general_purpose::STANDARD.encode(text);
    {
        let mut out = io::stdout().lock();
        // ESC ] 52 ; c ; <base64> ESC \
        write!(out, "\x1b]52;c;{b64}\x1b\\")?;
        out.flush()?;
    }
    #[cfg(target_os = "macos")]
    {
        use std::process::{Command, Stdio};
        if let Ok(mut child) = Command::new("pbcopy")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
            let _ = child.wait();
        }
    }
    Ok(())
}
