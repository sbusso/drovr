//! Local PNG images in the doc pane, shown with Kitty Unicode placeholders.
//!
//! Each image is sent once as a virtual placement. The rendered lines then
//! hold placeholder cells whose colour names the image, so the terminal
//! draws the image where those cells are and it scrolls like text.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use ratatui::style::Color;

use super::{resolve_link, stamp, Stamp, Target};

/// Largest image file the viewer sends.
const MAX_IMAGE_BYTES: u64 = 8 * 1024 * 1024;
/// Tallest image in rows, so one image cannot fill many screens.
const MAX_IMAGE_ROWS: u32 = 30;
/// Kitty Unicode placeholder base character.
const PLACEHOLDER: char = '\u{10EEEE}';

/// Cell box an image occupies, and its Kitty image id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placed {
    pub id: u32,
    pub cols: u16,
    pub rows: u16,
}

impl Placed {
    /// Foreground colour of the placeholder cells: the 24-bit image id.
    pub fn color(&self) -> Color {
        let [_, r, g, b] = self.id.to_be_bytes();
        Color::Rgb(r, g, b)
    }

    /// Placeholder text for one image row. Every cell carries its row and
    /// column diacritics, so no terminal has to infer them.
    pub fn row_text(&self, row: u16) -> String {
        let row_mark = ghostty_vt::kitty_placeholder_diacritic(usize::from(row));
        let mut text = String::new();
        for col in 0..self.cols {
            text.push(PLACEHOLDER);
            text.extend(row_mark);
            text.extend(ghostty_vt::kitty_placeholder_diacritic(usize::from(col)));
        }
        text
    }
}

struct Sent {
    stamp: Option<Stamp>,
    placed: Placed,
    used: bool,
}

/// Images sent to the terminal for the current document.
#[derive(Default)]
pub struct Images {
    /// Cell size in pixels. `None` turns images off.
    cell: Option<(u32, u32)>,
    sent: HashMap<(PathBuf, usize), Sent>,
    next_id: u32,
    /// Graphics commands to write before the next frame.
    pending: Vec<u8>,
}

impl Images {
    /// Sets the cell size in pixels. Returns true when it changed, which
    /// means the document needs a new layout.
    pub fn set_cell_size(&mut self, cell: Option<(u32, u32)>) -> bool {
        let cell = cell.filter(|(w, h)| *w > 0 && *h > 0);
        if cell == self.cell {
            return false;
        }
        self.cell = cell;
        true
    }

    pub fn begin(&mut self) {
        for sent in self.sent.values_mut() {
            sent.used = false;
        }
    }

    /// Deletes images the last layout did not use.
    pub fn finish(&mut self) {
        let pending = &mut self.pending;
        self.sent.retain(|_, sent| {
            if !sent.used {
                delete_image(pending, sent.placed.id);
            }
            sent.used
        });
    }

    pub fn take_pending(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending)
    }

    /// Places the image at `url` (relative to `doc`) in at most `width`
    /// columns. Returns `None` for remote, missing or non-PNG images, which
    /// the renderer shows as alt text.
    pub fn place(&mut self, url: &str, doc: &Path, width: usize) -> Option<Placed> {
        let (cell_w, cell_h) = self.cell?;
        let path = match resolve_link(url, doc) {
            Target::File(path) => path,
            _ => return None,
        };
        let stamp = stamp(&path);
        let key = (path, width);
        if let Some(sent) = self.sent.get_mut(&key) {
            if sent.stamp == stamp {
                sent.used = true;
                return Some(sent.placed);
            }
        }
        let data = read_png(&key.0)?;
        let (img_w, img_h) = png_size(&data)?;
        let (cols, rows) = fit(img_w, img_h, cell_w, cell_h, width as u32)?;
        self.next_id = self.next_id % 0xFF_FFFF + 1;
        let placed = Placed {
            id: self.next_id,
            cols,
            rows,
        };
        if let Some(old) = self.sent.remove(&key) {
            delete_image(&mut self.pending, old.placed.id);
        }
        let control = format!("a=T,U=1,f=100,t=d,q=2,i={},c={cols},r={rows}", placed.id);
        crate::kitty_graphics::write_kitty_data(&mut self.pending, &control, &data).ok()?;
        self.sent.insert(
            key,
            Sent {
                stamp,
                placed,
                used: true,
            },
        );
        Some(placed)
    }

    /// Commands that free every image, for when the viewer exits.
    pub fn clear_all(&mut self) -> Vec<u8> {
        for sent in self.sent.values() {
            delete_image(&mut self.pending, sent.placed.id);
        }
        self.sent.clear();
        self.take_pending()
    }
}

fn delete_image(out: &mut Vec<u8>, id: u32) {
    out.extend_from_slice(format!("\x1b_Ga=d,d=I,q=2,i={id}\x1b\\").as_bytes());
}

