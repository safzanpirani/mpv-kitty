//! Play video inside any rectangle of a terminal.
//!
//! mpv decodes the file, plays the audio, keeps time, seeks and runs playlists. This crate
//! relays its frames through the kitty graphics protocol, so kitty, Ghostty and WezTerm draw
//! real pixels and scale them to the box on the GPU. In a local kitty each frame travels
//! through a POSIX shared-memory object and the terminal receives about 100 bytes per frame;
//! elsewhere frames go through the pty as base64.
//!
//! Two ways to play:
//!
//! - [`Player`] embeds mpv in your UI. It never writes to the terminal: [`Player::render`]
//!   returns the bytes that draw the newest frame in your box, and you write them when you
//!   draw. Unix only.
//! - [`run`] hands the terminal to mpv (its keys, OSD and audio) and relays frames until it
//!   exits.
//!
//! ```no_run
//! # fn main() -> std::io::Result<()> {
//! use std::io::Write;
//! use mpv_kitty::{Options, Player, Viewport};
//!
//! // A 80×24-cell box whose cells are 10×20 pixels.
//! let view = Viewport { columns: 80, rows: 24, pixel_width: 800, pixel_height: 480 };
//! let mpv: Vec<String> = ["mpv", "--", "clip.mp4"].map(String::from).to_vec();
//! let mut player = Player::start(&mpv, view, Options::detect())?;
//! let mut out = std::io::stdout();
//! while !player.tick()? {
//!     if let Some(bytes) = player.render(0, 0, view, false)? {
//!         out.write_all(bytes)?;
//!         out.flush()?;
//!     }
//!     std::thread::sleep(std::time::Duration::from_millis(2));
//! }
//! out.write_all(mpv_kitty::clear().as_bytes())?;
//! # Ok(())
//! # }
//! ```
//!
//! mpv must be on `PATH` (tested with 0.41). Multiplexers (tmux, zellij) don't pass the kitty
//! graphics protocol through; [`kitty_graphics`] says no inside them.

#[cfg(unix)]
mod player;
mod video;

#[cfg(unix)]
pub use player::Player;
pub use video::{Progress, Viewport, clear, is_video_id, run};

/// How frames reach the terminal.
///
/// `Direct` relays mpv's base64 through the pty: about 3 MB per 720p frame. `Shm` and `File`
/// put the pixels in a POSIX shared-memory object or a temp file and send only its name, so
/// the pty carries about 100 bytes per frame and the terminal skips base64. Both need the
/// terminal on the same machine, and only kitty itself reads them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transfer {
    Direct,
    Shm,
    File,
}

impl Transfer {
    /// Shared memory for kitty on this machine; base64 for everything else (Ghostty, WezTerm,
    /// ssh, non-unix systems).
    pub fn detect(env: &dyn Fn(&str) -> Option<String>) -> Transfer {
        if real_kitty(env) && !remote(env) && cfg!(unix) { Transfer::Shm } else { Transfer::Direct }
    }

    /// `direct`, `shm` or `file`, for a user setting that overrides [`Transfer::detect`].
    pub fn parse(name: &str) -> Option<Transfer> {
        match name {
            "direct" => Some(Transfer::Direct),
            "shm" => Some(Transfer::Shm),
            "file" => Some(Transfer::File),
            _ => None,
        }
    }
}

/// How a [`Player`] or [`run`] sends frames, and the largest frame mpv renders.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Options {
    pub transfer: Transfer,
    /// Width and height in pixels; mpv renders at most this, keeping the box's aspect, and the
    /// terminal scales it up to the cells on the GPU.
    pub max_surface: (u32, u32),
}

