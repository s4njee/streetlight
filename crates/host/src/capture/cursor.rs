//! Bounds-checked parser for native-endian SPA cursor metadata (including row padding).
use streetlight_proto::cursor::Bitmap;
#[derive(Debug)]
pub struct Update {
    pub x: i32,
    pub y: i32,
    pub visible: bool,
    pub bitmap: Option<Bitmap>,
}
fn u32_at(b: &[u8], n: usize) -> Option<u32> {
    Some(u32::from_ne_bytes(b.get(n..n + 4)?.try_into().ok()?))
}
fn i32_at(b: &[u8], n: usize) -> Option<i32> {
    Some(u32_at(b, n)? as i32)
}
pub fn parse(b: &[u8], bgra: u32, rgba: u32) -> Option<Update> {
    if b.len() < 28 {
        return None;
    }
    let id = u32_at(b, 0)?;
    if id == 0 {
        return None;
    }
    let (x, y) = (i32_at(b, 8)?, i32_at(b, 12)?);
    let mut out = Update {
        x,
        y,
        visible: x >= 0 && y >= 0,
        bitmap: None,
    };
    let offset = u32_at(b, 24)? as usize;
    if offset == 0 {
        return Some(out);
    }
    if offset < 28 {
        return None;
    }
    let bitmap = b.get(offset..)?;
    let format = u32_at(bitmap, 0)?;
    if format == 0 {
        return Some(out);
    }
    let pixels = u32_at(bitmap, 16)? as usize;
    if pixels == 0 {
        out.visible = false;
        return Some(out);
    }
    let (width, height) = (u32_at(bitmap, 4)?, u32_at(bitmap, 8)?);
    let stride = i32_at(bitmap, 12)?;
    if pixels < 20
        || width == 0
        || height == 0
        || width > 256
        || height > 256
        || stride < (width * 4) as i32
    {
        return None;
    }
    if format != bgra && format != rgba {
        return None;
    }
    let mut data = Vec::with_capacity(width as usize * height as usize * 4);
    for row in 0..height as usize {
        let start = pixels.checked_add(row.checked_mul(stride as usize)?)?;
        let line = bitmap.get(start..start.checked_add(width as usize * 4)?)?;
        for pixel in line.as_chunks::<4>().0 {
            if format == bgra {
                data.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
            } else {
                data.extend_from_slice(pixel);
            }
        }
    }
    let bitmap = Bitmap {
        epoch: 0,
        id,
        width,
        height,
        hotspot_x: i32_at(b, 16)?,
        hotspot_y: i32_at(b, 20)?,
        rgba: data,
    };
    if !bitmap.valid() {
        return None;
    }
    out.bitmap = Some(bitmap);
    Some(out)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_padded_rows_and_checks_offsets_shapes_and_truncation() {
        let mut b = vec![0u8; 60];
        for (at, v) in [
            (0, 1),
            (8, 10),
            (12, 20),
            (24, 28),
            (28, 7),
            (32, 1),
            (36, 1),
            (40, 8),
            (44, 20),
        ] {
            b[at..at + 4].copy_from_slice(&u32::to_ne_bytes(v));
        }
        b[48..52].copy_from_slice(&[1, 2, 3, 255]);
        let u = parse(&b, 7, 8).unwrap();
        assert_eq!((u.x, u.y), (10, 20));
        assert_eq!(u.bitmap.unwrap().rgba, [3, 2, 1, 255]);
        for length in 0..52 {
            assert!(parse(&b[..length], 7, 8).is_none());
        }
        b[44..48].copy_from_slice(&0u32.to_ne_bytes());
        assert!(!parse(&b, 7, 8).unwrap().visible);
        b[24..28].copy_from_slice(&0u32.to_ne_bytes());
        assert!(parse(&b, 7, 8).unwrap().bitmap.is_none());
        b[0..4].fill(0);
        assert!(parse(&b, 7, 8).is_none());
    }
}
