use std::borrow::Cow;
use std::io::{self, Write};

use flate2::{Compression, Crc, write::ZlibEncoder};
use serde_json::Value;

#[path = "pxpipe_atlas.rs"]
mod atlas;
pub use atlas::Font;

const PADDING: usize = 4;
const SENTINEL: char = '↵';

#[derive(Clone, Copy)]
pub struct Style {
    pub font: Font,
    pub column_spacing: usize,
    pub row_spacing: usize,
    pub aa: bool,
    pub grid: bool,
    pub grid_cols: usize,
    pub color_cycle: bool,
    pub marker_scale: usize,
    pub marker_red: bool,
    pub ink_dilate: usize,
}

impl Default for Style {
    fn default() -> Self {
        Self {
            font: Font::default(),
            column_spacing: 0,
            row_spacing: 0,
            aa: true,
            grid: false,
            grid_cols: 0,
            color_cycle: false,
            marker_scale: 1,
            marker_red: false,
            ink_dilate: 0,
        }
    }
}

impl Style {
    pub fn override_with(self, config: &Value) -> Self {
        let integer = |name: &str, fallback: usize, positive: bool| {
            config[name]
                .as_f64()
                .filter(|value| value.is_finite() && (*value > 0.0 || !positive && *value == 0.0))
                .map_or(fallback, |value| value as usize)
        };
        Self {
            font: Font::parse(&config["font"]).unwrap_or(self.font),
            column_spacing: integer("cellWBonus", self.column_spacing, false),
            row_spacing: integer("cellHBonus", self.row_spacing, false),
            aa: config["aa"].as_bool().unwrap_or(self.aa),
            grid: config["grid"].as_bool().unwrap_or(self.grid),
            grid_cols: integer("gridCols", self.grid_cols, false),
            color_cycle: config["colorCycle"].as_bool().unwrap_or(self.color_cycle),
            marker_scale: integer("markerScale", self.marker_scale, true),
            marker_red: config["markerRed"].as_bool().unwrap_or(self.marker_red),
            ink_dilate: integer("inkDilate", self.ink_dilate, false),
        }
    }

    pub fn cell(self) -> (usize, usize) {
        let (width, height, _) = self.font.dimensions();
        (
            width.saturating_add(self.column_spacing),
            height.saturating_add(self.row_spacing),
        )
    }

    fn cells(self, ch: char) -> usize {
        if ch == SENTINEL && self.marker_scale > 1 {
            self.marker_scale
        } else {
            self.font.cells(ch)
        }
    }
}

pub struct Options {
    pub columns: usize,
    pub max_height: usize,
    pub max_chars_per_page: usize,
    pub page_limit: usize,
    pub style: Style,
    pub reflow: bool,
    pub shrink: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            columns: 312,
            max_height: 728,
            max_chars_per_page: 28_080,
            page_limit: 100,
            style: Style::default(),
            reflow: false,
            shrink: true,
        }
    }
}

pub struct Page {
    pub png: Vec<u8>,
    pub width: usize,
    pub height: usize,
    pub dropped: usize,
}

pub fn render(text: &str, options: &Options) -> io::Result<Vec<Page>> {
    render_inner(text, None, options)
}

pub fn render_roles(text: &str, slots: &str, options: &Options) -> io::Result<Vec<Page>> {
    render_inner(text, Some(slots), options)
}

