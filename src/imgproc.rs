use fast_image_resize::{FilterType, PixelType, Resizer};
use image::{codecs::gif::GifDecoder, AnimationDecoder, DynamicImage, RgbImage};
use std::{
    fs::File,
    io::{stdin, BufReader, Read},
    num::NonZeroU32,
    path::Path,
    time::Duration,
};

use utils::{
    communication::{self, Coord, Position},
    comp_decomp::BitPack,
};

use super::cli;

pub fn read_img(path: &Path) -> Result<(RgbImage, bool), String> {
    if let Some("-") = path.to_str() {
        let mut reader = BufReader::new(stdin());
        let mut buffer = Vec::new();
        if let Err(e) = reader.read_to_end(&mut buffer) {
            return Err(format!("failed to read stdin: {e}"));
        }

        return match image::load_from_memory(&buffer) {
            Ok(img) => Ok((img.into_rgb8(), false)),
            Err(e) => return Err(format!("failed load image from memory: {e}")),
        };
    }

    let imgbuf = match image::io::Reader::open(path) {
        Ok(img) => img,
        Err(e) => return Err(format!("failed to open image: {e}")),
    };

    let imgbuf = match imgbuf.with_guessed_format() {
        Ok(img) => img,
        Err(e) => return Err(format!("failed to detect the image's format: {e}")),
    };

    let is_gif = imgbuf.format() == Some(image::ImageFormat::Gif);
    match imgbuf.decode() {
        Ok(img) => Ok((img.into_rgb8(), is_gif)),
        Err(e) => Err(format!("failed to decode image: {e}")),
    }
}

#[inline]
pub fn frame_to_rgb(frame: image::Frame) -> RgbImage {
    DynamicImage::ImageRgba8(frame.into_buffer()).into_rgb8()
}

pub fn compress_frames(
    gif: GifDecoder<BufReader<File>>,
    dim: (u32, u32),
    filter: FilterType,
    no_resize: bool,
    color: &[u8; 3],
) -> Result<Vec<(BitPack, Duration)>, String> {
    let mut compressed_frames = Vec::new();
    let frames = gif.into_frames().collect_frames().unwrap();
    let frames: Vec<(Duration, Vec<u8>)> = frames
        .into_iter()
        .map(|fr| {
            let (dur_num, dur_div) = fr.delay().numer_denom_ms();
            let duration = Duration::from_millis((dur_num / dur_div).into());
            let img = if no_resize {
                img_pad(frame_to_rgb(fr), dim, color).unwrap()
            } else {
                img_resize(frame_to_rgb(fr), dim, filter).unwrap()
            };
            (duration, img)
        })
        .collect();
    let fr1 = frames.iter().cycle().take(frames.len());
    let fr2 = frames.iter().cycle().skip(1).take(frames.len());

    for (prev, cur) in fr1.zip(fr2) {
        compressed_frames.push((BitPack::pack(&prev.1, &cur.1)?, cur.0));
    }

    Ok(compressed_frames)
}

pub fn make_filter(filter: &cli::Filter) -> fast_image_resize::FilterType {
    match filter {
        cli::Filter::Nearest => fast_image_resize::FilterType::Box,
        cli::Filter::Bilinear => fast_image_resize::FilterType::Bilinear,
        cli::Filter::CatmullRom => fast_image_resize::FilterType::CatmullRom,
        cli::Filter::Mitchell => fast_image_resize::FilterType::Mitchell,
        cli::Filter::Lanczos3 => fast_image_resize::FilterType::Lanczos3,
    }
}

pub fn img_pad(
    mut img: RgbImage,
    dimensions: (u32, u32),
    color: &[u8; 3],
) -> Result<Vec<u8>, String> {
    let (padded_w, padded_h) = dimensions;
    let (padded_w, padded_h) = (padded_w as usize, padded_h as usize);
    let mut padded = Vec::with_capacity(padded_w * padded_w * 3);

    let img = image::imageops::crop(&mut img, 0, 0, dimensions.0, dimensions.1).to_image();
    let (img_w, img_h) = img.dimensions();
    let (img_w, img_h) = (img_w as usize, img_h as usize);
    let raw_img = img.into_vec();

    for _ in 0..(((padded_h - img_h) / 2) * padded_w) {
        padded.push(color[2]);
        padded.push(color[1]);
        padded.push(color[0]);
    }

    for row in 0..img_h {
        for _ in 0..(padded_w - img_w) / 2 {
            padded.push(color[2]);
            padded.push(color[1]);
            padded.push(color[0]);
        }

        for pixel in raw_img[(row * img_w * 3)..((row + 1) * img_w * 3)].chunks_exact(3) {
            padded.push(pixel[2]);
            padded.push(pixel[1]);
            padded.push(pixel[0]);
        }
        for _ in 0..(padded_w - img_w) / 2 {
            padded.push(color[2]);
            padded.push(color[1]);
            padded.push(color[0]);
        }
    }

    while padded.len() < (padded_h * padded_w * 3) {
        padded.push(color[2]);
        padded.push(color[1]);
        padded.push(color[0]);
    }

    Ok(padded)
}

