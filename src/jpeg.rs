//! Baseline JPEG decoding for `DCTDecode` image streams.
//!
//! Only sequential Huffman JPEG (SOF0/SOF1) with 8-bit samples and one or
//! three components is decoded; every other coding process is reported as
//! unsupported so the caller keeps the original stream.  Every size derived
//! from the input is checked before it is used, and truncated entropy-coded
//! data is an error rather than a source of invented samples.  Upsampling and
//! colour conversion follow libjpeg (fancy upsampling for 2:1 ratios, BT.601
//! full-range YCbCr).

use std::f32::consts::{FRAC_1_SQRT_2, PI};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum JpegError {
    Unsupported(&'static str),
    Invalid(&'static str),
    Limit,
}

impl JpegError {
    pub(crate) fn message(self) -> String {
        match self {
            Self::Unsupported(what) => format!("unsupported JPEG ({what})"),
            Self::Invalid(what) => format!("invalid JPEG data ({what})"),
            Self::Limit => "decoded JPEG exceeds max_decoded_stream_bytes".to_string(),
        }
    }
}

/// Zero bits the entropy decoder may consume past the end of a segment.
/// Encoders pad the final byte of a segment with one bits, so complete data
/// needs none.  libjpeg substitutes zeros without limit, and even a few zero
/// bits decode as end-of-block codes that complete truncated blocks with
/// invented samples, so none are allowed.
const MAX_ZERO_FILL_BITS: u32 = 0;

/// Plane storage allowed beyond `max_output_bytes`: component planes are
/// padded to whole 8x8 blocks, which matters for very thin images.
const PLANE_SLACK_BYTES: usize = 1 << 20;

/// Natural (row-major) position of each zig-zag coefficient index.
const ZIGZAG: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

/// Decode a baseline JPEG into interleaved 8-bit samples (gray or RGB),
/// row-major, exactly `expected_width * expected_height *
/// expected_components` bytes.
///
/// `color_transform` is the `/ColorTransform` decode parameter; without it
/// an Adobe APP14 marker decides, and otherwise three components are YCbCr.
pub(crate) fn decode(
    data: &[u8],
    expected_width: u32,
    expected_height: u32,
    expected_components: usize,
    max_output_bytes: usize,
    color_transform: Option<bool>,
) -> Result<Vec<u8>, JpegError> {
    if expected_components != 1 && expected_components != 3 {
        return Err(JpegError::Unsupported("component count"));
    }
    let width = usize::try_from(expected_width).map_err(|_| JpegError::Limit)?;
    let height = usize::try_from(expected_height).map_err(|_| JpegError::Limit)?;
    let output_len = width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(expected_components))
        .ok_or(JpegError::Limit)?;
    if output_len == 0 {
        return Err(JpegError::Invalid("empty image"));
    }
    if output_len > max_output_bytes {
        return Err(JpegError::Limit);
    }
    let mut decoder = Decoder {
        data,
        pos: 0,
        width,
        height,
        components: expected_components,
        plane_limit: max_output_bytes
            .saturating_mul(2)
            .saturating_add(PLANE_SLACK_BYTES),
        quant: [None; 4],
        dc_tables: [None, None, None, None],
        ac_tables: [None, None, None, None],
        restart_interval: 0,
        adobe_transform: None,
        frame: None,
        idct: Idct::new(),
    };
    let frame = decoder.run()?;
    let transform = expected_components == 3
        && color_transform.unwrap_or_else(|| decoder.adobe_transform.is_none_or(|t| t != 0));
    frame.render(transform, output_len)
}

struct Decoder<'a> {
    data: &'a [u8],
    pos: usize,
    width: usize,
    height: usize,
    components: usize,
    plane_limit: usize,
    quant: [Option<[u16; 64]>; 4],
    dc_tables: [Option<Huffman>; 4],
    ac_tables: [Option<Huffman>; 4],
    restart_interval: usize,
    adobe_transform: Option<u8>,
    frame: Option<Frame>,
    idct: Idct,
}

impl<'a> Decoder<'a> {
    fn run(&mut self) -> Result<Frame, JpegError> {
        if self.data.get(..2) != Some([0xFF, 0xD8].as_slice()) {
            return Err(JpegError::Invalid("missing SOI marker"));
        }
        self.pos = 2;
        loop {
            match self.next_marker()? {
                0xD9 => return self.finish(),
                0xC0 | 0xC1 => {
                    let body = self.segment()?;
                    self.read_frame(body)?;
                }
                0xC4 => {
                    let body = self.segment()?;
                    self.read_huffman_tables(body)?;
                }
                0xDB => {
                    let body = self.segment()?;
                    self.read_quant_tables(body)?;
                }
                0xDD => {
                    let body = self.segment()?;
                    let interval = match *body {
                        [high, low] => u16::from_be_bytes([high, low]),
                        _ => return Err(JpegError::Invalid("DRI length")),
                    };
                    self.restart_interval = usize::from(interval);
                }
                0xDA => {
                    let body = self.segment()?;
                    self.read_scan(body)?;
                }
                0xEE => {
                    let body = self.segment()?;
                    if body.len() >= 12 && body.starts_with(b"Adobe") {
                        self.adobe_transform = body.get(11).copied();
                    }
                }
                0xE0..=0xED | 0xEF..=0xFE => {
                    self.segment()?;
                }
                0x01 => {}
                0xC2 => return Err(JpegError::Unsupported("progressive JPEG")),
                0xC3 => return Err(JpegError::Unsupported("lossless JPEG")),
                0xC5..=0xC7 | 0xDE | 0xDF => {
                    return Err(JpegError::Unsupported("hierarchical JPEG"))
                }
                0xC8 => return Err(JpegError::Unsupported("JPEG extension marker")),
                0xC9..=0xCF => return Err(JpegError::Unsupported("arithmetic-coded JPEG")),
                0xDC => return Err(JpegError::Unsupported("DNL marker")),
                0xD0..=0xD7 => return Err(JpegError::Invalid("restart marker outside a scan")),
                0xD8 => return Err(JpegError::Invalid("repeated SOI marker")),
                _ => return Err(JpegError::Invalid("reserved marker")),
            }
        }
    }

    fn finish(&mut self) -> Result<Frame, JpegError> {
        let frame = self
            .frame
            .take()
            .ok_or(JpegError::Invalid("EOI before any frame"))?;
        if frame.components.iter().any(|component| !component.decoded) {
            return Err(JpegError::Invalid("image data is incomplete"));
        }
        Ok(frame)
    }