impl Options {
    /// Detected from this process's environment.
    pub fn detect() -> Options {
        Options::from_env(&|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
    }

    /// Detected from `env` (a lookup like `std::env::var`).
    pub fn from_env(env: &dyn Fn(&str) -> Option<String>) -> Options {
        Options { transfer: Transfer::detect(env), max_surface: max_surface(env) }
    }
}

/// Terminals that speak the kitty graphics protocol: kitty, Ghostty and WezTerm. False
/// inside tmux or zellij, which don't pass it through.
pub fn kitty_graphics(env: &dyn Fn(&str) -> Option<String>) -> bool {
    if env("TMUX").is_some() || env("ZELLIJ").is_some() {
        return false;
    }
    env("KITTY_WINDOW_ID").is_some()
        || env("TERM").is_some_and(|t| t.contains("kitty") || t.contains("ghostty"))
        || env("TERM_PROGRAM").is_some_and(|t| matches!(t.as_str(), "ghostty" | "WezTerm"))
        || env("GHOSTTY_RESOURCES_DIR").is_some()
}

/// kitty itself, which reads shared memory. Ghostty and WezTerm only take base64, and WezTerm
/// can run with `TERM=xterm-kitty`, so a `TERM_PROGRAM` rules the TERM hint out.
pub fn real_kitty(env: &dyn Fn(&str) -> Option<String>) -> bool {
    env("KITTY_WINDOW_ID").is_some() || (env("TERM").as_deref() == Some("xterm-kitty") && env("TERM_PROGRAM").is_none())
}

/// The terminal is on another machine, so frames cross the network as base64.
pub fn remote(env: &dyn Fn(&str) -> Option<String>) -> bool {
    env("SSH_CONNECTION").is_some() || env("SSH_TTY").is_some()
}

/// 1280×720 locally. Over ssh every frame crosses the network as base64, so it gets a
/// quarter of the pixels: 640×360.
pub fn max_surface(env: &dyn Fn(&str) -> Option<String>) -> (u32, u32) {
    if remote(env) { (640, 360) } else { (1280, 720) }
}

/// mpv arguments for low-latency audio, worth adding to any player's argv.
///
/// On macOS mpv 0.41 defaults to its AVFoundation output, which queues a 2-second buffer
/// behind the volume control, so sound trails the picture and volume changes take seconds to
/// be heard. CoreAudio buffers about 26 ms but fails to set a float channel layout on current
/// macOS, so these ask for CoreAudio with 16-bit samples and keep AVFoundation as the
/// fallback. Everywhere, mpv's own buffer drops from 0.2 s to 0.1 s.
pub fn audio_args() -> Vec<String> {
    let mut a = vec!["--audio-buffer=0.1".to_string()];
    if cfg!(target_os = "macos") {
        a.push("--ao=coreaudio,avfoundation,".into());
        a.push("--audio-format=s16".into());
    }
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |k: &str| pairs.iter().find(|p| p.0 == k).map(|p| p.1.to_string())
    }

    #[test]
    fn shm_only_for_kitty_on_this_machine() {
        assert_eq!(Transfer::detect(&env(&[("TERM", "xterm-kitty")])), if cfg!(unix) { Transfer::Shm } else { Transfer::Direct });
        assert_eq!(Transfer::detect(&env(&[("TERM", "xterm-kitty"), ("SSH_TTY", "/dev/ttys1")])), Transfer::Direct);
        assert_eq!(Transfer::detect(&env(&[("TERM", "xterm-ghostty")])), Transfer::Direct);
        assert_eq!(
            Transfer::detect(&env(&[("TERM", "xterm-kitty"), ("TERM_PROGRAM", "WezTerm")])),
            Transfer::Direct,
            "WezTerm with kitty's TERM can't read shm"
        );
        assert_eq!(Transfer::parse("file"), Some(Transfer::File));
        assert_eq!(Transfer::parse("pipe"), None);
    }

    #[test]
    fn multiplexers_hide_kitty_graphics() {
        assert!(kitty_graphics(&env(&[("TERM", "xterm-kitty")])));
        assert!(kitty_graphics(&env(&[("TERM_PROGRAM", "WezTerm")])));
        assert!(!kitty_graphics(&env(&[("TERM", "xterm-kitty"), ("TMUX", "1")])));
        assert!(!kitty_graphics(&env(&[("TERM", "xterm-256color")])));
    }

    #[test]
    fn audio_is_low_latency_and_on_macos_uses_coreaudio_in_16_bit() {
        let a = audio_args();
        assert!(a.contains(&"--audio-buffer=0.1".to_string()));
        if cfg!(target_os = "macos") {
            assert!(a.contains(&"--ao=coreaudio,avfoundation,".to_string()) && a.contains(&"--audio-format=s16".to_string()));
        }
    }
}