fn render_inner(text: &str, slots: Option<&str>, options: &Options) -> io::Result<Vec<Page>> {
    if !renderable(options.columns, options.max_height, options.style)
        || options.page_limit == 0
        || options.max_chars_per_page == 0
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Invalid PXPIPE render geometry",
        ));
    }
    let style = options.style;
    let text = if options.reflow && !text.contains(SENTINEL) {
        Cow::Owned(pack(text))
    } else {
        Cow::Borrowed(text)
    };
    let columns = if options.shrink {
        content_columns(&text, options.columns)
    } else {
        options.columns
    };
    let lines = wrap(&text, style, columns, true);
    let slots = slots.map(|slots| wrap(slots, style, columns, true));
    let cell_height = style.cell().1;
    let rows_by_height = options
        .max_height
        .saturating_sub(2 * PADDING)
        .div_euclid(cell_height)
        .max(1);
    let max_rows = rows_by_height.min((options.max_chars_per_page / columns).max(1));
    let mut pages = Vec::new();
    let mut start = 0;
    let mut chars = 0;
    for (index, line) in lines.iter().enumerate() {
        let line_chars = line.encode_utf16().count();
        let full = index - start >= max_rows
            || chars + line_chars + usize::from(index > start) > options.max_chars_per_page;
        if index > start && full {
            push_page(
                &mut pages,
                &lines[start..index],
                slots
                    .as_ref()
                    .map(|slots| slots.get(start..index.min(slots.len())).unwrap_or_default()),
                Layout {
                    style,
                    columns,
                    max_rows: rows_by_height,
                    page_limit: options.page_limit,
                },
            )?;
            start = index;
            chars = 0;
        }
        chars += line_chars + usize::from(index > start);
    }
    push_page(
        &mut pages,
        &lines[start..],
        slots
            .as_ref()
            .map(|slots| slots.get(start..).unwrap_or_default()),
        Layout {
            style,
            columns,
            max_rows: rows_by_height,
            page_limit: options.page_limit,
        },
    )?;
    Ok(pages)
}

pub fn renderable(columns: usize, max_height: usize, style: Style) -> bool {
    let (cell_width, cell_height) = style.cell();
    let Some(width) = columns
        .checked_mul(cell_width)
        .and_then(|w| w.checked_add(2 * PADDING))
    else {
        return false;
    };
    let Some(min_height) = cell_height.checked_add(2 * PADDING) else {
        return false;
    };
    columns > 0
        && max_height
            .max(min_height)
            .checked_mul(width)
            .is_some_and(|pixels| pixels <= 8 * 1024 * 1024)
        && style.marker_scale <= 512
        && style.ink_dilate <= 32
}

pub fn pack(text: &str) -> String {
    compact(text, 3)
        .split('\n')
        .map(expand_tabs)
        .collect::<Vec<_>>()
        .join(&SENTINEL.to_string())
}

pub fn content_columns(text: &str, cap: usize) -> usize {
    measure_columns(text, cap, Font::default())
}

pub fn measure_columns(text: &str, cap: usize, font: Font) -> usize {
    let mut widest = 1;
    for line in text.split('\n') {
        let width: usize = display_line(line, true)
            .chars()
            .map(|ch| font.cells(ch))
            .sum();
        widest = widest.max(width);
        if widest >= cap {
            return cap;
        }
    }
    widest
}

pub fn compact(text: &str, blank_limit: usize) -> String {
    let mut out = String::with_capacity(text.len());
    let mut newlines = 0;
    for (index, line) in text.split('\n').enumerate() {
        if index > 0 && newlines < blank_limit {
            out.push('\n');
            newlines += 1;
        }
        let line = line.trim_end_matches([' ', '\t']);
        if !line.is_empty() {
            newlines = 0;
            out.push_str(line);
        }
    }
    out
}

fn expand_tabs(line: &str) -> Cow<'_, str> {
    if !line.contains('\t') {
        return Cow::Borrowed(line);
    }
    let mut output = String::with_capacity(line.len());
    let mut column = 0;
    for ch in line.chars() {
        if ch == '\t' {
            let span = 4 - column % 4;
            output.push('→');
            output.extend(std::iter::repeat_n(' ', span - 1));
            column += span;
        } else {
            output.push(ch);
            column += Font::Spleen.cells(ch);
        }
    }
    Cow::Owned(output)
}

fn display_line(line: &str, pad_markers: bool) -> String {
    let expanded = expand_tabs(line);
    let mut output = String::with_capacity(expanded.len());
    for ch in expanded.chars() {
        if pad_markers && ch == SENTINEL {
            output.extend([' ', ch, ' ']);
        } else if Font::Spleen.glyph(ch, false).is_some() || exempt_missing(ch) {
            output.push(ch);
        } else {
            use std::fmt::Write;
            write!(&mut output, "[U+{:X}]", ch as u32).expect("writing to a string");
        }
    }
    output
}

fn exempt_missing(ch: char) -> bool {
    matches!(ch as u32, 0..=0x1f | 0x7f..=0x9f | 0x0300..=0x036f | 0x200b..=0x200d
        | 0x2060 | 0xfeff | 0xfe00..=0xfe0f | 0xe0100..=0xe01ef)
}