    /// Read the marker at `pos`, skipping 0xFF fill bytes.
    fn next_marker(&mut self) -> Result<u8, JpegError> {
        if self.data.get(self.pos) != Some(&0xFF) {
            return Err(JpegError::Invalid("expected a marker"));
        }
        let mut pos = self.pos;
        loop {
            pos = pos.saturating_add(1);
            match self.data.get(pos) {
                Some(0xFF) => {}
                Some(0) => return Err(JpegError::Invalid("expected a marker")),
                Some(&code) => {
                    self.pos = pos.saturating_add(1);
                    return Ok(code);
                }
                None => return Err(JpegError::Invalid("missing EOI marker")),
            }
        }
    }

    fn segment(&mut self) -> Result<&'a [u8], JpegError> {
        let data = self.data;
        let length =
            usize::from(read_u16(data, self.pos).ok_or(JpegError::Invalid("truncated segment"))?);
        if length < 2 {
            return Err(JpegError::Invalid("segment length"));
        }
        let start = self
            .pos
            .checked_add(2)
            .ok_or(JpegError::Invalid("truncated segment"))?;
        let end = self
            .pos
            .checked_add(length)
            .ok_or(JpegError::Invalid("truncated segment"))?;
        let body = data
            .get(start..end)
            .ok_or(JpegError::Invalid("truncated segment"))?;
        self.pos = end;
        Ok(body)
    }

    fn read_frame(&mut self, body: &[u8]) -> Result<(), JpegError> {
        if self.frame.is_some() {
            return Err(JpegError::Invalid("multiple frames"));
        }
        let truncated = JpegError::Invalid("truncated SOF");
        match body.first() {
            Some(8) => {}
            Some(12 | 16) => return Err(JpegError::Unsupported("12-bit precision")),
            Some(_) => return Err(JpegError::Invalid("sample precision")),
            None => return Err(truncated),
        }
        let height = usize::from(read_u16(body, 1).ok_or(truncated)?);
        let width = usize::from(read_u16(body, 3).ok_or(truncated)?);
        let count = usize::from(*body.get(5).ok_or(truncated)?);
        if height == 0 {
            return Err(JpegError::Unsupported("height defined by DNL"));
        }
        match count {
            1 | 3 => {}
            2 | 4 => return Err(JpegError::Unsupported("2 or 4 components (CMYK/YCCK)")),
            _ => return Err(JpegError::Invalid("component count")),
        }
        let specs = body.get(6..).ok_or(truncated)?;
        if specs.len() != count.saturating_mul(3) {
            return Err(JpegError::Invalid("SOF length"));
        }
        if width != self.width || height != self.height {
            return Err(JpegError::Invalid(
                "dimensions do not match the image dictionary",
            ));
        }
        if count != self.components {
            return Err(JpegError::Invalid(
                "component count does not match the colour space",
            ));
        }

        let mut parsed: Vec<(u8, usize, usize, usize)> = Vec::with_capacity(count);
        for spec in specs.chunks_exact(3) {
            let &[id, sampling, quant] = spec else {
                return Err(truncated);
            };
            let h = usize::from(sampling >> 4);
            let v = usize::from(sampling & 0x0F);
            if !(1..=4).contains(&h) || !(1..=4).contains(&v) {
                return Err(JpegError::Invalid("sampling factor"));
            }
            if quant > 3 {
                return Err(JpegError::Invalid("quantization table id"));
            }
            if parsed.iter().any(|&(other, ..)| other == id) {
                return Err(JpegError::Invalid("duplicate component id"));
            }
            parsed.push((id, h, v, usize::from(quant)));
        }
        let max_h = parsed.iter().map(|&(_, h, ..)| h).max().unwrap_or(1);
        let max_v = parsed.iter().map(|&(_, _, v, _)| v).max().unwrap_or(1);
        if parsed
            .iter()
            .any(|&(_, h, v, _)| !max_h.is_multiple_of(h) || !max_v.is_multiple_of(v))
        {
            return Err(JpegError::Unsupported("fractional sampling ratio"));
        }

        let mut components = Vec::with_capacity(count);
        let mut plane_lens = Vec::with_capacity(count);
        for &(id, h, v, quant_id) in &parsed {
            let component_width = width
                .checked_mul(h)
                .ok_or(JpegError::Limit)?
                .div_ceil(max_h);
            let component_height = height
                .checked_mul(v)
                .ok_or(JpegError::Limit)?
                .div_ceil(max_v);
            let blocks_x = component_width.div_ceil(8);
            let blocks_y = component_height.div_ceil(8);
            let stride = blocks_x.checked_mul(8).ok_or(JpegError::Limit)?;
            plane_lens.push(
                blocks_y
                    .checked_mul(8)
                    .and_then(|rows| rows.checked_mul(stride))
                    .ok_or(JpegError::Limit)?,
            );
            components.push(Component {
                id,
                h,
                v,
                quant_id,
                width: component_width,
                height: component_height,
                blocks_x,
                blocks_y,
                stride,
                plane: Vec::new(),
                decoded: false,
            });
        }
        let total = plane_lens
            .iter()
            .try_fold(0usize, |total, &len| total.checked_add(len))
            .ok_or(JpegError::Limit)?;
        if total > self.plane_limit {
            return Err(JpegError::Limit);
        }
        for (component, &len) in components.iter_mut().zip(&plane_lens) {
            component.plane = zeroed(len)?;
        }
        self.frame = Some(Frame {
            width,
            height,
            max_h,
            max_v,
            mcus_x: width.div_ceil(max_h.saturating_mul(8)),
            mcus_y: height.div_ceil(max_v.saturating_mul(8)),
            components,
        });
        Ok(())
    }

    fn read_huffman_tables(&mut self, body: &[u8]) -> Result<(), JpegError> {
        let truncated = JpegError::Invalid("truncated DHT");
        let mut rest = body;
        while let Some((&class_id, after)) = rest.split_first() {
            let class = class_id >> 4;
            let id = usize::from(class_id & 0x0F);
            if class > 1 || id > 3 {
                return Err(JpegError::Invalid("Huffman table class or id"));
            }
            let counts = after.get(..16).ok_or(truncated)?;
            let total: usize = counts.iter().map(|&count| usize::from(count)).sum();
            if total > 256 {
                return Err(JpegError::Invalid("Huffman table has too many symbols"));
            }
            let end = total.saturating_add(16);
            let values = after.get(16..end).ok_or(truncated)?;
            let table = Huffman::new(counts, values)?;
            let slot = if class == 0 {
                self.dc_tables.get_mut(id)
            } else {
                self.ac_tables.get_mut(id)
            }
            .ok_or(JpegError::Invalid("Huffman table class or id"))?;
            *slot = Some(table);
            rest = after.get(end..).ok_or(truncated)?;
        }
        Ok(())
    }

    fn read_quant_tables(&mut self, body: &[u8]) -> Result<(), JpegError> {
        let truncated = JpegError::Invalid("truncated DQT");
        let mut rest = body;
        while let Some((&spec, after)) = rest.split_first() {
            let id = usize::from(spec & 0x0F);
            let size = match spec >> 4 {
                0 => 64,
                1 => 128,
                _ => return Err(JpegError::Invalid("quantization table precision")),
            };
            let bytes = after.get(..size).ok_or(truncated)?;
            let mut table = [0u16; 64];
            if size == 64 {
                for (slot, &byte) in table.iter_mut().zip(bytes) {
                    *slot = u16::from(byte);
                }
            } else {
                for (slot, pair) in table.iter_mut().zip(bytes.chunks_exact(2)) {
                    if let &[high, low] = pair {
                        *slot = u16::from_be_bytes([high, low]);
                    }
                }
            }
            *self
                .quant
                .get_mut(id)
                .ok_or(JpegError::Invalid("quantization table id"))? = Some(table);
            rest = after.get(size..).ok_or(truncated)?;
        }
        Ok(())
    }

    fn read_scan(&mut self, body: &[u8]) -> Result<(), JpegError> {
        let frame = self
            .frame
            .as_mut()
            .ok_or(JpegError::Invalid("scan before frame"))?;
        let (&count, rest) = body
            .split_first()
            .ok_or(JpegError::Invalid("truncated SOS"))?;
        let count = usize::from(count);
        if count == 0 || count > frame.components.len() {
            return Err(JpegError::Invalid("scan component count"));
        }
        let specs_len = count.saturating_mul(2);
        if rest.len() != specs_len.saturating_add(3) {
            return Err(JpegError::Invalid("SOS length"));
        }
        let mut scan: Vec<ScanComponent<'_>> = Vec::with_capacity(count);
        for spec in rest.chunks_exact(2).take(count) {
            let &[id, tables] = spec else {
                return Err(JpegError::Invalid("truncated SOS"));
            };
            let index = frame
                .components
                .iter()
                .position(|component| component.id == id)
                .ok_or(JpegError::Invalid("scan references an unknown component"))?;
            if scan.iter().any(|other| other.index == index) {
                return Err(JpegError::Invalid("duplicate scan component"));
            }
            let component = frame
                .components
                .get(index)
                .ok_or(JpegError::Invalid("scan references an unknown component"))?;
            if component.decoded {
                return Err(JpegError::Invalid(
                    "component appears in more than one scan",
                ));
            }
            let dc = self
                .dc_tables
                .get(usize::from(tables >> 4))
                .and_then(Option::as_ref)
                .ok_or(JpegError::Invalid("undefined Huffman table"))?;
            let ac = self
                .ac_tables
                .get(usize::from(tables & 0x0F))
                .and_then(Option::as_ref)
                .ok_or(JpegError::Invalid("undefined Huffman table"))?;
            let quant = self
                .quant
                .get(component.quant_id)
                .copied()
                .flatten()
                .ok_or(JpegError::Invalid("undefined quantization table"))?;
            scan.push(ScanComponent {
                index,
                dc,
                ac,
                quant,
                predictor: 0,
            });
        }
        if rest.get(specs_len..) != Some([0, 63, 0].as_slice()) {
            return Err(JpegError::Invalid("not a sequential scan"));
        }
        if count > 1 {
            let blocks: usize = scan
                .iter()
                .filter_map(|entry| frame.components.get(entry.index))
                .map(|component| component.h.saturating_mul(component.v))
                .sum();
            if blocks > 10 {
                return Err(JpegError::Invalid("too many blocks per MCU"));
            }
        }
        let end = decode_scan(
            self.data,
            self.pos,
            frame,
            &mut scan,
            self.restart_interval,
            &self.idct,
        )?;
        for entry in &scan {
            if let Some(component) = frame.components.get_mut(entry.index) {
                component.decoded = true;
            }
        }
        self.pos = end;
        Ok(())
    }
}

