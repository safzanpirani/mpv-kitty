//! The frame pipeline: mpv's kitty output parsed into frames, frames handed to the terminal
//! (base64, shared memory or temp files), the embedded `Session` and the foreground `run`.
//! Nothing here writes to the terminal except `run`, which owns it by design.
use std::io::{self, BufRead, BufReader, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread::JoinHandle;

use base64::{Engine, engine::general_purpose::STANDARD};

use crate::Transfer;

// Two ids double-buffer frames. Retransmitting to a shown id makes kitty hide it
// until the last chunk lands, which flickers on every frame. ASCII "MPVK" and "MPVL".
const IMAGE_IDS: [u32; 2] = [0x4d50564b, 0x4d50564c];
const MAX_PACKET: usize = 8192;

/// The decoded frame stays under this (1920×1080 RGB is about 6 MB).
const MAX_FRAME: usize = 6 * 1024 * 1024;


/// Frame pixels, decoded from mpv's base64 as its chunks arrive.
struct Pixels {
    /// kitty's `f=`: 24 (RGB) or 32 (RGBA).
    format: u32,
    buf: PixelBuf,
    /// Decoded bytes so far, and base64 left over from a chunk that didn't end on a quad.
    filled: usize,
    carry: [u8; 4],
    carry_len: usize,
}

enum PixelBuf {
    Heap(Vec<u8>),
    /// Decoded straight into a shared-memory object, so publishing it copies nothing.
    #[cfg(unix)]
    Shm(ShmBuf),
}

impl Pixels {
    fn new(format: u32, len: usize, transfer: Transfer) -> io::Result<Pixels> {
        let buf = match transfer {
            #[cfg(unix)]
            Transfer::Shm => PixelBuf::Shm(ShmBuf::new(len)?),
            _ => PixelBuf::Heap(vec![0; len]),
        };
        Ok(Pixels { format, buf, filled: 0, carry: [0; 4], carry_len: 0 })
    }

    fn data(&self) -> &[u8] {
        match &self.buf {
            PixelBuf::Heap(v) => v,
            #[cfg(unix)]
            PixelBuf::Shm(s) => s.bytes(),
        }
    }

    /// Decode one chunk's base64 in place. False when it isn't base64 or overruns the frame.
    fn feed(&mut self, mut input: &[u8]) -> bool {
        if self.carry_len > 0 {
            let take = (4 - self.carry_len).min(input.len());
            self.carry[self.carry_len..self.carry_len + take].copy_from_slice(&input[..take]);
            self.carry_len += take;
            input = &input[take..];
            if self.carry_len < 4 {
                return true;
            }
            self.carry_len = 0;
            let quad = self.carry;
            if !self.quad(&quad) {
                return false;
            }
        }
        let whole = input.len() / 4 * 4;
        let (body, rest) = input.split_at(whole);
        // A padded quad decodes to fewer than 3 bytes, which decode_slice can't fit at the end.
        let (bulk, last) = if body.ends_with(b"=") { body.split_at(whole - 4) } else { (body, &[][..]) };
        if !bulk.is_empty() {
            let filled = self.filled;
            let out = match &mut self.buf {
                PixelBuf::Heap(v) => &mut v[filled..],
                #[cfg(unix)]
                PixelBuf::Shm(s) => &mut s.bytes_mut()[filled..],
            };
            match STANDARD.decode_slice(bulk, out) {
                Ok(n) => self.filled += n,
                Err(_) => return false,
            }
        }
        if !last.is_empty() && !self.quad(last) {
            return false;
        }
        self.carry[..rest.len()].copy_from_slice(rest);
        self.carry_len = rest.len();
        true
    }

    fn quad(&mut self, quad: &[u8]) -> bool {
        let mut tmp = [0u8; 3];
        let Ok(n) = STANDARD.decode_slice(quad, &mut tmp) else { return false };
        let filled = self.filled;
        let out = match &mut self.buf {
            PixelBuf::Heap(v) => v.get_mut(filled..filled + n),
            #[cfg(unix)]
            PixelBuf::Shm(s) => s.bytes_mut().get_mut(filled..filled + n),
        };
        let Some(out) = out else { return false };
        out.copy_from_slice(&tmp[..n]);
        self.filled += n;
        true
    }

    fn complete(&self) -> bool {
        self.carry_len == 0 && self.filled == self.data().len()
    }
}

/// Names handed to the terminal recently. kitty unlinks each one after reading it; a terminal
/// that ignores them would leak, so names older than `KEEP` frames are removed here too.
struct Published {
    names: std::collections::VecDeque<(Transfer, String)>,
}

// kitty reads and unlinks a frame within milliseconds, so only names a terminal ignored stay
// outstanding. 30 frames (half a second at 60 fps) leaves a slow reader ample time; acks would
// remove the window entirely but need the terminal to answer, which q=2 turns off.
const KEEP: usize = 30;

static PUBLISHED: std::sync::Mutex<Published> = std::sync::Mutex::new(Published { names: std::collections::VecDeque::new() });

fn remember(t: Transfer, name: String) {
    let mut p = PUBLISHED.lock().unwrap_or_else(|e| e.into_inner());
    p.names.push_back((t, name));
    while p.names.len() > KEEP {
        if let Some((t, old)) = p.names.pop_front() {
            unlink(t, &old);
        }
    }
}

/// Remove every name still outstanding (playback ended; kitty has read what it will).
pub(crate) fn cleanup() {
    let mut p = PUBLISHED.lock().unwrap_or_else(|e| e.into_inner());
    for (t, name) in p.names.drain(..) {
        unlink(t, &name);
    }
}

fn unlink(t: Transfer, name: &str) {
    match t {
        Transfer::File => {
            let _ = std::fs::remove_file(name);
        }
        #[cfg(unix)]
        Transfer::Shm => {
            if let Ok(c) = std::ffi::CString::new(name) {
                unsafe { libc::shm_unlink(c.as_ptr()) };
            }
        }
        _ => {}
    }
}

fn next_name() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::Relaxed)
}

/// A mapped POSIX shared-memory object. macOS limits names to 31 bytes and needs mmap
/// (write(2) on shm fails there). The object is unlinked on drop until `release` hands its
/// name to someone else (kitty unlinks what it reads; `PUBLISHED` covers the rest).
#[cfg(unix)]
struct ShmBuf {
    name: String,
    ptr: std::ptr::NonNull<u8>,
    len: usize,
    owned: bool,
}

// The mapping belongs to this value alone; it moves between threads with the frame.
#[cfg(unix)]
unsafe impl Send for ShmBuf {}

