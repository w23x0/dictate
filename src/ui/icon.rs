//! 托盘图标：程序图标按代码画，不依赖外部资源文件。

/// 画一个 64×64 的圆角方块 + 白色麦克风。返回 RGBA8。
pub fn rgba() -> Vec<u8> {
    const S: i32 = 64;
    let mut px = vec![0u8; (S * S * 4) as usize];
    let bg = [0x2f, 0x6f, 0xeb, 0xff];
    let fg = [0xff, 0xff, 0xff, 0xff];
    let r = 14.0f32; // 圆角半径
    for y in 0..S {
        for x in 0..S {
            let i = ((y * S + x) * 4) as usize;
            // 圆角矩形
            let cx = (x as f32 + 0.5).clamp(r, S as f32 - r);
            let cy = (y as f32 + 0.5).clamp(r, S as f32 - r);
            let d = ((x as f32 + 0.5 - cx).powi(2) + (y as f32 + 0.5 - cy).powi(2)).sqrt();
            if d > r {
                continue;
            }
            let mut color = bg;

            // 麦克风主体：竖胶囊
            let mx = (x as f32 + 0.5 - 32.0).abs();
            let my = y as f32 + 0.5;
            let in_capsule = mx <= 7.0 && (18.0..=38.0).contains(&my)
                || (my < 18.0 && (mx * mx + (18.0 - my).powi(2)) <= 49.0)
                || (my > 38.0 && (mx * mx + (my - 38.0).powi(2)) <= 49.0);
            // 支架：下方圆弧 + 立柱
            let arc = {
                let dx = x as f32 + 0.5 - 32.0;
                let dy = y as f32 + 0.5 - 40.0;
                let dist = (dx * dx + dy * dy).sqrt();
                dy >= 0.0 && (14.0..=17.0).contains(&dist) && dy < 14.0
            };
            let stand = (x as f32 + 0.5 - 32.0).abs() <= 2.0 && (48.0..=54.0).contains(&my);
            if in_capsule || arc || stand {
                color = fg;
            }
            px[i..i + 4].copy_from_slice(&color);
        }
    }
    px
}

/// 写成 PNG（给 .desktop 的图标目录用）。
pub fn write_png(path: &std::path::Path) -> anyhow::Result<()> {
    const S: u32 = 64;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let file = std::fs::File::create(path)?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), S, S);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header()?;
    writer.write_image_data(&rgba())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_has_opaque_pixels_and_transparent_corners() {
        let px = rgba();
        assert_eq!(px.len(), 64 * 64 * 4);
        // 左上角（圆角外）应全透明
        assert_eq!(&px[0..4], &[0, 0, 0, 0]);
        // 中心应是白的（麦克风）
        let c = ((32 * 64 + 32) * 4) as usize;
        assert_eq!(&px[c..c + 4], &[255, 255, 255, 255]);
    }

    #[test]
    fn png_is_written() {
        let p = std::env::temp_dir().join(format!("dictate-icon-{}.png", std::process::id()));
        write_png(&p).unwrap();
        let bytes = std::fs::read(&p).unwrap();
        assert_eq!(&bytes[1..4], b"PNG");
        std::fs::remove_file(p).ok();
    }
}