fn read_u16(data: &[u8], at: usize) -> Option<u16> {
    let high = *data.get(at)?;
    let low = *data.get(at.checked_add(1)?)?;
    Some(u16::from_be_bytes([high, low]))
}

fn zeroed(len: usize) -> Result<Vec<u8>, JpegError> {
    let mut buffer = Vec::new();
    buffer
        .try_reserve_exact(len)
        .map_err(|_| JpegError::Limit)?;
    buffer.resize(len, 0);
    Ok(buffer)
}

struct Frame {
    width: usize,
    height: usize,
    max_h: usize,
    max_v: usize,
    mcus_x: usize,
    mcus_y: usize,
    components: Vec<Component>,
}

impl Frame {
    fn render(&self, transform: bool, output_len: usize) -> Result<Vec<u8>, JpegError> {
        let mut output = Vec::new();
        output
            .try_reserve_exact(output_len)
            .map_err(|_| JpegError::Limit)?;
        let mut rows = Vec::with_capacity(self.components.len());
        for _ in &self.components {
            rows.push(zeroed(self.width)?);
        }
        let mut sums = Vec::new();
        for y in 0..self.height {
            for (component, row) in self.components.iter().zip(rows.iter_mut()) {
                component.upsample_row(y, self.max_h, self.max_v, row, &mut sums);
            }
            match rows.as_slice() {
                [gray] => output.extend_from_slice(gray),
                [first, second, third] => {
                    for ((&a, &b), &c) in first.iter().zip(second).zip(third) {
                        if transform {
                            output.extend_from_slice(&ycc_to_rgb(a, b, c));
                        } else {
                            output.extend_from_slice(&[a, b, c]);
                        }
                    }
                }
                _ => return Err(JpegError::Unsupported("component count")),
            }
        }
        if output.len() != output_len {
            return Err(JpegError::Invalid("decoded sample count"));
        }
        Ok(output)
    }
}

struct Component {
    id: u8,
    h: usize,
    v: usize,
    quant_id: usize,
    /// Sample dimensions of the (possibly subsampled) component.
    width: usize,
    height: usize,
    blocks_x: usize,
    blocks_y: usize,
    stride: usize,
    plane: Vec<u8>,
    decoded: bool,
}

impl Component {
    fn row(&self, y: usize) -> &[u8] {
        let start = y.saturating_mul(self.stride);
        self.plane
            .get(start..start.saturating_add(self.width))
            .unwrap_or_default()
    }

    fn store_block(&mut self, block_x: usize, block_y: usize, samples: &[u8; 64]) {
        if block_x >= self.blocks_x || block_y >= self.blocks_y {
            return;
        }
        let x = block_x.saturating_mul(8);
        for (row, chunk) in samples.chunks_exact(8).enumerate() {
            let start = block_y
                .saturating_mul(8)
                .saturating_add(row)
                .saturating_mul(self.stride)
                .saturating_add(x);
            if let Some(destination) = self.plane.get_mut(start..start.saturating_add(8)) {
                destination.copy_from_slice(chunk);
            }
        }
    }

