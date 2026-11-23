use std::io::Write;

use crate::capture::frame::Frame;

/// Writes one opaque RGB row at a time from the top-down BGRA buffer.
pub fn write_rgb(frame: &Frame, output: impl Write) -> Result<(), ::png::EncodingError> {
    write_rgb_with_compression(frame, output, Compression::Fast)
}

#[derive(Clone, Copy)]
pub enum Compression {
    Fast,
    Small,
}

pub fn write_rgb_with_compression(
    frame: &Frame,
    output: impl Write,
    compression: Compression,
) -> Result<(), ::png::EncodingError> {
    let mut encoder = ::png::Encoder::new(output, frame.width, frame.height);
    encoder.set_color(::png::ColorType::Rgb);
    let (level, filter) = match compression {
        Compression::Fast => (::png::Compression::Fast, ::png::Filter::Sub),
        Compression::Small => (::png::Compression::Balanced, ::png::Filter::Paeth),
    };
    encoder.set_compression(level);
    encoder.set_filter(filter);
    let mut writer = encoder.write_header()?;
    let mut stream = writer.stream_writer()?;
    let mut row = vec![0; frame.width as usize * 3];
    for y in 0..frame.height {
        for (bgra, rgb) in frame.row(y).chunks_exact(4).zip(row.chunks_exact_mut(3)) {
            rgb.copy_from_slice(&[bgra[2], bgra[1], bgra[0]]);
        }
        stream.write_all(&row)?;
    }
    stream.finish()?;
    writer.finish()
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[test]
    fn png_keeps_row_order_and_converts_bgra_to_rgb() {
        let frame = Frame::new(
            2,
            2,
            vec![0, 0, 255, 0, 0, 255, 0, 96, 255, 0, 0, 128, 10, 20, 30, 255],
        )
        .expect("2×2 frame");
        let mut output = Vec::new();
        write_rgb(&frame, &mut output).expect("PNG encoded");
        let mut reader = ::png::Decoder::new(Cursor::new(output))
            .read_info()
            .expect("PNG header");
        let mut pixels = [0; 12];
        let info = reader.next_frame(&mut pixels).expect("PNG pixels");
        assert_eq!(
            (info.width, info.height, info.color_type),
            (2, 2, ::png::ColorType::Rgb)
        );
        assert_eq!(pixels, [255, 0, 0, 0, 255, 0, 0, 0, 255, 30, 20, 10]);
    }
}