#[cfg(unix)]
impl ShmBuf {
    fn new(len: usize) -> io::Result<ShmBuf> {
        let name = format!("/mpvk-{:x}-{:x}", std::process::id(), next_name());
        let c = std::ffi::CString::new(name.clone()).map_err(io::Error::other)?;
        #[cfg(target_os = "macos")]
        let mode = 0o600 as libc::c_uint;
        #[cfg(not(target_os = "macos"))]
        let mode = 0o600 as libc::mode_t;
        unsafe {
            let fd = libc::shm_open(c.as_ptr(), libc::O_CREAT | libc::O_EXCL | libc::O_RDWR, mode);
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            let fail = |fd| {
                let e = io::Error::last_os_error();
                libc::close(fd);
                libc::shm_unlink(c.as_ptr());
                e
            };
            if libc::ftruncate(fd, len as libc::off_t) != 0 {
                return Err(fail(fd));
            }
            let p = libc::mmap(std::ptr::null_mut(), len.max(1), libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd, 0);
            if p == libc::MAP_FAILED {
                return Err(fail(fd));
            }
            libc::close(fd);
            Ok(ShmBuf { name, ptr: std::ptr::NonNull::new_unchecked(p.cast()), len, owned: true })
        }
    }

    fn bytes(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }

    fn bytes_mut(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }

    /// Hand the name over once; after that this value only unmaps.
    fn release(&mut self) -> Option<String> {
        std::mem::replace(&mut self.owned, false).then(|| self.name.clone())
    }
}

#[cfg(unix)]
impl Drop for ShmBuf {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.ptr.as_ptr().cast(), self.len.max(1)) };
        if self.owned {
            unlink(Transfer::Shm, &self.name);
        }
    }
}

/// Copy `data` into a fresh shared-memory object and return its name.
#[cfg(unix)]
fn shm_create(data: &[u8]) -> io::Result<String> {
    let mut buf = ShmBuf::new(data.len())?;
    buf.bytes_mut().copy_from_slice(data);
    Ok(buf.release().unwrap_or_default())
}

#[cfg(not(unix))]
fn shm_create(_: &[u8]) -> io::Result<String> {
    Err(io::Error::other("shared memory needs a unix system"))
}

fn shm_publish(data: &[u8]) -> io::Result<String> {
    let name = shm_create(data)?;
    remember(Transfer::Shm, name.clone());
    Ok(name)
}

/// Write `data` to a temp file kitty may read and delete (its name must say tty-graphics-protocol).
fn file_publish(data: &[u8]) -> io::Result<String> {
    let path = std::env::temp_dir().join(format!("mpv-kitty-tty-graphics-protocol-{}-{}.rgb", std::process::id(), next_name()));
    // A fresh, owner-only file: a path someone planted (or a symlink) fails instead of being written.
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(&path)?.write_all(data)?;
    let name = path.to_string_lossy().into_owned();
    remember(Transfer::File, name.clone());
    Ok(name)
}

/// The box video plays in: its size in cells and in pixels. Get the pixels from the
/// terminal's window size (`TIOCGWINSZ`) scaled to the box.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Viewport {
    pub columns: u16,
    pub rows: u16,
    pub pixel_width: u32,
    pub pixel_height: u32,
}

impl Viewport {
    /// Keep the original viewport aspect, within the `max` decode surface.
    pub fn render_size(self, max: (u32, u32)) -> (u32, u32) {
        let w = self.pixel_width.max(1);
        let h = self.pixel_height.max(1);
        let scale = (max.0 as f64 / w as f64).min(max.1 as f64 / h as f64).min(1.0);
        (
            (w as f64 * scale).round().max(1.0) as u32,
            (h as f64 * scale).round().max(1.0) as u32,
        )
    }

    /// The cells a `w`×`h` frame covers when scaled to fit the box, keeping its aspect.
    pub fn placement(self, w: u32, h: u32) -> (u16, u16) {
        let scale =
            (self.pixel_width.max(1) as f64 / w.max(1) as f64).min(self.pixel_height.max(1) as f64 / h.max(1) as f64);
        let cols = (w as f64 * scale * self.columns as f64 / self.pixel_width.max(1) as f64).round() as u16;
        let rows = (h as f64 * scale * self.rows as f64 / self.pixel_height.max(1) as f64).round() as u16;
        (cols.clamp(1, self.columns.max(1)), rows.clamp(1, self.rows.max(1)))
    }
}

/// mpv's argv with the kitty output, its sizing and the embedding flags inserted before the
/// `--` that starts the files. Inserted flags come last, so they win over `base`.
pub(crate) fn argv(base: &[String], view: Viewport, max: (u32, u32), embedded: bool, socket: Option<&std::path::Path>) -> Vec<String> {
    let (w, h) = view.render_size(max);
    let mut extra = vec![
        "--vo=kitty".into(),
        // Software scaling tuned for speed; the terminal scales up on the GPU anyway.
        "--profile=sw-fast".into(),
        // zimg's slice threads split each frame into column strips that don't line up,
        // which draws a vertical seam through the picture.
        "--zimg-threads=1".into(),
        // The kitty output needs frames in system memory; hardware decoders copy back.
        "--hwdec=auto-copy".into(),
        "--video-unscaled=downscale-big".into(),
        format!("--vo-kitty-width={w}"),
        format!("--vo-kitty-height={h}"),
        format!("--vo-kitty-cols={}", view.columns.max(1)),
        format!("--vo-kitty-rows={}", view.rows.max(1)),
        // mpv reuses one shm name, even in foreground playback. Always receive
        // frame-owned base64; this crate publishes independent objects for the terminal.
        "--vo-kitty-use-shm=no".into(),
    ];
    if embedded {
        extra.extend(
            [
                "--vo-kitty-alt-screen=no",
                "--vo-kitty-config-clear=no",
                "--input-terminal=no",
                "--input-default-bindings=no",
                "--osd-level=0",
            ]
            .map(str::to_string),
        );
    }
    if let Some(socket) = socket {
        extra.push(format!("--input-ipc-server={}", socket.display()));
    }
    let mut out = base.to_vec();
    let boundary = out.iter().position(|x| x == "--").unwrap_or(out.len());
    out.splice(boundary..boundary, extra);
    out
}

fn dimensions(body: &[u8]) -> io::Result<(u32, u32)> {
    let header = body.split(|&b| b == b';').next().unwrap_or_default();
    let header = std::str::from_utf8(header).map_err(io::Error::other)?;
    let value = |key: &str| {
        header
            .split(',')
            .find_map(|p| p.strip_prefix(key))
            .and_then(|s| s.parse::<u32>().ok())
            .filter(|&v| v > 0)
    };
    value("s=")
        .zip(value("v="))
        .ok_or_else(|| io::Error::other("mpv sent a kitty frame without valid dimensions"))
}