    /// Produce output row `y` of this component at full resolution, choosing
    /// the same upsampler libjpeg-turbo does for each sampling ratio.
    fn upsample_row(
        &self,
        y: usize,
        max_h: usize,
        max_v: usize,
        out: &mut [u8],
        sums: &mut Vec<i32>,
    ) {
        let h_ratio = max_h.checked_div(self.h).unwrap_or(1);
        let v_ratio = max_v.checked_div(self.v).unwrap_or(1);
        let fancy_h = h_ratio == 2 && self.width > 2;
        match (h_ratio, v_ratio) {
            (1, 1) => {
                for (sample, &value) in out.iter_mut().zip(self.row(y)) {
                    *sample = value;
                }
            }
            (2, 1) if fancy_h => {
                sums.clear();
                sums.extend(self.row(y).iter().map(|&value| i32::from(value) * 4));
                fancy_horizontal(sums, 4, 8, out);
            }
            (1, 2) => {
                let (near, far, bias) = self.vertical_pair(y, 1, 2);
                for (sample, (&a, &b)) in out.iter_mut().zip(near.iter().zip(far)) {
                    *sample = clamp_u8((i32::from(a) * 3 + i32::from(b) + bias) >> 2);
                }
            }
            (2, 2) if fancy_h => {
                let (near, far, _) = self.vertical_pair(y, 0, 0);
                sums.clear();
                sums.extend(
                    near.iter()
                        .zip(far)
                        .map(|(&a, &b)| i32::from(a) * 3 + i32::from(b)),
                );
                fancy_horizontal(sums, 8, 7, out);
            }
            _ => {
                let source = self.row(y.checked_div(v_ratio).unwrap_or(0));
                for (x, sample) in out.iter_mut().enumerate() {
                    let column = x.checked_div(h_ratio).unwrap_or(0);
                    *sample = source.get(column).copied().unwrap_or(0);
                }
            }
        }
    }

    /// The input row nearest to output row `y` for a 2:1 vertical ratio and
    /// its other neighbour; image edges replicate the first and last rows.
    fn vertical_pair(&self, y: usize, upper_bias: i32, lower_bias: i32) -> (&[u8], &[u8], i32) {
        let row = y / 2;
        if y.is_multiple_of(2) {
            (self.row(row), self.row(row.saturating_sub(1)), upper_bias)
        } else {
            let below = row.saturating_add(1).min(self.height.saturating_sub(1));
            (self.row(row), self.row(below), lower_bias)
        }
    }
}

/// libjpeg's triangle filter for a 2:1 horizontal ratio over column sums
/// scaled by 4 (h2v1) or vertically weighted 3:1 (h2v2).
fn fancy_horizontal(sums: &[i32], left_bias: i32, right_bias: i32, out: &mut [u8]) {
    for (index, &this) in sums.iter().enumerate() {
        let left = match index.checked_sub(1).and_then(|previous| sums.get(previous)) {
            Some(&previous) => (this * 3 + previous + left_bias) >> 4,
            None => (this * 4 + 8) >> 4,
        };
        let right = match sums.get(index.saturating_add(1)) {
            Some(&next) => (this * 3 + next + right_bias) >> 4,
            None => (this * 4 + 7) >> 4,
        };
        let column = index.saturating_mul(2);
        if let Some(sample) = out.get_mut(column) {
            *sample = clamp_u8(left);
        }
        if let Some(sample) = out.get_mut(column.saturating_add(1)) {
            *sample = clamp_u8(right);
        }
    }
}

fn clamp_u8(value: i32) -> u8 {
    value.clamp(0, 255) as u8
}

/// JFIF YCbCr to RGB with libjpeg's 16-bit fixed-point constants.
fn ycc_to_rgb(y: u8, cb: u8, cr: u8) -> [u8; 3] {
    const ONE_HALF: i32 = 1 << 15;
    let y = i32::from(y);
    let cb = i32::from(cb) - 128;
    let cr = i32::from(cr) - 128;
    [
        clamp_u8(y + ((91_881 * cr + ONE_HALF) >> 16)),
        clamp_u8(y + ((-22_554 * cb - 46_802 * cr + ONE_HALF) >> 16)),
        clamp_u8(y + ((116_130 * cb + ONE_HALF) >> 16)),
    ]
}

struct ScanComponent<'t> {
    index: usize,
    dc: &'t Huffman,
    ac: &'t Huffman,
    quant: [u16; 64],
    predictor: i32,
}