pub fn img_resize(
    img: RgbImage,
    dimensions: (u32, u32),
    filter: FilterType,
) -> Result<Vec<u8>, String> {
    let (width, height) = dimensions;
    let (img_w, img_h) = img.dimensions();
    let mut resized_img = if (img_w, img_h) != (width, height) {
        let src = match fast_image_resize::Image::from_vec_u8(
            // We unwrap below because we know the images's dimensions should never be 0
            NonZeroU32::new(img_w).unwrap(),
            NonZeroU32::new(img_h).unwrap(),
            img.into_raw(),
            PixelType::U8x3,
        ) {
            Ok(i) => i,
            Err(e) => return Err(e.to_string()),
        };

        // We unwrap below because we know the outputs's dimensions should never be 0
        let new_w = NonZeroU32::new(width).unwrap();
        let new_h = NonZeroU32::new(height).unwrap();
        let mut src_view = src.view();
        src_view.set_crop_box_to_fit_dst_size(new_w, new_h, Some((0.5, 0.5)));

        let mut dst = fast_image_resize::Image::new(new_w, new_h, PixelType::U8x3);
        let mut dst_view = dst.view_mut();

        let mut resizer = Resizer::new(fast_image_resize::ResizeAlg::Convolution(filter));
        if let Err(e) = resizer.resize(&src_view, &mut dst_view) {
            return Err(e.to_string());
        }

        dst.into_vec()
    } else {
        img.into_vec()
    };

    // The ARGB is 'little endian', so here we must  put the order
    // of bytes 'in reverse', so it needs to be BGRA.
    eprintln!("Todo: fast rgb -> bgr conversion");
    for pixel in resized_img.chunks_exact_mut(3) {
        pixel.swap(0, 2);
    }

    Ok(resized_img)
}

pub fn make_transition(img: &cli::Img) -> communication::Transition {
    let mut angle = img.transition_angle;

    let x = match img.transition_pos.x {
        cli::CliCoord::Percent(x) => {
            if !(0.0..=1.0).contains(&x) {
                println!(
                    "Warning: x value not in range [0,1] position might be set outside screen: {x}"
                );
            }
            Coord::Percent(x)
        }
        cli::CliCoord::Pixel(x) => Coord::Pixel(x),
    };

    let y = match img.transition_pos.y {
        cli::CliCoord::Percent(y) => {
            if !(0.0..=1.0).contains(&y) {
                println!(
                    "Warning: y value not in range [0,1] position might be set outside screen: {y}"
                );
            }
            Coord::Percent(y)
        }
        cli::CliCoord::Pixel(y) => Coord::Pixel(y),
    };

    let mut pos = Position::new(x, y);

    let transition_type = match img.transition_type {
        cli::TransitionType::Simple => communication::TransitionType::Simple,
        cli::TransitionType::Wipe => communication::TransitionType::Wipe,
        cli::TransitionType::Outer => communication::TransitionType::Outer,
        cli::TransitionType::Grow => communication::TransitionType::Grow,
        cli::TransitionType::Wave => communication::TransitionType::Wave,
        cli::TransitionType::Right => {
            angle = 0.0;
            communication::TransitionType::Wipe
        }
        cli::TransitionType::Top => {
            angle = 90.0;
            communication::TransitionType::Wipe
        }
        cli::TransitionType::Left => {
            angle = 180.0;
            communication::TransitionType::Wipe
        }
        cli::TransitionType::Bottom => {
            angle = 270.0;
            communication::TransitionType::Wipe
        }
        cli::TransitionType::Center => {
            pos = Position::new(Coord::Percent(0.5), Coord::Percent(0.5));
            communication::TransitionType::Grow
        }
        cli::TransitionType::Any => {
            pos = Position::new(
                Coord::Percent(rand::random::<f32>()),
                Coord::Percent(rand::random::<f32>()),
            );
            if rand::random::<u8>() % 2 == 0 {
                communication::TransitionType::Grow
            } else {
                communication::TransitionType::Outer
            }
        }
        cli::TransitionType::Random => {
            pos = Position::new(
                Coord::Percent(rand::random::<f32>()),
                Coord::Percent(rand::random::<f32>()),
            );
            angle = rand::random();
            match rand::random::<u8>() % 4 {
                0 => communication::TransitionType::Simple,
                1 => communication::TransitionType::Wipe,
                2 => communication::TransitionType::Outer,
                3 => communication::TransitionType::Grow,
                _ => unreachable!(),
            }
        }
    };

    communication::Transition {
        duration: img.transition_duration,
        step: img.transition_step,
        fps: img.transition_fps,
        bezier: img.transition_bezier,
        angle,
        pos,
        transition_type,
        wave: img.transition_wave,
    }
}