fn wrap(text: &str, style: Style, columns: usize, pad_markers: bool) -> Vec<String> {
    let mut output = Vec::new();
    for line in compact(text, 3).split('\n') {
        let mut current = String::new();
        let mut width = 0;
        for ch in display_line(line, pad_markers).chars() {
            let next = style.cells(ch);
            if width + next > columns {
                output.push(std::mem::take(&mut current));
                width = 0;
            }
            current.push(ch);
            width += next;
        }
        output.push(current);
    }
    output
}

struct Layout {
    style: Style,
    columns: usize,
    max_rows: usize,
    page_limit: usize,
}

fn push_page(
    pages: &mut Vec<Page>,
    rows: &[String],
    slots: Option<&[String]>,
    layout: Layout,
) -> io::Result<()> {
    let Layout {
        style,
        columns,
        max_rows,
        page_limit,
    } = layout;
    if pages.len() == page_limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "PXPIPE image page limit exceeded",
        ));
    }
    let wrapped = wrap(&rows.join("\n"), style, columns, false);
    let slots = slots.map(|slots| wrap(&slots.join("\n"), style, columns, false));
    let (glyph_width, glyph_height, ascent) = style.font.dimensions();
    let (cell_width, cell_height) = style.cell();
    let width = 2 * PADDING + columns * cell_width;
    let height = 2 * PADDING + wrapped.len().min(max_rows) * cell_height;
    let mut raster = vec![255; width * height];
    let role_color = slots.is_some();
    let mut colors = (role_color || style.color_cycle).then(|| vec![0u8; raster.len()]);
    let mut marker_red = (colors.is_none() && style.marker_red).then(|| vec![false; raster.len()]);
    let mut dropped = 0;
    let mut glyph_index = 0;
    for (row, line) in wrapped.iter().take(max_rows).enumerate() {
        let y = PADDING + row * cell_height;
        let mut column = 0;
        let mut marks = slots
            .as_ref()
            .and_then(|slots| slots.get(row))
            .map(|line| line.chars());
        for ch in line.chars() {
            if column >= columns {
                break;
            }
            let is_marker = ch == SENTINEL;
            let scale = if is_marker {
                style.marker_scale.max(1)
            } else {
                1
            };
            let aa = scale == 1 && style.aa;
            let color = if role_color {
                match marks.as_mut().and_then(Iterator::next) {
                    Some('\u{1}') => 1,
                    Some('\u{2}') => 2,
                    _ => 0,
                }
            } else {
                (glyph_index % 4 + 1) as u8
            };
            glyph_index += 1;
            if let Some(glyph) = style.font.glyph(ch, aa) {
                let x = PADDING + column * cell_width;
                let base_y = (y + ascent).saturating_sub(glyph.ascent);
                for dy in 0..glyph.height {
                    if scale > 1 && base_y + dy >= height {
                        break;
                    }
                    let row = (base_y + dy) * width;
                    for gx in 0..glyph.width {
                        let ink = glyph.ink(gx, dy);
                        if ink == 0 {
                            continue;
                        }
                        for sx in 0..scale {
                            let px = x + gx * scale + sx;
                            if scale > 1 && px >= width {
                                break;
                            }
                            let index = row + px;
                            if let Some(pixel) = raster.get_mut(index) {
                                *pixel = (*pixel).min(255 - ink);
                                if is_marker
                                    && !aa
                                    && let Some(mask) = &mut marker_red
                                {
                                    mask[index] = true;
                                }
                            }
                        }
                    }
                }
                let advance = glyph.cells * scale;
                if let Some(colors) = &mut colors {
                    let span = advance * if scale > 1 { cell_width } else { glyph_width };
                    for row in y..(y + glyph_height).min(height) {
                        for col in x..(x + span).min(width) {
                            let pixel = row * width + col;
                            if raster[pixel] < 255 {
                                colors[pixel] = color;
                            }
                        }
                    }
                }
                column += advance;
            } else {
                dropped += 1;
                column += 1;
            }
        }
    }
    if style.grid {
        grid(
            &mut raster,
            width,
            height,
            wrapped.len().min(max_rows),
            layout,
        );
    }
    if style.ink_dilate > 0 {
        dilate(&mut raster, width, height, style.ink_dilate);
    }
    let png = if let Some(colors) = colors {
        const ROLES: [[u16; 3]; 2] = [[150, 20, 20], [20, 40, 160]];
        const CYCLE: [[u16; 3]; 4] = [[20, 20, 20], [20, 40, 160], [150, 20, 20], [20, 110, 40]];
        let palette: &[[u16; 3]] = if role_color { &ROLES } else { &CYCLE };
        let mut rgb = Vec::with_capacity(raster.len() * 3);
        for (gray, role) in raster.into_iter().zip(colors) {
            if role == 0 {
                rgb.extend([gray; 3]);
            } else {
                let ink = u16::from(255 - gray);
                rgb.extend(
                    palette[role as usize - 1]
                        .map(|channel| ((255 * 255 - ink * (255 - channel) + 127) / 255) as u8),
                );
            }
        }
        encode_png(&rgb, width, height, 3)?
    } else if let Some(markers) = marker_red {
        let mut rgb = Vec::with_capacity(raster.len() * 3);
        for (gray, marker) in raster.into_iter().zip(markers) {
            rgb.extend(if marker && gray < 128 {
                [220, 0, 0]
            } else {
                [gray; 3]
            });
        }
        encode_png(&rgb, width, height, 3)?
    } else {
        encode_png(&raster, width, height, 1)?
    };
    pages.push(Page {
        png,
        width,
        height,
        dropped,
    });
    Ok(())
}