/// Decode one scan starting at `start`; returns the position of the marker
/// that follows its entropy-coded data.
fn decode_scan(
    data: &[u8],
    start: usize,
    frame: &mut Frame,
    scan: &mut [ScanComponent<'_>],
    restart_interval: usize,
    idct: &Idct,
) -> Result<usize, JpegError> {
    let interleaved = scan.len() > 1;
    let (mcus_x, mcus_y) = match scan {
        [single] => {
            let component = frame
                .components
                .get(single.index)
                .ok_or(JpegError::Invalid("scan references an unknown component"))?;
            (component.blocks_x, component.blocks_y)
        }
        _ => (frame.mcus_x, frame.mcus_y),
    };
    let mut reader = BitReader::new(data, start);
    let mut coefficients = [0i32; 64];
    let mut samples = [0u8; 64];
    let mut mcus_done = 0usize;
    let mut next_restart = 0u8;
    for mcu_y in 0..mcus_y {
        for mcu_x in 0..mcus_x {
            if restart_interval > 0 && mcus_done > 0 && mcus_done.is_multiple_of(restart_interval) {
                reader.restart(next_restart)?;
                next_restart = (next_restart + 1) & 7;
                for entry in scan.iter_mut() {
                    entry.predictor = 0;
                }
            }
            for entry in scan.iter_mut() {
                let component = frame
                    .components
                    .get_mut(entry.index)
                    .ok_or(JpegError::Invalid("scan references an unknown component"))?;
                let (h, v) = if interleaved {
                    (component.h, component.v)
                } else {
                    (1, 1)
                };
                for block_row in 0..v {
                    for block_column in 0..h {
                        decode_block(&mut reader, entry, &mut coefficients)?;
                        idct.transform(&coefficients, &mut samples);
                        component.store_block(
                            mcu_x.saturating_mul(h).saturating_add(block_column),
                            mcu_y.saturating_mul(v).saturating_add(block_row),
                            &samples,
                        );
                    }
                }
            }
            mcus_done = mcus_done.saturating_add(1);
        }
    }
    reader.skip_to_marker()?;
    Ok(reader.pos)
}

/// Huffman-decode and dequantize one 8x8 block into natural order.
fn decode_block(
    reader: &mut BitReader<'_>,
    entry: &mut ScanComponent<'_>,
    coefficients: &mut [i32; 64],
) -> Result<(), JpegError> {
    coefficients.fill(0);
    let size = reader.decode(entry.dc)?;
    if size > 11 {
        return Err(JpegError::Invalid("DC coefficient size"));
    }
    let difference = reader.receive_extend(size)?;
    entry.predictor = entry
        .predictor
        .checked_add(difference)
        .ok_or(JpegError::Invalid("DC predictor overflow"))?;
    if let (Some(slot), Some(&quant)) = (coefficients.first_mut(), entry.quant.first()) {
        *slot = entry.predictor.saturating_mul(i32::from(quant));
    }
    let mut index = 1usize;
    while index < 64 {
        let symbol = reader.decode(entry.ac)?;
        let run = usize::from(symbol >> 4);
        let size = symbol & 0x0F;
        if size == 0 {
            if run != 15 {
                break;
            }
            index = index.saturating_add(16);
            if index > 64 {
                return Err(JpegError::Invalid("AC run past the end of the block"));
            }
            continue;
        }
        index = index.saturating_add(run);
        if index > 63 {
            return Err(JpegError::Invalid("AC run past the end of the block"));
        }
        if size > 10 {
            return Err(JpegError::Invalid("AC coefficient size"));
        }
        let value = reader.receive_extend(size)?;
        let natural = ZIGZAG.get(index).copied().unwrap_or(0);
        let quant = entry.quant.get(index).copied().unwrap_or(0);
        if let Some(slot) = coefficients.get_mut(natural) {
            *slot = value.saturating_mul(i32::from(quant));
        }
        index = index.saturating_add(1);
    }
    Ok(())
}

struct Huffman {
    /// `(length << 8) | symbol` for every 8-bit prefix that starts with a
    /// code of at most 8 bits; 0 elsewhere.
    lookup: [u16; 256],
    /// Largest code of each length, or -1 when there is none.
    max_code: [i32; 17],
    /// Symbol index minus code for the codes of each length.
    value_offset: [i32; 17],
    values: [u8; 256],
}

impl Huffman {
    fn new(counts: &[u8], symbols: &[u8]) -> Result<Self, JpegError> {
        let mut table = Self {
            lookup: [0; 256],
            max_code: [-1; 17],
            value_offset: [0; 17],
            values: [0; 256],
        };
        for (slot, &symbol) in table.values.iter_mut().zip(symbols) {
            *slot = symbol;
        }
        let mut code = 0u32;
        let mut index = 0u32;
        for (length, &count) in (1u32..=16).zip(counts) {
            let count = u32::from(count);
            if count > 0 {
                let first = code;
                code = code.saturating_add(count);
                // As in libjpeg, the all-ones code of a length is reserved.
                if code >= 1u32 << length {
                    return Err(JpegError::Invalid("Huffman code overflow"));
                }
                let slot = length as usize;
                if let Some(max) = table.max_code.get_mut(slot) {
                    *max = (code - 1) as i32;
                }
                if let Some(offset) = table.value_offset.get_mut(slot) {
                    *offset = index as i32 - first as i32;
                }
                if length <= 8 {
                    let shift = 8 - length;
                    for k in 0..count {
                        let symbol = symbols
                            .get(index.saturating_add(k) as usize)
                            .copied()
                            .ok_or(JpegError::Invalid("truncated DHT"))?;
                        let start = ((first + k) << shift) as usize;
                        let entry = ((length << 8) | u32::from(symbol)) as u16;
                        for slot in table.lookup.iter_mut().skip(start).take(1 << shift) {
                            *slot = entry;
                        }
                    }
                }
                index = index.saturating_add(count);
            }
            code <<= 1;
        }
        Ok(table)
    }
}

struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    bits: u64,
    count: u32,
    /// Zero bits appended past the end of the segment that are still buffered.
    fill_bits: u32,
    fill_used: u32,
    at_marker: bool,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8], pos: usize) -> Self {
        Self {
            data,
            pos,
            bits: 0,
            count: 0,
            fill_bits: 0,
            fill_used: 0,
            at_marker: false,
        }
    }

    fn refill(&mut self) {
        while self.count <= 56 {
            let byte = if self.at_marker {
                None
            } else {
                self.next_byte()
            };
            self.bits = (self.bits << 8) | u64::from(byte.unwrap_or(0));
            self.count += 8;
            if byte.is_none() {
                self.fill_bits += 8;
            }
        }
    }

    /// The next entropy-coded byte, undoing 0xFF00 stuffing; `None` at a
    /// marker or the end of the data.
    fn next_byte(&mut self) -> Option<u8> {
        match self.data.get(self.pos).copied() {
            Some(0xFF) => {
                let mut next = self.pos.saturating_add(1);
                while self.data.get(next) == Some(&0xFF) {
                    next = next.saturating_add(1);
                }
                if self.data.get(next) == Some(&0) {
                    self.pos = next.saturating_add(1);
                    Some(0xFF)
                } else {
                    self.at_marker = true;
                    None
                }
            }
            Some(byte) => {
                self.pos = self.pos.saturating_add(1);
                Some(byte)
            }
            None => {
                self.at_marker = true;
                None
            }
        }
    }

    /// The next `n` (1..=16) buffered bits; the caller has refilled.
    fn peek(&self, n: u32) -> u32 {
        let shift = self.count.saturating_sub(n);
        let mask = (1u64 << n.min(16)) - 1;
        (self.bits.checked_shr(shift).unwrap_or(0) & mask) as u32
    }

    fn consume(&mut self, n: u32) -> Result<(), JpegError> {
        let real = self.count.saturating_sub(self.fill_bits);
        if n > real {
            let fill = n - real;
            self.fill_bits = self.fill_bits.saturating_sub(fill);
            self.fill_used = self.fill_used.saturating_add(fill);
            if self.fill_used > MAX_ZERO_FILL_BITS {
                return Err(JpegError::Invalid("truncated entropy-coded data"));
            }
        }
        self.count = self.count.saturating_sub(n);
        Ok(())
    }

    fn decode(&mut self, table: &Huffman) -> Result<u8, JpegError> {
        if self.count < 16 {
            self.refill();
        }
        let entry = table
            .lookup
            .get(self.peek(8) as usize)
            .copied()
            .unwrap_or(0);
        if entry != 0 {
            self.consume(u32::from(entry >> 8))?;
            return Ok((entry & 0xFF) as u8);
        }
        for length in 9u32..=16 {
            let code = self.peek(length) as i32;
            let slot = length as usize;
            if code <= table.max_code.get(slot).copied().unwrap_or(-1) {
                let offset = table.value_offset.get(slot).copied().unwrap_or(0);
                let symbol = usize::try_from(code + offset)
                    .ok()
                    .and_then(|index| table.values.get(index))
                    .copied()
                    .ok_or(JpegError::Invalid("invalid Huffman code"))?;
                self.consume(length)?;
                return Ok(symbol);
            }
        }
        Err(JpegError::Invalid("invalid Huffman code"))
    }

    /// Read `size` (0..=15) bits as a JPEG signed magnitude value.
    fn receive_extend(&mut self, size: u8) -> Result<i32, JpegError> {
        if size == 0 {
            return Ok(0);
        }
        let size = u32::from(size.min(15));
        if self.count < size {
            self.refill();
        }
        let value = self.peek(size) as i32;
        self.consume(size)?;
        Ok(if value < 1 << (size - 1) {
            value - (1 << size) + 1
        } else {
            value
        })
    }

    fn restart(&mut self, expected: u8) -> Result<(), JpegError> {
        self.skip_to_marker()?;
        let mut next = self.pos;
        while self.data.get(next) == Some(&0xFF) {
            next = next.saturating_add(1);
        }
        let marker = self
            .data
            .get(next)
            .copied()
            .ok_or(JpegError::Invalid("missing restart marker"))?;
        if marker != 0xD0 + (expected & 7) {
            return Err(JpegError::Invalid("restart marker out of sequence"));
        }
        self.pos = next.saturating_add(1);
        self.bits = 0;
        self.count = 0;
        self.fill_bits = 0;
        self.fill_used = 0;
        self.at_marker = false;
        Ok(())
    }

    /// Drop buffered bits and move `pos` to the 0xFF that starts the next
    /// marker, skipping any stray bytes before it as libjpeg does.
    fn skip_to_marker(&mut self) -> Result<(), JpegError> {
        let missing = JpegError::Invalid("missing marker after entropy-coded data");
        loop {
            match self.data.get(self.pos) {
                None => return Err(missing),
                Some(0xFF) => {
                    let mut next = self.pos.saturating_add(1);
                    while self.data.get(next) == Some(&0xFF) {
                        next = next.saturating_add(1);
                    }
                    match self.data.get(next) {
                        None => return Err(missing),
                        Some(0) => self.pos = next.saturating_add(1),
                        Some(_) => return Ok(()),
                    }
                }
                Some(_) => self.pos = self.pos.saturating_add(1),
            }
        }
    }
}