/// Append `body` as a packet. Only the first packet of a frame is rewritten (image id,
/// placement); continuation payloads go out byte-for-byte.
fn encode_into(body: &[u8], view: Viewport, slot: usize, out: &mut Vec<u8>) -> io::Result<()> {
    let normalized = finish_small_frame(body)?;
    let body = normalized.as_deref().unwrap_or(body);
    if !body.starts_with(b"a=T,") {
        push_packet(out, body);
        return Ok(());
    }
    let (w, h) = dimensions(body)?;
    let split = body
        .iter()
        .position(|&b| b == b';')
        .ok_or_else(|| io::Error::other("missing kitty payload"))?;
    let header = std::str::from_utf8(&body[..split]).map_err(io::Error::other)?;
    let payload = &body[split + 1..];
    let shm = header.split(',').any(|p| p == "t=s");
    let (cols, rows) = view.placement(w, h);
    out.extend_from_slice(b"\x1b_G");
    for p in header.split(',').filter(|p| !["c=", "r=", "i=", "p=", "S="].iter().any(|k| p.starts_with(k))) {
        out.extend_from_slice(p.as_bytes());
        out.push(b',');
    }
    write!(out, "i={},p=1,c={cols},r={rows}", IMAGE_IDS[slot])?;
    if shm {
        let bpp = if header.split(',').any(|p| p == "f=32") { 4 } else { 3 };
        write!(out, ",S={}", u64::from(w) * u64::from(h) * bpp)?;
        out.push(b';');
        let name = STANDARD.decode(payload).map_err(io::Error::other)?;
        let name = if name.starts_with(b"/") { name } else { [b"/".as_slice(), &name].concat() };
        out.extend_from_slice(STANDARD.encode(name).as_bytes());
    } else {
        out.push(b';');
        out.extend_from_slice(payload);
    }
    out.extend_from_slice(b"\x1b\\");
    Ok(())
}

fn push_packet(out: &mut Vec<u8>, body: &[u8]) {
    out.extend_from_slice(b"\x1b_G");
    out.extend_from_slice(body);
    out.extend_from_slice(b"\x1b\\");
}

// mpv 0.41 forgets m=0 when RGB base64 fits in one 4096-byte packet.
// Infer completion only when the payload contains the entire declared frame.
fn finish_small_frame(body: &[u8]) -> io::Result<Option<Vec<u8>>> {
    if !body.starts_with(b"a=T,") {
        return Ok(None);
    }
    let Some(split) = body.iter().position(|&b| b == b';') else {
        return Ok(None);
    };
    let header = std::str::from_utf8(&body[..split]).map_err(io::Error::other)?;
    if header.split(',').any(|p| p == "t=s") || !header.split(',').any(|p| p == "m=1") {
        return Ok(None);
    }
    let (w, h) = dimensions(body)?;
    let size = u64::from(w) * u64::from(h) * 4;
    if size > 4096 || body.len() - split - 1 != size as usize {
        return Ok(None);
    }
    let header = header
        .split(',')
        .map(|p| if p == "m=1" { "m=0" } else { p })
        .collect::<Vec<_>>()
        .join(",");
    Ok(Some([header.as_bytes(), b";", &body[split + 1..]].concat()))
}

/// An ordinary terminal fragment or one complete APC body, bounded to 8 KiB.
struct Unit {
    ordinary: Vec<u8>,
    graphics: Option<Vec<u8>>,
}

fn unit(input: &mut impl BufRead) -> io::Result<Option<Unit>> {
    let mut prefix = Vec::new();
    if (&mut *input).take(MAX_PACKET as u64).read_until(0x1b, &mut prefix)? == 0 {
        return Ok(None);
    }
    if prefix.last() != Some(&0x1b) {
        return Ok(Some(Unit {
            ordinary: prefix,
            graphics: None,
        }));
    }
    let mut next = [0u8; 1];
    if input.read(&mut next)? == 0 {
        return Ok(Some(Unit {
            ordinary: prefix,
            graphics: None,
        }));
    }
    prefix.push(next[0]);
    if next[0] != b'_' {
        return Ok(Some(Unit {
            ordinary: prefix,
            graphics: None,
        }));
    }
    input.read_exact(&mut next)?;
    prefix.push(next[0]);
    if next[0] != b'G' {
        return Ok(Some(Unit {
            ordinary: prefix,
            graphics: None,
        }));
    }
    let mut body = Vec::new();
    (&mut *input)
        .take((MAX_PACKET + 1) as u64)
        .read_until(0x1b, &mut body)?;
    if body.len() > MAX_PACKET || body.pop() != Some(0x1b) {
        return Err(io::Error::other("mpv sent an oversized or truncated kitty packet"));
    }
    input.read_exact(&mut next)?;
    if next[0] != b'\\' {
        // mpv 0.41's uninit writes a=d without ST, then restores the cursor.
        // Finish that known cleanup command and preserve the following CSI.
        if body == b"a=d;" {
            prefix.truncate(prefix.len() - 3);
            push_packet(&mut prefix, &body);
            prefix.extend([0x1b, next[0]]);
            return Ok(Some(Unit {
                ordinary: prefix,
                graphics: None,
            }));
        }
        return Err(io::Error::other("mpv sent an invalid kitty packet terminator"));
    }
    prefix.truncate(prefix.len() - 3);
    Ok(Some(Unit {
        ordinary: prefix,
        graphics: Some(body),
    }))
}

/// Whether `id` is one of the two kitty image ids video frames use, so an app that places its
/// own kitty images can keep clear of them.
pub fn is_video_id(id: u32) -> bool {
    IMAGE_IDS.contains(&id)
}

/// Free one buffer's image and placements.
fn delete_slot(slot: usize) -> String {
    format!("\x1b_Ga=d,d=I,i={},q=2;\x1b\\", IMAGE_IDS[slot])
}

/// The escape sequence that removes the video from the terminal (both frame buffers). Write
/// it when playback ends, or before drawing something that should cover the video: kitty
/// draws images above text.
pub fn clear() -> String {
    delete_slot(0) + &delete_slot(1)
}