fn grid(raster: &mut [u8], width: usize, height: usize, rows: usize, layout: Layout) {
    let (cell_width, cell_height) = layout.style.cell();
    let (_, glyph_height, _) = layout.style.font.dimensions();
    let shade = |pixel: &mut u8| {
        if *pixel == 255 {
            *pixel = 230;
        }
    };
    for index in 0..rows {
        let y = PADDING + index * cell_height + glyph_height - 1;
        if y >= height {
            break;
        }
        for pixel in &mut raster[y * width..(y + 1) * width] {
            shade(pixel);
        }
    }
    let step = layout.style.grid_cols;
    if step > 0 && step < layout.columns {
        for column in (step..layout.columns).step_by(step) {
            let x = PADDING + column * cell_width;
            for row in raster.chunks_exact_mut(width) {
                shade(&mut row[x]);
            }
        }
    }
}

fn dilate(raster: &mut [u8], width: usize, height: usize, passes: usize) {
    let mut copy = raster.to_vec();
    for _ in 0..passes {
        for y in 0..height {
            for x in 0..width {
                let i = y * width + x;
                if raster[i] == 255
                    && ((x > 0 && raster[i - 1] < 255)
                        || (x + 1 < width && raster[i + 1] < 255)
                        || (y > 0 && raster[i - width] < 255)
                        || (y + 1 < height && raster[i + width] < 255))
                {
                    copy[i] = 0;
                }
            }
        }
        raster.copy_from_slice(&copy);
    }
}

fn encode_png(raster: &[u8], width: usize, height: usize, channels: usize) -> io::Result<Vec<u8>> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    let stride = width * channels;
    let mut scanline = vec![0; stride + 1];
    scanline[0] = 3;
    for (y, row) in raster.chunks_exact(stride).enumerate() {
        for (x, value) in row.iter().enumerate() {
            let left = if x >= channels {
                u16::from(row[x - channels])
            } else {
                0
            };
            let above = if y > 0 {
                u16::from(raster[(y - 1) * stride + x])
            } else {
                0
            };
            scanline[x + 1] = value.wrapping_sub(((left + above) / 2) as u8);
        }
        encoder.write_all(&scanline)?;
    }
    let data = encoder.finish()?;
    let mut output = Vec::with_capacity(data.len() + 57);
    output.extend(b"\x89PNG\r\n\x1a\n");
    let mut header = [0; 13];
    header[..4].copy_from_slice(&(width as u32).to_be_bytes());
    header[4..8].copy_from_slice(&(height as u32).to_be_bytes());
    header[8] = 8;
    header[9] = if channels == 3 { 2 } else { 0 };
    chunk(&mut output, b"IHDR", &header);
    chunk(&mut output, b"IDAT", &data);
    chunk(&mut output, b"IEND", &[]);
    Ok(output)
}

fn chunk(output: &mut Vec<u8>, name: &[u8; 4], data: &[u8]) {
    let mut crc = Crc::new();
    crc.update(name);
    crc.update(data);
    output.extend((data.len() as u32).to_be_bytes());
    output.extend(name);
    output.extend(data);
    output.extend(crc.sum().to_be_bytes());
}
