//! A tiny X11 client for end-to-end tests and demos of the Linux backend.
//!
//! ```text
//! cargo run -p screensense-platform --example demo_window -- \
//!     --title "cargo test - demo" --class demo-term --seconds 20 \
//!     --line "error[E0308]: mismatched types" --line "  expected `u32`, found `&str`"
//! ```
//!
//! It opens a ~900x500 white window with black text, sets the EWMH/ICCCM
//! properties a window manager would read, takes input focus, and exits after
//! `--seconds`.
//!
//! Options:
//!
//! * `--title <s>`       window title (`_NET_WM_NAME` and `WM_NAME`)
//! * `--class <s>`       `WM_CLASS` class part (default `demo-term`)
//! * `--instance <s>`    `WM_CLASS` instance part (default `demo_window`)
//! * `--seconds <n>`     lifetime (default 20)
//! * `--line <s>`        a line of text, repeatable
//! * `--title-file <p>`  re-read the title from this file every 500 ms
//! * `--text-file <p>`   re-read the lines from this file every 500 ms
//! * `--scale <n>`       draw text `n` times larger (default: automatic, see below)
//!
//! Text is drawn with a core X font. If the server only has small fonts the
//! glyphs are magnified with a nearest-neighbour blit so that OCR has
//! something to read; the chosen font and scale are printed to stderr.

