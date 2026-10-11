//! OCR through the `tesseract` command-line program.
//!
//! No C dependency: an in-memory PPM is piped to tesseract's stdin. It returns
//! TSV word boxes on stdout. Unredacted pixels are never written to a temporary file.
//!
//! Page segmentation mode 6 ("a single uniform block of text") is used: on
//! the test material it recognised as many words as modes 3, 4 and 11 (within
//! one or two words out of 64) while keeping each screen row as one line in
//! top-to-bottom order, which is what the error detector and the privacy
//! scanner read. Mode 11 (sparse text) split rows such as `12 | let x`
//! into fragments; mode 3 reorders by block and lost a row in the editor
//! sample. The price of mode 6 is that a sidebar next to code ends up on the
//! same line as the code, left to right.
//!
//! Small images are upscaled 2x (bilinear) first: terminal text rendered at
//! 10-14 px reads far better that way (about 80% to 90% mean word confidence,
//! and words that were missed or mangled come out right). Boxes are divided
//! back, so they are always in pixels of the image that was passed in.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::{Image, OcrEngine, OcrLine, OcrWord, PlatformError, Result};

const NOT_FOUND: &str =
    "tesseract not found on PATH (install tesseract-ocr to enable text recognition)";
/// Hard limit for one recognition.
const OCR_TIMEOUT: Duration = Duration::from_secs(20);
/// Words below this confidence retain their geometry but no text, for pixel masking.
const MIN_CONF: f64 = 30.0;
/// Images narrower than this are upscaled 2x before OCR.
const UPSCALE_BELOW_WIDTH: u32 = 1000;
/// Never upscale an image with more source pixels than this (memory bound).
const UPSCALE_MAX_PIXELS: u64 = 4_000_000;

pub fn create(language: &str) -> std::result::Result<Box<dyn OcrEngine>, String> {
    let exe = crate::x11::find_in_path("tesseract").ok_or_else(|| NOT_FOUND.to_string())?;
    let mut cmd = Command::new(&exe);
    cmd.arg("--list-langs");
    let out = run_capture(cmd, Duration::from_secs(10))
        .map_err(|e| format!("cannot run tesseract --list-langs: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "tesseract --list-langs failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    // tesseract 5 prints the list on stdout, 4.x on stderr.
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push('\n');
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    let installed = parse_list_langs(&text);
    let lang = pick_languages(language, &installed)?;
    Ok(Box::new(Tesseract { exe, lang }))
}

struct Tesseract {
    exe: PathBuf,
    /// Validated `-l` argument, e.g. `eng` or `eng+deu`.
    lang: String,
}

impl OcrEngine for Tesseract {
    fn name(&self) -> String {
        format!("tesseract({})", self.lang)
    }

    fn recognize(&mut self, image: &Image) -> Result<Vec<OcrLine>> {
        let (w, h) = (image.width as usize, image.height as usize);
        if w == 0 || h == 0 {
            return Ok(Vec::new());
        }
        if image.rgba.len() != w * h * 4 {
            return Err(PlatformError::Os(format!(
                "image buffer is {} bytes, expected {} for {}x{}",
                image.rgba.len(),
                w * h * 4,
                w,
                h
            )));
        }

        let upscale = image.width < UPSCALE_BELOW_WIDTH && (w * h) as u64 <= UPSCALE_MAX_PIXELS;
        let (scale, pw, ph, rgb) = if upscale {
            (2u32, w * 2, h * 2, upscale2x_rgb(&image.rgba, w, h))
        } else {
            (1u32, w, h, rgba_to_rgb(&image.rgba))
        };

        let data = ppm_bytes(pw, ph, &rgb);
        let mut cmd = Command::new(&self.exe);
        cmd.arg("stdin")
            .arg("stdout")
            .args(["-l", &self.lang, "--psm", "6", "tsv"])
            // One OCR thread: recognition must not hog every core of a dev machine.
            .env("OMP_THREAD_LIMIT", "1");
        let out = run_capture_input(cmd, OCR_TIMEOUT, Some(data))
            .map_err(|e| PlatformError::Os(format!("tesseract: {e}")))?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            let err: String = err.trim().chars().take(300).collect();
            return Err(PlatformError::Os(format!(
                "tesseract failed ({}): {err}",
                out.status
            )));
        }
        Ok(parse_tsv(&String::from_utf8_lossy(&out.stdout), scale))
    }
}

// ---------------------------------------------------------------------------
// Language selection
// ---------------------------------------------------------------------------

/// Language names from `tesseract --list-langs` output (header line skipped).
fn parse_list_langs(output: &str) -> Vec<String> {
    let mut langs: Vec<String> = Vec::new();
    for line in output.lines() {
        let l = line.trim();
        if l.is_empty() || l.starts_with("List of available") {
            continue;
        }
        let ok = l
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '/'));
        if ok && !langs.iter().any(|x| x == l) {
            langs.push(l.to_string());
        }
    }
    langs
}

