use std::io::Read;
use std::sync::OnceLock;

use flate2::read::ZlibDecoder;

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub enum Font {
    #[default]
    Spleen,
    JetBrains10,
    JetBrains12,
    JetBrains14,
}

impl Font {
    pub fn parse(value: &serde_json::Value) -> Option<Self> {
        match value.as_str()? {
            "spleen-5x8" => Some(Self::Spleen),
            "jetbrains-mono-10" => Some(Self::JetBrains10),
            "jetbrains-mono-12" => Some(Self::JetBrains12),
            "jetbrains-mono-14" => Some(Self::JetBrains14),
            _ => None,
        }
    }

    pub fn dimensions(self) -> (usize, usize, usize) {
        match self {
            Self::Spleen => (5, 8, 7),
            Self::JetBrains10 => (6, 11, 9),
            Self::JetBrains12 => (8, 13, 11),
            Self::JetBrains14 => (9, 16, 13),
        }
    }

    fn atlas(self, aa: bool) -> &'static Atlas {
        macro_rules! load {
            ($cell:ident, $file:literal) => {
                $cell.get_or_init(|| Atlas::load(self, aa, include_bytes!($file)))
            };
        }
        match (self, aa) {
            (Self::Spleen, true) => {
                load!(SPLEEN_GRAY, "../assets/pxpipe/spleen-unifont-atlas.zlib")
            }
            (Self::Spleen, false) => {
                load!(SPLEEN_BITS, "../assets/pxpipe/spleen-unifont-bitmap.zlib")
            }
            (Self::JetBrains10, true) => {
                load!(JB10_GRAY, "../assets/pxpipe/jetbrains-mono-10-gray.zlib")
            }
            (Self::JetBrains10, false) => {
                load!(JB10_BITS, "../assets/pxpipe/jetbrains-mono-10-bitmap.zlib")
            }
            (Self::JetBrains12, true) => {
                load!(JB12_GRAY, "../assets/pxpipe/jetbrains-mono-12-gray.zlib")
            }
            (Self::JetBrains12, false) => {
                load!(JB12_BITS, "../assets/pxpipe/jetbrains-mono-12-bitmap.zlib")
            }
            (Self::JetBrains14, true) => {
                load!(JB14_GRAY, "../assets/pxpipe/jetbrains-mono-14-gray.zlib")
            }
            (Self::JetBrains14, false) => {
                load!(JB14_BITS, "../assets/pxpipe/jetbrains-mono-14-bitmap.zlib")
            }
        }
    }

    pub(super) fn cells(self, ch: char) -> usize {
        self.glyph(ch, false).map_or(1, |glyph| glyph.cells)
    }

    pub(super) fn glyph(self, ch: char, aa: bool) -> Option<Glyph> {
        self.atlas(aa).glyph(ch).or_else(|| {
            if self == Self::Spleen {
                None
            } else {
                Self::Spleen.atlas(aa).glyph(ch)
            }
        })
    }
}

struct Atlas {
    aa: bool,
    font: Font,
    codepoints: Vec<u32>,
    offsets: Vec<u32>,
    wide: Vec<u8>,
    pixels: Vec<u8>,
}

impl Atlas {
    fn load(font: Font, aa: bool, compressed: &[u8]) -> Self {
        let mut raw = Vec::new();
        ZlibDecoder::new(compressed)
            .read_to_end(&mut raw)
            .expect("embedded PXPIPE glyph atlas");
        let count = words(&raw[..4]).next().unwrap() as usize;
        let raw = &raw[4..];
        let codepoints = words(&raw[..4 * count]).collect();
        let offsets = words(&raw[4 * count..8 * count]).collect();
        let (wide, pixels) = raw[8 * count..].split_at(count);
        Self {
            aa,
            font,
            codepoints,
            offsets,
            wide: wide.to_vec(),
            pixels: pixels.to_vec(),
        }
    }

    fn glyph(&'static self, ch: char) -> Option<Glyph> {
        let index = self.codepoints.binary_search(&(ch as u32)).ok()?;
        let (width, height, ascent) = self.font.dimensions();
        let cells = 1 + usize::from(self.wide[index]);
        Some(Glyph {
            atlas: self,
            cells,
            width: width * cells,
            height,
            ascent,
            offset: self.offsets[index] as usize,
        })
    }
}

pub(super) struct Glyph {
    atlas: &'static Atlas,
    offset: usize,
    pub cells: usize,
    pub width: usize,
    pub height: usize,
    pub ascent: usize,
}

impl Glyph {
    pub fn ink(&self, x: usize, y: usize) -> u8 {
        let index = self.offset + y * self.width + x;
        if self.atlas.aa {
            self.atlas.pixels[index]
        } else {
            let mask = 0x80 >> (index % 8);
            if self.atlas.pixels[index / 8] & mask == 0 {
                0
            } else {
                255
            }
        }
    }
}

fn words(bytes: &[u8]) -> impl Iterator<Item = u32> + '_ {
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .copied()
        .map(u32::from_le_bytes)
}

static SPLEEN_GRAY: OnceLock<Atlas> = OnceLock::new();
static SPLEEN_BITS: OnceLock<Atlas> = OnceLock::new();
static JB10_GRAY: OnceLock<Atlas> = OnceLock::new();
static JB10_BITS: OnceLock<Atlas> = OnceLock::new();
static JB12_GRAY: OnceLock<Atlas> = OnceLock::new();
static JB12_BITS: OnceLock<Atlas> = OnceLock::new();
static JB14_GRAY: OnceLock<Atlas> = OnceLock::new();
static JB14_BITS: OnceLock<Atlas> = OnceLock::new();