#[cfg(target_os = "linux")]
fn main() {
    if let Err(e) = imp::run() {
        eprintln!("demo_window: {e}");
        std::process::exit(1);
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("demo_window is only available on Linux (X11)");
}

#[cfg(target_os = "linux")]
mod imp {
    use std::time::{Duration, Instant};

    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{
        AtomEnum, ChangeGCAux, ConnectionExt as _, CreateGCAux, CreateWindowAux, EventMask,
        ImageFormat, PropMode, Rectangle, Window, WindowClass,
    };
    use x11rb::protocol::Event;
    use x11rb::rust_connection::RustConnection;
    use x11rb::wrapper::ConnectionExt as _;
    use x11rb::CURRENT_TIME;

    const WIN_W: u16 = 900;
    const WIN_H: u16 = 500;
    const MARGIN: i16 = 16;
    /// Target glyph height in pixels for readable OCR.
    const TARGET_CELL_H: i32 = 20;

    struct Args {
        title: String,
        class: String,
        instance: String,
        seconds: u64,
        lines: Vec<String>,
        title_file: Option<String>,
        text_file: Option<String>,
        scale: Option<u16>,
    }

    fn parse_args() -> Result<Args, String> {
        let mut a = Args {
            title: "demo_window".to_string(),
            class: "demo-term".to_string(),
            instance: "demo_window".to_string(),
            seconds: 20,
            lines: Vec::new(),
            title_file: None,
            text_file: None,
            scale: None,
        };
        let mut it = std::env::args().skip(1);
        while let Some(flag) = it.next() {
            let mut val = |name: &str| it.next().ok_or_else(|| format!("{name} needs a value"));
            match flag.as_str() {
                "--title" => a.title = val("--title")?,
                "--class" => a.class = val("--class")?,
                "--instance" => a.instance = val("--instance")?,
                "--seconds" => {
                    a.seconds = val("--seconds")?
                        .parse()
                        .map_err(|e| format!("--seconds: {e}"))?
                }
                "--line" => a.lines.push(val("--line")?),
                "--title-file" => a.title_file = Some(val("--title-file")?),
                "--text-file" => a.text_file = Some(val("--text-file")?),
                "--scale" => {
                    a.scale = Some(
                        val("--scale")?
                            .parse()
                            .map_err(|e| format!("--scale: {e}"))?,
                    )
                }
                other => return Err(format!("unknown argument {other}")),
            }
        }
        Ok(a)
    }

    /// Pixel height from an XLFD name (`-misc-fixed-medium-r-normal--20-...`).
    fn xlfd_pixel_size(name: &str) -> Option<u32> {
        name.split('-').nth(7)?.parse().ok()
    }

    struct Font {
        id: u32,
        name: String,
        ascent: i32,
        descent: i32,
        char_w: i32,
    }

    fn pick_font(conn: &RustConnection) -> Result<Font, String> {
        let patterns = [
            "-misc-fixed-medium-r-normal--20-*-iso8859-1",
            "-*-fixed-*-*-*-*-20-*",
            "10x20",
            "9x15",
            "-*-*-medium-r-normal-*-20-*",
        ];
        let mut name: Option<String> = None;
        for pat in patterns {
            let r = conn
                .list_fonts(50, pat.as_bytes())
                .map_err(|e| e.to_string())?
                .reply()
                .map_err(|e| e.to_string())?;
            if let Some(f) = r.names.first() {
                name = Some(String::from_utf8_lossy(&f.name).into_owned());
                break;
            }
        }
        if name.is_none() {
            // Any font: the largest pixel size we can parse, else the first.
            let r = conn
                .list_fonts(1000, b"*")
                .map_err(|e| e.to_string())?
                .reply()
                .map_err(|e| e.to_string())?;
            let all: Vec<String> = r
                .names
                .iter()
                .map(|f| String::from_utf8_lossy(&f.name).into_owned())
                .collect();
            name = all
                .iter()
                .filter(|n| !n.contains("cursor"))
                .max_by_key(|n| xlfd_pixel_size(n).unwrap_or(0))
                .or_else(|| all.first())
                .cloned();
        }
        let name = name.ok_or("the X server has no fonts at all")?;
        let id = conn.generate_id().map_err(|e| e.to_string())?;
        conn.open_font(id, name.as_bytes())
            .map_err(|e| e.to_string())?
            .check()
            .map_err(|e| format!("open_font {name}: {e}"))?;
        let q = conn
            .query_font(id)
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| e.to_string())?;
        let char_w = i32::from(q.max_bounds.character_width.max(1));
        Ok(Font {
            id,
            name,
            ascent: i32::from(q.font_ascent),
            descent: i32::from(q.font_descent),
            char_w,
        })
    }

    struct Ui {
        conn: RustConnection,
        win: Window,
        root: Window,
        depth: u8,
        white: u32,
        black: u32,
        gc: u32,
        font: Font,
        scale: u16,
        net_wm_name: u32,
        utf8: u32,
        wm_protocols: u32,
        wm_delete: u32,
    }

    impl Ui {
        fn set_title(&self, title: &str) -> Result<(), String> {
            let e = |e: x11rb::errors::ConnectionError| e.to_string();
            self.conn
                .change_property8(
                    PropMode::REPLACE,
                    self.win,
                    self.net_wm_name,
                    self.utf8,
                    title.as_bytes(),
                )
                .map_err(e)?;
            let latin1: Vec<u8> = title
                .chars()
                .map(|c| u8::try_from(u32::from(c)).unwrap_or(b'?'))
                .collect();
            self.conn
                .change_property8(
                    PropMode::REPLACE,
                    self.win,
                    AtomEnum::WM_NAME,
                    AtomEnum::STRING,
                    &latin1,
                )
                .map_err(e)?;
            self.conn.flush().map_err(|e| e.to_string())
        }

        fn draw(&self, lines: &[String]) -> Result<(), String> {
            let e = |e: x11rb::errors::ConnectionError| e.to_string();
            self.conn
                .clear_area(false, self.win, 0, 0, 0, 0)
                .map_err(e)?;
            let s = i32::from(self.scale);
            let line_h = (self.font.ascent + self.font.descent + 4) * s;
            for (i, line) in lines.iter().enumerate() {
                let y_base = i32::from(MARGIN) + i as i32 * line_h + self.font.ascent * s;
                if y_base > i32::from(WIN_H) {
                    break;
                }
                let bytes: Vec<u8> = line
                    .chars()
                    .map(|c| u8::try_from(u32::from(c)).unwrap_or(b'?'))
                    .take(255)
                    .collect();
                if self.scale <= 1 {
                    self.conn
                        .image_text8(self.win, self.gc, MARGIN, y_base as i16, &bytes)
                        .map_err(e)?;
                } else {
                    self.draw_scaled(&bytes, i32::from(MARGIN), y_base)?;
                }
            }
            self.conn.flush().map_err(|e| e.to_string())
        }

        /// Render `bytes` at native size into a 1-bit-like pixmap, read it back
        /// and blit every set pixel as an `scale`x`scale` rectangle.
        fn draw_scaled(&self, bytes: &[u8], x: i32, y_base: i32) -> Result<(), String> {
            let cw = self.font.char_w;
            let w = (cw * bytes.len() as i32).clamp(1, 4000) as u16;
            let h = (self.font.ascent + self.font.descent).max(1) as u16;
            let pm = self.conn.generate_id().map_err(|e| e.to_string())?;
            self.conn
                .create_pixmap(self.depth, pm, self.root, w, h)
                .map_err(|e| e.to_string())?;
            let gc = self.conn.generate_id().map_err(|e| e.to_string())?;
            self.conn
                .create_gc(
                    gc,
                    pm,
                    &CreateGCAux::new()
                        .foreground(self.white)
                        .background(self.white)
                        .font(self.font.id),
                )
                .map_err(|e| e.to_string())?;
            self.conn
                .poly_fill_rectangle(
                    pm,
                    gc,
                    &[Rectangle {
                        x: 0,
                        y: 0,
                        width: w,
                        height: h,
                    }],
                )
                .map_err(|e| e.to_string())?;
            self.conn
                .change_gc(gc, &ChangeGCAux::new().foreground(self.black))
                .map_err(|e| e.to_string())?;
            self.conn
                .image_text8(pm, gc, 0, self.font.ascent as i16, bytes)
                .map_err(|e| e.to_string())?;
            let img = self
                .conn
                .get_image(ImageFormat::Z_PIXMAP, pm, 0, 0, w, h, !0)
                .map_err(|e| e.to_string())?
                .reply()
                .map_err(|e| e.to_string());
            let _ = self.conn.free_gc(gc);
            let _ = self.conn.free_pixmap(pm);
            let img = img?;
            // Server pixel size: 4 bytes for a 24/32-bit screen. A pixel is "ink"
            // when it is not white.
            let bpp_bytes = 4usize;
            let stride = (usize::from(w) * bpp_bytes + 3) & !3;
            let s = i32::from(self.scale);
            let mut rects: Vec<Rectangle> = Vec::new();
            for row in 0..usize::from(h) {
                let mut col = 0usize;
                while col < usize::from(w) {
                    let off = row * stride + col * bpp_bytes;
                    let ink = img
                        .data
                        .get(off..off + 3)
                        .map(|p| p.iter().map(|&v| u32::from(v)).sum::<u32>() < 3 * 128)
                        .unwrap_or(false);
                    if !ink {
                        col += 1;
                        continue;
                    }
                    let start = col;
                    while col < usize::from(w) {
                        let o = row * stride + col * bpp_bytes;
                        let on = img
                            .data
                            .get(o..o + 3)
                            .map(|p| p.iter().map(|&v| u32::from(v)).sum::<u32>() < 3 * 128)
                            .unwrap_or(false);
                        if !on {
                            break;
                        }
                        col += 1;
                    }
                    rects.push(Rectangle {
                        x: (x + start as i32 * s) as i16,
                        y: (y_base - self.font.ascent * s + row as i32 * s) as i16,
                        width: ((col - start) as i32 * s) as u16,
                        height: s as u16,
                    });
                }
            }
            for chunk in rects.chunks(2000) {
                self.conn
                    .poly_fill_rectangle(self.win, self.gc, chunk)
                    .map_err(|e| e.to_string())?;
            }
            Ok(())
        }
    }

    fn read_trimmed(path: &str) -> Option<String> {
        std::fs::read_to_string(path)
            .ok()
            .map(|s| s.trim_end_matches(['\n', '\r']).to_string())
    }

    pub fn run() -> Result<(), String> {
        let args = parse_args()?;
        let (conn, screen_num) =
            x11rb::connect(None).map_err(|e| format!("cannot connect: {e}"))?;
        let screen = conn.setup().roots[screen_num].clone();
        let font = pick_font(&conn)?;

        let atom = |name: &str| -> Result<u32, String> {
            conn.intern_atom(false, name.as_bytes())
                .map_err(|e| e.to_string())?
                .reply()
                .map(|r| r.atom)
                .map_err(|e| e.to_string())
        };
        let net_wm_name = atom("_NET_WM_NAME")?;
        let utf8 = atom("UTF8_STRING")?;
        let net_wm_pid = atom("_NET_WM_PID")?;
        let wm_protocols = atom("WM_PROTOCOLS")?;
        let wm_delete = atom("WM_DELETE_WINDOW")?;

        let cell_h = font.ascent + font.descent;
        let scale = args
            .scale
            .unwrap_or_else(|| ((TARGET_CELL_H + cell_h - 1) / cell_h.max(1)).clamp(1, 4) as u16)
            .max(1);
        eprintln!(
            "demo_window: font {} (cell {}x{}), scale {}",
            font.name, font.char_w, cell_h, scale
        );

        let win = conn.generate_id().map_err(|e| e.to_string())?;
        conn.create_window(
            screen.root_depth,
            win,
            screen.root,
            40,
            40,
            WIN_W,
            WIN_H,
            0,
            WindowClass::INPUT_OUTPUT,
            screen.root_visual,
            &CreateWindowAux::new()
                .background_pixel(screen.white_pixel)
                .event_mask(EventMask::EXPOSURE | EventMask::STRUCTURE_NOTIFY),
        )
        .map_err(|e| e.to_string())?;

        let gc = conn.generate_id().map_err(|e| e.to_string())?;
        conn.create_gc(
            gc,
            win,
            &CreateGCAux::new()
                .foreground(screen.black_pixel)
                .background(screen.white_pixel)
                .font(font.id),
        )
        .map_err(|e| e.to_string())?;

        let ui = Ui {
            conn,
            win,
            root: screen.root,
            depth: screen.root_depth,
            white: screen.white_pixel,
            black: screen.black_pixel,
            gc,
            font,
            scale,
            net_wm_name,
            utf8,
            wm_protocols,
            wm_delete,
        };

        let mut title = args
            .title_file
            .as_deref()
            .and_then(read_trimmed)
            .unwrap_or(args.title);
        ui.set_title(&title)?;
        let wm_class = format!("{}\0{}\0", args.instance, args.class);
        ui.conn
            .change_property8(
                PropMode::REPLACE,
                win,
                AtomEnum::WM_CLASS,
                AtomEnum::STRING,
                wm_class.as_bytes(),
            )
            .map_err(|e| e.to_string())?;
        ui.conn
            .change_property32(
                PropMode::REPLACE,
                win,
                net_wm_pid,
                AtomEnum::CARDINAL,
                &[std::process::id()],
            )
            .map_err(|e| e.to_string())?;
        ui.conn
            .change_property32(
                PropMode::REPLACE,
                win,
                wm_protocols,
                AtomEnum::ATOM,
                &[wm_delete],
            )
            .map_err(|e| e.to_string())?;
        ui.conn.map_window(win).map_err(|e| e.to_string())?;
        ui.conn.flush().map_err(|e| e.to_string())?;

        let mut lines: Vec<String> = match args.text_file.as_deref().and_then(read_trimmed) {
            Some(t) => t.lines().map(str::to_string).collect(),
            None => args.lines,
        };

        let deadline = Instant::now() + Duration::from_secs(args.seconds);
        let mut next_poll = Instant::now() + Duration::from_millis(500);
        let mut focused = false;
        eprintln!("demo_window: pid {} window 0x{:x}", std::process::id(), win);
        while Instant::now() < deadline {
            while let Some(ev) = ui.conn.poll_for_event().map_err(|e| e.to_string())? {
                match ev {
                    Event::Expose(e) if e.count == 0 => {
                        ui.draw(&lines)?;
                        if !focused {
                            // Only a viewable window can take focus (BadMatch otherwise).
                            // A failure is not fatal: a window manager may refuse.
                            let r = ui
                                .conn
                                .set_input_focus(
                                    x11rb::protocol::xproto::InputFocus::PARENT,
                                    win,
                                    CURRENT_TIME,
                                )
                                .map_err(|e| e.to_string())?
                                .check();
                            if let Err(e) = r {
                                eprintln!("demo_window: set_input_focus failed: {e}");
                            }
                            focused = true;
                        }
                    }
                    Event::ClientMessage(m) if m.type_ == ui.wm_protocols => {
                        if m.data.as_data32()[0] == ui.wm_delete {
                            return Ok(());
                        }
                    }
                    Event::DestroyNotify(_) => return Ok(()),
                    _ => {}
                }
            }
            if Instant::now() >= next_poll {
                next_poll = Instant::now() + Duration::from_millis(500);
                if let Some(t) = args.title_file.as_deref().and_then(read_trimmed) {
                    if t != title {
                        title = t;
                        ui.set_title(&title)?;
                    }
                }
                if let Some(t) = args.text_file.as_deref().and_then(read_trimmed) {
                    let new: Vec<String> = t.lines().map(str::to_string).collect();
                    if new != lines {
                        lines = new;
                        ui.draw(&lines)?;
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        // Tidy exit: let the server see the window go away before the process does.
        let _ = ui.conn.destroy_window(win);
        let _ = ui.conn.flush();
        Ok(())
    }
}
