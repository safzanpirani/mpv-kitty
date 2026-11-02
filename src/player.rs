use std::io::{self, Write};
use std::sync::mpsc::TryRecvError;

use crate::Options;
use crate::video::{Frame, Progress, Session, Viewport};

/// mpv playing inside a box of your UI.
///
/// mpv runs as a child process with its terminal input off; control it with the methods
/// below or [`Player::command`]. The player never writes to the terminal: call
/// [`Player::tick`] often (every few milliseconds while playing) and write what
/// [`Player::render`] returns. Dropping the player stops mpv; write [`crate::clear`] to remove
/// the last frame from the screen.
pub struct Player {
    session: Session,
    frame: Option<Frame>,
    dirty: bool,
    slot: usize,
    out: Vec<u8>,
}

impl Player {
    /// Start mpv. `mpv` is its full argv: the program, your own options, then `--` and the
    /// files. The kitty output, its sizing and the embedding flags are added before `--`.
    pub fn start(mpv: &[String], view: Viewport, options: Options) -> io::Result<Player> {
        Ok(Player { session: Session::start(mpv, view, options)?, frame: None, dirty: false, slot: 0, out: Vec::new() })
    }

    /// Take the newest frame without waiting. Frames that arrive faster than you render are
    /// skipped. True once mpv has exited and its last frame has been rendered; an error when
    /// mpv failed (exit code 4, a user quit, counts as success).
    pub fn tick(&mut self) -> io::Result<bool> {
        loop {
            match self.session.rx.as_ref().map(|rx| rx.try_recv()) {
                Some(Ok(Ok(frame))) => {
                    self.frame = Some(frame);
                    self.dirty = true;
                }
                Some(Ok(Err(e))) => return Err(e),
                Some(Err(TryRecvError::Empty)) => return Ok(false),
                None | Some(Err(TryRecvError::Disconnected)) => {
                    // Present the final frame before reporting the end.
                    if self.dirty {
                        return Ok(false);
                    }
                    return match self.session.status()? {
                        Some(status) if !status.success() && status.code() != Some(4) => Err(io::Error::other(format!("mpv exited with {status}"))),
                        Some(_) => Ok(true),
                        None => Ok(false),
                    };
                }
            }
        }
    }

    /// Whether a frame has arrived yet (until then, whatever you drew in the box shows).
    pub fn has_frame(&self) -> bool {
        self.frame.is_some()
    }

    /// The bytes that draw the newest frame, centred in the box whose top-left cell is
    /// (`column`, `row`), counted from 0. None when nothing changed since the last call;
    /// `force` draws again anyway (after a resize, or after [`crate::clear`]). The cursor is
    /// saved and restored around the image.
    pub fn render(&mut self, column: u16, row: u16, view: Viewport, force: bool) -> io::Result<Option<&[u8]>> {
        if view.columns == 0 || view.rows == 0 || (!self.dirty && !force) {
            return Ok(None);
        }
        let Some(frame) = &mut self.frame else { return Ok(None) };
        let (cols, rows) = view.placement(frame.width, frame.height);
        let x = column + (view.columns - cols) / 2;
        let y = row + (view.rows - rows) / 2;
        self.out.clear();
        write!(self.out, "\x1b7\x1b[{};{}H", y + 1, x + 1)?;
        frame.encode(view, self.slot, &mut self.out)?;
        self.out.extend_from_slice(b"\x1b8");
        self.dirty = false;
        self.slot = 1 - self.slot;
        Ok(Some(&self.out))
    }

    /// Position, duration and pause state, as mpv last reported them.
    pub fn progress(&self) -> Progress {
        self.session.progress()
    }

    /// Send any mpv input command, such as `json!(["seek", 5, "relative"])`.
    pub fn command(&self, command: serde_json::Value) -> io::Result<()> {
        self.session.command(command)
    }

    pub fn toggle_pause(&self) -> io::Result<()> {
        self.command(serde_json::json!(["cycle", "pause"]))
    }

    /// Seek by `seconds` from the current position (negative goes back).
    pub fn seek(&self, seconds: f64) -> io::Result<()> {
        self.command(serde_json::json!(["seek", seconds, "relative+exact"]))
    }

    /// Change the volume by `percent` (mpv's range is 0–130).
    pub fn add_volume(&self, percent: i64) -> io::Result<()> {
        self.command(serde_json::json!(["add", "volume", percent]))
    }

    pub fn toggle_mute(&self) -> io::Result<()> {
        self.command(serde_json::json!(["cycle", "mute"]))
    }

    pub fn set_speed(&self, speed: f64) -> io::Result<()> {
        self.command(serde_json::json!(["set", "speed", speed]))
    }
}