/// Reads a regular file of at most `MAX_IMAGE_BYTES` that starts with the
/// PNG signature. A FIFO or device would block the viewer, so only regular
/// files are opened.
fn read_png(path: &Path) -> Option<Vec<u8>> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_IMAGE_BYTES {
        return None;
    }
    let mut data = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(MAX_IMAGE_BYTES)
        .read_to_end(&mut data)
        .ok()?;
    Some(data)
}

/// Pixel size from the PNG header, without decoding the image.
fn png_size(data: &[u8]) -> Option<(u32, u32)> {
    let info = png::Decoder::new(std::io::Cursor::new(data))
        .read_info()
        .ok()?;
    let info = info.info();
    Some((info.width, info.height))
}

/// Cell box for an image: native size when it fits, scaled down to
/// `max_cols` columns and `MAX_IMAGE_ROWS` rows otherwise.
fn fit(img_w: u32, img_h: u32, cell_w: u32, cell_h: u32, max_cols: u32) -> Option<(u16, u16)> {
    if img_w == 0 || img_h == 0 || max_cols == 0 {
        return None;
    }
    let (img_w, img_h) = (u64::from(img_w), u64::from(img_h));
    let (cell_w, cell_h) = (u64::from(cell_w), u64::from(cell_h));
    let mut width_px = img_w.min(u64::from(max_cols) * cell_w);
    let mut height_px = img_h * width_px / img_w;
    let max_height_px = u64::from(MAX_IMAGE_ROWS) * cell_h;
    if height_px > max_height_px {
        height_px = max_height_px;
        width_px = img_w * height_px / img_h;
    }
    let cols = width_px.div_ceil(cell_w).clamp(1, u64::from(max_cols));
    let rows = height_px
        .div_ceil(cell_h)
        .clamp(1, u64::from(MAX_IMAGE_ROWS));
    Some((cols as u16, rows as u16))
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn fit_keeps_native_size_and_scales_down() {
        // 100x40 px in 10x20 px cells: 10 columns, 2 rows.
        assert_eq!(fit(100, 40, 10, 20, 80), Some((10, 2)));
        // Too wide: scaled to 20 columns, height follows.
        assert_eq!(fit(1000, 400, 10, 20, 20), Some((20, 4)));
        // Too tall: capped at MAX_IMAGE_ROWS, width follows.
        assert_eq!(fit(100, 6000, 10, 20, 80), Some((1, 30)));
        assert_eq!(fit(0, 10, 10, 20, 80), None);
    }

    #[test]
    fn placeholder_rows_are_one_cell_per_column() {
        let placed = Placed {
            id: 0x01_02_03,
            cols: 3,
            rows: 2,
        };
        assert_eq!(placed.color(), Color::Rgb(1, 2, 3));
        let row = placed.row_text(1);
        assert_eq!(row.width(), 3);
        assert_eq!(row.chars().filter(|c| *c == PLACEHOLDER).count(), 3);
        // Row 1 and column 2 use the second and third table entries.
        let mut chars = row.chars().skip(6);
        assert_eq!(chars.next(), Some(PLACEHOLDER));
        assert_eq!(chars.next(), ghostty_vt::kitty_placeholder_diacritic(1));
        assert_eq!(chars.next(), ghostty_vt::kitty_placeholder_diacritic(2));
    }

    #[test]
    fn place_sends_local_png_once_and_skips_others() {
        let dir = std::env::temp_dir().join(format!("drovr-doc-image-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("dot.png");
        let mut data = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut data, 20, 40);
            encoder.set_color(png::ColorType::Rgba);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&[0; 20 * 40 * 4]).unwrap();
        }
        std::fs::write(&png, &data).unwrap();
        std::fs::write(dir.join("text.png"), "not a png").unwrap();
        let doc = dir.join("doc.md");

        let mut images = Images::default();
        assert_eq!(
            images.place("dot.png", &doc, 40),
            None,
            "off without cell size"
        );
        assert!(images.set_cell_size(Some((10, 20))));
        images.begin();
        let placed = images.place("dot.png", &doc, 40).unwrap();
        assert_eq!((placed.cols, placed.rows), (2, 2));
        let sent = String::from_utf8(images.take_pending()).unwrap();
        assert!(sent.starts_with("\x1b_Ga=T,U=1,f=100,t=d,q=2,i=1,c=2,r=2,m=0;"));
        assert_eq!(images.place("dot.png", &doc, 40), Some(placed));
        assert!(images.take_pending().is_empty(), "sent once");
        assert_eq!(images.place("text.png", &doc, 40), None);
        assert_eq!(images.place("missing.png", &doc, 40), None);
        assert_eq!(images.place("https://x.test/a.png", &doc, 40), None);
        images.finish();

        images.begin();
        images.finish();
        let deleted = String::from_utf8(images.take_pending()).unwrap();
        assert_eq!(deleted, "\x1b_Ga=d,d=I,q=2,i=1\x1b\\");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
