//! Play a file in the terminal with a status line under it.
//!
//!     cargo run --example play -- clip.mp4
//!
//! space pauses, ←/→ seek 5 s, ↑/↓ change the volume, m mutes, q or esc quits.
use std::io::{self, Write};
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::{cursor, execute, terminal};
use mpv_kitty::{Options, Player, Viewport};

/// The box above the status line, in cells and pixels.
fn video_box() -> io::Result<Viewport> {
    let size = terminal::window_size()?;
    let rows = size.rows.saturating_sub(1).max(1);
    let (cell_w, cell_h) = match (size.width, size.height) {
        (0, _) | (_, 0) => (10, 20),
        (w, h) => (u32::from(w) / u32::from(size.columns.max(1)), u32::from(h) / u32::from(size.rows.max(1))),
    };
    Ok(Viewport { columns: size.columns, rows, pixel_width: u32::from(size.columns) * cell_w, pixel_height: u32::from(rows) * cell_h })
}

fn clock(seconds: f64) -> String {
    let s = seconds.max(0.0) as u64;
    format!("{}:{:02}", s / 60, s % 60)
}

fn play(file: &str) -> io::Result<()> {
    let mut mpv = vec!["mpv".to_string(), "--really-quiet".into()];
    mpv.extend(mpv_kitty::audio_args());
    mpv.extend(["--".into(), file.into()]);
    let mut view = video_box()?;
    let mut player = Player::start(&mpv, view, Options::detect())?;
    let mut out = io::stdout();
    let mut shown = String::new();
    while !player.tick()? {
        let mut force = false;
        if event::poll(Duration::from_millis(2))? {
            match event::read()? {
                Event::Key(k) if k.kind == KeyEventKind::Press => match k.code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Char(' ') => player.toggle_pause()?,
                    KeyCode::Left => player.seek(-5.0)?,
                    KeyCode::Right => player.seek(5.0)?,
                    KeyCode::Up => player.add_volume(5)?,
                    KeyCode::Down => player.add_volume(-5)?,
                    KeyCode::Char('m') => player.toggle_mute()?,
                    _ => {}
                },
                Event::Resize(..) => {
                    view = video_box()?;
                    write!(out, "{}\x1b[2J", mpv_kitty::clear())?;
                    force = true;
                }
                _ => {}
            }
        }
        if let Some(bytes) = player.render(0, 0, view, force)? {
            out.write_all(bytes)?;
        }
        let p = player.progress();
        let status = format!(
            "{} / {}{}   space pause · ←/→ seek · ↑/↓ volume · m mute · q quit",
            clock(p.pos),
            clock(p.duration),
            if p.paused { "  paused" } else { "" }
        );
        if status != shown || force {
            write!(out, "\x1b[{};1H\x1b[2K{status}", view.rows + 1)?;
            shown = status;
        }
        out.flush()?;
    }
    write!(out, "{}", mpv_kitty::clear())?;
    out.flush()
}

fn main() {
    let Some(file) = std::env::args().nth(1) else {
        eprintln!("usage: play FILE");
        std::process::exit(2);
    };
    if !mpv_kitty::kitty_graphics(&|k| std::env::var(k).ok()) {
        eprintln!("this terminal doesn't speak the kitty graphics protocol (try kitty, Ghostty or WezTerm, outside tmux)");
        std::process::exit(2);
    }
    terminal::enable_raw_mode().expect("raw mode");
    execute!(io::stdout(), terminal::EnterAlternateScreen, cursor::Hide).expect("alternate screen");
    let result = play(&file);
    let _ = execute!(io::stdout(), cursor::Show, terminal::LeaveAlternateScreen);
    let _ = terminal::disable_raw_mode();
    if let Err(e) = result {
        eprintln!("{e}");
        std::process::exit(1);
    }
}