/// Turn a requested spec (`eng+deu`, empty for the default) into the `-l`
/// argument, dropping parts that are not installed.
fn pick_languages(requested: &str, installed: &[String]) -> std::result::Result<String, String> {
    let requested = if requested.trim().is_empty() {
        "eng"
    } else {
        requested.trim()
    };
    // `osd` is orientation data, not a recognition language.
    let usable = |l: &str| l != "osd" && installed.iter().any(|i| i == l);
    let mut keep: Vec<&str> = Vec::new();
    let mut missing: Vec<&str> = Vec::new();
    for part in requested
        .split('+')
        .map(str::trim)
        .filter(|p| !p.is_empty())
    {
        if usable(part) {
            if !keep.contains(&part) {
                keep.push(part);
            }
        } else {
            missing.push(part);
        }
    }
    if keep.is_empty() {
        let have: Vec<&str> = installed
            .iter()
            .map(String::as_str)
            .filter(|l| *l != "osd")
            .collect();
        return Err(format!(
            "tesseract language pack not installed: {} (installed: {})",
            if missing.is_empty() {
                requested.to_string()
            } else {
                missing.join(", ")
            },
            if have.is_empty() {
                "none".to_string()
            } else {
                have.join(", ")
            }
        ));
    }
    Ok(keep.join("+"))
}

// ---------------------------------------------------------------------------
// TSV
// ---------------------------------------------------------------------------

/// Parse tesseract's `tsv` output into lines of words.
///
/// Columns: level, page_num, block_num, par_num, line_num, word_num, left,
/// top, width, height, conf, text. Only level-5 (word) rows with a non-empty
/// text and a confidence of at least 30 are kept; consecutive words with the
/// same (block, paragraph, line) form one [`OcrLine`]. `scale` is the factor
/// the image was enlarged by; boxes are divided by it, rounding outwards so a
/// box never shrinks away from its glyphs. Malformed rows are ignored.
fn parse_tsv(tsv: &str, scale: u32) -> Vec<OcrLine> {
    let scale = scale.max(1);
    let mut lines: Vec<OcrLine> = Vec::new();
    let mut last_key: Option<(u32, u32, u32)> = None;
    for row in tsv.lines() {
        let row = row.trim_end_matches('\r');
        let cols: Vec<&str> = row.splitn(12, '\t').collect();
        if cols.len() != 12 {
            continue;
        }
        if cols[0].trim() != "5" {
            continue; // header row, page/block/paragraph/line rows
        }
        let text = cols[11].trim();
        if text.is_empty() {
            continue;
        }
        let conf = cols[10].trim().parse::<f64>().unwrap_or(f64::NAN);
        let num = |i: usize| cols[i].trim().parse::<u32>().ok();
        let (Some(block), Some(par), Some(line)) = (num(2), num(3), num(4)) else {
            continue;
        };
        let (Some(left), Some(top), Some(width), Some(height)) = (num(6), num(7), num(8), num(9))
        else {
            continue;
        };
        let x = left / scale;
        let y = top / scale;
        let right = (left.saturating_add(width)).div_ceil(scale);
        let bottom = (top.saturating_add(height)).div_ceil(scale);
        let word = OcrWord {
            text: if conf.is_finite() && conf >= MIN_CONF {
                text.to_string()
            } else {
                String::new()
            },
            x,
            y,
            w: right.saturating_sub(x),
            h: bottom.saturating_sub(y),
        };
        let key = (block, par, line);
        match (last_key == Some(key), lines.last_mut()) {
            (true, Some(l)) => l.words.push(word),
            _ => {
                lines.push(OcrLine { words: vec![word] });
                last_key = Some(key);
            }
        }
    }
    lines
}