/// Separable floating-point 8x8 inverse DCT.
struct Idct {
    /// `basis[x][u] = C(u) / 2 * cos((2x + 1) u pi / 16)`.
    basis: [[f32; 8]; 8],
}

impl Idct {
    fn new() -> Self {
        let mut basis = [[0.0; 8]; 8];
        for (x, row) in basis.iter_mut().enumerate() {
            for (u, value) in row.iter_mut().enumerate() {
                let scale = if u == 0 { FRAC_1_SQRT_2 } else { 1.0 };
                *value = 0.5 * scale * (((2 * x + 1) * u) as f32 * PI / 16.0).cos();
            }
        }
        Self { basis }
    }

    fn transform(&self, coefficients: &[i32; 64], out: &mut [u8; 64]) {
        if coefficients.iter().skip(1).all(|&value| value == 0) {
            let dc = coefficients.first().copied().unwrap_or(0);
            out.fill(clamp_sample(dc as f32 / 8.0));
            return;
        }
        // rows[v][x]: the horizontal transform of coefficient row v.
        let mut rows = [[0.0f32; 8]; 8];
        for (row, input) in rows.iter_mut().zip(coefficients.chunks_exact(8)) {
            if input.iter().all(|&value| value == 0) {
                continue;
            }
            for (value, basis) in row.iter_mut().zip(&self.basis) {
                *value = input
                    .iter()
                    .zip(basis)
                    .map(|(&coefficient, &weight)| coefficient as f32 * weight)
                    .sum();
            }
        }
        for (output, basis) in out.chunks_exact_mut(8).zip(&self.basis) {
            for (x, sample) in output.iter_mut().enumerate() {
                let value: f32 = rows
                    .iter()
                    .zip(basis)
                    .map(|(row, &weight)| row.get(x).copied().unwrap_or(0.0) * weight)
                    .sum();
                *sample = clamp_sample(value);
            }
        }
    }
}