pub(crate) struct Frame {
    pub(crate) width: u32,
    pub(crate) height: u32,
    /// mpv's packets, relayed as they are (the `Direct` transfer), and their total size.
    bodies: Vec<Vec<u8>>,
    bytes: usize,
    /// Decoded pixels and how to hand them over, when the transfer isn't `Direct`.
    pixels: Option<(Transfer, Pixels)>,
    /// A chunk failed to decode; the frame is dropped when it ends.
    broken: bool,
}
impl Frame {
    /// Append the packets that show this frame in `slot`, then the delete of the other buffer
    /// once it is placed. A second call (a redraw) publishes the pixels again.
    pub(crate) fn encode(&mut self, view: Viewport, slot: usize, out: &mut Vec<u8>) -> io::Result<()> {
        let (width, height) = (self.width, self.height);
        if let Some((t, px)) = &mut self.pixels {
            let (medium, name) = match (*t, &mut px.buf) {
                #[cfg(unix)]
                (_, PixelBuf::Shm(b)) => (
                    "s",
                    match b.release() {
                        Some(name) => {
                            remember(Transfer::Shm, name.clone());
                            name
                        }
                        None => shm_publish(b.bytes())?,
                    },
                ),
                (Transfer::Shm, PixelBuf::Heap(d)) => ("s", shm_publish(d)?),
                (_, PixelBuf::Heap(d)) => ("t", file_publish(d)?),
            };
            let body = format!("a=T,f={},s={width},v={height},C=1,q=2,t={medium};{}", px.format, STANDARD.encode(name));
            encode_into(body.as_bytes(), view, slot, out)?;
        } else {
            for body in &self.bodies {
                encode_into(body, view, slot, out)?;
            }
        }
        out.extend_from_slice(delete_slot(1 - slot).as_bytes());
        Ok(())
    }
}

/// The pixel format, when this first packet carries plain base64 pixels that can be decoded.
fn decodable(body: &[u8]) -> Option<u32> {
    let header = std::str::from_utf8(body.split(|&b| b == b';').next()?).ok()?;
    let fields = || header.split(',');
    if fields().any(|p| p.starts_with("t=") && p != "t=d" || p.starts_with("o=")) {
        return None;
    }
    if fields().any(|p| p == "f=32") {
        Some(32)
    } else if fields().any(|p| p == "f=24") || !fields().any(|p| p.starts_with("f=")) {
        Some(24)
    } else {
        None
    }
}

#[cfg(test)]
fn frames(input: &mut impl BufRead, tx: &SyncSender<io::Result<Frame>>) -> io::Result<()> {
    frames_with(input, tx, Transfer::Direct)
}

fn push_frame(frame: &mut Option<Frame>, body: Vec<u8>, transfer: Transfer) -> io::Result<Option<Frame>> {
    let body = finish_small_frame(&body)?.unwrap_or(body);
    if body.starts_with(b"a=T,") {
        if frame.is_some() {
            return Err(io::Error::other("mpv started a frame before finishing its previous frame"));
        }
        let (width, height) = dimensions(&body)?;
        let pixels = match decodable(&body) {
            Some(format) if transfer != Transfer::Direct => {
                let len = width as usize * height as usize * (format / 8) as usize;
                if len > MAX_FRAME {
                    return Err(io::Error::other("mpv frame exceeded the bounded video surface"));
                }
                Some((transfer, Pixels::new(format, len, transfer)?))
            }
            _ => None,
        };
        *frame = Some(Frame { width, height, bodies: vec![], bytes: 0, pixels, broken: false });
    } else if !body.starts_with(b"m=") {
        return Ok(None);
    }
    let Some(f) = frame.as_mut() else { return Ok(None) };
    let split = body.iter().position(|&b| b == b';');
    let done = !body[..split.unwrap_or(body.len())].split(|&b| b == b',').any(|p| p == b"m=1");
    if let Some((_, px)) = &mut f.pixels {
        let payload = split.map_or(&[][..], |s| &body[s + 1..]);
        if !f.broken && !px.feed(payload) {
            f.broken = true;
        }
    } else {
        f.bytes += body.len();
        if f.bytes > MAX_FRAME / 3 * 4 + MAX_PACKET {
            return Err(io::Error::other("mpv frame exceeded the bounded video surface"));
        }
        f.bodies.push(body);
    }
    if !done {
        return Ok(None);
    }
    let f = frame.take().unwrap();
    // Pixels that don't add up to the declared frame are dropped, never drawn.
    if f.pixels.as_ref().is_some_and(|(_, px)| f.broken || !px.complete()) {
        return Ok(None);
    }
    Ok(Some(f))
}

fn frames_with(input: &mut impl BufRead, tx: &SyncSender<io::Result<Frame>>, transfer: Transfer) -> io::Result<()> {
    let mut frame = None;
    while let Some(unit) = unit(input)? {
        if let Some(body) = unit.graphics
            && let Some(f) = push_frame(&mut frame, body, transfer)?
            && tx.send(Ok(f)).is_err()
        {
            return Ok(());
        }
    }
    if frame.is_some() {
        return Err(io::Error::other("mpv stopped during a kitty frame"));
    }
    Ok(())
}