// ---------------------------------------------------------------------------
// Pixels
// ---------------------------------------------------------------------------

fn rgba_to_rgb(rgba: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rgba.len() / 4 * 3);
    for px in rgba.chunks_exact(4) {
        out.extend_from_slice(&px[..3]);
    }
    out
}

/// 2x bilinear upscale of an RGBA image into packed RGB (`2w x 2h x 3`).
/// Each output sample is `(3 * nearest + next_nearest) / 4`, the exact
/// bilinear weights for a 2x enlargement with pixel-centre alignment.
fn upscale2x_rgb(rgba: &[u8], w: usize, h: usize) -> Vec<u8> {
    // Horizontal pass: w x h -> 2w x h.
    let w2 = w * 2;
    let mut tmp = vec![0u8; w2 * h * 3];
    for y in 0..h {
        for x in 0..w {
            let left = x.saturating_sub(1);
            let right = (x + 1).min(w - 1);
            for ch in 0..3 {
                let at = |xx: usize| u32::from(rgba[(y * w + xx) * 4 + ch]);
                let c = at(x);
                tmp[(y * w2 + 2 * x) * 3 + ch] = ((3 * c + at(left) + 2) / 4) as u8;
                tmp[(y * w2 + 2 * x + 1) * 3 + ch] = ((3 * c + at(right) + 2) / 4) as u8;
            }
        }
    }
    // Vertical pass: 2w x h -> 2w x 2h.
    let mut out = vec![0u8; w2 * h * 2 * 3];
    for y in 0..h {
        let up = y.saturating_sub(1);
        let down = (y + 1).min(h - 1);
        for x in 0..w2 * 3 {
            let at = |yy: usize| u32::from(tmp[yy * w2 * 3 + x]);
            let c = at(y);
            out[(2 * y) * w2 * 3 + x] = ((3 * c + at(up) + 2) / 4) as u8;
            out[(2 * y + 1) * w2 * 3 + x] = ((3 * c + at(down) + 2) / 4) as u8;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// In-memory input and child process
// ---------------------------------------------------------------------------

/// Binary PPM stays in memory and reaches tesseract only through stdin.
fn ppm_bytes(w: usize, h: usize, rgb: &[u8]) -> Vec<u8> {
    let mut data = format!("P6\n{w} {h}\n255\n").into_bytes();
    data.extend_from_slice(rgb);
    data
}

/// Run a command with null stdin, capturing stdout and stderr, killing it
/// after `timeout`. The pipes are drained on threads so a large TSV cannot
/// block the child.
fn run_capture(
    cmd: Command,
    timeout: Duration,
) -> std::result::Result<std::process::Output, String> {
    run_capture_input(cmd, timeout, None)
}

fn run_capture_input(
    mut cmd: Command,
    timeout: Duration,
    data: Option<Vec<u8>>,
) -> std::result::Result<std::process::Output, String> {
    let mut child = cmd
        .stdin(if data.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let writer = data.map(|data| {
        let pipe = child.stdin.take();
        std::thread::spawn(move || -> std::io::Result<()> {
            if let Some(mut pipe) = pipe {
                pipe.write_all(&data)?;
            }
            Ok(())
        })
    });
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut p) = pipe {
                let _ = p.read_to_end(&mut buf);
            }
            buf
        })
    };
    let t_out = drain(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let t_err = drain(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );

    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if start.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                break Err(format!("timed out after {} s", timeout.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                break Err(e.to_string());
            }
        }
    };
    let stdout = t_out.join().unwrap_or_default();
    let stderr = t_err.join().unwrap_or_default();
    if let Some(writer) = writer {
        let result = writer
            .join()
            .map_err(|_| "OCR input writer stopped".to_string())?;
        if status.is_ok() {
            result.map_err(|_| "cannot write OCR stdin".to_string())?;
        }
    }
    status.map(|status| std::process::Output {
        status,
        stdout,
        stderr,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEADER: &str =
        "level\tpage_num\tblock_num\tpar_num\tline_num\tword_num\tleft\ttop\twidth\theight\tconf\ttext";

    fn row(level: u32, ids: [u32; 4], geom: [u32; 4], conf: &str, text: &str) -> String {
        let [b, p, l, wn] = ids;
        format!(
            "{level}\t1\t{b}\t{p}\t{l}\t{wn}\t{}\t{}\t{}\t{}\t{conf}\t{text}",
            geom[0], geom[1], geom[2], geom[3]
        )
    }

    fn tsv(rows: &[String]) -> String {
        let mut s = String::from(HEADER);
        for r in rows {
            s.push('\n');
            s.push_str(r);
        }
        s.push('\n');
        s
    }

    #[test]
    fn header_and_structure_rows_are_skipped() {
        let t = tsv(&[
            row(1, [0, 0, 0, 0], [0, 0, 100, 50], "-1", ""),
            row(2, [1, 0, 0, 0], [0, 0, 100, 50], "-1", ""),
            row(4, [1, 1, 1, 0], [10, 10, 80, 12], "-1", ""),
            row(5, [1, 1, 1, 1], [10, 10, 40, 12], "96.5", "hello"),
        ]);
        let lines = parse_tsv(&t, 1);
        assert_eq!(lines.len(), 1);
        assert_eq!(
            lines[0].words,
            vec![OcrWord {
                text: "hello".into(),
                x: 10,
                y: 10,
                w: 40,
                h: 12
            }]
        );
    }

    #[test]
    fn low_confidence_boxes_are_kept_without_untrusted_text() {
        let t = tsv(&[
            row(5, [1, 1, 1, 1], [0, 0, 10, 10], "29.9", "noise"),
            row(5, [1, 1, 1, 2], [20, 0, 10, 10], "30", "keep"),
            row(5, [1, 1, 1, 3], [40, 0, 10, 10], "95", "   "),
            row(5, [1, 1, 1, 4], [60, 0, 10, 10], "-1", "unrated"),
            row(5, [1, 1, 1, 5], [80, 0, 10, 10], "88", " padded "),
        ]);
        let words: Vec<String> = parse_tsv(&t, 1)
            .into_iter()
            .flat_map(|l| l.words)
            .map(|w| w.text)
            .collect();
        assert_eq!(words, vec!["", "keep", "", "padded"]);
    }

    #[test]
    fn float_confidence_is_accepted() {
        let t = tsv(&[
            row(5, [1, 1, 1, 1], [0, 0, 10, 10], "30.000000", "edge"),
            row(5, [1, 1, 1, 2], [20, 0, 10, 10], "9.1e1", "sci"),
        ]);
        assert_eq!(parse_tsv(&t, 1)[0].words.len(), 2);
    }

    #[test]
    fn words_group_by_block_paragraph_line_in_order() {
        let t = tsv(&[
            row(5, [1, 1, 1, 1], [0, 0, 10, 10], "90", "a"),
            row(5, [1, 1, 1, 2], [20, 0, 10, 10], "90", "b"),
            row(5, [1, 1, 2, 1], [0, 20, 10, 10], "90", "c"),
            row(5, [1, 2, 1, 1], [0, 40, 10, 10], "90", "d"),
            row(5, [2, 1, 1, 1], [0, 60, 10, 10], "90", "e"),
            row(5, [2, 1, 1, 2], [20, 60, 10, 10], "90", "f"),
        ]);
        let lines = parse_tsv(&t, 1);
        let text: Vec<String> = lines
            .iter()
            .map(|l| {
                l.words
                    .iter()
                    .map(|w| w.text.as_str())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect();
        assert_eq!(text, vec!["a b", "c", "d", "e f"]);
    }

    #[test]
    fn a_low_confidence_line_keeps_its_boxes() {
        let t = tsv(&[
            row(5, [1, 1, 1, 1], [0, 0, 10, 10], "5", "junk"),
            row(5, [1, 1, 2, 1], [0, 20, 10, 10], "90", "real"),
        ]);
        let lines = parse_tsv(&t, 1);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].words[0].text, "");
        assert_eq!(lines[1].words[0].text, "real");
    }

    #[test]
    fn scale_divides_boxes_back_to_original_pixels() {
        let t = tsv(&[row(5, [1, 1, 1, 1], [100, 60, 80, 24], "90", "word")]);
        let w = &parse_tsv(&t, 2)[0].words[0];
        assert_eq!((w.x, w.y, w.w, w.h), (50, 30, 40, 12));
        // Odd numbers round outwards: left floors, right edge ceils.
        let t = tsv(&[row(5, [1, 1, 1, 1], [101, 61, 80, 24], "90", "word")]);
        let w = &parse_tsv(&t, 2)[0].words[0];
        assert_eq!((w.x, w.y), (50, 30));
        assert_eq!((w.x + w.w, w.y + w.h), (91, 43));
        // Scale 0 is treated as 1, not a division by zero.
        assert_eq!(parse_tsv(&t, 0)[0].words[0].x, 101);
    }

    #[test]
    fn malformed_rows_are_ignored() {
        let mut t = tsv(&[row(5, [1, 1, 1, 1], [0, 0, 10, 10], "90", "ok")]);
        t.push_str("5\t1\t1\n"); // too few columns
        t.push_str("5\t1\tx\t1\t1\t1\t0\t0\t10\t10\t90\tbadblock\n");
        t.push_str("5\t1\t1\t1\t1\t1\t-4\t0\t10\t10\t90\tnegative\n");
        t.push_str("5\t1\t1\t1\t1\t1\t0\t0\t10\t10\tabc\tbadconf\n");
        t.push_str("garbage line without tabs\n\n");
        t.push_str("5\t1\t1\t1\t1\t9\t0\t0\t10\t10\tNaN\tnan\n");
        let words: Vec<String> = parse_tsv(&t, 1)
            .into_iter()
            .flat_map(|l| l.words)
            .map(|w| w.text)
            .collect();
        assert_eq!(words, vec!["ok", "", ""]);
        assert!(parse_tsv("", 1).is_empty());
    }

    #[test]
    fn text_with_quotes_and_crlf_survives() {
        let t = "5\t1\t1\t1\t1\t1\t0\t0\t10\t10\t90\t\"hello\"\r\n";
        assert_eq!(parse_tsv(t, 1)[0].words[0].text, "\"hello\"");
    }

    #[test]
    fn list_langs_output_is_parsed() {
        let out = "List of available languages in \"/usr/share/tesseract-ocr/5/tessdata/\" (2):\neng\nosd\n";
        assert_eq!(parse_list_langs(out), vec!["eng", "osd"]);
        assert_eq!(
            parse_list_langs("List of available languages (1):\nscript/Latin\nchi_sim\n\n"),
            vec!["script/Latin", "chi_sim"]
        );
    }

    #[test]
    fn language_selection_drops_missing_parts() {
        let installed: Vec<String> = ["eng", "deu", "osd"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(pick_languages("", &installed).unwrap(), "eng");
        assert_eq!(pick_languages("  ", &installed).unwrap(), "eng");
        assert_eq!(pick_languages("eng+deu", &installed).unwrap(), "eng+deu");
        assert_eq!(pick_languages("eng+chi_tra", &installed).unwrap(), "eng");
        assert_eq!(
            pick_languages("chi_tra+eng+eng", &installed).unwrap(),
            "eng"
        );
        let e = pick_languages("chi_tra+jpn", &installed).unwrap_err();
        assert!(
            e.contains("chi_tra") && e.contains("jpn") && e.contains("eng, deu"),
            "{e}"
        );
        let e = pick_languages("eng", &["deu".to_string()]).unwrap_err();
        assert!(e.contains("eng") && e.contains("deu"), "{e}");
        // osd is not a language to recognise with.
        assert!(pick_languages("osd", &installed).is_err());
    }

    #[test]
    fn upscale_doubles_dimensions_and_keeps_flat_colours() {
        let (w, h) = (3, 2);
        let rgba: Vec<u8> = (0..w * h).flat_map(|_| [10u8, 200, 77, 0]).collect();
        let out = upscale2x_rgb(&rgba, w, h);
        assert_eq!(out.len(), 6 * 4 * 3);
        assert!(out.chunks_exact(3).all(|p| p == [10, 200, 77]));
    }

    #[test]
    fn upscale_interpolates_a_step_edge() {
        // Black | white, one row: 2 pixels -> 4 samples 0, 64, 191, 255.
        let rgba = [0, 0, 0, 255, 255, 255, 255, 255];
        let out = upscale2x_rgb(&rgba, 2, 1);
        let r: Vec<u8> = out.chunks_exact(3).take(4).map(|p| p[0]).collect();
        assert_eq!(r, vec![0, 64, 191, 255]);
        // One pixel high -> two identical rows.
        assert_eq!(&out[..12], &out[12..24]);
    }

    #[test]
    fn rgb_conversion_drops_alpha() {
        assert_eq!(
            rgba_to_rgb(&[1, 2, 3, 4, 5, 6, 7, 8]),
            vec![1, 2, 3, 5, 6, 7]
        );
    }

    #[test]
    fn ppm_is_piped_without_a_temporary_file() {
        let data = ppm_bytes(2, 1, &[1, 2, 3, 4, 5, 6]);
        assert_eq!(
            data,
            [b"P6\n2 1\n255\n".as_slice(), &[1, 2, 3, 4, 5, 6]].concat()
        );
        let out = run_capture_input(
            Command::new("cat"),
            Duration::from_secs(5),
            Some(data.clone()),
        )
        .unwrap();
        assert!(out.status.success());
        assert_eq!(out.stdout, data);
    }

    #[test]
    fn run_capture_times_out_and_kills_the_child() {
        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        let t = Instant::now();
        let r = run_capture(cmd, Duration::from_millis(200));
        assert!(r.is_err());
        assert!(t.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn run_capture_reads_large_output_without_deadlock() {
        let mut cmd = Command::new("sh");
        cmd.args([
            "-c",
            "head -c 500000 /dev/zero | tr '\\0' 'x'; echo oops >&2",
        ]);
        let out = run_capture(cmd, Duration::from_secs(10)).unwrap();
        assert!(out.status.success());
        assert_eq!(out.stdout.len(), 500_000);
        assert_eq!(String::from_utf8_lossy(&out.stderr).trim(), "oops");
    }

    // --- real tesseract, skipped when it is not installed ---------------------

    /// 5x7 glyphs for the letters used below; `#` is ink.
    fn glyph(c: char) -> [&'static str; 7] {
        match c {
            'H' => [
                "#...#", "#...#", "#...#", "#####", "#...#", "#...#", "#...#",
            ],
            'E' => [
                "#####", "#....", "#....", "####.", "#....", "#....", "#####",
            ],
            'L' => [
                "#....", "#....", "#....", "#....", "#....", "#....", "#####",
            ],
            'O' => [
                ".###.", "#...#", "#...#", "#...#", "#...#", "#...#", ".###.",
            ],
            'K' => [
                "#...#", "#..#.", "#.#..", "##...", "#.#..", "#..#.", "#...#",
            ],
            _ => ["     "; 7],
        }
    }

    /// White RGBA canvas with `text` drawn in black blocks, 4x magnified,
    /// top-left of the first letter at (`x0`, `y0`).
    fn render(w: u32, h: u32, text: &str, x0: u32, y0: u32) -> Image {
        const Z: u32 = 4;
        let mut rgba = vec![255u8; (w * h * 4) as usize];
        for (i, ch) in text.chars().enumerate() {
            for (gy, row) in glyph(ch).iter().enumerate() {
                for (gx, cell) in row.chars().enumerate() {
                    if cell != '#' {
                        continue;
                    }
                    for dy in 0..Z {
                        for dx in 0..Z {
                            let x = x0 + (i as u32 * 6 + gx as u32) * Z + dx;
                            let y = y0 + gy as u32 * Z + dy;
                            if x < w && y < h {
                                let o = ((y * w + x) * 4) as usize;
                                rgba[o..o + 3].copy_from_slice(&[0, 0, 0]);
                            }
                        }
                    }
                }
            }
        }
        Image {
            width: w,
            height: h,
            rgba,
        }
    }

    fn engine() -> Option<Box<dyn OcrEngine>> {
        match create("") {
            Ok(e) => Some(e),
            Err(e) => {
                eprintln!("skipping real tesseract test: {e}");
                None
            }
        }
    }

    fn find<'a>(lines: &'a [OcrLine], text: &str) -> Option<&'a OcrWord> {
        lines
            .iter()
            .flat_map(|l| &l.words)
            .find(|w| w.text.eq_ignore_ascii_case(text))
    }

    #[test]
    fn real_tesseract_boxes_are_in_original_pixels_when_upscaled() {
        let Some(mut e) = engine() else { return };
        assert_eq!(e.name(), "tesseract(eng)");
        // 400 px wide: takes the 2x upscale path. "HELLO" glyphs are 20x28 px.
        let img = render(400, 120, "HELLO HELLO", 50, 40);
        let lines = e.recognize(&img).unwrap();
        let w = find(&lines, "HELLO").expect("HELLO not recognised");
        assert!((w.x as i32 - 50).abs() <= 6, "x = {}", w.x);
        assert!((w.y as i32 - 40).abs() <= 6, "y = {}", w.y);
        assert!((w.h as i32 - 28).abs() <= 8, "h = {}", w.h);
        assert!(w.w >= 90 && w.w <= 130, "w = {}", w.w);
        assert!(w.x + w.w <= 400 && w.y + w.h <= 120);
    }

    #[test]
    fn real_tesseract_boxes_are_in_original_pixels_when_not_upscaled() {
        let Some(mut e) = engine() else { return };
        // 1000 px wide: no upscale (scale 1).
        let img = render(1000, 120, "HELLO HELLO", 50, 40);
        let lines = e.recognize(&img).unwrap();
        let w = find(&lines, "HELLO").expect("HELLO not recognised");
        assert!((w.x as i32 - 50).abs() <= 6, "x = {}", w.x);
        assert!((w.y as i32 - 40).abs() <= 6, "y = {}", w.y);
    }

    #[test]
    fn real_tesseract_handles_blank_and_degenerate_images() {
        let Some(mut e) = engine() else { return };
        let blank = Image {
            width: 200,
            height: 100,
            rgba: vec![255; 200 * 100 * 4],
        };
        assert!(e.recognize(&blank).unwrap().is_empty());
        assert!(e.recognize(&Image::default()).unwrap().is_empty());
        let bad = Image {
            width: 10,
            height: 10,
            rgba: vec![0; 5],
        };
        assert!(e.recognize(&bad).is_err());
    }

    #[test]
    fn unknown_language_is_reported() {
        if crate::x11::find_in_path("tesseract").is_none() {
            return;
        }
        let e = create("xx_not_a_language").err().expect("must fail");
        assert!(e.contains("xx_not_a_language"), "{e}");
        // A partly valid spec keeps the valid part.
        let e = create("eng+xx_not_a_language").unwrap();
        assert_eq!(e.name(), "tesseract(eng)");
    }
}