fn clamp_sample(value: f32) -> u8 {
    (value + 128.0).round().clamp(0.0, 255.0) as u8
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::codecs::jpeg::JpegEncoder;
    use image::ExtendedColorType;
    use std::panic::{catch_unwind, AssertUnwindSafe};

    macro_rules! fixture {
        ($name:literal) => {
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/jpeg/",
                $name
            ))
            .as_slice()
        };
    }

    /// Dimensions of every image in tests/fixtures/jpeg.
    const FIXTURE_SIZE: (u32, u32) = (37, 29);

    /// `(jpeg, djpeg reference, components)` for the decodable fixtures.
    const REFERENCE_FIXTURES: [(&str, &[u8], &[u8], usize); 10] = [
        ("s444", fixture!("s444.jpg"), fixture!("s444.ppm"), 3),
        ("s422", fixture!("s422.jpg"), fixture!("s422.ppm"), 3),
        (
            "s420_restart",
            fixture!("s420_restart.jpg"),
            fixture!("s420_restart.ppm"),
            3,
        ),
        ("s440", fixture!("s440.jpg"), fixture!("s440.ppm"), 3),
        ("s31", fixture!("s31.jpg"), fixture!("s31.ppm"), 3),
        (
            "s32_odd",
            fixture!("s32_odd.jpg"),
            fixture!("s32_odd.ppm"),
            3,
        ),
        (
            "adobe_rgb",
            fixture!("adobe_rgb.jpg"),
            fixture!("adobe_rgb.ppm"),
            3,
        ),
        (
            "multiscan",
            fixture!("multiscan.jpg"),
            fixture!("multiscan.ppm"),
            3,
        ),
        (
            "sof1_q16",
            fixture!("sof1_q16.jpg"),
            fixture!("sof1_q16.ppm"),
            3,
        ),
        ("gray", fixture!("gray.jpg"), fixture!("gray.pgm"), 1),
    ];

    fn test_image(width: u32, height: u32, channels: usize) -> Vec<u8> {
        let mut state = 0x9e37_79b9_u32 ^ width.wrapping_mul(31) ^ height;
        let mut samples = Vec::new();
        for y in 0..height {
            for x in 0..width {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                let noise = (state >> 27) as u8;
                let edge = if x * 3 > y * 2 + width / 2 { 90 } else { 0 };
                let gradient = [
                    (x * 255 / width.max(2).saturating_sub(1).max(1)) as u8,
                    (y * 255 / height.max(2).saturating_sub(1).max(1)) as u8,
                    ((x + y) * 4) as u8,
                ];
                for channel in gradient.iter().take(channels) {
                    samples.push(channel.wrapping_add(edge).wrapping_add(noise));
                }
            }
        }
        samples
    }

    fn encode(samples: &[u8], width: u32, height: u32, channels: usize, quality: u8) -> Vec<u8> {
        let mut encoded = Vec::new();
        let color = if channels == 1 {
            ExtendedColorType::L8
        } else {
            ExtendedColorType::Rgb8
        };
        JpegEncoder::new_with_quality(&mut encoded, quality)
            .encode(samples, width, height, color)
            .unwrap();
        encoded
    }

    fn image_crate_decode(jpeg: &[u8], channels: usize) -> Vec<u8> {
        let image = image::load_from_memory_with_format(jpeg, image::ImageFormat::Jpeg).unwrap();
        if channels == 1 {
            image.to_luma8().into_raw()
        } else {
            image.to_rgb8().into_raw()
        }
    }

    /// Maximum and mean absolute per-sample difference.
    fn difference(ours: &[u8], reference: &[u8]) -> (u8, f64) {
        assert_eq!(ours.len(), reference.len());
        let mut max = 0;
        let mut total = 0u64;
        for (&a, &b) in ours.iter().zip(reference) {
            let delta = a.abs_diff(b);
            max = max.max(delta);
            total += u64::from(delta);
        }
        (max, total as f64 / ours.len() as f64)
    }

    fn parse_pnm(bytes: &[u8]) -> (u32, u32, Vec<u8>) {
        let mut fields = Vec::new();
        let mut position = 0;
        while fields.len() < 4 {
            while bytes[position].is_ascii_whitespace() {
                position += 1;
            }
            let start = position;
            while !bytes[position].is_ascii_whitespace() {
                position += 1;
            }
            fields.push(std::str::from_utf8(&bytes[start..position]).unwrap());
        }
        assert!(fields[0] == "P5" || fields[0] == "P6");
        assert_eq!(fields[3], "255");
        (
            fields[1].parse().unwrap(),
            fields[2].parse().unwrap(),
            bytes[position + 1..].to_vec(),
        )
    }

    /// Offset of the first segment with `marker`, walking the header.
    fn segment_offset(data: &[u8], marker: u8) -> usize {
        let mut position = 2;
        loop {
            assert_eq!(data[position], 0xFF);
            if data[position + 1] == marker {
                return position;
            }
            let length = usize::from(u16::from_be_bytes([data[position + 2], data[position + 3]]));
            position += 2 + length;
        }
    }

    fn decode_fixture(jpeg: &[u8], channels: usize) -> Result<Vec<u8>, JpegError> {
        decode(
            jpeg,
            FIXTURE_SIZE.0,
            FIXTURE_SIZE.1,
            channels,
            1 << 20,
            None,
        )
    }

    #[test]
    fn matches_image_crate_decoder() {
        let mut worst = 0;
        let mut worst_mean = 0.0f64;
        for (width, height) in [(1, 1), (7, 9), (16, 16), (17, 33), (33, 17), (640, 480)] {
            for channels in [1, 3] {
                for quality in [50, 90, 100] {
                    let samples = test_image(width, height, channels);
                    let jpeg = encode(&samples, width, height, channels, quality);
                    let ours = decode(&jpeg, width, height, channels, usize::MAX, None).unwrap();
                    let (max, mean) = difference(&ours, &image_crate_decode(&jpeg, channels));
                    assert!(
                        max <= 3 && mean < 0.5,
                        "{width}x{height}x{channels} q{quality}: max {max}, mean {mean}"
                    );
                    worst = worst.max(max);
                    worst_mean = worst_mean.max(mean);
                }
            }
        }
        println!("image crate reference: max diff {worst}, worst mean {worst_mean:.4}");
    }

    #[test]
    fn matches_libjpeg_reference_fixtures() {
        for (name, jpeg, reference, channels) in REFERENCE_FIXTURES {
            let (width, height, reference) = parse_pnm(reference);
            assert_eq!((width, height), FIXTURE_SIZE);
            let ours = decode_fixture(jpeg, channels).unwrap();
            let (max, mean) = difference(&ours, &reference);
            println!("{name}: max diff {max}, mean {mean:.4}");
            assert!(max <= 3 && mean < 0.1, "{name}: max {max}, mean {mean}");
        }
    }

    #[test]
    fn color_transform_parameter_takes_precedence() {
        let jpeg = fixture!("adobe_rgb.jpg");
        let (_, _, rgb) = parse_pnm(fixture!("adobe_rgb.ppm"));
        let forced = decode(jpeg, FIXTURE_SIZE.0, FIXTURE_SIZE.1, 3, 1 << 20, Some(true)).unwrap();
        assert!(difference(&forced, &rgb).0 > 20);
        let explicit = decode(
            jpeg,
            FIXTURE_SIZE.0,
            FIXTURE_SIZE.1,
            3,
            1 << 20,
            Some(false),
        )
        .unwrap();
        assert!(difference(&explicit, &rgb).0 <= 2);

        let jpeg = fixture!("s444.jpg");
        let (_, _, rgb) = parse_pnm(fixture!("s444.ppm"));
        let raw = decode(
            jpeg,
            FIXTURE_SIZE.0,
            FIXTURE_SIZE.1,
            3,
            1 << 20,
            Some(false),
        )
        .unwrap();
        let converted: Vec<u8> = raw
            .chunks_exact(3)
            .flat_map(|ycc| ycc_to_rgb(ycc[0], ycc[1], ycc[2]))
            .collect();
        assert_eq!(converted, decode_fixture(jpeg, 3).unwrap());
        assert!(difference(&raw, &rgb).0 > 20);
    }

    #[test]
    fn rejects_unsupported_coding_processes() {
        assert_eq!(
            decode_fixture(fixture!("progressive.jpg"), 3),
            Err(JpegError::Unsupported("progressive JPEG"))
        );
        assert_eq!(
            decode_fixture(fixture!("arithmetic.jpg"), 3),
            Err(JpegError::Unsupported("arithmetic-coded JPEG"))
        );
        let base = fixture!("s444.jpg");
        let sof = segment_offset(base, 0xC0);
        let patch = |offset: usize, value: u8| {
            let mut patched = base.to_vec();
            patched[sof + offset] = value;
            decode_fixture(&patched, 3)
        };
        assert_eq!(
            patch(1, 0xC2),
            Err(JpegError::Unsupported("progressive JPEG"))
        );
        assert_eq!(patch(1, 0xC3), Err(JpegError::Unsupported("lossless JPEG")));
        assert_eq!(
            patch(1, 0xC5),
            Err(JpegError::Unsupported("hierarchical JPEG"))
        );
        assert_eq!(
            patch(1, 0xCB),
            Err(JpegError::Unsupported("arithmetic-coded JPEG"))
        );
        assert_eq!(
            patch(4, 12),
            Err(JpegError::Unsupported("12-bit precision"))
        );
        assert_eq!(
            patch(9, 4),
            Err(JpegError::Unsupported("2 or 4 components (CMYK/YCCK)"))
        );
        assert_eq!(
            patch(9, 2),
            Err(JpegError::Unsupported("2 or 4 components (CMYK/YCCK)"))
        );
        let mut dnl = base.to_vec();
        dnl[sof + 5] = 0;
        dnl[sof + 6] = 0;
        assert_eq!(
            decode_fixture(&dnl, 3),
            Err(JpegError::Unsupported("height defined by DNL"))
        );
    }

    #[test]
    fn rejects_mismatched_dimensions_and_components() {
        let jpeg = fixture!("s444.jpg");
        for (width, height) in [(36, 29), (37, 30), (1, 1)] {
            assert!(matches!(
                decode(jpeg, width, height, 3, 1 << 20, None),
                Err(JpegError::Invalid(_))
            ));
        }
        assert!(matches!(
            decode(jpeg, FIXTURE_SIZE.0, FIXTURE_SIZE.1, 1, 1 << 20, None),
            Err(JpegError::Invalid(_))
        ));
        assert!(matches!(
            decode(
                fixture!("gray.jpg"),
                FIXTURE_SIZE.0,
                FIXTURE_SIZE.1,
                3,
                1 << 20,
                None
            ),
            Err(JpegError::Invalid(_))
        ));
        assert!(matches!(
            decode(jpeg, FIXTURE_SIZE.0, FIXTURE_SIZE.1, 4, 1 << 20, None),
            Err(JpegError::Unsupported(_))
        ));
    }

    #[test]
    fn enforces_output_limit_before_allocating() {
        let jpeg = fixture!("s444.jpg");
        let exact = 37 * 29 * 3;
        assert!(decode(jpeg, 37, 29, 3, exact, None).is_ok());
        assert_eq!(
            decode(jpeg, 37, 29, 3, exact - 1, None),
            Err(JpegError::Limit)
        );
        // Larger than any allocation could satisfy; must fail before parsing.
        assert_eq!(
            decode(jpeg, u32::MAX, u32::MAX, 3, usize::MAX, None),
            Err(JpegError::Limit)
        );
        assert_eq!(
            decode(jpeg, 65_535, 65_535, 3, 64 << 20, None),
            Err(JpegError::Limit)
        );
    }

    #[test]
    fn truncated_entropy_data_is_an_error() {
        for (name, jpeg, _, channels) in REFERENCE_FIXTURES {
            let scan = segment_offset(jpeg, 0xDA);
            let entropy_start = scan + 2 + usize::from(jpeg[scan + 3]);
            let entropy_end = jpeg.len() - 2;
            for cut in entropy_start..entropy_end {
                let mut truncated = jpeg[..cut].to_vec();
                truncated.extend_from_slice(&[0xFF, 0xD9]);
                assert!(
                    decode_fixture(&truncated, channels).is_err(),
                    "{name} cut at {cut}"
                );
            }
        }
    }

    fn robustness_corpus() -> Vec<(Vec<u8>, u32, u32, usize)> {
        let mut corpus: Vec<_> = REFERENCE_FIXTURES
            .iter()
            .map(|&(_, jpeg, _, channels)| {
                (jpeg.to_vec(), FIXTURE_SIZE.0, FIXTURE_SIZE.1, channels)
            })
            .collect();
        for (width, height, channels) in [(23, 17, 3), (9, 40, 1)] {
            let samples = test_image(width, height, channels);
            corpus.push((
                encode(&samples, width, height, channels, 75),
                width,
                height,
                channels,
            ));
        }
        corpus
    }

    fn decode_without_panic(
        jpeg: &[u8],
        width: u32,
        height: u32,
        channels: usize,
    ) -> Result<Vec<u8>, JpegError> {
        match catch_unwind(AssertUnwindSafe(|| {
            decode(jpeg, width, height, channels, 1 << 20, None)
        })) {
            Ok(result) => result,
            Err(_) => panic!("decoder panicked on {} bytes: {:02x?}", jpeg.len(), jpeg),
        }
    }

    #[test]
    fn truncated_prefixes_fail_without_panicking() {
        let mut prefixes = 0;
        for (jpeg, width, height, channels) in robustness_corpus() {
            assert!(decode_without_panic(&jpeg, width, height, channels).is_ok());
            for len in 0..jpeg.len() {
                assert!(
                    decode_without_panic(&jpeg[..len], width, height, channels).is_err(),
                    "prefix {len} of {} decoded",
                    jpeg.len()
                );
                prefixes += 1;
            }
        }
        println!("{prefixes} truncated prefixes rejected");
    }

    #[test]
    fn mutated_streams_never_panic() {
        const MUTATIONS: usize = 20_000;
        let corpus = robustness_corpus();
        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let (mut ok, mut err) = (0usize, 0usize);
        for _ in 0..MUTATIONS {
            let (base, width, height, channels) = &corpus[next() as usize % corpus.len()];
            let mut data = base.clone();
            for _ in 0..1 + next() % 4 {
                // Half the edits land in the first 700 bytes, where the
                // marker segments are.
                let span = if next() % 2 == 0 {
                    data.len().min(700)
                } else {
                    data.len()
                };
                let position = next() as usize % span.max(1);
                let value = next() as u8;
                match next() % 5 {
                    0 if position < data.len() => data[position] ^= 1 << (value % 8),
                    1 if position < data.len() => data[position] = value,
                    2 if position < data.len() => {
                        data[position] = [0x00, 0xFF, 0xD9, 0xDA][usize::from(value % 4)]
                    }
                    3 => data.insert(position.min(data.len()), value),
                    _ if position < data.len() => {
                        data.remove(position);
                    }
                    _ => {}
                }
            }
            match decode_without_panic(&data, *width, *height, *channels) {
                Ok(samples) => {
                    assert_eq!(samples.len(), *width as usize * *height as usize * channels);
                    ok += 1;
                }
                Err(_) => err += 1,
            }
        }
        println!("{MUTATIONS} mutations: {ok} decoded, {err} rejected, 0 panics");
    }
}
