//! Decode off the UI thread; render only cached pixels or locally encoded protocol payloads.
use base64::{Engine, engine::general_purpose::STANDARD};
use eden_protocol::coding::Block;
use image::{ImageFormat, ImageReader};
use ratatui::{buffer::Buffer, layout::Rect, style::Color};
use std::{
    io::{self, Cursor, Write},
    sync::Arc,
};

pub struct Decoded {
    pub width: u32,
    pub height: u32,
    raster: image::RgbaImage,
    png: String,
}
pub fn decode(block: &Block) -> Result<Decoded, String> {
    let Block::Image { data, .. } = block else {
        return Err("Not an image payload".into());
    };
    let bytes = STANDARD.decode(data).map_err(|e| e.to_string())?;
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| e.to_string())?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(128 * 1024 * 1024);
    reader.limits(limits);
    let image = reader.decode().map_err(|e| e.to_string())?;
    let (width, height) = (image.width(), image.height());
    let preview = image.thumbnail(1024, 1024);
    let mut png = Cursor::new(Vec::new());
    preview
        .write_to(&mut png, ImageFormat::Png)
        .map_err(|e| e.to_string())?;
    Ok(Decoded {
        width,
        height,
        raster: preview.to_rgba8(),
        png: STANDARD.encode(png.into_inner()),
    })
}
#[derive(Clone)]
pub struct Placement {
    pub image: Arc<Decoded>,
    pub area: Rect,
}
impl std::fmt::Debug for Placement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Placement")
            .field("area", &self.area)
            .finish_non_exhaustive()
    }
}
pub fn fit(image: &Decoded, area: Rect) -> Rect {
    if area.is_empty() {
        return area;
    }
    let ratio = image.width as f64 / image.height.max(1) as f64;
    let width = area
        .width
        .min((f64::from(area.height) * 2.0 * ratio).ceil().max(1.0) as u16);
    let height = area
        .height
        .min((f64::from(width) / ratio / 2.0).ceil().max(1.0) as u16);
    Rect::new(area.x, area.y, width, height)
}
pub fn raster(image: &Decoded, buf: &mut Buffer, area: Rect, mono: bool) {
    let area = fit(image, area);
    for y in 0..area.height {
        for x in 0..area.width {
            let pixel = |row: u32| {
                let px = u32::from(x) * image.raster.width() / u32::from(area.width).max(1);
                let py = row * image.raster.height() / (u32::from(area.height) * 2).max(1);
                image
                    .raster
                    .get_pixel(
                        px.min(image.raster.width() - 1),
                        py.min(image.raster.height() - 1),
                    )
                    .0
            };
            let top = pixel(u32::from(y) * 2);
            let bottom = pixel(u32::from(y) * 2 + 1);
            if let Some(cell) = buf.cell_mut((area.x + x, area.y + y)) {
                if mono {
                    let brightness =
                        (u32::from(top[0]) + u32::from(top[1]) + u32::from(top[2])) / 3;
                    cell.set_symbol(
                        [" ", ".", ":", "*", "#", "@"][((255 - brightness) * 5 / 255) as usize],
                    )
                    .set_fg(Color::Reset)
                    .set_bg(Color::Reset);
                } else {
                    let color = |p: [u8; 4]| {
                        if p[3] < 128 {
                            Color::Reset
                        } else {
                            Color::Rgb(p[0], p[1], p[2])
                        }
                    };
                    if top[3] < 128 {
                        cell.set_symbol(if bottom[3] < 128 { " " } else { "▄" })
                            .set_fg(color(bottom))
                            .set_bg(Color::Reset);
                    } else {
                        cell.set_symbol("▀")
                            .set_fg(color(top))
                            .set_bg(color(bottom));
                    }
                }
            }
        }
    }
}
pub fn kitty_available() -> bool {
    std::env::var_os("KITTY_WINDOW_ID").is_some()
        && std::env::var_os("TMUX").is_none()
        && std::env::var_os("STY").is_none()
}
const DELETE: &[u8] = b"\x1b_Ga=d,d=I,i=33773,q=2\x1b\\";
/// One terminal owner holds one private placement and removes it before handing off the TTY.
#[derive(Default)]
pub struct Kitty {
    current: Option<Placement>,
}
impl Kitty {
    pub fn clear(&mut self, output: &mut impl Write) -> io::Result<()> {
        if self.current.take().is_some() {
            output.write_all(DELETE)?;
            output.flush()?;
        }
        Ok(())
    }
    pub fn draw(
        &mut self,
        placement: Option<&Placement>,
        output: &mut impl Write,
    ) -> io::Result<()> {
        if self
            .current
            .as_ref()
            .zip(placement)
            .is_some_and(|(old, new)| old.area == new.area && Arc::ptr_eq(&old.image, &new.image))
        {
            return Ok(());
        }
        self.clear(output)?;
        let Some(placement) = placement else {
            return Ok(());
        };
        if placement.area.is_empty() {
            return Ok(());
        }
        // Fixed control fields and numeric geometry only; image bytes are re-encoded PNG/base64.
        write!(
            output,
            "\x1b7\x1b[{};{}H",
            placement.area.y + 1,
            placement.area.x + 1
        )?;
        let chunks = placement.image.png.as_bytes().chunks(4096);
        let count = chunks.len();
        for (index, chunk) in chunks.enumerate() {
            if index == 0 {
                write!(
                    output,
                    "\x1b_Ga=T,f=100,i=33773,q=2,C=1,c={},r={},m={};",
                    placement.area.width,
                    placement.area.height,
                    usize::from(index + 1 < count)
                )?;
            } else {
                write!(output, "\x1b_Gm={};", usize::from(index + 1 < count))?;
            }
            output.write_all(chunk)?;
            output.write_all(b"\x1b\\")?;
        }
        output.write_all(b"\x1b8")?;
        output.flush()?;
        self.current = Some(placement.clone());
        Ok(())
    }
}
impl Drop for Kitty {
    fn drop(&mut self) {
        let _ = self.clear(&mut io::stdout());
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    pub(crate) fn block() -> Block {
        let pixels = image::RgbaImage::from_pixel(4, 2, image::Rgba([220, 50, 80, 255]));
        let mut png = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(pixels)
            .write_to(&mut png, ImageFormat::Png)
            .unwrap();
        Block::Image {
            media_type: "image/png".into(),
            data: STANDARD.encode(png.into_inner()),
        }
    }
    #[test]
    fn actual_pixels_render_as_half_blocks_and_monochrome_without_escape_data() {
        let decoded = decode(&block()).unwrap();
        assert_eq!((decoded.width, decoded.height), (4, 2));
        let rect = Rect::new(0, 0, 12, 4);
        let mut buffer = Buffer::empty(rect);
        raster(&decoded, &mut buffer, rect, false);
        assert!(
            buffer
                .content
                .iter()
                .any(|cell| cell.symbol() == "▀" && cell.fg == Color::Rgb(220, 50, 80))
        );
        raster(&decoded, &mut buffer, rect, true);
        assert!(
            buffer
                .content
                .iter()
                .all(|cell| !cell.symbol().contains('\x1b'))
        );
        assert!(buffer.content.iter().all(|cell| cell.fg == Color::Reset));
        assert!(
            decode(&Block::Image {
                media_type: "image/png".into(),
                data: STANDARD.encode(b"\x1b]52;unsafe\x07")
            })
            .is_err()
        );
    }
    #[test]
    fn kitty_placement_is_quiet_cached_and_deleted_before_terminal_handoff() {
        let placement = Placement {
            image: Arc::new(decode(&block()).unwrap()),
            area: Rect::new(2, 3, 8, 2),
        };
        let mut kitty = Kitty::default();
        let mut output = Vec::new();
        kitty.draw(Some(&placement), &mut output).unwrap();
        let count = output.len();
        kitty.draw(Some(&placement), &mut output).unwrap();
        assert_eq!(output.len(), count);
        let protocol = String::from_utf8(output.clone()).unwrap();
        assert!(protocol.contains("q=2,C=1,c=8,r=2"));
        assert!(protocol.starts_with("\x1b7\x1b[4;3H"));
        kitty.clear(&mut output).unwrap();
        assert!(output.ends_with(DELETE));
        let count = output.len();
        kitty.clear(&mut output).unwrap();
        assert_eq!(output.len(), count);
    }
}