/// Kitty unlinks the name once it has opened the object. Wait for that handoff,
/// not a frame-count deadline: no later frame can revoke the name first.
#[cfg(unix)]
fn wait_consumed(name: &str) -> io::Result<()> {
    let c = std::ffi::CString::new(name).map_err(io::Error::other)?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let fd = unsafe { libc::shm_open(c.as_ptr(), libc::O_RDONLY, 0) };
        if fd < 0 {
            let error = io::Error::last_os_error();
            return if error.raw_os_error() == Some(libc::ENOENT) { Ok(()) } else { Err(error) };
        }
        unsafe { libc::close(fd) };
        if std::time::Instant::now() >= deadline {
            // Leave this name available to a delayed consumer, even on cancellation.
            return Err(io::Error::new(io::ErrorKind::TimedOut, "the terminal did not take the video frame within five seconds"));
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

#[cfg(not(unix))]
fn wait_consumed(_: &str) -> io::Result<()> {
    Err(io::Error::other("shared memory needs a unix system"))
}

/// Play in the foreground: mpv keeps the terminal, its key bindings and OSD, and this call
/// relays its frames to `output` until mpv exits. `mpv` is the full argv (program first,
/// files after `--`). Shared memory needs `Transfer::Shm`; anything else relays base64.
/// Each shm frame waits until the terminal has opened it, at most five seconds.
pub fn run(mpv: &[String], view: Viewport, options: crate::Options, output: &mut impl Write) -> io::Result<std::process::ExitStatus> {
    let args = argv(mpv, view, options.max_surface, false, None);
    let mut child = Command::new(&args[0]).args(&args[1..]).stdout(Stdio::piped()).spawn()?;
    let mut input = BufReader::new(child.stdout.take().unwrap());
    let result = relay(&mut input, view, output, options.transfer == Transfer::Shm);
    if result.is_err() {
        let _ = child.kill();
    }
    let status = child.wait()?;
    result?;
    Ok(status)
}

/// Writes are batched and flushed once a frame or a piece of ordinary output is complete,
/// not once per 4 KB chunk.
fn relay(input: &mut impl BufRead, view: Viewport, output: &mut impl Write, shm: bool) -> io::Result<()> {
    let mut output = io::BufWriter::with_capacity(1 << 18, output);
    let mut packet = Vec::with_capacity(MAX_PACKET + 256);
    let mut pending = None;
    let mut slot = 1;
    while let Some(unit) = unit(input)? {
        output.write_all(&unit.ordinary)?;
        let mut flush = true;
        if let Some(body) = unit.graphics {
            if shm && (body.starts_with(b"a=T,") || body.starts_with(b"m=")) {
                if let Some(mut frame) = push_frame(&mut pending, body, Transfer::Shm)? {
                    slot = 1 - slot;
                    let (cols, rows) = view.placement(frame.width, frame.height);
                    write!(output, "\x1b[{};{}H", (view.rows.saturating_sub(rows)) / 2 + 1, (view.columns.saturating_sub(cols)) / 2 + 1)?;
                    packet.clear();
                    if let Some((_, px)) = &mut frame.pixels {
                        // The relay waits for kitty to take the name, so it never joins `PUBLISHED`.
                        let name = match &mut px.buf {
                            #[cfg(unix)]
                            PixelBuf::Shm(b) => b.release(),
                            PixelBuf::Heap(_) => None,
                        };
                        let name = match name {
                            Some(name) => name,
                            None => shm_create(px.data())?,
                        };
                        let body = format!("a=T,f={},s={},v={},C=1,q=2,t=s;{}", px.format, frame.width, frame.height, STANDARD.encode(&name));
                        encode_into(body.as_bytes(), view, slot, &mut packet)?;
                        packet.extend_from_slice(delete_slot(1 - slot).as_bytes());
                        output.write_all(&packet)?;
                        output.flush()?;
                        wait_consumed(&name)?;
                    } else {
                        frame.encode(view, slot, &mut packet)?;
                        output.write_all(&packet)?;
                    }
                } else {
                    flush = false;
                }
            } else {
                if body.starts_with(b"a=T,") {
                    slot = 1 - slot;
                    let (w, h) = dimensions(&body)?;
                    let (cols, rows) = view.placement(w, h);
                    write!(output, "\x1b[{};{}H", (view.rows.saturating_sub(rows)) / 2 + 1, (view.columns.saturating_sub(cols)) / 2 + 1)?;
                }
                packet.clear();
                encode_into(&body, view, slot, &mut packet)?;
                output.write_all(&packet)?;
                let body = finish_small_frame(&body)?.unwrap_or(body);
                let header = body.split(|&b| b == b';').next().unwrap_or_default();
                if body.starts_with(b"a=T,") || body.starts_with(b"m=") {
                    if header.split(|&b| b == b',').any(|p| p == b"m=1") {
                        flush = false;
                    } else {
                        output.write_all(delete_slot(1 - slot).as_bytes())?;
                    }
                }
            }
        }
        if flush {
            output.flush()?;
        }
    }
    output.flush()?;
    if pending.is_some() {
        return Err(io::Error::other("mpv stopped during a kitty frame"));
    }
    Ok(())
}

/// Where playback is, as mpv reports it over IPC. Seconds; `duration` is 0 until mpv knows it.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Progress {
    pub pos: f64,
    pub duration: f64,
    pub paused: bool,
}
impl Progress {
    /// The IPC lines that subscribe to the properties `apply` reads.
    pub(crate) const OBSERVE: &str = concat!(
        r#"{"command":["observe_property",1,"time-pos"]}"#, "\n",
        r#"{"command":["observe_property",2,"duration"]}"#, "\n",
        r#"{"command":["observe_property",3,"pause"]}"#, "\n",
    );
    /// Fold one line of mpv's IPC output in. Other events and replies are ignored.
    pub(crate) fn apply(&mut self, line: &str) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { return };
        if v["event"] != "property-change" {
            return;
        }
        let data = &v["data"];
        match v["name"].as_str() {
            Some("time-pos") => self.pos = data.as_f64().unwrap_or(0.0),
            Some("duration") => self.duration = data.as_f64().unwrap_or(0.0),
            Some("pause") => self.paused = data.as_bool().unwrap_or(false),
            _ => {}
        }
    }
    /// The played fraction, 0–1.
    pub fn fraction(&self) -> f64 {
        if self.duration > 0.0 { (self.pos / self.duration).clamp(0.0, 1.0) } else { 0.0 }
    }
}

/// mpv embedded: frames arrive on `rx`, progress over its IPC socket.
#[cfg(unix)]
pub(crate) struct Session {
    pub(crate) rx: Option<Receiver<io::Result<Frame>>>,
    child: Child,
    reader: Option<JoinHandle<()>>,
    socket: std::path::PathBuf,
    progress: std::sync::Arc<std::sync::Mutex<Progress>>,
    observer: Option<JoinHandle<()>>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
}
#[cfg(unix)]
impl Session {
    pub(crate) fn start(mpv: &[String], view: Viewport, options: crate::Options) -> io::Result<Self> {
        let transfer = options.transfer;
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let socket = std::env::temp_dir().join(format!(
            "mpv-kitty-{}-{}.sock",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let args = argv(mpv, view, options.max_surface, true, Some(&socket));
        let mut child = Command::new(&args[0])
            .args(&args[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::sync_channel(1);
        let reader = std::thread::spawn(move || {
            if let Err(e) = frames_with(&mut BufReader::with_capacity(1 << 20, stdout), &tx, transfer) {
                let _ = tx.send(Err(e));
            }
        });
        let progress = std::sync::Arc::new(std::sync::Mutex::new(Progress::default()));
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observer = {
            let (socket, progress, stop) = (socket.clone(), progress.clone(), stop.clone());
            std::thread::spawn(move || observe(&socket, &progress, &stop))
        };
        Ok(Self {
            rx: Some(rx),
            child,
            reader: Some(reader),
            socket,
            progress,
            observer: Some(observer),
            stop,
        })
    }
    pub(crate) fn progress(&self) -> Progress {
        *self.progress.lock().unwrap_or_else(|e| e.into_inner())
    }
    /// Send one IPC command. mpv creates its socket shortly after it starts, so a command sent
    /// right after `start` waits up to a second for it.
    pub(crate) fn command(&self, command: serde_json::Value) -> io::Result<()> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        let mut socket = loop {
            match std::os::unix::net::UnixStream::connect(&self.socket) {
                Ok(socket) => break socket,
                Err(e)
                    if matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused)
                        && std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(10))
                }
                Err(e) => return Err(e),
            }
        };
        socket.set_write_timeout(Some(std::time::Duration::from_millis(50)))?;
        let line = serde_json::to_string(&serde_json::json!({"command": command})).map_err(io::Error::other)?;
        writeln!(socket, "{line}")
    }
    pub(crate) fn status(&mut self) -> io::Result<Option<std::process::ExitStatus>> {
        self.child.try_wait()
    }
}
/// Wait for mpv's IPC socket, subscribe to the position, and keep `progress` current until mpv exits.
#[cfg(unix)]
fn observe(socket: &std::path::Path, progress: &std::sync::Mutex<Progress>, stop: &std::sync::atomic::AtomicBool) {
    use std::sync::atomic::Ordering;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut conn = loop {
        if stop.load(Ordering::Relaxed) || std::time::Instant::now() > deadline {
            return;
        }
        match std::os::unix::net::UnixStream::connect(socket) {
            Ok(c) => break c,
            Err(_) => std::thread::sleep(std::time::Duration::from_millis(20)),
        }
    };
    if conn.write_all(Progress::OBSERVE.as_bytes()).is_err() {
        return;
    }
    for line in BufReader::new(conn).lines() {
        let Ok(line) = line else { return };
        progress.lock().unwrap_or_else(|e| e.into_inner()).apply(&line);
    }
}

#[cfg(unix)]
impl Drop for Session {
    fn drop(&mut self) {
        drop(self.rx.take());
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        // mpv's exit closes the socket, which ends the observer's read.
        if let Some(observer) = self.observer.take() {
            let _ = observer.join();
        }
        let _ = std::fs::remove_file(&self.socket);
        cleanup();
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    fn view() -> Viewport {
        Viewport {
            columns: 200,
            rows: 60,
            pixel_width: 3000,
            pixel_height: 1800,
        }
    }
    fn encode(body: &[u8], view: Viewport, slot: usize) -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        encode_into(body, view, slot, &mut out)?;
        Ok(out)
    }
    fn packet(body: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        push_packet(&mut out, body);
        out
    }
    fn encode_frame(frame: &mut Frame, slot: usize) -> String {
        let mut out = Vec::new();
        frame.encode(view(), slot, &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }
    const LOCAL: (u32, u32) = (1280, 720);

    #[test]
    fn cap_and_gpu_placement_preserve_aspect() {
        assert_eq!(view().render_size(LOCAL), (1200, 720));
        assert_eq!(view().placement(1200, 510), (200, 43));
        assert_eq!(
            Viewport {
                pixel_width: 640,
                pixel_height: 480,
                ..view()
            }
            .render_size(LOCAL),
            (640, 480)
        );
        let env = |k: &str| (k == "SSH_TTY").then(|| "/dev/ttys1".to_string());
        assert_eq!(view().render_size(crate::max_surface(&env)), (600, 360), "ssh gets a quarter of the pixels");
        assert_eq!(crate::max_surface(&|_: &str| None), LOCAL);
    }
    #[test]
    fn argv_keeps_files_and_audio_and_never_reuses_mpv_shm() {
        let base: Vec<String> = ["mpv", "--volume=50", "--", "a.webm", "b.gif"].map(String::from).to_vec();
        let args = argv(&base, view(), LOCAL, true, Some(std::path::Path::new("/tmp/mpv-kitty.sock")));
        assert!(args.contains(&"--vo=kitty".into()));
        assert!(args.contains(&"--vo-kitty-width=1200".into()));
        assert!(args.contains(&"--vo-kitty-use-shm=no".into()));
        assert!(args.contains(&"--input-ipc-server=/tmp/mpv-kitty.sock".into()));
        assert!(!args.contains(&"--no-audio".into()));
        assert_eq!(&args[args.len() - 3..], &["--", "a.webm", "b.gif"]);
        assert!(argv(&base, view(), LOCAL, false, None).contains(&"--vo-kitty-use-shm=no".into()));
    }
    #[test]
    fn chunks_and_id_are_stable_and_continuations_stay_unchanged() {
        let payload = STANDARD.encode(vec![71u8; 6000]);
        let first = format!("a=T,f=24,s=50,v=40,C=1,q=2,m=1;{}", &payload[..4096]);
        let last = format!("m=0;{}", &payload[4096..]);
        let first_encoded = encode(first.as_bytes(), view(), 0).unwrap();
        let last_encoded = encode(last.as_bytes(), view(), 0).unwrap();
        assert_eq!(last_encoded, packet(last.as_bytes()));
        let header = String::from_utf8(first_encoded.clone()).unwrap();
        assert!(header.contains(&format!("i={},p=1,c=150,r=60;", IMAGE_IDS[0])));
        assert_eq!(first_encoded, encode(first.as_bytes(), view(), 0).unwrap());
        assert_eq!(
            STANDARD
                .decode(format!("{}{}", &payload[..4096], &payload[4096..]))
                .unwrap(),
            vec![71u8; 6000]
        );
        let (tx, rx) = mpsc::sync_channel(1);
        frames(&mut Cursor::new([first_encoded, last_encoded].concat()), &tx).unwrap();
        let frame = rx.try_recv().unwrap().unwrap();
        assert_eq!((frame.width, frame.height), (50, 40));
        assert_eq!(frame.bodies.len(), 2);
    }
    #[test]
    fn frames_alternate_ids_and_free_the_previous_buffer_only_after_the_last_chunk() {
        let mut frame = Frame {
            width: 1,
            height: 1,
            bodies: vec![b"a=T,f=24,s=1,v=1,m=1;AAAA".to_vec(), b"m=0;".to_vec()],
            bytes: 0,
            pixels: None,
            broken: false,
        };
        for slot in [0, 1] {
            let text = encode_frame(&mut frame, slot);
            let shown = text.find(&format!("i={}", IMAGE_IDS[slot])).unwrap();
            let last = text.find("\x1b_Gm=0;").unwrap();
            let freed = text.find(&delete_slot(1 - slot)).unwrap();
            assert!(shown < last && last < freed);
            assert!(!text.contains(&delete_slot(slot)));
        }
    }
    #[test]
    fn a_single_small_rgb_packet_finishes_without_a_missing_continuation() {
        let body = b"a=T,f=24,s=1,v=1,C=1,q=2,m=1;AAAA";
        let encoded = encode(body, view(), 0).unwrap();
        assert!(String::from_utf8(encoded.clone()).unwrap().contains("m=0"));
        let (tx, rx) = mpsc::sync_channel(1);
        frames(&mut Cursor::new(packet(body)), &tx).unwrap();
        assert_eq!(rx.try_recv().unwrap().unwrap().bodies.len(), 1);
        assert!(finish_small_frame(b"a=T,f=24,s=2,v=2,m=1;AAAA").unwrap().is_none());
    }
    #[test]
    fn shm_has_leading_slash_exact_size_and_no_double_slash() {
        for name in ["mpv-kitty-1", "/mpv-kitty-1"] {
            let body = format!("a=T,t=s,f=24,s=1280,v=544,C=1,q=2,m=1;{}", STANDARD.encode(name));
            let encoded = encode(body.as_bytes(), view(), 0).unwrap();
            let text = String::from_utf8(encoded).unwrap();
            assert!(text.contains("S=2088960;"));
            let payload = text.split_once(';').unwrap().1.strip_suffix("\x1b\\").unwrap();
            assert_eq!(STANDARD.decode(payload).unwrap(), b"/mpv-kitty-1");
        }
    }
    /// Tests that publish and clean up share the global name list.
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    #[test]
    fn decoded_frames_go_out_as_one_small_packet_naming_the_pixels() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let pixels = vec![7u8; 4 * 3 * 3];
        let b64 = STANDARD.encode(&pixels);
        // Chunks that don't end on a base64 quad carry the remainder into the next one.
        let stream = format!("\x1b_Ga=T,f=24,s=4,v=3,C=1,q=2,m=1;{}\x1b\\\x1b_Gm=1;{}\x1b\\\x1b_Gm=0;{}\x1b\\", &b64[..7], &b64[7..9], &b64[9..]);
        let (tx, rx) = mpsc::sync_channel(1);
        frames_with(&mut Cursor::new(stream.into_bytes()), &tx, Transfer::File).unwrap();
        let mut frame = rx.try_recv().unwrap().unwrap();
        let text = encode_frame(&mut frame, 0);
        assert!(text.starts_with("\x1b_Ga=T,f=24,s=4,v=3,C=1,q=2,t=t,"), "{text}");
        assert!(text.len() < 400, "no pixels in the escape");
        let name = STANDARD.decode(text.split_once(';').unwrap().1.split_once('\x1b').unwrap().0).unwrap();
        let path = String::from_utf8(name).unwrap();
        assert!(path.contains("tty-graphics-protocol"));
        assert_eq!(std::fs::read(&path).unwrap(), pixels);
        cleanup();
        assert!(!std::path::Path::new(&path).exists(), "cleanup removes what kitty never read");
        // Pixels that fall short of the declared size, or aren't base64, are never drawn.
        let short = format!("\x1b_Ga=T,f=24,s=9,v=9,C=1,q=2,m=0;{b64}\x1b\\");
        frames_with(&mut Cursor::new(short.into_bytes()), &tx, Transfer::File).unwrap();
        assert!(rx.try_recv().is_err());
        let junk = "\x1b_Ga=T,f=24,s=4,v=3,C=1,q=2,m=0;!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!\x1b\\";
        frames_with(&mut Cursor::new(junk.as_bytes()), &tx, Transfer::File).unwrap();
        assert!(rx.try_recv().is_err());
    }
    #[cfg(unix)]
    #[test]
    fn shm_frames_decode_into_their_own_object_and_publish_it_without_a_copy() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        // RGBA 1×1 is 4 bytes, so its base64 ends in a padded quad, split across chunks.
        let b64 = STANDARD.encode([1u8, 2, 3, 4]);
        assert!(b64.ends_with("=="));
        let stream = format!("\x1b_Ga=T,f=32,s=1,v=1,C=1,q=2,m=1;{}\x1b\\\x1b_Gm=0;{}\x1b\\", &b64[..3], &b64[3..]);
        let (tx, rx) = mpsc::sync_channel(1);
        frames_with(&mut Cursor::new(stream.into_bytes()), &tx, Transfer::Shm).unwrap();
        let mut frame = rx.try_recv().unwrap().unwrap();
        let own = match &frame.pixels.as_ref().unwrap().1.buf {
            PixelBuf::Shm(b) => b.name.clone(),
            PixelBuf::Heap(_) => panic!("shm frames decode into shared memory"),
        };
        let read = |name: &str| {
            let c = std::ffi::CString::new(name).unwrap();
            let fd = unsafe { libc::shm_open(c.as_ptr(), libc::O_RDONLY, 0) };
            assert!(fd >= 0, "{name} exists");
            let p = unsafe { libc::mmap(std::ptr::null_mut(), 4, libc::PROT_READ, libc::MAP_SHARED, fd, 0) };
            let bytes = unsafe { std::slice::from_raw_parts(p.cast::<u8>(), 4) }.to_vec();
            unsafe {
                libc::munmap(p, 4);
                libc::close(fd);
            }
            bytes
        };
        let name_of = |text: &str| String::from_utf8(STANDARD.decode(text.split_once(';').unwrap().1.split_once('\x1b').unwrap().0).unwrap()).unwrap();
        let first = name_of(&encode_frame(&mut frame, 0));
        assert_eq!(first, own, "the first draw names the object the pixels were decoded into");
        assert_eq!(read(&first), [1, 2, 3, 4]);
        // A redraw after the terminal took (and unlinked) the first name copies into a new one.
        unlink(Transfer::Shm, &first);
        let second = name_of(&encode_frame(&mut frame, 1));
        assert_ne!(second, first);
        assert_eq!(read(&second), [1, 2, 3, 4]);
        drop(frame);
        cleanup();
        // A frame dropped before it was drawn unlinks its own object.
        let stream = format!("\x1b_Ga=T,f=32,s=1,v=1,C=1,q=2,m=0;{b64}\x1b\\");
        frames_with(&mut Cursor::new(stream.into_bytes()), &tx, Transfer::Shm).unwrap();
        let frame = rx.try_recv().unwrap().unwrap();
        let PixelBuf::Shm(b) = &frame.pixels.as_ref().unwrap().1.buf else { unreachable!() };
        let c = std::ffi::CString::new(b.name.clone()).unwrap();
        drop(frame);
        assert!(unsafe { libc::shm_open(c.as_ptr(), libc::O_RDONLY, 0) } < 0, "unlinked on drop");
    }
    #[cfg(unix)]
    #[test]
    fn shm_objects_hold_the_pixels_and_cleanup_unlinks_them() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let name = shm_publish(&[1, 2, 3, 4, 5, 6]).unwrap();
        assert!(name.len() <= 31 && name.starts_with('/'));
        let c = std::ffi::CString::new(name.clone()).unwrap();
        let fd = unsafe { libc::shm_open(c.as_ptr(), libc::O_RDONLY, 0) };
        assert!(fd >= 0, "object exists");
        let p = unsafe { libc::mmap(std::ptr::null_mut(), 6, libc::PROT_READ, libc::MAP_SHARED, fd, 0) };
        assert_eq!(unsafe { std::slice::from_raw_parts(p.cast::<u8>(), 6) }, &[1, 2, 3, 4, 5, 6]);
        unsafe {
            libc::munmap(p, 6);
            libc::close(fd);
        }
        cleanup();
        assert!(unsafe { libc::shm_open(c.as_ptr(), libc::O_RDONLY, 0) } < 0, "unlinked");
    }
    #[cfg(unix)]
    #[test]
    fn foreground_shm_waits_for_delayed_consumers_and_never_reuses_a_name() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        struct Terminal {
            pending: Vec<u8>,
            all: Vec<u8>,
            frames: std::sync::mpsc::Sender<String>,
        }
        impl Write for Terminal {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.pending.extend_from_slice(bytes);
                self.all.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                let mut input = Cursor::new(std::mem::take(&mut self.pending));
                while let Some(unit) = unit(&mut input)? {
                    if let Some(body) = unit.graphics && body.starts_with(b"a=T,") {
                        let split = body.iter().position(|&b| b == b';').unwrap();
                        assert!(body[..split].split(|&b| b == b',').any(|p| p == b"t=s"));
                        let name = String::from_utf8(STANDARD.decode(&body[split + 1..]).unwrap()).unwrap();
                        self.frames.send(name).unwrap();
                    }
                }
                Ok(())
            }
        }
        let (tx, rx) = mpsc::channel::<String>();
        let consumer = std::thread::spawn(move || {
            let mut names = std::collections::HashSet::new();
            for (index, name) in rx.into_iter().enumerate() {
                assert!(names.insert(name.clone()), "each frame has its own object");
                std::thread::sleep(std::time::Duration::from_millis(2));
                cleanup();
                let c = std::ffi::CString::new(name).unwrap();
                let fd = unsafe { libc::shm_open(c.as_ptr(), libc::O_RDONLY, 0) };
                assert!(fd >= 0, "delayed terminal can still open the frame");
                let p = unsafe { libc::mmap(std::ptr::null_mut(), 16 * 16 * 3, libc::PROT_READ, libc::MAP_SHARED, fd, 0) };
                assert_ne!(p, libc::MAP_FAILED);
                // Kitty unlinks after opening; the independent mapping remains valid.
                unsafe { libc::shm_unlink(c.as_ptr()) };
                assert!(unsafe { std::slice::from_raw_parts(p.cast::<u8>(), 16 * 16 * 3) }.iter().all(|&b| b == index as u8));
                unsafe {
                    libc::munmap(p, 16 * 16 * 3);
                    libc::close(fd);
                }
            }
            assert_eq!(names.len(), 40);
        });
        let mut stream = b"hello\x1b[H".to_vec();
        for index in 0..40 {
            let payload = STANDARD.encode(vec![index; 16 * 16 * 3]);
            stream.extend(packet(format!("a=T,f=24,s=16,v=16,C=1,q=2,m=0;{payload}").as_bytes()));
        }
        stream.extend(b"bye");
        let mut output = Terminal { pending: vec![], all: vec![], frames: tx };
        relay(&mut Cursor::new(stream), view(), &mut output, true).unwrap();
        assert!(output.all.starts_with(b"hello\x1b[H"));
        assert!(output.all.ends_with(b"bye"));
        assert!(output.all.len() < 40 * 400, "the terminal receives names, not pixels");
        drop(output);
        consumer.join().unwrap();
    }
    #[test]
    fn ordinary_terminal_output_survives_the_relay_parser() {
        let stream = b"hello\x1b[H\x1b_Ga=d;\x1b\\bye";
        let mut input = Cursor::new(stream);
        let mut out = Vec::new();
        relay(&mut input, view(), &mut out, false).unwrap();
        assert_eq!(out, stream);
    }
    #[test]
    fn mpv_041_cleanup_without_st_is_repaired_and_cursor_restoration_survives() {
        let mut input = Cursor::new(b"\x1b_Ga=d;\x1b[?25h");
        let unit = unit(&mut input).unwrap().unwrap();
        assert!(unit.graphics.is_none());
        assert_eq!(unit.ordinary, b"\x1b_Ga=d;\x1b\\\x1b[");
        let tail = self::unit(&mut input).unwrap().unwrap();
        assert_eq!(tail.ordinary, b"?25h");
    }
    #[test]
    fn malformed_and_incomplete_frames_fail_without_unbounded_buffering() {
        assert!(encode(b"a=T,f=24,s=0,v=5;AAAA", view(), 0).is_err());
        assert!(unit(&mut Cursor::new(b"\x1b_Gunterminated")).is_err());
        let long = [b"\x1b_G".as_slice(), &vec![b'A'; MAX_PACKET + 1], b"\x1b\\"].concat();
        assert!(unit(&mut Cursor::new(long)).is_err());
        let (tx, _rx) = mpsc::sync_channel(1);
        assert!(frames(&mut Cursor::new(b"\x1b_Ga=T,f=24,s=50,v=40,m=1;AAAA\x1b\\"), &tx).is_err());
    }
    #[cfg(unix)]
    #[test]
    fn session_drop_unblocks_a_full_frame_queue_and_reaps_the_child() {
        let (tx, rx) = mpsc::sync_channel(1);
        let reader = std::thread::spawn(move || {
            let mut data = Cursor::new(b"\x1b_Ga=T,f=24,s=1,v=1,m=0;AAAA\x1b\\\x1b_Ga=T,f=24,s=1,v=1,m=0;AAAA\x1b\\");
            frames(&mut data, &tx).unwrap();
        });
        let child = Command::new("sh").args(["-c", "exec sleep 30"]).spawn().unwrap();
        let session = Session {
            rx: Some(rx),
            child,
            reader: Some(reader),
            socket: std::env::temp_dir().join(format!("mpv-kitty-nonexistent-{}", std::process::id())),
            progress: Default::default(),
            observer: None,
            stop: Default::default(),
        };
        drop(session);
    }
    #[test]
    fn progress_follows_mpvs_property_changes() {
        let mut p = Progress::default();
        for line in [
            r#"{"request_id":0,"error":"success"}"#,
            r#"{"event":"property-change","id":2,"name":"duration","data":34.5}"#,
            r#"{"event":"property-change","id":1,"name":"time-pos","data":12.25}"#,
            r#"{"event":"property-change","id":3,"name":"pause","data":true}"#,
            r#"{"event":"playback-restart"}"#,
            "not json",
        ] {
            p.apply(line);
        }
        assert_eq!(p, Progress { pos: 12.25, duration: 34.5, paused: true });
        assert!((p.fraction() - 12.25 / 34.5).abs() < 1e-9);
        // A new playlist file reports no time yet.
        p.apply(r#"{"event":"property-change","id":1,"name":"time-pos"}"#);
        assert_eq!(p.pos, 0.0);
        assert_eq!(Progress::default().fraction(), 0.0);
    }
}

